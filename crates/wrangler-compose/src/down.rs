//! `down` (DES-009) and `down --purge` (DES-011).

use crate::confirm;
use crate::d1;
use crate::interrupt;
use crate::project::Project;
use crate::r2::{self, Store};
use crate::up::{dry_run_done, read_env_file, say};
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

/// One "  label: value" line of the list of things to delete.
fn row(label_ja: &str, label_en: &str, value: String) {
    let label = if crate::i18n::is_ja() {
        label_ja
    } else {
        label_en
    };
    let pad = 18usize.saturating_sub(display_width(label));
    eprintln!("  {label}{} {value}", " ".repeat(pad));
}

/// Terminal columns: CJK and full-width characters take two.
fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| match c as u32 {
            0x1100..=0x115F
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6 => 2,
            _ => 1,
        })
        .sum()
}

fn show_worker_losses(project: &Project, exists: bool) {
    let c = &project.config;
    if !exists {
        row(
            "Worker:",
            "Worker:",
            tr!(
                "{} はありません(消すものは無い)",
                "{} does not exist (nothing to delete)",
                c.worker
            ),
        );
        return;
    }
    row("Worker:", "Worker:", c.worker.clone());
    let secrets = secret_names(project);
    let list = |v: &[String]| {
        if v.is_empty() {
            tr!("(なし)", "(none)")
        } else {
            v.join(", ")
        }
    };
    row("シークレット:", "secrets:", list(&secrets));
    row("ルート・ドメイン:", "routes/domains:", list(&c.routes));
    row("Cron:", "cron triggers:", list(&c.crons));
    row(
        "Durable Objects:",
        "Durable Objects:",
        tr!(
            "{}(保存したデータも Worker と一緒に消えます)",
            "{} (their stored data is deleted with the Worker)",
            list(&c.durable_objects)
        ),
    );
}

pub fn down(project: &Project) -> Result<()> {
    let env = project
        .env
        .clone()
        .ok_or_else(|| anyhow!(tr!("環境の指定が要ります", "an environment is required")))?;
    if !project.dry_run {
        confirm::require_terminal("down")?;
    }
    interrupt::check()?;
    if !project.worker_exists()? {
        say(tr!(
            "{env} に Worker {} はありません。消すものはありません",
            "Worker {} does not exist in {env}; nothing to delete",
            project.config.worker
        ));
        if project.dry_run {
            dry_run_done();
        }
        return Ok(());
    }
    say(tr!(
        "`down` は Worker と次のものを消します。D1 と R2 のデータは残します",
        "`down` deletes the Worker and everything below. D1 and R2 data are kept."
    ));
    show_worker_losses(project, true);
    if project.dry_run {
        dry_run_done();
        return Ok(());
    }
    confirm::retype_env(&env, &tr!("Worker を消す", "delete the Worker"))?;
    project.wrangler_interactive(["delete"])?;
    say(tr!(
        "{} を消しました。D1 と R2 は残っています。`up` で戻せます",
        "deleted {}; D1 and R2 were kept, `up` brings it back",
        project.config.worker
    ));
    Ok(())
}

