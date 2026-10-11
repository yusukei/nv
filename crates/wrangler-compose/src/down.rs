//! `down` (DES-009) and `down --purge` (DES-011).

use crate::confirm;
use crate::d1;
use crate::interrupt;
use crate::project::Project;
use crate::r2::{self, Store};
use crate::up::read_env_file;
use anyhow::{Result, anyhow};

fn secret_names(project: &Project) -> Vec<String> {
    // A Worker that was never deployed has no secrets to list.
    project
        .wrangler_json(["secret", "list", "--format", "json"])
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect()
}

fn show_worker_losses(project: &Project) {
    let c = &project.config;
    eprintln!("  Worker:          {}", c.worker);
    let secrets = secret_names(project);
    let list = |v: &[String]| {
        if v.is_empty() {
            "(none)".to_string()
        } else {
            v.join(", ")
        }
    };
    eprintln!("  secrets:         {}", list(&secrets));
    eprintln!("  routes/domains:  {}", list(&c.routes));
    eprintln!("  cron triggers:   {}", list(&c.crons));
    eprintln!(
        "  Durable Objects: {} (their stored data is deleted with the Worker)",
        list(&c.durable_objects)
    );
}

pub fn down(project: &Project) -> Result<()> {
    let env = project
        .env
        .clone()
        .ok_or_else(|| anyhow!("an environment is required"))?;
    confirm::require_terminal("down")?;
    interrupt::check()?;
    eprintln!(
        "wrangler-compose: `down` deletes the Worker and everything below. D1 and R2 data are kept."
    );
    show_worker_losses(project);
    confirm::retype_env(&env, "delete the Worker")?;
    project.wrangler_interactive(["delete"])?;
    eprintln!(
        "wrangler-compose: deleted {}; D1 and R2 were kept, `up` brings it back",
        project.config.worker
    );
    Ok(())
}

pub fn purge(project: &Project) -> Result<()> {
    let env = project
        .env
        .clone()
        .ok_or_else(|| anyhow!("an environment is required"))?;
    confirm::require_terminal("down --purge")?;
    // Validates that .env.<env> is kept out of git before reading keys from it.
    read_env_file(project)?;

    // Keys first: without working keys nothing at all is deleted (REQ-006).
    let mut stores = Vec::new();
    if !project.config.r2.is_empty() {
        let creds = r2::credentials(project)
            .ok_or_else(|| anyhow!("{}; nothing was deleted", r2::missing_keys_message()))?;
        let account = r2::account_id(project)?;
        for b in &project.config.r2 {
            let store = Store::new(&account, b, &creds)?;
            store
                .check_access()
                .map_err(|e| anyhow!("{e:#}; nothing was deleted"))?;
            stores.push(store);
        }
    }
    let mut counts = Vec::new();
    for s in &stores {
        counts.push(s.list_all()?.len());
    }

    eprintln!("wrangler-compose: `down --purge` deletes the Worker AND its data:");
    show_worker_losses(project);
    for d in &project.config.d1 {
        eprintln!("  D1 database:     {} ({})", d.name, d.binding);
        if d.id.is_some() {
            eprintln!(
                "                   database_id is set in the config, so `up` will NOT recreate it; remove database_id first"
            );
        }
    }
    for (s, n) in stores.iter().zip(&counts) {
        eprintln!(
            "  R2 bucket:       {} ({}), {n} objects",
            s.bucket_name, s.binding
        );
    }
    confirm::retype_env(&env, "delete the Worker and all its data")?;

    let mut done: Vec<String> = Vec::new();
    let result = (|| -> Result<()> {
        project.wrangler_interactive(["delete"])?;
        done.push(format!("Worker {}", project.config.worker));
        for (s, b) in stores.iter().zip(&project.config.r2) {
            let mut n = 0;
            if let Err(e) = s.delete_all(&mut n) {
                if n > 0 {
                    done.push(format!("{n} objects in R2 bucket {}", b.bucket));
                }
                return Err(e);
            }
            let mut args = vec![
                "r2".to_string(),
                "bucket".into(),
                "delete".into(),
                b.bucket.clone(),
            ];
            if let Some(j) = &b.jurisdiction {
                args.push("--jurisdiction".into());
                args.push(j.clone());
            }
            project.wrangler(&args)?;
            done.push(format!("R2 bucket {} ({n} objects)", b.bucket));
        }
        let remote = d1::remote(project)?;
        for d in &project.config.d1 {
            if d1::exists(d, &remote) {
                project.wrangler(["d1", "delete", &d.name, "-y"])?;
                done.push(format!("D1 {}", d.name));
            } else {
                eprintln!("wrangler-compose: D1 {} did not exist", d.name);
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            eprintln!("wrangler-compose: deleted {}", done.join(", "));
            Ok(())
        }
        Err(e) => {
            let deleted = if done.is_empty() {
                "nothing".to_string()
            } else {
                done.join(", ")
            };
            Err(e.context(format!(
                "stopped part way: deleted {deleted}; everything else is still there"
            )))
        }
    }
}
