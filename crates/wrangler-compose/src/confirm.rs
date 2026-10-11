//! Retyping the environment name before deleting anything (DES-008).

use anyhow::{Result, bail};
use std::io::{BufRead, IsTerminal, Write};

/// Checked before any other work, so a run without a terminal stops before
/// it even talks to Cloudflare.
pub fn require_terminal(action: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        bail!(
            "{action} needs confirmation from a terminal; run it yourself in a terminal (nothing was changed)"
        );
    }
    Ok(())
}

pub fn retype_env(env: &str, action: &str) -> Result<()> {
    require_terminal(action)?;
    eprint!("Type the environment name ({env}) to {action}: ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    if line.trim() != env {
        bail!("the name did not match; nothing was changed");
    }
    Ok(())
}
