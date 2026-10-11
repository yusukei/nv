//! `build` (DES-006) and `up` (DES-007).

use crate::d1;
use crate::dotenv;
use crate::files::TempDir;
use crate::git;
use crate::project::Project;
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};

/// One line of wrangler-compose's own output.
pub fn say(msg: String) {
    eprintln!("wrangler-compose: {msg}");
}

pub fn build(project: &Project) -> Result<()> {
    let out = TempDir::new("build")?;
    let outdir = out.path().to_string_lossy().into_owned();
    project.wrangler(["deploy", "--dry-run", "--outdir", &outdir])?;
    say(tr!(
        "{} を組めました(何も上げていません)",
        "{} builds (nothing was uploaded)",
        project.config.worker
    ));
    Ok(())
}

/// `.env.<env>` split into Worker secrets and wrangler-compose's own values,
/// after making sure the file stays out of git (REQ-004, REQ-009).
pub fn read_env_file(project: &Project) -> Result<Option<BTreeMap<String, String>>> {
    let Some(env) = &project.env else {
        return Ok(None);
    };
    let path = project.dir.join(format!(".env.{env}"));
    if !path.is_file() {
        return Ok(None);
    }
    git::require_ignored(
        &project.dir,
        &path,
        &tr!("シークレットのファイル", "secrets file"),
    )?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| tr!("{} を読めません", "cannot read {}", path.display()))?;
    Ok(Some(dotenv::parse(&text)))
}

