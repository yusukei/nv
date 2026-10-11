//! Small file helpers: hashing copies and private temporary files.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Copy `reader` into a new file at `dest`, returning the SHA-256 of the bytes.
pub fn copy_hashing(reader: &mut dyn Read, dest: &Path) -> Result<String> {
    let mut file = File::create(dest)
        .with_context(|| crate::tr!("{} を作れません", "cannot create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).context(crate::tr!(
            "取得が途中で切れました",
            "the download was interrupted"
        ))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).with_context(|| {
            crate::tr!("{} に書き込めません", "cannot write {}", dest.display())
        })?;
    }
    file.sync_all().ok();
    Ok(hex(&hasher.finalize()))
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .with_context(|| crate::tr!("{} を開けません", "cannot open {}", path.display()))?;
    copy_hashing_to_sink(&mut file)
}

fn copy_hashing_to_sink(reader: &mut dyn Read) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// A folder only this process uses, removed when dropped.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Result<TempDir> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "wrangler-compose-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir(&dir)
            .with_context(|| crate::tr!("{} を作れません", "cannot create {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(TempDir(dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
