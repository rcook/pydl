//! `pydl cache {info,clear}` — inspect and wipe the on-disc HTTP cache under
//! `$HOME/.pydl/cache/`.
//!
//! The cache is populated by `pydl-cache` whenever any subcommand makes a
//! GET request. It's safe to clear at any time — the next call just re-fetches.

use std::path::Path;
use std::{fs, io};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use log::info;
use pydl_common::cache_dir;

#[derive(Parser, Debug)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: CacheCmd,
}

#[derive(Subcommand, Debug)]
pub enum CacheCmd {
    /// Print the cache directory path, entry count and total size.
    Info,

    /// Remove all files under the cache directory. Requires `--yes` to
    /// actually delete; without it, prints what would be removed and exits
    /// non-zero so callers can dry-run safely.
    Clear {
        /// Confirm the destructive operation.
        #[arg(long)]
        r#yes: bool,
    },
}

// `args` by value matches the dispatch shape of every other subcommand.
#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> Result<()> {
    match args.cmd {
        CacheCmd::Info => run_info(),
        CacheCmd::Clear { r#yes } => run_clear(r#yes),
    }
}

fn run_info() -> Result<()> {
    let dir = cache_dir()?;
    let (entries, bytes) = walk(&dir)?;
    println!("path: {}", dir.display());
    println!("entries: {entries}");
    println!("bytes: {bytes}");
    Ok(())
}

fn run_clear(confirmed: bool) -> Result<()> {
    let dir = cache_dir()?;
    let (entries, bytes) = walk(&dir)?;

    if !confirmed {
        println!(
            "would remove {entries} entries ({bytes} bytes) from {} — pass --yes to confirm",
            dir.display()
        );
        std::process::exit(2);
    }

    if entries == 0 {
        info!("cache at {} is already empty", dir.display());
        return Ok(());
    }

    let read = match fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in read {
        let entry = entry.with_context(|| format!("iterating {}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("stat {}", path.display()))?;
        if file_type.is_dir() {
            fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))?;
        } else {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    info!(
        "cleared {entries} entries ({bytes} bytes) from {}",
        dir.display()
    );
    Ok(())
}

/// Return `(entry_count, total_bytes)` for every file reachable from `root`.
/// A missing directory is treated as empty (zero entries, zero bytes) —
/// makes `cache info` work before the cache has ever been populated.
fn walk(root: &Path) -> Result<(u64, u64)> {
    let mut entries = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };
        for entry in read {
            let entry = entry.with_context(|| format!("iterating {}", dir.display()))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .with_context(|| format!("stat {}", path.display()))?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                entries += 1;
                let meta = entry
                    .metadata()
                    .with_context(|| format!("metadata for {}", path.display()))?;
                bytes += meta.len();
            }
        }
    }
    Ok((entries, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_missing_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope");
        let (entries, bytes) = walk(&missing).unwrap();
        assert_eq!(entries, 0);
        assert_eq!(bytes, 0);
    }

    #[test]
    fn walk_counts_files_and_bytes_recursively() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("a"), b"hello").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub").join("b"), b"world!").unwrap();
        let (entries, bytes) = walk(root).unwrap();
        assert_eq!(entries, 2);
        assert_eq!(bytes, 5 + 6);
    }
}
