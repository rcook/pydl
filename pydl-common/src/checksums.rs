//! Compile-time embedded SHA-256 checksums from `../checksums/<tag>.sha256sums`.
//!
//! The build script (`build.rs`) generates a `&[(tag, raw_contents)]` slice;
//! this module parses it into a lookup map on demand and verifies downloads.

use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

include!(concat!(env!("OUT_DIR"), "/embedded_checksums.rs"));

/// Parsed view of the embedded checksums: `tag → (asset_name → hex hash)`.
type Table = HashMap<&'static str, HashMap<&'static str, &'static str>>;

fn table() -> &'static Table {
    static CELL: OnceLock<Table> = OnceLock::new();
    CELL.get_or_init(|| {
        let mut t: Table = HashMap::with_capacity(EMBEDDED_CHECKSUMS.len());
        for (tag, contents) in EMBEDDED_CHECKSUMS {
            t.insert(tag, parse_sha256sums(contents));
        }
        t
    })
}

fn parse_sha256sums_inner<'a, N, H>(
    body: &'a str,
    make_name: impl Fn(&'a str) -> N,
    make_hash: impl Fn(&'a str) -> H,
) -> HashMap<N, H>
where
    N: Eq + std::hash::Hash,
{
    let mut map = HashMap::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let Some(hash) = parts.next() else { continue };
        let Some(rest) = parts.next() else { continue };
        let name = rest.trim_start();
        if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) && !name.is_empty() {
            map.insert(make_name(name), make_hash(hash));
        }
    }
    map
}

fn parse_sha256sums(body: &'static str) -> HashMap<&'static str, &'static str> {
    parse_sha256sums_inner(body, |n| n, |h| h)
}

/// Owned-string variant of [`parse_sha256sums`] for runtime-fetched manifests
/// (e.g. `pydl self-update` downloading a release's `SHA256SUMS`). Same
/// permissive parsing rules as the build-time version.
#[must_use]
pub fn parse_sha256sums_owned(body: &str) -> HashMap<String, String> {
    parse_sha256sums_inner(body, str::to_owned, str::to_ascii_lowercase)
}

/// Stream `path` through SHA-256 and return the digest as lowercase hex.
///
/// Reused by `pydl install` (verifying a downloaded asset against its embedded
/// expected hash) and `pydl self-update` (verifying a release archive against
/// the `SHA256SUMS` manifest). The 64 KiB buffer is heap-allocated to keep the
/// stack below clippy's large-array threshold.
pub fn sha256_file(path: &Path) -> Result<String> {
    let file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_digest(hasher))
}

/// Build a `{sha256-of-asset-name-hex → asset_name}` map on demand.
///
/// This is the install-dir identifier used by `pydl install`, so
/// `pydl installed` can reverse the opaque directory names back into the
/// human-readable asset names.
fn install_hash_index() -> &'static HashMap<String, &'static str> {
    static CELL: OnceLock<HashMap<String, &'static str>> = OnceLock::new();
    CELL.get_or_init(|| {
        let t = table();
        let mut idx = HashMap::new();
        for by_name in t.values() {
            for name in by_name.keys() {
                let mut h = Sha256::new();
                h.update(name.as_bytes());
                let hex = hex_digest(h);
                // The same asset filename routinely appears under multiple
                // tags (each release republishes the per-platform builds).
                // The reverse-lookup target is the *name*, not the tag, so
                // first-write-wins is correct and any subsequent insert for
                // the same hash is a no-op.
                idx.entry(hex).or_insert(*name);
            }
        }
        idx
    })
}

/// Iterate every `(tag, asset_name)` pair the binary carries at build time.
///
/// Source of truth for offline filter resolution (`pydl install`, `python`,
/// `uninstall`, `pin`): the CLI walks this iterator instead of the live
/// GitHub release list. Order is unspecified; callers that need
/// deterministic output should sort themselves.
pub fn iter_embedded_assets() -> impl Iterator<Item = (&'static str, &'static str)> {
    table()
        .iter()
        .flat_map(|(tag, by_name)| by_name.keys().map(move |name| (*tag, *name)))
}

/// The newest tag in the embedded table, or `None` if the table is empty.
///
/// Tags are `YYYYMMDD` date stamps, so the lexicographic max is also the
/// chronologically newest.
#[must_use]
pub fn newest_embedded_tag() -> Option<&'static str> {
    table().keys().copied().max()
}

/// Whether the binary's embedded table has checksums for `tag`.
#[must_use]
pub fn has_tag(tag: &str) -> bool {
    table().contains_key(tag)
}

/// Reverse-lookup the install-dir identifier back into the asset name.
///
/// Given the hex SHA-256 of the asset name (the directory name used by
/// `pydl install`), return the asset name if we have it embedded. Used by
/// `pydl installed` to label otherwise-opaque `<hash>` directories.
#[must_use]
pub fn asset_name_for_install_hash(install_hash: &str) -> Option<&'static str> {
    install_hash_index().get(install_hash).copied()
}

/// Look up the expected SHA-256 for `asset_name` in release `tag`.
///
/// Errors distinguish "no checksums embedded for this tag" from "tag is
/// present but this specific asset isn't listed", so a missing checksum is
/// never silently treated as a pass.
pub fn expected_hash(tag: &str, asset_name: &str) -> Result<&'static str> {
    let Some(by_name) = table().get(tag) else {
        bail!(
            "no embedded checksums for release {tag:?} — rerun `get-checksums ./checksums` and rebuild"
        );
    };
    let Some(hash) = by_name.get(asset_name) else {
        bail!("release {tag:?} has embedded checksums but none for asset {asset_name:?}");
    };
    Ok(hash)
}

