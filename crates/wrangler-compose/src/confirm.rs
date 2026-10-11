//! Retyping the environment name before deleting anything (DES-008).

use anyhow::{Result, bail};
use std::io::{BufRead, IsTerminal, Write};

/// Checked before any other work, so a run without a terminal stops before
/// it even talks to Cloudflare.
pub fn require_terminal(action: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        bail!(crate::tr!(
            "{action} は端末での確かめが要ります。ご自身で端末から打ってください(何も変えていません)",
            "{action} needs confirmation from a terminal; run it yourself in a terminal (nothing was changed)"
        ));
    }
    Ok(())
}

/// `action` is already in the display language, e.g. "Worker を消す".
pub fn retype_env(env: &str, action: &str) -> Result<()> {
    require_terminal(action)?;
    eprint!(
        "{}",
        crate::tr!(
            "{action}には、環境名({env})を打ってください: ",
            "Type the environment name ({env}) to {action}: "
        )
    );
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    // The name itself is never translated (REQ-012).
    if line.trim() != env {
        bail!(crate::tr!(
            "環境名が違います。何も変えていません",
            "the name did not match; nothing was changed"
        ));
    }
    Ok(())
}
