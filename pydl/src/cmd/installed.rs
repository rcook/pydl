use std::path::{Path, PathBuf};
use std::{fs, io};

use anyhow::{Context, Result};
use clap::Parser;
use log::debug;
use pydl_common::asset::ParsedAsset;
use pydl_common::filter::FilterArgs;
use pydl_common::{checksums, pydl_root};

#[derive(Parser, Debug)]
pub struct Args {
    #[command(flatten)]
    pub filter: FilterArgs,
}

/// The install-dir hash is a fixed-length lowercase hex SHA-256. We only
/// consider entries that match exactly so staging dirs (`<hash>.xxxxxx`)
/// and any unrelated leftovers are ignored.
#[must_use]
fn is_install_hash(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// One row in the output listing.
#[derive(Debug, PartialEq, Eq)]
struct Listing {
    path: PathBuf,
    asset_name: Option<&'static str>,
}

fn collect_installed(asset_root: &Path) -> Result<Vec<Listing>> {
    let entries = match fs::read_dir(asset_root) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", asset_root.display()));
        }
    };

    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading entry in {}", asset_root.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("stat {}", entry.path().display()))?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !is_install_hash(name_str) {
            debug!("skipping non-hash entry {name_str}");
            continue;
        }
        out.push(Listing {
            path: entry.path(),
            asset_name: checksums::asset_name_for_install_hash(name_str),
        });
    }
    // Stable output; the hash prefix is sortable.
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

// `args` by value matches the dispatch shape of every other subcommand.
#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> Result<()> {
    let asset_root = pydl_root()?.join("asset");
    let listings = collect_installed(&asset_root)?;
    let filtered: Vec<&Listing> = listings
        .iter()
        .filter(|l| matches_filter(l, &args.filter))
        .collect();

    if listings.is_empty() {
        println!(
            "no installed assets under {} (install one with `pydl install --tag ... --version ...`)",
            asset_root.display()
        );
        return Ok(());
    }

    if filtered.is_empty() {
        println!(
            "no installed assets under {} match the current filter",
            asset_root.display()
        );
        return Ok(());
    }

    for Listing { path, asset_name } in filtered {
        let label = asset_name.unwrap_or("(unknown — no matching embedded checksum)");
        println!("{}  {label}", path.display());
    }
    Ok(())
}

