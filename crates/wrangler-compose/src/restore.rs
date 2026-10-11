//! `restore [--drop]` (DES-013): contents only; `up` makes the containers.

use crate::backup::{read_manifest, unpairs};
use crate::confirm;
use crate::d1;
use crate::files::sha256_file;
use crate::project::Project;
use crate::r2::{self, Store};
use crate::up::{dry_run_done, read_env_file, say};
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
        .ok_or_else(|| anyhow!(tr!("環境の指定が要ります", "an environment is required")))?;
    let manifest = read_manifest(from)?;
    if drop && !project.dry_run {
        confirm::require_terminal("restore --drop")?;
    }
    read_env_file(project)?;
    let unchanged = || tr!("何も変えていません", "nothing was changed");

    // Match backup parts to this environment's bindings.
    let mut d1_plan = Vec::new();
    for entry in manifest["d1"].as_array().into_iter().flatten() {
        let binding = entry["binding"].as_str().unwrap_or_default();
        let target = project
            .config
            .d1
            .iter()
            .find(|d| d.binding == binding)
            .ok_or_else(|| {
                anyhow!(tr!(
                    "控えには D1 {binding} がありますが、環境 {env} にはそのバインディングがありません。{}",
                    "the backup has D1 {binding}, but environment {env} has no such binding; {}",
                    unchanged()
                ))
            })?;
        let file = from.join(entry["file"].as_str().unwrap_or_default());
        if entry["sha256"].as_str() != Some(sha256_file(&file)?.as_str()) {
            bail!(tr!(
                "{} が控えの目録と合いません。{}",
                "{} does not match the backup manifest; {}",
                file.display(),
                unchanged()
            ));
        }
        let sql = fs::read_to_string(&file)
            .with_context(|| tr!("{} を読めません", "cannot read {}", file.display()))?;
        if !sql.contains("\"d1_migrations\"") && !sql.contains("d1_migrations(") {
            bail!(tr!(
                "{} には移行の履歴(d1_migrations)が無いので、戻したあとに残りの移行を当てられません。{}",
                "{} has no migration history (d1_migrations), so later migrations could not be applied after restoring; {}",
                file.display(),
                unchanged()
            ));
        }
        d1_plan.push((target, file));
    }
    let mut r2_plan = Vec::new();
    let mut creds = None;
    let account = if manifest["r2"].as_array().is_some_and(|a| !a.is_empty()) {
        creds = Some(
            r2::credentials(project)
                .ok_or_else(|| anyhow!("{}; {}", r2::missing_keys_message(), unchanged()))?,
        );
        Some(r2::account_id(project)?)
    } else {
        None
    };
    for entry in manifest["r2"].as_array().into_iter().flatten() {
        let binding = entry["binding"].as_str().unwrap_or_default();
        let target = project
            .config
            .r2
            .iter()
            .find(|b| b.binding == binding)
            .ok_or_else(|| {
                anyhow!(tr!(
                    "控えには R2 {binding} がありますが、環境 {env} にはそのバインディングがありません。{}",
                    "the backup has R2 {binding}, but environment {env} has no such binding; {}",
                    unchanged()
                ))
            })?;
        // Check every backed-up object now, before anything is deleted.
        for o in entry["objects"].as_array().into_iter().flatten() {
            crate::interrupt::check()?;
            if o["size"].as_u64().unwrap_or(0) > MAX_SINGLE_PUT {
                bail!(tr!(
                    "オブジェクト {} は 5 GiB を超えていて、restore ではまだ上げられません。{}",
                    "object {} is larger than 5 GiB, which restore cannot upload yet; {}",
                    o["key"],
                    unchanged()
                ));
            }
            let file = from.join(o["file"].as_str().unwrap_or_default());
            if !file.is_file() || o["sha256"].as_str() != Some(sha256_file(&file)?.as_str()) {
                bail!(tr!(
                    "{}(オブジェクト {})が無いか、控えの目録と合いません。{}",
                    "{} (object {}) is missing or does not match the backup manifest; {}",
                    file.display(),
                    o["key"],
                    unchanged()
                ));
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
            bail!(tr!(
                "D1 {} がありません。先に `wrangler-compose --env {env} up` を打ってください({})",
                "D1 {} does not exist; run `wrangler-compose --env {env} up` first ({})",
                d.name,
                unchanged()
            ));
        }
        let tables = d1::user_tables(project, d)?;
        if !tables.is_empty() {
            busy.push(tr!(
                "D1 {}: 表 {} 個",
                "D1 {}: {} tables",
                d.name,
                tables.len()
            ));
        }
    }
    for (s, _) in &r2_plan {
        let has_objects = s.check_access().map_err(|e| {
            anyhow!(tr!(
                "{e:#}。バケットが無ければ、先に `wrangler-compose --env {env} up` を打ってください({})",
                "{e:#}; run `wrangler-compose --env {env} up` first if the bucket is missing ({})",
                unchanged()
            ))
        })?;
        if has_objects {
            busy.push(tr!(
                "R2 {}: オブジェクト {} 件",
                "R2 {}: {} objects",
                s.bucket_name,
                s.list_all()?.len()
            ));
        }
    }
    if !busy.is_empty() && !drop {
        bail!(tr!(
            "戻す先が空ではありません({})。消して控えに入れ替えるなら `restore --drop` を使ってください({})",
            "the target is not empty ({}); use `restore --drop` to delete it and replace it with the backup ({})",
            busy.join(", "),
            unchanged()
        ));
    }
    if drop {
        say(tr!(
            "`restore --drop` は {env} のいまのデータを消して、控えに入れ替えます:",
            "`restore --drop` deletes the current data of {env} and replaces it with the backup:"
        ));
        for b in &busy {
            eprintln!("  {b}");
        }
        if busy.is_empty() {
            eprintln!(
                "  {}",
                tr!("(戻す先はすでに空です)", "(the target is already empty)")
            );
        }
    }
    say(tr!(
        "restore の間も Worker は動いているので、終わるまでは欠けたデータが見えることがあります",
        "the Worker keeps serving while restore runs, so users may see missing data until it finishes"
    ));
    if project.dry_run {
        say(tr!(
            "{} からの `restore` の dry-run:",
            "dry run of `restore` from {}:",
            from.display()
        ));
        for (d, _) in &d1_plan {
            eprintln!(
                "  {}",
                tr!(
                    "D1 {} ({}): 控えを入れ、そのあと残りの移行を当てます",
                    "D1 {} ({}): would load the backup, then apply later migrations",
                    d.name,
                    d.binding
                )
            );
        }
        for (s, entry) in &r2_plan {
            let n = entry["objects"].as_array().map_or(0, Vec::len);
            eprintln!(
                "  {}",
                tr!(
                    "R2 {} ({}): オブジェクト {n} 件を上げます(すべて目録と照らし済み)",
                    "R2 {} ({}): would upload {n} objects (all checked against the manifest)",
                    s.bucket_name,
                    s.binding
                )
            );
        }
        dry_run_done();
        return Ok(());
    }
    if drop {
        confirm::retype_env(&env, &tr!("データを入れ替える", "replace its data"))?;
    }

    let mut done: Vec<String> = Vec::new();
    let result = (|| -> Result<()> {
        for (d, file) in &d1_plan {
            if drop {
                d1::drop_everything(project, d)?;
                done.push(tr!("D1 {} を空にした", "emptied D1 {}", d.name));
            }
            let f = file.to_string_lossy().into_owned();
            project.wrangler([
                "d1", "execute", &d.binding, "--remote", "--yes", "--file", &f,
            ])?;
            done.push(tr!("D1 {} を戻した", "restored D1 {}", d.name));
        }
        for (s, entry) in &r2_plan {
            if drop {
                let mut n = 0;
                let emptied = s.delete_all(&mut n);
                done.push(tr!(
                    "R2 {} のオブジェクト {n} 件を消した",
                    "deleted {n} objects in R2 {}",
                    s.bucket_name
                ));
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
                    done.push(tr!(
                        "R2 {bucket} へオブジェクト {total} 件中 {i} 件を上げた",
                        "uploaded {i} of {total} objects to R2 {bucket}",
                        bucket = s.bucket_name,
                        total = objects.len()
                    ));
                    return Err(e);
                }
            }
            done.push(tr!(
                "R2 {} を戻した(オブジェクト {} 件)",
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
            say(tr!(
                "{env} を {} から戻しました",
                "restored {env} from {}",
                from.display()
            ));
            Ok(())
        }
        Err(e) => {
            let did = if done.is_empty() {
                tr!("何もしていません", "nothing")
            } else {
                done.join(", ")
            };
            Err(e.context(tr!(
                "途中で止まりました。{did}。残りは戻していません",
                "stopped part way: {did}; the rest was not restored"
            )))
        }
    }
}