fn worker_secrets(all: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    all.iter()
        .filter(|(k, _)| !k.starts_with("WRANGLER_COMPOSE_"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

pub fn up(project: &Project) -> Result<()> {
    let env_file = read_env_file(project)?;
    let secrets = env_file.as_ref().map(worker_secrets);
    let before = project.config_file_bytes()?;

    let remote = d1::remote(project)?;
    d1::require_pinned_exist(project, &remote)?;
    let all_exist = project.config.d1.iter().all(|d| d1::exists(d, &remote));

    if project.dry_run {
        return plan(project, secrets.as_ref(), &remote, all_exist);
    }

    if all_exist {
        stage(1, 2, tr!("D1 の移行", "D1 migrations"));
        d1::apply_migrations(project).context(tr!(
            "デプロイの前で止まりました。Worker は変わっていません",
            "stopped before deploying; the Worker was not changed"
        ))?;
        stage(2, 2, tr!("デプロイ", "deploy"));
        deploy(project, secrets.as_ref())?;
    } else {
        stage(
            1,
            2,
            tr!(
                "デプロイ(無い D1 と R2 バケットを作る)",
                "deploy (creates missing D1 databases and R2 buckets)"
            ),
        );
        deploy(project, secrets.as_ref())?;
        stage(2, 2, tr!("D1 の移行", "D1 migrations"));
        d1::apply_migrations(project).context(tr!(
            "Worker はデプロイ済みですが、D1 の移行が当たっていません。移行を直して、もう一度 `up` を打ってください",
            "the Worker is deployed, but its D1 migrations are not applied; fix the migration and run `up` again"
        ))?;
    }

    // The deploy is done; a failed listing must not hide that.
    if let Some(secrets) = &secrets
        && let Err(e) = report_leftover_secrets(project, secrets)
    {
        say(tr!(
            "注意: Worker のシークレットを一覧できませんでした: {e:#}",
            "warning: could not list the Worker's secrets: {e:#}"
        ));
    }
    if project.config_file_bytes()? != before {
        bail!(tr!(
            "デプロイの間に {} が変わりました。wrangler-compose は書き換えないので、変更を確かめて git で戻してください",
            "{} was changed during deploy; wrangler-compose never edits it, so review the change and restore it with git",
            project.config.config_path.display()
        ));
    }
    say(tr!(
        "{} を出しました({})",
        "{} is up ({})",
        project.config.worker,
        project.env_name()
    ));
    Ok(())
}

/// `up --dry-run` (REQ-011): what `up` would do, using reads only.
fn plan(
    project: &Project,
    secrets: Option<&BTreeMap<String, String>>,
    remote: &[(String, String)],
    all_exist: bool,
) -> Result<()> {
    say(tr!(
        "`up` の dry-run({}):",
        "dry run of `up` for {}:",
        project.env_name()
    ));
    let order = if all_exist {
        tr!(
            "D1 の移行を当ててから、デプロイする",
            "apply D1 migrations, then deploy"
        )
    } else {
        tr!(
            "先にデプロイして(無い D1 と R2 バケットを Wrangler が作る)、そのあと D1 の移行を当てる",
            "deploy first (Wrangler creates the missing D1 databases and R2 buckets), then apply D1 migrations"
        )
    };
    eprintln!("  {} {order}", tr!("順番:", "order:"));
    for d in &project.config.d1 {
        let pending = if d1::exists(d, remote) {
            d1::pending_migrations(project, d)?
        } else {
            eprintln!(
                "  {}",
                tr!(
                    "D1 {} ({}) はまだ無く、作られます",
                    "D1 {} ({}) does not exist yet and would be created",
                    d.name,
                    d.binding
                )
            );
            d1::pending_migrations_all(d)
        };
        if pending.is_empty() {
            eprintln!(
                "  {}",
                tr!(
                    "D1 {} ({}): 当てる移行はありません",
                    "D1 {} ({}): no migrations to apply",
                    d.name,
                    d.binding
                )
            );
        } else {
            eprintln!(
                "  {}",
                tr!(
                    "D1 {} ({}): {} を当てます",
                    "D1 {} ({}): would apply {}",
                    d.name,
                    d.binding,
                    pending.join(", ")
                )
            );
        }
    }
    for b in &project.config.r2 {
        eprintln!(
            "  {}",
            tr!(
                "R2 バケット {} ({}): 無ければ Wrangler が作ります",
                "R2 bucket {} ({}): created by Wrangler if missing",
                b.bucket,
                b.binding
            )
        );
    }
    let env = project.env_name();
    match secrets {
        Some(s) if !s.is_empty() => {
            let names = s.keys().map(String::as_str).collect::<Vec<_>>().join(", ");
            eprintln!(
                "  {}",
                tr!(
                    ".env.{env} のシークレット: {names}",
                    "secrets from .env.{env}: {names}"
                )
            );
        }
        Some(_) => eprintln!(
            "  {}",
            tr!(
                ".env.{env} に Worker のシークレットはありません",
                ".env.{env} has no Worker secrets"
            )
        ),
        None => eprintln!(
            "  {}",
            tr!(
                ".env.{env} はありません。Worker はいまのシークレットのままです",
                "no .env.{env}: the Worker keeps its current secrets"
            )
        ),
    }
    eprintln!(
        "  {}",
        tr!(
            "{} が組めるかを確かめます:",
            "checking that {} builds:",
            project.config.worker
        )
    );
    build(project)?;
    // A Worker that was never deployed has no secrets to compare with.
    if let Some(s) = secrets
        && project.worker_exists()?
        && let Err(e) = report_leftover_secrets(project, s)
    {
        say(tr!(
            "メモ: Worker のシークレットを一覧できませんでした: {e:#}",
            "note: could not list the Worker's secrets: {e:#}"
        ));
    }
    dry_run_done();
    Ok(())
}

pub fn dry_run_done() {
    say(tr!(
        "dry-run です。何も変えていません",
        "dry run: nothing was changed"
    ));
}

fn stage(n: usize, total: usize, what: String) {
    say(format!("[{n}/{total}] {what}"));
}

fn deploy(project: &Project, secrets: Option<&BTreeMap<String, String>>) -> Result<()> {
    match secrets {
        Some(s) if !s.is_empty() => {
            // The secrets file lives in a private temp folder that is removed
            // even when deploy fails (dropping TempDir).
            let tmp = TempDir::new("secrets")?;
            let file = tmp.path().join("secrets.json");
            std::fs::write(&file, serde_json::to_vec(s)?)?;
            let file = file.to_string_lossy().into_owned();
            project.wrangler(["deploy", "--secrets-file", &file])
        }
        _ => project.wrangler(["deploy"]),
    }
}

/// Secrets removed from .env.<env> stay on the Worker; name them (REQ-004).
fn report_leftover_secrets(project: &Project, secrets: &BTreeMap<String, String>) -> Result<()> {
    let v = project.wrangler_json(["secret", "list", "--format", "json"])?;
    let on_worker: BTreeSet<String> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    let extra: Vec<&String> = on_worker
        .iter()
        .filter(|n| !secrets.contains_key(*n))
        .collect();
    if !extra.is_empty() {
        let env = project.env_name();
        let names = extra
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        say(tr!(
            "次のシークレットは Worker にありますが、.env.{env} にはありません: {names}(消すには `nv run wrangler secret delete <名前> --env {env}`)",
            "these secrets are on the Worker but not in .env.{env}: {names} (remove them with `nv run wrangler secret delete <name> --env {env}`)"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_own_values_out_of_worker_secrets() {
        let all: BTreeMap<String, String> = [
            ("API_KEY", "a"),
            ("WRANGLER_COMPOSE_R2_ACCESS_KEY_ID", "k"),
            ("WRANGLER_COMPOSE_R2_SECRET_ACCESS_KEY", "s"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let s = worker_secrets(&all);
        assert_eq!(s.keys().collect::<Vec<_>>(), vec!["API_KEY"]);
    }
}