/// Does `listing` satisfy the (already-CLI-parsed) filter flags?
///
/// `--tag` / `--version` match against the parsed build tag and version of
/// the listing's reverse-looked-up asset name. `--platform` / `--default-attrs`
/// are intentionally ignored here — they're about *selecting* among upstream
/// assets, not about querying the local install set, which only contains
/// assets that satisfied those filters at install time.
///
/// Listings whose asset name couldn't be reverse-looked-up (unknown install
/// hash) are kept when no tag/version filter is set, but filtered out when
/// either is set — there's nothing to match against.
fn matches_filter(listing: &Listing, filter: &FilterArgs) -> bool {
    if filter.tag.is_none() && filter.version.is_none() {
        return true;
    }
    let Some(name) = listing.asset_name else {
        return false;
    };
    let Ok(parsed) = ParsedAsset::parse(name) else {
        return false;
    };
    if let Some(tag) = filter.tag.as_deref()
        && parsed.build_tag != tag
    {
        return false;
    }
    if let Some(version) = filter.version.as_deref()
        && parsed.version != version
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_install_hash_accepts_64_lowercase_hex() {
        assert!(is_install_hash(&"a".repeat(64)));
        assert!(is_install_hash(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        ));
    }

    #[test]
    fn is_install_hash_rejects_wrong_length() {
        assert!(!is_install_hash(""));
        assert!(!is_install_hash("abc"));
        assert!(!is_install_hash(&"a".repeat(63)));
        assert!(!is_install_hash(&"a".repeat(65)));
    }

    #[test]
    fn is_install_hash_rejects_uppercase() {
        // Our install hashes are always written lowercase by `hex_digest`.
        // Rejecting uppercase prevents weird filesystems from accidentally
        // matching two distinct dir names as "the same" install.
        assert!(!is_install_hash(&"A".repeat(64)));
    }

    #[test]
    fn is_install_hash_rejects_non_hex_chars() {
        let mut bad = String::from("z");
        bad.push_str(&"a".repeat(63));
        assert!(!is_install_hash(&bad));
    }

    #[test]
    fn is_install_hash_rejects_staging_dir_suffix() {
        // The install flow creates staging dirs as `<hash>.<suffix>`. The dot
        // is the marker that disqualifies them.
        let staging = format!("{}.abcdef", "a".repeat(64));
        assert!(!is_install_hash(&staging));
    }

    #[test]
    fn collect_installed_returns_empty_when_root_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope");
        let out = collect_installed(&missing).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn collect_installed_returns_empty_for_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let out = collect_installed(tmp.path()).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn collect_installed_filters_non_hash_dirs_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let hash1 = "a".repeat(64);
        let hash2 = "b".repeat(64);
        fs::create_dir(root.join(&hash1)).unwrap();
        fs::create_dir(root.join(&hash2)).unwrap();
        fs::create_dir(root.join("not-a-hash")).unwrap();
        // staging-like dir
        fs::create_dir(root.join(format!("{hash1}.staging"))).unwrap();
        // regular file at the root
        fs::write(root.join(format!("{}.txt", "c".repeat(64))), b"x").unwrap();

        let out = collect_installed(root).unwrap();
        assert_eq!(out.len(), 2);
        let names: Vec<String> = out
            .iter()
            .map(|l| l.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&hash1));
        assert!(names.contains(&hash2));
    }

    #[test]
    fn collect_installed_marks_unknown_hash_with_none() {
        let tmp = tempfile::tempdir().unwrap();
        let unknown = "0".repeat(64);
        fs::create_dir(tmp.path().join(&unknown)).unwrap();
        let out = collect_installed(tmp.path()).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].asset_name.is_none());
    }

    #[test]
    fn collect_installed_is_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let c = "c".repeat(64);
        // Create them out of order.
        fs::create_dir(tmp.path().join(&c)).unwrap();
        fs::create_dir(tmp.path().join(&a)).unwrap();
        fs::create_dir(tmp.path().join(&b)).unwrap();

        let out = collect_installed(tmp.path()).unwrap();
        let names: Vec<String> = out
            .iter()
            .map(|l| l.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![a, b, c]);
    }

    fn listing(asset_name: Option<&'static str>) -> Listing {
        Listing {
            path: PathBuf::from("/fake"),
            asset_name,
        }
    }

    fn bare_filter() -> FilterArgs {
        FilterArgs {
            tag: None,
            version: None,
            platform: false,
            no_platform: true,
            default_attrs: false,
            no_default_attrs: true,
        }
    }

    #[test]
    fn matches_filter_no_filters_accepts_everything() {
        let l = listing(None);
        assert!(matches_filter(&l, &bare_filter()));
    }

    #[test]
    fn matches_filter_tag_set_rejects_unknown_hash() {
        let l = listing(None);
        let mut f = bare_filter();
        f.tag = Some("20260101".to_owned());
        assert!(!matches_filter(&l, &f));
    }

    #[test]
    fn matches_filter_tag_matches() {
        let l = listing(Some(
            "cpython-3.13.2+20260101-x86_64-unknown-linux-gnu-install_only.tar.gz",
        ));
        let mut f = bare_filter();
        f.tag = Some("20260101".to_owned());
        assert!(matches_filter(&l, &f));
    }

    #[test]
    fn matches_filter_tag_mismatch() {
        let l = listing(Some(
            "cpython-3.13.2+20260101-x86_64-unknown-linux-gnu-install_only.tar.gz",
        ));
        let mut f = bare_filter();
        f.tag = Some("20250101".to_owned());
        assert!(!matches_filter(&l, &f));
    }

    #[test]
    fn matches_filter_version_matches() {
        let l = listing(Some(
            "cpython-3.13.2+20260101-x86_64-unknown-linux-gnu-install_only.tar.gz",
        ));
        let mut f = bare_filter();
        f.version = Some("3.13.2".to_owned());
        assert!(matches_filter(&l, &f));
    }

    #[test]
    fn matches_filter_version_mismatch() {
        let l = listing(Some(
            "cpython-3.13.2+20260101-x86_64-unknown-linux-gnu-install_only.tar.gz",
        ));
        let mut f = bare_filter();
        f.version = Some("3.12.0".to_owned());
        assert!(!matches_filter(&l, &f));
    }
}
