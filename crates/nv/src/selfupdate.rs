//! `nv self update` (DES-011). Replaces nv and, when it sits next to nv,
//! wrangler-compose with the latest release, so both stay on one version.

use crate::http;
use crate::paths::remove_any;
use crate::version::Version;
use anyhow::{Context, Result, anyhow, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const TARGET: &str = env!("NV_TARGET");
const EXE_SUFFIX: &str = std::env::consts::EXE_SUFFIX;
const TOOLS: &[&str] = &["nv", "wrangler-compose"];

fn release_base() -> Result<String> {
    if let Ok(m) = std::env::var("NV_RELEASE_MIRROR") {
        return Ok(m.trim_end_matches('/').to_string());
    }
    option_env!("NV_RELEASE_BASE")
        .map(|b| b.trim_end_matches('/').to_string())
        .ok_or_else(|| {
            anyhow!("this nv was built without a release location; set NV_RELEASE_MIRROR")
        })
}

fn current() -> Version {
    env!("CARGO_PKG_VERSION")
        .parse()
        .expect("package version is x.y.z")
}

pub fn update(check_only: bool) -> Result<()> {
    let base = release_base()?;
    let latest: Version = http::get_text(&format!("{base}/VERSION"))?
        .parse()
        .map_err(|e| anyhow!("the release VERSION file is invalid: {e}"))?;
    let now = current();
    if latest <= now {
        eprintln!("nv: {now} is the latest version");
        return Ok(());
    }
    if check_only {
        eprintln!("nv: {latest} is available (installed: {now}); run `nv self update`");
        return Ok(());
    }

    let me = std::env::current_exe().context("cannot find the nv executable")?;
    let me = fs::canonicalize(&me).unwrap_or(me);
    let dir = me
        .parent()
        .ok_or_else(|| anyhow!("cannot find the folder of {}", me.display()))?
        .to_path_buf();
    let targets: Vec<(&str, PathBuf)> = TOOLS
        .iter()
        .map(|t| (*t, dir.join(format!("{t}{EXE_SUFFIX}"))))
        .filter(|(t, p)| *t == "nv" || p.is_file())
        .collect();

    let sums = http::get_text(&format!("{base}/SHA256SUMS"))?;
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    let downloaded = (|| -> Result<()> {
        for (tool, dest) in &targets {
            let asset = format!("{tool}-{TARGET}{EXE_SUFFIX}");
            let new = dir.join(format!(".{tool}{EXE_SUFFIX}.new"));
            let actual = http::download(&format!("{base}/{asset}"), &new)
                .with_context(|| format!("cannot write into {}", dir.display()))?;
            staged.push((new.clone(), dest.clone()));
            http::verify(http::checksum_for(&sums, &asset), &actual, &asset)?;
            make_executable(&new)?;
        }
        Ok(())
    })();
    if let Err(e) = downloaded {
        for (new, _) in &staged {
            let _ = remove_any(new);
        }
        return Err(e.context("nothing was replaced"));
    }

    for (new, dest) in &staged {
        swap(new, dest)?;
        eprintln!("nv: updated {} {now} -> {latest}", dest.display());
    }

    // The new nv carries the new skill text; let it refresh installed skills.
    let status = Command::new(&me)
        .args(["install", "skill", "--if-installed"])
        .status();
    if !matches!(status, Ok(s) if s.success()) {
        eprintln!("nv: warning: could not refresh installed skills; run `nv install skill`");
    }
    Ok(())
}

/// Move `dest` aside and put `new` in its place. Renaming a running
/// executable works on Windows, overwriting it does not.
fn swap(new: &Path, dest: &Path) -> Result<()> {
    let old = dest.with_file_name(format!(
        "{}.{}.old",
        dest.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    let had_old = dest.exists();
    if had_old {
        fs::rename(dest, &old).with_context(|| format!("cannot replace {}", dest.display()))?;
    }
    if let Err(e) = fs::rename(new, dest) {
        if had_old {
            let _ = fs::rename(&old, dest);
        }
        let _ = remove_any(new);
        bail!("cannot replace {}: {e}", dest.display());
    }
    let _ = fs::remove_file(&old); // fails on Windows while running; cleaned up next start
    Ok(())
}

/// Delete `<tool>.<pid>.old` files left by a previous update.
pub fn clean_old() {
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    else {
        return;
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".old")
            && TOOLS
                .iter()
                .any(|t| name.starts_with(&format!("{t}{EXE_SUFFIX}.")))
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_: &Path) -> Result<()> {
    Ok(())
}
