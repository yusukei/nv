//! `nv sync` and the "are dependencies up to date?" check (DES-005).

use crate::exec;
use crate::http;
use crate::node::{self, Node};
use crate::paths::NvHome;
use crate::project::Project;
use anyhow::{Context, Result};
use std::fs;

const STAMP: &str = ".nv-sync";

fn stamp_text(project: &Project, node: &Node) -> Result<String> {
    let lock_hash = http::sha256_file(&project.require_lockfile()?)?;
    Ok(format!("lock {lock_hash}\nnode {}\n", node.version))
}

pub fn is_current(project: &Project, node: &Node) -> Result<bool> {
    let current = stamp_text(project, node)?;
    let recorded = fs::read_to_string(project.node_modules().join(STAMP)).unwrap_or_default();
    Ok(recorded == current)
}

/// Install exactly what package-lock.json says (`npm ci`) and record it.
pub fn install(home: &NvHome, project: &Project, node: &Node) -> Result<()> {
    let stamp = stamp_text(project, node)?;
    eprintln!(
        "nv: {}",
        crate::tr!(
            "package-lock.json のとおりに依存を入れます",
            "installing dependencies from package-lock.json"
        )
    );
    exec::npm(home, node, &project.dir, &["ci", "--no-audit", "--no-fund"])?;
    let path = project.node_modules().join(STAMP);
    fs::write(&path, stamp)
        .with_context(|| crate::tr!("{} に書き込めません", "cannot write {}", path.display()))?;
    Ok(())
}

/// Node and dependencies for `project`, installing whatever is missing.
pub fn ensure(home: &NvHome, project: &Project, force: bool) -> Result<Node> {
    let node = node::ensure(home, project.node_version()?)?;
    project.require_lockfile()?;
    if !force && is_current(project, &node)? {
        return Ok(node);
    }
    // Two nv in one project must not run `npm ci` into the same node_modules at once.
    let key = http::sha256_bytes_hex(project.dir.to_string_lossy().as_bytes());
    let _lock = home.lock(&format!("project-{}", &key[..16]))?;
    if force || !is_current(project, &node)? {
        install(home, project, &node)?;
    }
    Ok(node)
}
