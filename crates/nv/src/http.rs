//! Downloads. ureq's default config reads ALL_PROXY / HTTPS_PROXY / HTTP_PROXY
//! and NO_PROXY from the environment (REQ-002), so one shared agent is enough.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::OnceLock;

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .user_agent(concat!("nv/", env!("CARGO_PKG_VERSION")))
            .build()
            .into()
    })
}

pub fn get_text(url: &str) -> Result<String> {
    let mut res = agent()
        .get(url)
        .call()
        .with_context(|| format!("cannot download {url}"))?;
    res.body_mut()
        .read_to_string()
        .with_context(|| format!("cannot read {url}"))
}

/// Stream `url` into `dest` and return the SHA-256 of what was written.
pub fn download(url: &str, dest: &Path) -> Result<String> {
    let res = agent()
        .get(url)
        .call()
        .with_context(|| format!("cannot download {url}"))?;
    let mut reader = res.into_body().into_reader();
    let mut file =
        File::create(dest).with_context(|| format!("cannot create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("download of {url} was interrupted"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .with_context(|| format!("cannot write {}", dest.display()))?;
    }
    file.sync_all().ok();
    Ok(hex(&hasher.finalize()))
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

pub fn sha256_bytes_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Find the hash for `file_name` in a `SHASUMS256.txt` / `SHA256SUMS` listing
/// (`<hex>  <name>` per line).
pub fn checksum_for<'a>(listing: &'a str, file_name: &str) -> Option<&'a str> {
    listing.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let hash = it.next()?;
        let name = it.next()?.trim_start_matches('*');
        (name == file_name).then_some(hash)
    })
}

pub fn verify(expected: Option<&str>, actual: &str, what: &str) -> Result<()> {
    match expected {
        None => bail!("{what} is not listed in the checksum file"),
        Some(e) if e.eq_ignore_ascii_case(actual) => Ok(()),
        Some(e) => bail!("checksum mismatch for {what}: expected {e}, got {actual}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = "\
aaaa  node-v24.11.1-darwin-arm64.tar.gz
bbbb  node-v24.11.1-win-x64.zip
cccc *nv-x86_64-pc-windows-msvc.exe
";

    #[test]
    fn finds_listed_files() {
        assert_eq!(
            checksum_for(LISTING, "node-v24.11.1-win-x64.zip"),
            Some("bbbb")
        );
        assert_eq!(
            checksum_for(LISTING, "nv-x86_64-pc-windows-msvc.exe"),
            Some("cccc")
        );
        assert_eq!(
            checksum_for(LISTING, "node-v24.11.1-linux-x64.tar.gz"),
            None
        );
    }

    #[test]
    fn verify_rejects_mismatch_and_missing() {
        assert!(verify(Some("AbC"), "abc", "x").is_ok());
        assert!(verify(Some("abc"), "abd", "x").is_err());
        assert!(verify(None, "abc", "x").is_err());
    }

    #[test]
    fn hashes_bytes() {
        assert_eq!(
            sha256_bytes_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
