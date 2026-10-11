//! Finding the project and its pinned Node version (DES-003).

use crate::version::Version;
use anyhow::{Context, Result, anyhow, bail};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Project {
    pub dir: PathBuf,
}

fn node_version_help() -> String {
    crate::tr!(
        "package.json の隣の .node-version に、Node の版をそのまま書いてください(例: `24.11.1`)",
        "write the exact Node version in .node-version next to package.json, e.g. `24.11.1`"
    )
}

impl Project {
    /// The nearest folder at or above `start` that holds a package.json.
    pub fn find_from(start: &Path) -> Option<Project> {
        start
            .ancestors()
            .find(|d| d.join("package.json").is_file())
            .map(|d| Project {
                dir: d.to_path_buf(),
            })
    }

    pub fn find() -> Result<Project> {
        let cwd = std::env::current_dir().context(crate::tr!(
            "いまのフォルダを読めません",
            "cannot read the current folder"
        ))?;
        Self::find_from(&cwd).ok_or_else(|| {
            anyhow!(crate::tr!(
                "{} にも、その上のフォルダにも package.json がありません",
                "no package.json in {} or any folder above it",
                cwd.display()
            ))
        })
    }

    pub fn node_version(&self) -> Result<Version> {
        let path = self.dir.join(".node-version");
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!(crate::tr!(
                    "{} に .node-version がありません。{}",
                    "{} has no .node-version; {}",
                    self.dir.display(),
                    node_version_help()
                ))
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    crate::tr!("{} を読めません", "cannot read {}", path.display())
                });
            }
        };
        let line = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        line.parse::<Version>().map_err(|e| {
            anyhow!(crate::tr!(
                "{}: {e}。{}",
                "{}: {e}; {}",
                path.display(),
                node_version_help()
            ))
        })
    }

    pub fn lockfile(&self) -> PathBuf {
        self.dir.join("package-lock.json")
    }

    pub fn node_modules(&self) -> PathBuf {
        self.dir.join("node_modules")
    }

    pub fn require_lockfile(&self) -> Result<PathBuf> {
        let path = self.lockfile();
        if !path.is_file() {
            let mut msg = crate::tr!(
                "{} に package-lock.json がありません。`nv run npm install` で作り、git に入れてください",
                "{} has no package-lock.json; create it with `nv run npm install` and commit it",
                self.dir.display()
            );
            if let Some(parent) = self.dir.parent().and_then(Self::find_from)
                && parent.lockfile().is_file()
            {
                msg.push_str(&crate::tr!(
                    "(npm のワークスペースは扱えません。{} で nv を打ってください)",
                    " (npm workspaces are not supported; run nv in {} instead)",
                    parent.dir.display()
                ));
            }
            bail!(msg);
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nv-test-project-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn finds_nearest_package_json_upwards() {
        let root = temp("find");
        fs::write(root.join("package.json"), "{}").unwrap();
        let deep = root.join("a/b");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(Project::find_from(&deep).unwrap().dir, root);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn reads_node_version_and_rejects_ranges() {
        let root = temp("ver");
        let p = Project { dir: root.clone() };
        assert!(p.node_version().is_err());
        fs::write(root.join(".node-version"), "v24.11.1\n").unwrap();
        assert_eq!(p.node_version().unwrap().to_string(), "24.11.1");
        fs::write(root.join(".node-version"), "24\n").unwrap();
        assert!(p.node_version().is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
