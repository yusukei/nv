//! `restore [--drop]` (DES-013): contents only; `up` makes the containers.

use crate::backup::{read_manifest, unpairs};
use crate::confirm;
use crate::d1;
use crate::files::sha256_file;
use crate::project::Project;
use crate::r2::{self, Store};
use crate::up::read_env_file;
use anyhow::{Context, Result, anyhow, bail};
use std::fs;
use std::path::Path;

/// R2 accepts up to 5 GiB in one PutObject; bigger objects need multipart
/// upload, which wrangler-compose does not do yet (design: unverified list).
const MAX_SINGLE_PUT: u64 = 5 * 1024 * 1024 * 1024;

pub fn restore(project: &Project, from: &Path, drop: bool) -> Result<()> {
    let env = project
        .env
        .clone()
        .ok_or_else(|| anyhow!("an environment is required"))?;
    let manifest = read_manifest(from)?;
    if drop {
        confirm::require_terminal("restore --drop")?;
    }
    read_env_file(project)?;

    // Match backup parts to this environment's bindings.
    let mut d1_plan = Vec::new();
    for entry in manifest["d1"].as_array().into_iter().flatten() {
        let binding = entry["binding"].as_str().unwrap_or_default();
        let target = project.config.d1.iter().find(|d| d.binding == binding).ok_or_else(|| {
            anyhow!("the backup has D1 {binding}, but environment {env} has no such binding; nothing was changed")
        })?;
        let file = from.join(entry["file"].as_str().unwrap_or_default());
        if entry["sha256"].as_str() != Some(sha256_file(&file)?.as_str()) {
            bail!(
                "{} does not match the backup manifest; nothing was changed",
                file.display()
            );
        }
        let sql =
            fs::read_to_string(&file).with_context(|| format!("cannot read {}", file.display()))?;
        if !sql.contains("\"d1_migrations\"") && !sql.contains("d1_migrations(") {
            bail!(
                "{} has no migration history (d1_migrations), so later migrations could not be applied after restoring; nothing was changed",
                file.display()
            );
        }
        d1_plan.push((target, file));
    }
    let mut r2_plan = Vec::new();
    let mut creds = None;
    let account = if manifest["r2"].as_array().is_some_and(|a| !a.is_empty()) {
        creds = Some(
            r2::credentials(project)
                .ok_or_else(|| anyhow!("{}; nothing was changed", r2::missing_keys_message()))?,
        );
        Some(r2::account_id(project)?)
    } else {
        None
    };
    for entry in manifest["r2"].as_array().into_iter().flatten() {
        let binding = entry["binding"].as_str().unwrap_or_default();
        let target = project.config.r2.iter().find(|b| b.binding == binding).ok_or_else(|| {
            anyhow!("the backup has R2 {binding}, but environment {env} has no such binding; nothing was changed")
        })?;
        // Check every backed-up object now, before anything is deleted.
        for o in entry["objects"].as_array().into_iter().flatten() {
            crate::interrupt::check()?;
            if o["size"].as_u64().unwrap_or(0) > MAX_SINGLE_PUT {
                bail!(
                    "object {} is larger than 5 GiB, which restore cannot upload yet; nothing was changed",
                    o["key"]
                );
            }
            let file = from.join(o["file"].as_str().unwrap_or_default());
            if !file.is_file() || o["sha256"].as_str() != Some(sha256_file(&file)?.as_str()) {
                bail!(
                    "{} (object {}) is missing or does not match the backup manifest; nothing was changed",
                    file.display(),
                    o["key"]
                );
            }
        }
        let store = Store::new(account.as_ref().unwrap(), target, creds.as_ref().unwrap())?;
        r2_plan.push((store, entry));
    }

    // Containers must exist (`up` makes them); then check they are empty.
    let remote = d1::remote(project)?;
    let mut busy = Vec::new();
    for (d, _) in &d1_plan {
        if !d1::exists(d, &remote) {
            bail!(
                "D1 {} does not exist; run `wrangler-compose --env {env} up` first (nothing was changed)",
                d.name
            );
        }
        let tables = d1::user_tables(project, d)?;
        if !tables.is_empty() {
            busy.push(format!("D1 {}: {} tables", d.name, tables.len()));
        }
    }
    for (s, _) in &r2_plan {
        let has_objects = s
            .check_access()
            .map_err(|e| anyhow!("{e:#}; run `wrangler-compose --env {env} up` first if the bucket is missing (nothing was changed)"))?;
        if has_objects {
            busy.push(format!(
                "R2 {}: {} objects",
                s.bucket_name,
                s.list_all()?.len()
            ));
        }
    }
    if !busy.is_empty() && !drop {
        bail!(
            "the target is not empty ({}); use `restore --drop` to delete it and replace it with the backup (nothing was changed)",
            busy.join(", ")
        );
    }
    if drop {
        eprintln!(
            "wrangler-compose: `restore --drop` deletes the current data of {env} and replaces it with the backup:"
        );
        for b in &busy {
            eprintln!("  {b}");
        }
        if busy.is_empty() {
            eprintln!("  (the target is already empty)");
        }
    }
    eprintln!(
        "wrangler-compose: the Worker keeps serving while restore runs, so users may see missing data until it finishes"
    );
    if drop {
        confirm::retype_env(&env, "replace its data")?;
    }

    let mut done: Vec<String> = Vec::new();
    let result = (|| -> Result<()> {
        for (d, file) in &d1_plan {
            if drop {
                d1::drop_everything(project, d)?;
                done.push(format!("emptied D1 {}", d.name));
            }
            let f = file.to_string_lossy().into_owned();
            project.wrangler([
                "d1", "execute", &d.binding, "--remote", "--yes", "--file", &f,
            ])?;
            done.push(format!("restored D1 {}", d.name));
        }
        for (s, entry) in &r2_plan {
            if drop {
                let mut n = 0;
                let emptied = s.delete_all(&mut n);
                done.push(format!("deleted {n} objects in R2 {}", s.bucket_name));
                emptied?;
            }
            let objects = entry["objects"].as_array().cloned().unwrap_or_default();
            for (i, o) in objects.iter().enumerate() {
                let key = o["key"].as_str().unwrap_or_default();
                let file = from.join(o["file"].as_str().unwrap_or_default());
                let put = crate::interrupt::check().and_then(|_| {
                    s.upload(
                        key,
                        &file,
                        &unpairs(&o["headers"]),
                        &unpairs(&o["metadata"]),
                    )
                });
                if let Err(e) = put {
                    done.push(format!(
                        "uploaded {i} of {} objects to R2 {}",
                        objects.len(),
                        s.bucket_name
                    ));
                    return Err(e);
                }
            }
            done.push(format!(
                "restored R2 {} ({} objects)",
                s.bucket_name,
                objects.len()
            ));
        }
        d1::apply_migrations(project)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            eprintln!("wrangler-compose: restored {env} from {}", from.display());
            Ok(())
        }
        Err(e) => {
            let did = if done.is_empty() {
                "nothing".to_string()
            } else {
                done.join(", ")
            };
            Err(e.context(format!(
                "stopped part way: {did}; the rest was not restored"
            )))
        }
    }
}
