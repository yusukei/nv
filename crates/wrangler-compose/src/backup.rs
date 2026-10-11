//! `backup` (DES-012) and the backup manifest shared with `restore`.

use crate::files::sha256_file;
use crate::git;
use crate::project::Project;
use crate::r2::{self, Store};
use crate::up::{dry_run_done, read_env_file, say};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

pub const MANIFEST: &str = "manifest.json";

pub fn backup(project: &Project, dest: Option<PathBuf>) -> Result<()> {
    let env = project
        .env
        .clone()
        .ok_or_else(|| anyhow!(tr!("環境の指定が要ります", "an environment is required")))?;
    read_env_file(project)?;
    let base = match dest {
        Some(d) => std::env::current_dir()?.join(d),
        None => project.dir.join("backups"),
    };
    // Backups hold the environment's data: keep them out of git (REQ-007).
    // A trailing separator makes git match directory patterns like `backups/`.
    let mut probe = base.clone().into_os_string();
    probe.push(std::path::MAIN_SEPARATOR_STR);
    git::require_ignored(
        &project.dir,
        Path::new(&probe),
        &tr!("控えの書き出し先", "backup folder"),
    )?;
    let nothing = || tr!("何も書き出していません", "nothing was written");

    let mut stores = Vec::new();
    if !project.config.r2.is_empty() {
        let creds = r2::credentials(project)
            .ok_or_else(|| anyhow!("{}; {}", r2::missing_keys_message(), nothing()))?;
        let account = r2::account_id(project)?;
        for b in &project.config.r2 {
            let store = Store::new(&account, b, &creds)?;
            store
                .check_access()
                .map_err(|e| anyhow!("{e:#}; {}", nothing()))?;
            stores.push(store);
        }
    }

    if project.dry_run {
        say(tr!(
            "`backup` の dry-run({env}):",
            "dry run of `backup` for {env}:"
        ));
        eprintln!(
            "  {}",
            tr!(
                "書き出し先: {}",
                "would write into {}",
                base.join(&env).join("<time>").display()
            )
        );
        for d in &project.config.d1 {
            eprintln!(
                "  {}",
                tr!(
                    "D1 {} ({}): SQL に書き出します。書き出しの間は問い合わせに応えません",
                    "D1 {} ({}): exported to SQL; it will not answer queries while exporting",
                    d.name,
                    d.binding
                )
            );
        }
        for s in &stores {
            let objects = s.list_all()?;
            let bytes: u64 = objects.iter().map(|o| o.size).sum();
            eprintln!(
                "  {}",
                tr!(
                    "R2 {} ({}): オブジェクト {} 件、{bytes} バイト",
                    "R2 {} ({}): {} objects, {bytes} bytes",
                    s.bucket_name,
                    s.binding,
                    objects.len()
                )
            );
        }
        dry_run_done();
        return Ok(());
    }

    // A new folder every time: never write into an existing backup.
    fs::create_dir_all(base.join(&env))?;
    let created = timestamp();
    let dir = base.join(&env).join(&created);
    fs::create_dir(&dir).with_context(|| {
        tr!(
            "{} がすでにあります。1 秒おいて、もう一度 backup を打ってください",
            "{} already exists; wait a second and run backup again",
            dir.display()
        )
    })?;
    fs::create_dir(dir.join("d1"))?;
    say(tr!(
        "{env} を {} へ控えます",
        "backing up {env} into {}",
        dir.display()
    ));
    let incomplete = || {
        tr!(
            "{} の控えは書きかけです",
            "the backup in {} is incomplete",
            dir.display()
        )
    };

    let mut d1_entries = Vec::new();
    for d in &project.config.d1 {
        say(tr!(
            "D1 {} は書き出しの間、問い合わせに応えません",
            "D1 {} will not answer queries while it is exported",
            d.name
        ));
        let rel = format!("d1/{}.sql", d.binding);
        let out = dir.join(&rel).to_string_lossy().into_owned();
        project
            .wrangler([
                "d1", "export", &d.binding, "--remote", "--output", &out, "-y",
            ])
            .with_context(|| {
                tr!(
                    "D1 {} を書き出せません。{}",
                    "cannot export D1 {}; {}",
                    d.name,
                    incomplete()
                )
            })?;
        d1_entries.push(json!({ "binding": d.binding, "name": d.name, "file": rel, "sha256": sha256_file(&dir.join(&rel))? }));
    }

    let mut r2_entries = Vec::new();
    for s in &stores {
        let objects = s.list_all()?;
        let folder = format!("r2/{}", s.binding);
        fs::create_dir_all(dir.join(&folder))?;
        say(tr!(
            "R2 {}: オブジェクト {} 件",
            "R2 {}: {} objects",
            s.bucket_name,
            objects.len()
        ));
        let mut list = Vec::new();
        for (i, o) in objects.iter().enumerate() {
            crate::interrupt::check().with_context(incomplete)?;
            let rel = format!("{folder}/{i}");
            let got = s
                .download(&o.key, &dir.join(&rel))
                .with_context(incomplete)?;
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
        "created": created,
        "wrangler": project.wrangler_version,
        "d1": d1_entries,
        "r2": r2_entries,
    });
    let tmp = dir.join(".manifest.json.partial");
    fs::write(&tmp, serde_json::to_vec_pretty(&manifest)?)?;
    fs::rename(&tmp, dir.join(MANIFEST))?;
    say(tr!(
        "控えができました: {}",
        "backup complete: {}",
        dir.display()
    ));
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
        bail!(tr!(
            "{} に {MANIFEST} がありません。控えではないか、書き出しが終わっていません",
            "{} has no {MANIFEST}: it is not a backup, or the backup did not finish",
            dir.display()
        ));
    }
    let v: Value = serde_json::from_slice(&fs::read(&path)?)
        .with_context(|| tr!("{} が壊れています", "{} is damaged", path.display()))?;
    if v["format"].as_u64() != Some(1) {
        bail!(tr!(
            "{} は知らない版の wrangler-compose が書いたものです",
            "{} was written by an unknown version of wrangler-compose",
            path.display()
        ));
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
