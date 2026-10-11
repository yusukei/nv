//! Keeping secrets and backups out of git (DES-005). Read-only git queries;
//! no git means no check.

use anyhow::{Result, bail};
use std::path::Path;
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

/// Stop if `path` (a file or folder, existing or not) could end up in git:
/// not ignored, or already tracked. Outside a git work tree nothing is checked.
pub fn require_ignored(project: &Path, path: &Path, what: &str) -> Result<()> {
    let inside = git(project, &["rev-parse", "--is-inside-work-tree"]) == Some(true);
    if !inside {
        return Ok(());
    }
    let shown = path.strip_prefix(project).unwrap_or(path);
    let shown_str = shown.to_string_lossy();
    let target = path.to_string_lossy();
    if git(project, &["ls-files", "--error-unmatch", "--", &target]) == Some(true) {
        bail!(
            "{what} {shown_str} is tracked by git; remove it from git (`git rm --cached`) and ignore it in .gitignore"
        );
    }
    // check-ignore exits 0 when the path is ignored.
    if git(
        project,
        &["check-ignore", "-q", "--no-index", "--", &target],
    ) != Some(true)
    {
        bail!("{what} {shown_str} is not ignored by git; add it to .gitignore first");
    }
    Ok(())
}
