//! Ctrl+C handling. The terminal sends Ctrl+C to Wrangler too, which stops
//! it; wrangler-compose keeps running long enough to remove temporary
//! secrets and report what was and was not done (REQ-006, REQ-008, REQ-009).

use anyhow::{Result, bail};
use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub fn install() {
    let _ = ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::SeqCst));
}

/// Call between steps that do not run Wrangler (e.g. per R2 object).
pub fn check() -> Result<()> {
    if INTERRUPTED.load(Ordering::SeqCst) {
        bail!("interrupted (Ctrl+C)");
    }
    Ok(())
}
