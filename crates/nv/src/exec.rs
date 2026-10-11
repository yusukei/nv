//! Starting child processes with the managed Node first on PATH (DES-006).

use crate::node::Node;
use crate::paths::NvHome;
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Environment every child of nv gets: managed Node, then the given tool
/// folders, then the user's PATH; and nv's own npm cache (REQ-003).
pub fn prepare(cmd: &mut Command, home: &NvHome, node: &Node, bin_dirs: &[PathBuf]) -> Result<()> {
    let mut dirs: Vec<PathBuf> = vec![node.bin_dir()];
    dirs.extend(bin_dirs.iter().cloned());
    if let Some(old) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&old));
    }
    let path: OsString =
        std::env::join_paths(dirs).context("a folder on PATH contains an invalid character")?;
    cmd.env("PATH", path);
    cmd.env("npm_config_cache", home.npm_cache());
    // npm should not nag about updating itself; nv decides npm's version via Node.
    cmd.env("npm_config_update_notifier", "false");
    Ok(())
}

/// Run npm for nv's own bookkeeping. npm's stdout goes to our stderr so that
/// `nv run` keeps stdout for the user's command only (DES-012).
pub fn npm(home: &NvHome, node: &Node, cwd: &Path, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new(node.exe());
    cmd.arg(node.npm_cli()).args(args).current_dir(cwd);
    prepare(&mut cmd, home, node, &[])?;
    cmd.stdin(Stdio::null()).stdout(std::io::stderr());
    let status = cmd
        .status()
        .with_context(|| format!("cannot start {}", node.exe().display()))?;
    if !status.success() {
        bail!(
            "`npm {}` failed in {} ({status})",
            args.join(" "),
            cwd.display()
        );
    }
    Ok(())
}

/// Replace nv with `cmd` (Unix) or run it and exit with its code (Windows).
pub fn hand_over(mut cmd: Command) -> Result<std::convert::Infallible> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(err).with_context(|| format!("cannot start {}", cmd.get_program().to_string_lossy()))
    }
    #[cfg(windows)]
    {
        // Ctrl+C reaches the child too; nv keeps waiting so the child's exit code wins.
        let _ = ctrlc::set_handler(|| {});
        let status = cmd
            .status()
            .with_context(|| format!("cannot start {}", cmd.get_program().to_string_lossy()))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}
