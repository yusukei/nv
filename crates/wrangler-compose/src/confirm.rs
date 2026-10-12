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

/// Delete, keep or stop, chosen on the spot (REQ-010): the environment
/// name deletes, `keep` keeps, anything else (an empty line too) stops.
/// Without a terminal (CI) it stops: going on with a choice nobody made
/// would not do either.
pub fn delete_or_keep(env: &str, what: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        bail!(crate::tr!(
            "{what}。消すか残すかは端末で選んでください(何も変えていません)",
            "{what}. Choose whether to delete or keep it in a terminal (nothing was changed)"
        ));
    }
    eprint!(
        "{}",
        crate::tr!(
            "{what}。消すなら環境名({env})、残すなら keep、やめるなら何も打たずに Enter: ",
            "{what}. Type the environment name ({env}) to delete, keep to keep, or just Enter to stop: "
        )
    );
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    match line.trim() {
        "keep" => Ok(false),
        x if x == env => Ok(true),
        _ => bail!(crate::tr!(
            "やめました。何も変えていません",
            "stopped; nothing was changed"
        )),
    }
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
    // The name itself is never translated (REQ-019).
    if line.trim() != env {
        bail!(crate::tr!(
            "環境名が違います。何も変えていません",
            "the name did not match; nothing was changed"
        ));
    }
    Ok(())
}
