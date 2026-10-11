//! `backup` (DES-012) and the backup manifest shared with `restore`.

use crate::files::sha256_file;
use crate::git;
use crate::project::Project;
use crate::r2::{self, Store};
use crate::up::read_env_file;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

pub const MANIFEST: &str = "manifest.json";

pub fn backup(project: &Project, dest: Option<PathBuf>) -> Result<()> {
    let env = project
        .env
        .clone()
        .ok_or_else(|| anyhow!("an environment is required"))?;
    read_env_file(project)?;
    let base = match dest {
        Some(d) => std::env::current_dir()?.join(d),
        None => project.dir.join("backups"),
    };
    // Backups hold the environment's data: keep them out of git (REQ-007).
    // A trailing separator makes git match directory patterns like `backups/`.
    let mut probe = base.clone().into_os_string();
    probe.push(std::path::MAIN_SEPARATOR_STR);
    git::require_ignored(&project.dir, Path::new(&probe), "backup folder")?;

    let mut stores = Vec::new();
    if !project.config.r2.is_empty() {
        let creds = r2::credentials(project)
            .ok_or_else(|| anyhow!("{}; nothing was written", r2::missing_keys_message()))?;
        let account = r2::account_id(project)?;
        for b in &project.config.r2 {
            let store = Store::new(&account, b, &creds)?;
            store
                .check_access()
                .map_err(|e| anyhow!("{e:#}; nothing was written"))?;
            stores.push(store);
        }
    }

    let dir = base.join(&env).join(timestamp());
    fs::create_dir_all(dir.join("d1"))?;
    eprintln!("wrangler-compose: backing up {env} into {}", dir.display());

    let mut d1_entries = Vec::new();
    for d in &project.config.d1 {
        eprintln!(
            "wrangler-compose: D1 {} will not answer queries while it is exported",
            d.name
        );
        let rel = format!("d1/{}.sql", d.binding);
        let out = dir.join(&rel).to_string_lossy().into_owned();
        project
            .wrangler([
                "d1", "export", &d.binding, "--remote", "--output", &out, "-y",
            ])
            .with_context(|| {
                format!(
                    "cannot export D1 {}; the backup in {} is incomplete",
                    d.name,
                    dir.display()
                )
            })?;
        d1_entries.push(json!({ "binding": d.binding, "name": d.name, "file": rel, "sha256": sha256_file(&dir.join(&rel))? }));
    }

    let mut r2_entries = Vec::new();
    for s in &stores {
        let objects = s.list_all()?;
        let folder = format!("r2/{}", s.binding);
        fs::create_dir_all(dir.join(&folder))?;
        eprintln!(
            "wrangler-compose: R2 {}: {} objects",
            s.bucket_name,
            objects.len()
        );
        let mut list = Vec::new();
        for (i, o) in objects.iter().enumerate() {
            let rel = format!("{folder}/{i}");
            let got = s
                .download(&o.key, &dir.join(&rel))
                .with_context(|| format!("the backup in {} is incomplete", dir.display()))?;
            list.push(json!({
                "key": o.key, "file": rel, "size": o.size, "sha256": got.sha256,
                "headers": pairs(&got.headers), "metadata": pairs(&got.metadata),
            }));
        }
        r2_entries.push(json!({ "binding": s.binding, "bucket": s.bucket_name, "objects": list }));
    }

    // Written last: a folder without it is an incomplete backup.
    let manifest = json!({
        "format": 1,
        "environment": env,
        "worker": project.config.worker,
        "created": timestamp(),
        "d1": d1_entries,
        "r2": r2_entries,
    });
    let tmp = dir.join(".manifest.json.partial");
    fs::write(&tmp, serde_json::to_vec_pretty(&manifest)?)?;
    fs::rename(&tmp, dir.join(MANIFEST))?;
    eprintln!("wrangler-compose: backup complete: {}", dir.display());
    Ok(())
}

fn pairs(v: &[(String, String)]) -> Value {
    Value::Array(v.iter().map(|(k, x)| json!([k, x])).collect())
}

pub fn unpairs(v: &Value) -> Vec<(String, String)> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some((p[0].as_str()?.to_string(), p[1].as_str()?.to_string())))
        .collect()
}

pub fn read_manifest(dir: &Path) -> Result<Value> {
    let path = dir.join(MANIFEST);
    if !path.is_file() {
        bail!(
            "{} has no {MANIFEST}: it is not a backup, or the backup did not finish",
            dir.display()
        );
    }
    let v: Value = serde_json::from_slice(&fs::read(&path)?)
        .with_context(|| format!("{} is damaged", path.display()))?;
    if v["format"].as_u64() != Some(1) {
        bail!(
            "{} was written by an unknown version of wrangler-compose",
            path.display()
        );
    }
    Ok(v)
}

/// UTC time as yyyyMMddTHHmmssZ, without a date library.
pub fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_utc(secs)
}

fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc_timestamps() {
        assert_eq!(format_utc(0), "19700101T000000Z");
        assert_eq!(format_utc(1_791_696_543), "20261011T052903Z");
        assert_eq!(format_utc(951_782_400), "20000229T000000Z");
    }

    #[test]
    fn manifest_requires_completion() {
        let dir = std::env::temp_dir().join(format!("wc-test-manifest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        assert!(read_manifest(&dir).is_err());
        fs::write(dir.join(MANIFEST), r#"{"format":1,"d1":[],"r2":[]}"#).unwrap();
        assert!(read_manifest(&dir).is_ok());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn metadata_pairs_round_trip() {
        let v = vec![("content-type".to_string(), "image/png".to_string())];
        assert_eq!(unpairs(&pairs(&v)), v);
    }
}
