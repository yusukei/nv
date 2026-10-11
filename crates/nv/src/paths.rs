//! Where nv keeps its files (DES-002). Everything lives under `~/.nv`.

use anyhow::{Context, Result, anyhow};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub fn home_dir() -> Result<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot find your home folder (${var} is not set)"))
}

pub struct NvHome {
    root: PathBuf,
}

impl NvHome {
    /// `NV_HOME` replaces `~/.nv` so tests never touch the user's real folder.
    pub fn locate() -> Result<Self> {
        let root = match std::env::var_os("NV_HOME").filter(|v| !v.is_empty()) {
            Some(p) => PathBuf::from(p),
            None => home_dir()?.join(".nv"),
        };
        Ok(NvHome { root })
    }

    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }
    pub fn node_dir(&self) -> PathBuf {
        self.cache().join("node")
    }
    pub fn npm_cache(&self) -> PathBuf {
        self.cache().join("npm")
    }
    pub fn tools_dir(&self) -> PathBuf {
        self.cache().join("tools")
    }
    pub fn tmp_dir(&self) -> PathBuf {
        self.cache().join("tmp")
    }
    fn locks_dir(&self) -> PathBuf {
        self.cache().join("locks")
    }

    /// A fresh, empty folder under `tmp/`, on the same disk as the cache so
    /// that finished work can be moved into place with a rename.
    pub fn new_tmp(&self, label: &str) -> Result<PathBuf> {
        let tmp = self.tmp_dir();
        fs::create_dir_all(&tmp).with_context(|| format!("cannot create {}", tmp.display()))?;
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = tmp.join(format!("{label}-{}-{nanos}", std::process::id()));
        fs::create_dir(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        Ok(dir)
    }

    /// Exclusive lock held until the returned file is dropped. Blocks while
    /// another nv holds it.
    pub fn lock(&self, name: &str) -> Result<File> {
        let dir = self.locks_dir();
        fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let path = dir.join(format!("{name}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        file.lock()
            .with_context(|| format!("cannot lock {}", path.display()))?;
        Ok(file)
    }

    /// Remove leftovers of interrupted downloads older than a day.
    pub fn clean_tmp(&self) {
        let Ok(entries) = fs::read_dir(self.tmp_dir()) else {
            return;
        };
        let cutoff = SystemTime::now() - Duration::from_secs(24 * 60 * 60);
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t < cutoff)
                .unwrap_or(false);
            if old {
                let _ = remove_any(&entry.path());
            }
        }
    }
}

pub fn remove_any(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
