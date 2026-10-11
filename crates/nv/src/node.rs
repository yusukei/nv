//! Getting Node itself (DES-004) and managing installed versions (DES-008).

use crate::http;
use crate::paths::{NvHome, remove_any};
use crate::version::Version;
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_MIRROR: &str = "https://nodejs.org/dist";

/// The Node distribution name for this machine (`darwin-arm64`, `win-x64`, ...).
fn platform() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("linux", "x86_64") => "linux-x64",
        ("windows", "x86_64") => "win-x64",
        (os, arch) => bail!(crate::tr!(
            "nv は {os}/{arch} 向けの Node を扱えません",
            "Node is not provided by nv for {os}/{arch}"
        )),
    })
}

fn archive_ext() -> &'static str {
    if cfg!(windows) { "zip" } else { "tar.gz" }
}

/// An installed Node, ready to run.
pub struct Node {
    pub version: Version,
    root: PathBuf,
}

impl Node {
    fn at(version: Version, root: PathBuf) -> Node {
        Node { version, root }
    }

    /// Folder to put first on PATH so children find this node and npm.
    pub fn bin_dir(&self) -> PathBuf {
        if cfg!(windows) {
            self.root.clone()
        } else {
            self.root.join("bin")
        }
    }

    pub fn exe(&self) -> PathBuf {
        if cfg!(windows) {
            self.root.join("node.exe")
        } else {
            self.root.join("bin/node")
        }
    }

    fn npm_pkg(&self) -> PathBuf {
        if cfg!(windows) {
            self.root.join("node_modules/npm")
        } else {
            self.root.join("lib/node_modules/npm")
        }
    }

    pub fn npm_cli(&self) -> PathBuf {
        self.npm_pkg().join("bin/npm-cli.js")
    }

    pub fn npx_cli(&self) -> PathBuf {
        self.npm_pkg().join("bin/npx-cli.js")
    }
}

fn install_dir(home: &NvHome, v: Version) -> PathBuf {
    home.node_dir().join(v.to_string())
}

/// Return the requested Node, downloading it first if needed.
pub fn ensure(home: &NvHome, v: Version) -> Result<Node> {
    let dir = install_dir(home, v);
    if dir.is_dir() {
        return Ok(Node::at(v, dir));
    }
    let _lock = home.lock(&format!("node-{v}"))?;
    if dir.is_dir() {
        // Another nv finished it while we waited.
        return Ok(Node::at(v, dir));
    }
    install(home, v, &dir)?;
    Ok(Node::at(v, dir))
}

fn install(home: &NvHome, v: Version, dest: &Path) -> Result<()> {
    let mirror = std::env::var("NV_NODE_MIRROR").unwrap_or_else(|_| DEFAULT_MIRROR.to_string());
    let mirror = mirror.trim_end_matches('/');
    let stem = format!("node-v{v}-{}", platform()?);
    let file = format!("{stem}.{}", archive_ext());

    eprintln!(
        "nv: {}",
        crate::tr!(
            "Node {v} を {mirror} から取ります",
            "downloading Node {v} from {mirror}"
        )
    );
    let sums = http::get_text(&format!("{mirror}/v{v}/SHASUMS256.txt"))?;
    let tmp = home.new_tmp(&format!("node-{v}"))?;
    let result = (|| -> Result<()> {
        let archive = tmp.join(&file);
        let actual = http::download(&format!("{mirror}/v{v}/{file}"), &archive)?;
        http::verify(http::checksum_for(&sums, &file), &actual, &file)?;

        let unpacked = tmp.join("unpacked");
        fs::create_dir(&unpacked)?;
        extract(&archive, &unpacked)
            .with_context(|| crate::tr!("{file} を展開できません", "cannot unpack {file}"))?;
        let top = unpacked.join(&stem);
        if !top.is_dir() {
            bail!(crate::tr!(
                "{file} に {stem}/ がありません",
                "{file} does not contain {stem}/"
            ));
        }
        fs::create_dir_all(home.node_dir())?;
        // The rename is the last step: a version folder only exists once complete.
        fs::rename(&top, dest).with_context(|| {
            crate::tr!(
                "Node を {} へ移せません",
                "cannot move Node into {}",
                dest.display()
            )
        })?;
        Ok(())
    })();
    let _ = remove_any(&tmp);
    result?;
    eprintln!(
        "nv: {}",
        crate::tr!("Node {v} を入れました", "installed Node {v}")
    );
    Ok(())
}

fn extract(archive: &Path, into: &Path) -> Result<()> {
    let file = fs::File::open(archive)?;
    if cfg!(windows) {
        zip::ZipArchive::new(file)?.extract(into)?;
    } else {
        let gz = flate2::read::GzDecoder::new(file);
        let mut tar = tar::Archive::new(gz);
        tar.set_preserve_permissions(true);
        tar.unpack(into)?;
    }
    Ok(())
}

pub fn installed(home: &NvHome) -> Result<Vec<Version>> {
    let dir = home.node_dir();
    let mut versions: Vec<Version> = match fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str()?.parse().ok())
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            return Err(e)
                .with_context(|| crate::tr!("{} を読めません", "cannot read {}", dir.display()));
        }
    };
    versions.sort();
    Ok(versions)
}

pub fn uninstall(home: &NvHome, v: Version) -> Result<bool> {
    let dir = install_dir(home, v);
    let _lock = home.lock(&format!("node-{v}"))?;
    if !dir.is_dir() {
        return Ok(false);
    }
    remove_any(&dir)
        .with_context(|| crate::tr!("{} を消せません", "cannot remove {}", dir.display()))?;
    Ok(true)
}
