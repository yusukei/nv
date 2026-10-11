//! Keeping secrets and backups out of git (DES-005). Read-only git queries;
//! no git means no check.

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn git(dir: &Path, args: &[&str]) -> Option<bool> {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .map(|s| s.success())
}

fn toplevel(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    Some(std::fs::canonicalize(&p).unwrap_or(p))
}

/// Resolve `path` through its nearest existing ancestor, so a folder that does
/// not exist yet can still be compared with the work tree.
fn resolve(path: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut cur = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(cur) {
            let mut out = real;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                cur = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Stop if `path` (a file or folder, existing or not) could end up in the
/// project's git: not ignored, or already tracked. Paths outside the
/// project's git work tree (or no git at all) are not checked (AC-007).
pub fn require_ignored(project: &Path, path: &Path, what: &str) -> Result<()> {
    let Some(top) = toplevel(project) else {
        return Ok(());
    };
    if !resolve(path).starts_with(&top) {
        return Ok(());
    }
    let shown = path.strip_prefix(project).unwrap_or(path);
    let shown_str = shown.to_string_lossy();
    let target = path.to_string_lossy();
    if git(project, &["ls-files", "--error-unmatch", "--", &target]) == Some(true) {
        bail!(crate::tr!(
            "{what} {shown_str} が git に入っています。git から外し(`git rm --cached`)、.gitignore に書いてください",
            "{what} {shown_str} is tracked by git; remove it from git (`git rm --cached`) and ignore it in .gitignore"
        ));
    }
    // check-ignore exits 0 when the path is ignored.
    if git(
        project,
        &["check-ignore", "-q", "--no-index", "--", &target],
    ) != Some(true)
    {
        bail!(crate::tr!(
            "{what} {shown_str} が git から外れていません。先に .gitignore に書いてください",
            "{what} {shown_str} is not ignored by git; add it to .gitignore first"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_missing_folders_through_existing_parents() {
        let tmp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let p = tmp.join("wc-does-not-exist/backups");
        assert_eq!(resolve(&p), p);
    }
}
