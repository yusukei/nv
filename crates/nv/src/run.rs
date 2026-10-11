//! `nv run` (DES-006) and the shared way of turning a tool name into a process.

use crate::exec;
use crate::node::Node;
use crate::paths::NvHome;
use crate::project::Project;
use crate::sync;
use anyhow::{Context, Result};
use serde_json::Value;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run(
    home: &NvHome,
    program: &OsString,
    args: &[OsString],
) -> Result<std::convert::Infallible> {
    let project = Project::find()?;
    // Without a lockfile there is nothing to sync yet; still allow
    // `nv run npm install` so the project can create one.
    let has_lock = project.lockfile().is_file();
    let node = if has_lock {
        sync::ensure(home, &project, false)?
    } else {
        crate::node::ensure(home, project.node_version()?)?
    };
    let bins = vec![project.node_modules().join(".bin")];
    let tools = if has_lock {
        project_bins(&project)?
    } else {
        Vec::new()
    };
    let target = resolve(&node, program, &tools, &bins);
    let mut cmd = target.command(&node);
    cmd.args(args);
    exec::prepare(&mut cmd, home, &node, &bins)?;
    exec::hand_over(cmd)
}

/// What a tool name turns into.
#[derive(Debug, PartialEq)]
pub enum Target {
    /// Run a file directly.
    File(PathBuf),
    /// Run a JavaScript entry point with the managed node.
    Script(PathBuf),
    /// Not ours: look it up on the child's PATH.
    External(OsString),
}

impl Target {
    pub fn command(&self, node: &Node) -> Command {
        match self {
            Target::File(p) => Command::new(p),
            Target::Script(p) => {
                let mut c = Command::new(node.exe());
                c.arg(p);
                c
            }
            Target::External(name) => Command::new(name),
        }
    }
}

/// Tool name → entry point, from the `bin` fields of the top-level packages in
/// package-lock.json (lockfile v2/v3).
pub fn project_bins(project: &Project) -> Result<Vec<(String, PathBuf)>> {
    let path = project.lockfile();
    let text = fs::read_to_string(&path)
        .with_context(|| crate::tr!("{} を読めません", "cannot read {}", path.display()))?;
    let json: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    Ok(bins_from_lock(&json, &project.dir))
}

fn bins_from_lock(lock: &Value, dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Some(packages) = lock.get("packages").and_then(Value::as_object) else {
        return out;
    };
    for (key, pkg) in packages {
        let Some(rest) = key.strip_prefix("node_modules/") else {
            continue;
        };
        // Only packages installed at the top level put tools in node_modules/.bin.
        if rest.contains("/node_modules/") {
            continue;
        }
        if let Some(bins) = pkg.get("bin").and_then(Value::as_object) {
            for (name, rel) in bins {
                if let Some(rel) = rel.as_str() {
                    out.push((name.clone(), dir.join(key).join(rel)));
                }
            }
        }
    }
    out
}

/// Tools listed in a package.json `bin` field (string or object form).
pub fn bins_from_package_json(pkg_dir: &Path, pkg_name: &str) -> Result<Vec<(String, PathBuf)>> {
    let path = pkg_dir.join("package.json");
    let text = fs::read_to_string(&path)
        .with_context(|| crate::tr!("{} を読めません", "cannot read {}", path.display()))?;
    let json: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    let short = pkg_name.rsplit('/').next().unwrap_or(pkg_name).to_string();
    Ok(match json.get("bin") {
        Some(Value::String(rel)) => vec![(short, pkg_dir.join(rel))],
        Some(Value::Object(map)) => map
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), pkg_dir.join(v.as_str()?))))
            .collect(),
        _ => Vec::new(),
    })
}

pub fn resolve(
    node: &Node,
    program: &OsString,
    bins: &[(String, PathBuf)],
    bin_dirs: &[PathBuf],
) -> Target {
    match program.to_str() {
        Some("node") => return Target::File(node.exe()),
        Some("npm") => return Target::Script(node.npm_cli()),
        Some("npx") => return Target::Script(node.npx_cli()),
        Some(name) => {
            if let Some((_, entry)) = bins.iter().find(|(n, _)| n == name) {
                return entry_target(entry);
            }
        }
        None => {}
    }
    external(program, node, bin_dirs)
}

/// JavaScript entry points run through node so Windows never routes their
/// arguments through cmd.exe; native executables run as they are.
pub fn entry_target(entry: &Path) -> Target {
    if is_javascript(entry) {
        Target::Script(entry.to_path_buf())
    } else {
        Target::File(entry.to_path_buf())
    }
}

fn is_javascript(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str())
        && matches!(ext, "js" | "mjs" | "cjs")
    {
        return true;
    }
    let mut head = [0u8; 128];
    let n = fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    let head = &head[..n];
    head.starts_with(b"#!") && {
        let line_end = head.iter().position(|&b| b == b'\n').unwrap_or(head.len());
        String::from_utf8_lossy(&head[..line_end]).contains("node")
    }
}

/// Windows only finds `.exe` without an extension, so look for `.cmd`/`.bat`
/// shims ourselves; elsewhere the child's PATH lookup is enough.
fn external(program: &OsString, node: &Node, bin_dirs: &[PathBuf]) -> Target {
    if cfg!(windows) {
        let mut dirs = vec![node.bin_dir()];
        dirs.extend(bin_dirs.iter().cloned());
        if let Some(old) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&old));
        }
        for dir in dirs {
            for ext in ["exe", "cmd", "bat"] {
                let mut name = program.clone();
                name.push(".");
                name.push(ext);
                let candidate = dir.join(&name);
                if candidate.is_file() {
                    return Target::File(candidate);
                }
            }
        }
    }
    Target::External(program.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_top_level_bins_from_lockfile() {
        let lock = json!({
            "lockfileVersion": 3,
            "packages": {
                "": {"name": "app"},
                "node_modules/wrangler": {"bin": {"wrangler": "bin/wrangler.js", "wrangler2": "bin/wrangler.js"}},
                "node_modules/@scope/tool": {"bin": {"tool": "cli.mjs"}},
                "node_modules/a/node_modules/nested": {"bin": {"nested": "x.js"}}
            }
        });
        let dir = Path::new("/p");
        let bins = bins_from_lock(&lock, dir);
        assert!(bins.contains(&(
            "wrangler".into(),
            dir.join("node_modules/wrangler/bin/wrangler.js")
        )));
        assert!(bins.contains(&("tool".into(), dir.join("node_modules/@scope/tool/cli.mjs"))));
        assert!(!bins.iter().any(|(n, _)| n == "nested"));
    }

    #[test]
    fn detects_javascript_entry_points() {
        let dir = std::env::temp_dir().join(format!("nv-test-js-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let shebang = dir.join("tool");
        fs::write(&shebang, "#!/usr/bin/env node\nconsole.log(1)\n").unwrap();
        let native = dir.join("esbuild");
        fs::write(&native, [0x7f, b'E', b'L', b'F', 0, 0]).unwrap();
        let sh = dir.join("script");
        fs::write(&sh, "#!/bin/sh\necho node\n").unwrap();
        assert!(is_javascript(&shebang));
        assert!(is_javascript(Path::new("/nope/cli.mjs")));
        assert!(!is_javascript(&native));
        assert!(!is_javascript(&sh));
        fs::remove_dir_all(&dir).unwrap();
    }
}
