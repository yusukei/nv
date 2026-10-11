//! `nv x <package>@<version>`: run a tool without adding it to the project (DES-007).

use crate::exec;
use crate::node::{self, Node};
use crate::paths::{NvHome, remove_any};
use crate::project::Project;
use crate::run;
use crate::version::Version;
use anyhow::{Context, Result, anyhow, bail};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, PartialEq)]
pub struct PackageSpec {
    pub name: String,
    pub version: Version,
}

impl PackageSpec {
    pub fn parse(s: &str) -> Result<PackageSpec> {
        let at = s
            .rfind('@')
            .filter(|&i| i > 0)
            .ok_or_else(|| anyhow!("`{s}` needs an exact version, e.g. `{s}@1.2.3`"))?;
        let (name, version) = (&s[..at], &s[at + 1..]);
        let version: Version = version.parse().map_err(|e| {
            anyhow!("{e} (nv x needs an exact version so every machine runs the same tool)")
        })?;
        Ok(PackageSpec {
            name: name.to_string(),
            version,
        })
    }

    fn folder_name(&self) -> String {
        format!("{}@{}", self.name.replace('/', "+"), self.version)
    }
}

pub fn run(
    home: &NvHome,
    spec: &str,
    node_flag: Option<Version>,
    args: &[OsString],
) -> Result<std::convert::Infallible> {
    let spec = PackageSpec::parse(spec)?;
    let node_version = match Project::find().ok() {
        Some(p) if p.dir.join(".node-version").is_file() => p.node_version()?,
        _ => node_flag.ok_or_else(|| {
            anyhow!("no .node-version here; pass the Node version with `--node <version>`")
        })?,
    };
    let node = node::ensure(home, node_version)?;
    let dir = ensure_tool(home, &node, &spec)?;

    let pkg_dir = dir.join("node_modules").join(&spec.name);
    let bins = run::bins_from_package_json(&pkg_dir, &spec.name)?;
    let short = spec.name.rsplit('/').next().unwrap_or(&spec.name);
    let entry = match bins.as_slice() {
        [] => bail!("{} does not provide a command", spec.name),
        [(_, only)] => only.clone(),
        many => match many.iter().find(|(n, _)| n == short) {
            Some((_, p)) => p.clone(),
            None => bail!(
                "{} provides several commands ({}); none is named {short}",
                spec.name,
                many.iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    };
    let mut cmd = run::entry_target(&entry).command(&node);
    cmd.args(args);
    exec::prepare(&mut cmd, home, &node, &[dir.join("node_modules/.bin")])?;
    exec::hand_over(cmd)
}

fn ensure_tool(home: &NvHome, node: &Node, spec: &PackageSpec) -> Result<PathBuf> {
    let dir = home
        .tools_dir()
        .join(spec.folder_name())
        .join(format!("node-{}", node.version));
    if dir.is_dir() {
        return Ok(dir);
    }
    let _lock = home.lock(&format!(
        "tool-{}-node-{}",
        spec.folder_name(),
        node.version
    ))?;
    if dir.is_dir() {
        return Ok(dir);
    }
    eprintln!("nv: installing {}@{}", spec.name, spec.version);
    let tmp = home.new_tmp("tool")?;
    let result = (|| -> Result<()> {
        fs::write(tmp.join("package.json"), "{\"private\":true}\n")?;
        let wanted = format!("{}@{}", spec.name, spec.version);
        exec::npm(
            home,
            node,
            &tmp,
            &["install", "--no-audit", "--no-fund", &wanted],
        )?;
        fs::create_dir_all(dir.parent().unwrap())?;
        fs::rename(&tmp, &dir)
            .with_context(|| format!("cannot move the tool into {}", dir.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = remove_any(&tmp);
    }
    result?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_scoped_packages() {
        let p = PackageSpec::parse("cowsay@1.6.0").unwrap();
        assert_eq!(
            (p.name.as_str(), p.version.to_string()),
            ("cowsay", "1.6.0".to_string())
        );
        let p = PackageSpec::parse("@scope/tool@2.0.1").unwrap();
        assert_eq!(p.name, "@scope/tool");
        assert_eq!(p.folder_name(), "@scope+tool@2.0.1");
    }

    #[test]
    fn rejects_missing_or_loose_versions() {
        for s in [
            "cowsay",
            "cowsay@latest",
            "cowsay@4",
            "@scope/tool",
            "@scope/tool@^1.0.0",
        ] {
            assert!(PackageSpec::parse(s).is_err(), "{s} should be rejected");
        }
    }
}
