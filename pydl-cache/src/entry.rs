use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log::warn;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize, Clone)]
pub struct EntryMeta {
    pub status: u16,
    pub fetched_at: u64,
    pub expires_at: Option<u64>,
    pub must_revalidate: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

pub struct EntryPaths {
    pub meta: PathBuf,
    pub body: PathBuf,
}

impl EntryPaths {
    pub fn tmp_body(&self) -> PathBuf {
        self.body.with_extension("body.tmp")
    }
}

pub fn read_meta(path: &Path) -> Result<Option<EntryMeta>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("reading cache meta"),
    };
    let meta: EntryMeta = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing meta at {}", path.display()))?;
    Ok(Some(meta))
}

pub fn write_meta(path: &Path, meta: &EntryMeta) -> Result<()> {
    let tmp = path.with_extension("meta.tmp");
    fs::write(&tmp, serde_json::to_vec(meta)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

pub fn entry_paths(cache_dir: &Path, canonical: &str) -> EntryPaths {
    let digest = Sha256::digest(canonical.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        write!(&mut hex, "{byte:02x}").expect("write to String never fails");
    }
    let stem = cache_dir.join(hex);
    EntryPaths {
        meta: stem.with_extension("meta"),
        body: stem.with_extension("body"),
    }
}

pub fn load_entry(cache_dir: &Path, canonical: &str) -> Result<Option<(EntryPaths, EntryMeta)>> {
    let paths = entry_paths(cache_dir, canonical);
    let Some(meta) = read_meta(&paths.meta)? else {
        return Ok(None);
    };
    if !paths.body.exists() {
        warn!(
            "cache meta without body at {}, ignoring",
            paths.body.display()
        );
        return Ok(None);
    }
    Ok(Some((paths, meta)))
}

pub fn file_len(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|m| m.len())
}
