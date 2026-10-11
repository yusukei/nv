//! `build` (DES-006) and `up` (DES-007).

use crate::d1;
use crate::dotenv;
use crate::files::TempDir;
use crate::git;
use crate::project::Project;
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};

pub fn build(project: &Project) -> Result<()> {
    let out = TempDir::new("build")?;
    let outdir = out.path().to_string_lossy().into_owned();
    project.wrangler(["deploy", "--dry-run", "--outdir", &outdir])?;
    eprintln!(
        "wrangler-compose: {} builds (nothing was uploaded)",
        project.config.worker
    );
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
    git::require_ignored(&project.dir, &path, "secrets file")?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
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

    if all_exist {
        stage(1, 2, "D1 migrations");
        d1::apply_migrations(project)
            .context("stopped before deploying; the Worker was not changed")?;
        stage(2, 2, "deploy");
        deploy(project, secrets.as_ref())?;
    } else {
        stage(1, 2, "deploy (creates missing D1 databases and R2 buckets)");
        deploy(project, secrets.as_ref())?;
        stage(2, 2, "D1 migrations");
        d1::apply_migrations(project)
            .context("the Worker is deployed, but its D1 migrations are not applied; fix the migration and run `up` again")?;
    }

    // The deploy is done; a failed listing must not hide that.
    if let Some(secrets) = &secrets
        && let Err(e) = report_leftover_secrets(project, secrets)
    {
        eprintln!("wrangler-compose: warning: could not list the Worker's secrets: {e:#}");
    }
    if project.config_file_bytes()? != before {
        bail!(
            "{} was changed during deploy; wrangler-compose never edits it, so review the change and restore it with git",
            project.config.config_path.display()
        );
    }
    eprintln!(
        "wrangler-compose: {} is up ({})",
        project.config.worker,
        project.env_name()
    );
    Ok(())
}

fn stage(n: usize, total: usize, what: &str) {
    eprintln!("wrangler-compose: [{n}/{total}] {what}");
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
        eprintln!(
            "wrangler-compose: these secrets are on the Worker but not in .env.{}: {} (remove them with `nv run wrangler secret delete <name> --env {}`)",
            project.env_name(),
            extra
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            project.env_name()
        );
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