/// Render a `Sha256` hasher's digest as lower-case hex.
///
/// Matches the format used in the `.sha256sums` files.
#[must_use]
pub fn hex_digest(h: Sha256) -> String {
    let out = h.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for byte in out {
        use std::fmt::Write;
        write!(&mut s, "{byte:02x}").expect("write to String never fails");
    }
    s
}

/// Compare `actual` against `expected` case-insensitively on the hex chars.
///
/// The upstream `.sha256sums` files are lower-case in practice, but make the
/// check robust anyway — an upper-case hex digit should still verify.
#[must_use]
pub const fn hashes_match(expected: &str, actual: &str) -> bool {
    // `str::eq_ignore_ascii_case` is const since 1.87.
    expected.eq_ignore_ascii_case(actual)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111  one.tar.gz
bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222  two.tar.zst

# a comment
not a valid line
cccc3333cccc3333cccc3333cccc3333cccc3333cccc3333cccc3333cccc3333 three-tab.tar.gz
";

    #[test]
    fn parses_well_formed_lines() {
        let parsed = parse_sha256sums(SAMPLE);
        assert_eq!(parsed.len(), 3);
        assert_eq!(
            parsed.get("one.tar.gz"),
            Some(&"aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111"),
        );
        assert_eq!(
            parsed.get("two.tar.zst"),
            Some(&"bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222"),
        );
        assert_eq!(
            parsed.get("three-tab.tar.gz"),
            Some(&"cccc3333cccc3333cccc3333cccc3333cccc3333cccc3333cccc3333cccc3333"),
        );
    }

    #[test]
    fn skips_comments_and_garbage() {
        let parsed = parse_sha256sums("# only a comment\ngibberish\n");
        assert!(parsed.is_empty());
    }

    #[test]
    fn rejects_non_hex_hash() {
        // 64 chars but contains non-hex `z`.
        let bad = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz  bad.tar.gz\n";
        let parsed = parse_sha256sums(bad);
        assert!(parsed.is_empty());
    }

    #[test]
    fn rejects_short_hash() {
        let parsed = parse_sha256sums("deadbeef  short.tar.gz\n");
        assert!(parsed.is_empty());
    }

    #[test]
    fn hex_digest_is_lowercase_64_chars() {
        let mut h = Sha256::new();
        h.update(b"hello");
        let hex = hex_digest(h);
        assert_eq!(hex.len(), 64);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
        // Known value for SHA-256 of "hello"
        assert_eq!(
            hex,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn expected_hash_errors_when_tag_missing() {
        let err = expected_hash("definitely-not-a-real-tag", "x.tar.gz").unwrap_err();
        assert!(
            err.to_string().contains("no embedded checksums"),
            "got: {err}"
        );
    }

    #[test]
    fn hashes_match_accepts_equal() {
        let h = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(hashes_match(h, h));
    }

    #[test]
    fn hashes_match_rejects_different() {
        assert!(!hashes_match(
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ));
    }

    #[test]
    fn hashes_match_is_case_insensitive() {
        assert!(hashes_match(
            "2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824",
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
        ));
    }

    /// End-to-end: the same hex digest produced by `hex_digest` after hashing
    /// a known payload should pass `hashes_match` against the canonical
    /// lower-case hex of that payload. This is the invariant the runtime
    /// verifier depends on.
    #[test]
    fn hex_digest_plus_hashes_match_succeed_on_good_data() {
        let mut h = Sha256::new();
        h.update(b"hello");
        let actual = hex_digest(h);
        let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(hashes_match(expected, &actual));
    }

    #[test]
    fn hex_digest_plus_hashes_match_reject_tampered_data() {
        let mut h = Sha256::new();
        h.update(b"hello but tampered");
        let actual = hex_digest(h);
        let expected_for_original =
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(!hashes_match(expected_for_original, &actual));
    }

    /// The verifier must also reject a legitimate-looking hash that differs
    /// only in a few characters — common for bit-flip / truncation scenarios.
    #[test]
    fn hashes_match_rejects_one_char_off() {
        let good = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let bad = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9825";
        assert!(!hashes_match(good, bad));
    }

    #[test]
    fn asset_name_for_install_hash_returns_none_for_unknown() {
        // 64 hex chars, deterministically not in the embedded table.
        let unknown = "0".repeat(64);
        assert!(asset_name_for_install_hash(&unknown).is_none());
    }

    #[test]
    fn asset_name_for_install_hash_round_trips_known_asset() {
        // Pick any asset from the embedded table, compute its install hash,
        // and verify the reverse lookup returns the same name. Depends on
        // the committed checksums containing at least one entry — which is
        // guaranteed by the checksums/ directory being present in the repo.
        let t = table();
        let Some((_, by_name)) = t.iter().next() else {
            // If the embedded table is empty, there's nothing to test.
            return;
        };
        let Some(name) = by_name.keys().next() else {
            return;
        };

        let mut h = Sha256::new();
        h.update(name.as_bytes());
        let install_hash = hex_digest(h);

        assert_eq!(asset_name_for_install_hash(&install_hash), Some(*name));
    }
}
