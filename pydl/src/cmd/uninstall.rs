//! `pydl uninstall` — remove an installed asset directory from
//! `$HOME/.pydl/asset/`.
//!
//! Two modes:
//!
//! - Filter mode: resolve a single asset the same way `pydl install` does and
//!   remove `$HOME/.pydl/asset/<sha256(asset.name)>/`.
//! - `--all` mode: walk every hash-shaped directory under the install root and
//!   remove them all.
//!
//! Both modes default to a dry-run preview (non-zero exit) and require
//! `--yes` to actually delete. Alongside the final `<hash>` directory, any
//! leftover staging directories matching `<hash>.<suffix>` at the same level
//! are also removed when `--yes` is passed — these are created by
//! `pydl-common::install::install_from_archive` and normally cleaned up by
//! the `TempDir` drop, but a crash mid-install can leave them behind.

use std::path::{Path, PathBuf};
use std::{fs, io};

use anyhow::{Context, Result, bail};
use clap::Parser;
use pydl_common::checksums;
use pydl_common::filter::{
    FilterArgs, apply_config_defaults, auto_select_tag_embedded, filter_embedded,
    pick_single_embedded,
};
use pydl_common::install::{asset_hash, install_root, is_install_hash};

#[derive(Parser, Debug)]
pub struct Args {
    #[command(flatten)]
    pub filter: FilterArgs,

    /// Remove every installed asset under `$HOME/.pydl/asset/`. Mutually
    /// exclusive with the filter flags — `--all` implies "match everything".
    #[arg(long, conflicts_with_all = ["tag", "version"])]
    pub all: bool,

    /// Confirm the destructive operation. Without this flag, `uninstall`
    /// prints what would be removed and exits non-zero.
    #[arg(long)]
    pub r#yes: bool,
}

// `args` by value matches the dispatch shape of every other subcommand.
#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> Result<()> {
    let Args { filter, all, r#yes } = args;

    if all {
        return run_all(r#yes);
    }
    run_single(filter, r#yes)
}

fn run_single(filter: FilterArgs, confirmed: bool) -> Result<()> {
    let mut filter = apply_config_defaults(filter)?;
    auto_select_tag_embedded(&mut filter)?;

    let hits = filter_embedded(&filter)?;
    // The tag is only used to resolve the asset name through the filter; the
    // install dir is named by SHA-256 of the asset name alone (see
    // `pydl-common::install::asset_hash`), so the resolved tag is dropped
    // once we have the name.
    let (_tag, asset_name) = pick_single_embedded(&hits)?;

    let hash = asset_hash(asset_name);
    let root = install_root()?;
    let final_dir = root.join(&hash);

    if !final_dir.exists() {
        bail!(
            "{asset_name} is not installed at {} (nothing to uninstall)",
            final_dir.display()
        );
    }

    let staging = staging_dirs_for_hash(&root, &hash)?;

    if !confirmed {
        println!("would remove {asset_name} at {}", final_dir.display());
        for s in &staging {
            println!("would remove {}", s.display());
        }
        std::process::exit(2);
    }

    remove_path(&final_dir)?;
    println!("removed {asset_name} at {}", final_dir.display());
    for s in &staging {
        remove_path(s)?;
        println!("removed {}", s.display());
    }
    Ok(())
}

fn run_all(confirmed: bool) -> Result<()> {
    let root = install_root()?;
    let hash_dirs = hash_shaped_children(&root)?;

    if hash_dirs.is_empty() {
        println!("no installed assets under {}", root.display());
        return Ok(());
    }

    if !confirmed {
        for dir in &hash_dirs {
            println!("would remove {}", format_asset_dir(dir));
        }
        std::process::exit(2);
    }

    for dir in &hash_dirs {
        remove_path(dir)?;
        println!("removed {}", format_asset_dir(dir));
    }
    Ok(())
}

fn format_asset_dir(dir: &Path) -> String {
    let hash = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    checksums::asset_name_for_install_hash(hash).map_or_else(
        || format!("{}", dir.display()),
        |name| format!("{name} at {}", dir.display()),
    )
}

fn collect_dir_entries(root: &Path, pred: impl Fn(&str) -> bool) -> Result<Vec<PathBuf>> {
    let read = match fs::read_dir(root) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
    };
    let mut out = Vec::new();
    for entry in read {
        let entry = entry.with_context(|| format!("iterating {}", root.display()))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if pred(&name) {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

fn hash_shaped_children(root: &Path) -> Result<Vec<PathBuf>> {
    collect_dir_entries(root, is_install_hash)
}

fn staging_dirs_for_hash(root: &Path, hash: &str) -> Result<Vec<PathBuf>> {
    let prefix = format!("{hash}.");
    collect_dir_entries(root, |name| name.starts_with(&prefix))
}

fn remove_path(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.is_dir() {
        fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
    } else {
        fs::remove_file(path).with_context(|| format!("removing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_shaped_children_filters_staging_and_non_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        fs::create_dir(root.join(&a)).unwrap();
        fs::create_dir(root.join(&b)).unwrap();
        fs::create_dir(root.join(format!("{a}.staging"))).unwrap();
        fs::create_dir(root.join("not-a-hash")).unwrap();
        let found = hash_shaped_children(root).unwrap();
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![a, b]);
    }

    #[test]
    fn staging_dirs_for_hash_matches_only_prefixed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let h = "a".repeat(64);
        fs::create_dir(root.join(&h)).unwrap();
        fs::create_dir(root.join(format!("{h}.abc"))).unwrap();
        fs::create_dir(root.join(format!("{h}.xyz"))).unwrap();
        fs::create_dir(root.join(format!("{}.foo", "b".repeat(64)))).unwrap();
        let out = staging_dirs_for_hash(root, &h).unwrap();
        let names: Vec<String> = out
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![format!("{h}.abc"), format!("{h}.xyz")]);
    }

    #[test]
    fn hash_shaped_children_returns_empty_when_root_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope");
        let out = hash_shaped_children(&missing).unwrap();
        assert!(out.is_empty());
    }
}