pub fn purge(project: &Project) -> Result<()> {
    let env = project
        .env
        .clone()
        .ok_or_else(|| anyhow!(tr!("環境の指定が要ります", "an environment is required")))?;
    if !project.dry_run {
        confirm::require_terminal("down --purge")?;
    }
    // Validates that .env.<env> is kept out of git before reading keys from it.
    read_env_file(project)?;
    let nothing = || tr!("何も消していません", "nothing was deleted");

    // Keys first: without working keys nothing at all is deleted (REQ-006).
    // Buckets that are already gone (an earlier, interrupted purge) are
    // skipped, so purge can be run again to finish the rest.
    let mut stores = Vec::new();
    let mut missing_buckets = Vec::new();
    if !project.config.r2.is_empty() {
        let creds = r2::credentials(project)
            .ok_or_else(|| anyhow!("{}; {}", r2::missing_keys_message(), nothing()))?;
        let account = r2::account_id(project)?;
        for b in &project.config.r2 {
            let store = Store::new(&account, b, &creds)?;
            let exists = store
                .exists()
                .map_err(|e| anyhow!("{e:#}; {}", nothing()))?;
            if exists {
                stores.push((store, b));
            } else {
                missing_buckets.push(b);
            }
        }
    }
    let mut counts = Vec::new();
    for (s, _) in &stores {
        counts.push(s.list_all()?.len());
    }

    let worker_exists = project.worker_exists()?;
    let remote = d1::remote(project)?;
    say(tr!(
        "`down --purge` は Worker と、そのデータを消します:",
        "`down --purge` deletes the Worker AND its data:"
    ));
    show_worker_losses(project, worker_exists);
    for d in &project.config.d1 {
        if !d1::exists(d, &remote) {
            row(
                "D1:",
                "D1 database:",
                tr!(
                    "{} ({}) はありません",
                    "{} ({}) does not exist",
                    d.name,
                    d.binding
                ),
            );
            continue;
        }
        row("D1:", "D1 database:", format!("{} ({})", d.name, d.binding));
        if d.id.is_some() {
            row(
                "",
                "",
                tr!(
                    "設定に database_id があるので、`up` では作り直されません。先に database_id を外してください",
                    "database_id is set in the config, so `up` will NOT recreate it; remove database_id first"
                ),
            );
        }
    }
    for ((s, _), n) in stores.iter().zip(&counts) {
        row(
            "R2 バケット:",
            "R2 bucket:",
            tr!(
                "{} ({})、オブジェクト {n} 件",
                "{} ({}), {n} objects",
                s.bucket_name,
                s.binding
            ),
        );
    }
    for b in &missing_buckets {
        row(
            "R2 バケット:",
            "R2 bucket:",
            tr!(
                "{} ({}) はありません",
                "{} ({}) does not exist",
                b.bucket,
                b.binding
            ),
        );
    }
    let any_d1 = project.config.d1.iter().any(|d| d1::exists(d, &remote));
    if !worker_exists && stores.is_empty() && !any_d1 {
        say(tr!(
            "{env} には消すものが残っていません",
            "nothing of {env} is left to delete"
        ));
        if project.dry_run {
            dry_run_done();
        }
        return Ok(());
    }
    if project.dry_run {
        dry_run_done();
        return Ok(());
    }
    confirm::retype_env(
        &env,
        &tr!(
            "Worker とすべてのデータを消す",
            "delete the Worker and all its data"
        ),
    )?;

    let mut done: Vec<String> = Vec::new();
    let result = (|| -> Result<()> {
        // After `down` the Worker is already gone; go straight to the data.
        if worker_exists {
            project.wrangler_interactive(["delete"])?;
            done.push(format!("Worker {}", project.config.worker));
        }
        for (s, b) in &stores {
            let mut n = 0;
            if let Err(e) = s.delete_all(&mut n) {
                if n > 0 {
                    done.push(tr!(
                        "R2 バケット {} のオブジェクト {n} 件",
                        "{n} objects in R2 bucket {}",
                        b.bucket
                    ));
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
            done.push(tr!(
                "R2 バケット {}(オブジェクト {n} 件)",
                "R2 bucket {} ({n} objects)",
                b.bucket
            ));
        }
        let remote = d1::remote(project)?;
        for d in &project.config.d1 {
            if d1::exists(d, &remote) {
                project.wrangler(["d1", "delete", &d.name, "-y"])?;
                done.push(format!("D1 {}", d.name));
            } else {
                say(tr!(
                    "D1 {} はありませんでした",
                    "D1 {} did not exist",
                    d.name
                ));
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            say(tr!("消しました: {}", "deleted {}", done.join(", ")));
            Ok(())
        }
        Err(e) => {
            let deleted = if done.is_empty() {
                tr!("何も", "nothing")
            } else {
                done.join(", ")
            };
            Err(e.context(tr!(
                "途中で止まりました。消したのは {deleted} で、ほかはすべて残っています",
                "stopped part way: deleted {deleted}; everything else is still there"
            )))
        }
    }
}
