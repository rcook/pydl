//! Shared install pipeline: verify a previously-cached archive, unpack into
//! `$HOME/.pydl/asset/<hash>/`.
//!
//! Used by the `pydl install` subcommand (install and exit) and by
//! `pydl python` (install and then invoke the bundled interpreter). Both
//! expect the archive bytes to already be on disc (typically warmed by
//! `pydl download` into the pull-through HTTP cache); this module never
//! touches the network.
//!
//! The staging/rename pattern keeps the final `<hash>` directory atomic:
//! it either contains a fully-unpacked distribution or doesn't exist at all.

use std::fmt::Write as _;
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use log::debug;
use sha2::{Digest, Sha256};

use crate::asset::asset_sort_key;
use crate::filter::{Asset, Release};
use crate::{checksums, pydl_root};

/// Per-asset directory name — SHA-256 of `asset.name`, hex-encoded.
#[must_use]
pub fn asset_hash(name: &str) -> String {
    let mut h = Sha256::new();
    h.update(name.as_bytes());
    checksums::hex_digest(h)
}

/// Root install directory — `$HOME/.pydl/asset/`.
pub fn install_root() -> Result<PathBuf> {
    Ok(pydl_root()?.join("asset"))
}

/// Outcome of [`install_from_archive`].
#[derive(Debug)]
pub struct Installation {
    /// Absolute path of `$HOME/.pydl/asset/<hash>/`.
    pub dir: PathBuf,
    /// `true` if the directory already existed and nothing was downloaded.
    pub already_present: bool,
}

/// Flatten the filter result into a single `(release, asset)` pair, or return
/// a descriptive message explaining why it couldn't. Shared with the other
/// binaries because the phrasing should be consistent.
pub fn pick_single_asset<'a>(
    groups: &'a [(&'a Release, Vec<&'a Asset>)],
) -> Result<(&'a Release, &'a Asset)> {
    let total: usize = groups.iter().map(|(_, a)| a.len()).sum();
    if total == 0 {
        bail!("no assets matched the filter — widen it (try --no-default-attrs or --no-platform)");
    }
    if total > 1 {
        let mut msg = format!("{total} assets matched — narrow the filter. Candidates:\n");
        for (release, assets) in groups {
            // Sort each release's assets by name so the error is deterministic
            // and easy to scan. Outer release order is preserved.
            let mut sorted: Vec<&Asset> = assets.clone();
            sorted.sort_by_cached_key(|a| asset_sort_key(&a.name));
            for asset in sorted {
                writeln!(
                    msg,
                    "  {} / {} ({} bytes)",
                    release.tag_name, asset.name, asset.size
                )
                .expect("write to String never fails");
            }
        }
        bail!("{msg}");
    }
    let (release, assets) = &groups[0];
    Ok((release, assets[0]))
}

/// Install `asset_name` (from release `tag`) if
/// `$HOME/.pydl/asset/<asset-hash>/` doesn't already exist.
///
/// Reads the archive bytes from `archive_path` (typically the `pydl-cache`
/// body path for a previously-`pydl download`-warmed entry), verifies the
/// SHA-256 against the embedded expected hash, and unpacks into the final
/// install directory via the staging/rename pattern.
///
/// This function never touches the network. Call sites wanting to ensure
/// the archive is present on disc first should prefer
/// [`pydl_cache::CachingClient::cached_body_path`] for the cached body
/// location; `pydl download` is the canonical way to warm that cache.
pub fn install_from_archive(
    archive_path: &Path,
    tag: &str,
    asset_name: &str,
) -> Result<Installation> {
    // Verify a checksum exists for this asset before we read anything. An
    // un-checksummed asset is never installed, even partially.
    let expected_hash = checksums::expected_hash(tag, asset_name)?;

    let hash = asset_hash(asset_name);
    let root = install_root()?;
    let final_dir = root.join(&hash);

    if final_dir.exists() {
        debug!("{asset_name} already installed at {}", final_dir.display());
        return Ok(Installation {
            dir: final_dir,
            already_present: true,
        });
    }

    // Verify the archive bytes match the expected hash before we commit to
    // unpacking. Streaming-hash the file so we don't need to fit it all in
    // memory — asset archives can be 20MB+.
    verify_sha256(archive_path, expected_hash, asset_name)?;

    fs::create_dir_all(&root)
        .with_context(|| format!("creating install root {}", root.display()))?;

    // Stage under a sibling directory so the final `<hash>` path is never
    // partially populated. `tempfile::TempDir` cleans up on drop if any step
    // fails before the rename.
    let staging = tempfile::Builder::new()
        .prefix(&format!("{hash}."))
        .tempdir_in(&root)
        .with_context(|| format!("creating staging dir in {}", root.display()))?;

    debug!(
        "unpacking {asset_name} from {} into {}",
        archive_path.display(),
        staging.path().display()
    );

    // Unpack into a sub-directory so we can atomically rename it into place.
    let unpack_into = staging.path().join("unpack");
    fs::create_dir_all(&unpack_into)
        .with_context(|| format!("creating {}", unpack_into.display()))?;
    unpack(archive_path, asset_name, &unpack_into)?;

    fs::rename(&unpack_into, &final_dir).with_context(|| {
        format!(
            "renaming {} -> {}",
            unpack_into.display(),
            final_dir.display()
        )
    })?;

    debug!("installed {asset_name} at {}", final_dir.display());
    drop(staging);

    Ok(Installation {
        dir: final_dir,
        already_present: false,
    })
}

/// Stream `archive_path` through SHA-256 and compare against `expected_hex`.
/// Errors (with `asset_name` in the message) on mismatch so installs against
/// a corrupted cache entry bail before we touch the install root.
fn verify_sha256(archive_path: &Path, expected_hex: &str, asset_name: &str) -> Result<()> {
    let file = fs::File::open(archive_path)
        .with_context(|| format!("opening {}", archive_path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    // Heap-allocated to keep the stack below clippy's large-array threshold.
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("reading {}", archive_path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let actual = checksums::hex_digest(hasher);
    if !checksums::hashes_match(expected_hex, &actual) {
        bail!(
            "sha256 mismatch for {asset_name} at {}: expected {expected_hex}, got {actual}",
            archive_path.display()
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compression {
    Gzip,
    Zstd,
}

fn classify(asset_name: &str) -> Result<Compression> {
    if asset_name.ends_with(".tar.gz") {
        Ok(Compression::Gzip)
    } else if asset_name.ends_with(".tar.zst") {
        Ok(Compression::Zstd)
    } else {
        bail!("unsupported archive extension for {asset_name:?}");
    }
}

fn unpack(archive_path: &Path, asset_name: &str, dest_dir: &Path) -> Result<()> {
    let compression = classify(asset_name)?;
    let file = fs::File::open(archive_path)
        .with_context(|| format!("opening {}", archive_path.display()))?;
    let reader = BufReader::new(file);
    match compression {
        Compression::Gzip => unpack_tar(flate2::read::GzDecoder::new(reader), dest_dir),
        Compression::Zstd => {
            let zst = zstd::Decoder::new(reader).with_context(|| {
                format!("initialising zstd decoder for {}", archive_path.display())
            })?;
            unpack_tar(zst, dest_dir)
        }
    }
}

fn unpack_tar<R: Read>(reader: R, dest_dir: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    // The archive bytes are SHA-256-verified before this point (see
    // `verify_sha256` in this module), so we treat them as trusted. The
    // settings below pin `tar`'s defaults so a future crate update can't
    // quietly relax them under us:
    //   - `set_overwrite(true)` — replace any pre-existing file under
    //     `dest_dir`. The caller always passes a freshly-created staging
    //     directory, so there's nothing to clobber in practice; this just
    //     locks the behaviour in.
    //   - `set_preserve_permissions(true)` — needed so the bundled
    //     `bin/python3` lands executable on Unix.
    //   - `set_unpack_xattrs(false)` — keeps platform-specific xattrs out
    //     of the install dir; upstream archives don't carry meaningful
    //     ones.
    archive.set_overwrite(true);
    archive.set_preserve_permissions(true);
    archive.set_unpack_xattrs(false);
    archive
        .unpack(dest_dir)
        .with_context(|| format!("unpacking archive into {}", dest_dir.display()))?;
    Ok(())
}

/// Absolute path to the bundled `python` interpreter inside an install dir.
///
/// python-build-standalone lays the distribution out with an `install/` root
/// named `python/`, with POSIX `bin/python3` on Unix and `python.exe` at the
/// top level on Windows.
#[must_use]
pub fn python_binary(install_dir: &Path) -> PathBuf {
    if cfg!(windows) {
        install_dir.join("python").join("python.exe")
    } else {
        install_dir.join("python").join("bin").join("python3")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_hash_is_deterministic() {
        let a = asset_hash("cpython-3.14.4+20260414-aarch64-apple-darwin-install_only.tar.gz");
        let b = asset_hash("cpython-3.14.4+20260414-aarch64-apple-darwin-install_only.tar.gz");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn asset_hash_distinguishes_version_only_change() {
        let a = asset_hash("cpython-3.14.3+20260414-aarch64-apple-darwin-install_only.tar.gz");
        let b = asset_hash("cpython-3.14.4+20260414-aarch64-apple-darwin-install_only.tar.gz");
        assert_ne!(a, b);
    }

    #[test]
    fn classify_known_extensions() {
        assert_eq!(classify("foo.tar.gz").unwrap(), Compression::Gzip);
        assert_eq!(classify("foo.tar.zst").unwrap(), Compression::Zstd);
    }

    #[test]
    fn classify_rejects_unknown_extension() {
        let err = classify("something.zip").unwrap_err();
        assert!(
            err.to_string().contains("unsupported archive extension"),
            "got: {err}"
        );
    }

    #[test]
    fn unpack_tar_gz_roundtrip() {
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let archive_path = tmp.path().join("fake.tar.gz");
        {
            let file = fs::File::create(&archive_path).unwrap();
            let enc = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut builder = tar::Builder::new(enc);
            let content = b"hello from the test\n";
            let mut header = tar::Header::new_gnu();
            header.set_path("hello/world.txt").unwrap();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, &content[..]).unwrap();
            let enc = builder.into_inner().unwrap();
            let mut file = enc.finish().unwrap();
            file.flush().unwrap();
        }
        let dest = tmp.path().join("out");
        fs::create_dir_all(&dest).unwrap();
        unpack(&archive_path, "fake.tar.gz", &dest).unwrap();
        let body = fs::read_to_string(dest.join("hello").join("world.txt")).unwrap();
        assert_eq!(body, "hello from the test\n");
    }

    #[test]
    fn unpack_tar_zst_roundtrip() {
        use std::io::Write;

        let tmp = tempfile::tempdir().unwrap();
        let archive_path = tmp.path().join("fake.tar.zst");
        {
            let file = fs::File::create(&archive_path).unwrap();
            let enc = zstd::Encoder::new(file, 0).unwrap();
            let mut builder = tar::Builder::new(enc);
            let content = b"zstd payload\n";
            let mut header = tar::Header::new_gnu();
            header.set_path("inside/zst.txt").unwrap();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, &content[..]).unwrap();
            let enc = builder.into_inner().unwrap();
            let mut file = enc.finish().unwrap();
            file.flush().unwrap();
        }
        let dest = tmp.path().join("out");
        fs::create_dir_all(&dest).unwrap();
        unpack(&archive_path, "fake.tar.zst", &dest).unwrap();
        let body = fs::read_to_string(dest.join("inside").join("zst.txt")).unwrap();
        assert_eq!(body, "zstd payload\n");
    }

    #[test]
    fn python_binary_is_platform_appropriate() {
        let dir = Path::new("/tmp/fake-install");
        let p = python_binary(dir);
        if cfg!(windows) {
            assert!(p.ends_with("python/python.exe") || p.ends_with("python\\python.exe"));
        } else {
            assert!(p.ends_with("python/bin/python3"));
        }
    }

    // ----- pick_single_asset sort-order tests -----

    fn fake_asset(name: &str) -> Asset {
        Asset {
            name: name.to_owned(),
            size: 0,
            browser_download_url: String::new(),
        }
    }

    fn fake_release(tag: &str, assets: Vec<Asset>) -> Release {
        Release {
            tag_name: tag.to_owned(),
            name: Some(tag.to_owned()),
            draft: false,
            prerelease: false,
            published_at: None,
            assets,
        }
    }

    #[test]
    fn pick_single_asset_error_sorts_candidates_within_a_release_by_name() {
        // Deliberately shuffled input; the error message should list names
        // in ascending order regardless.
        let r = fake_release(
            "20260414",
            vec![
                fake_asset("z-last.tar.gz"),
                fake_asset("a-first.tar.gz"),
                fake_asset("m-middle.tar.gz"),
            ],
        );
        let groups: Vec<(&Release, Vec<&Asset>)> = vec![(&r, r.assets.iter().collect())];
        let err = pick_single_asset(&groups).unwrap_err();
        let msg = err.to_string();
        let i_a = msg.find("a-first.tar.gz").expect("a-first present");
        let i_m = msg.find("m-middle.tar.gz").expect("m-middle present");
        let i_z = msg.find("z-last.tar.gz").expect("z-last present");
        assert!(i_a < i_m && i_m < i_z, "got: {msg}");
    }

    #[test]
    fn pick_single_asset_error_preserves_release_order_but_sorts_within_each() {
        // Two releases, each with shuffled assets. The outer order is
        // preserved (r1 before r2), but within each release assets appear in
        // ascending name order.
        let r1 = fake_release(
            "20260414",
            vec![fake_asset("beta.tar.gz"), fake_asset("alpha.tar.gz")],
        );
        let r2 = fake_release(
            "20260101",
            vec![fake_asset("delta.tar.gz"), fake_asset("charlie.tar.gz")],
        );
        let groups: Vec<(&Release, Vec<&Asset>)> = vec![
            (&r1, r1.assets.iter().collect()),
            (&r2, r2.assets.iter().collect()),
        ];
        let err = pick_single_asset(&groups).unwrap_err();
        let msg = err.to_string();

        // Within r1: alpha before beta.
        let i_alpha = msg.find("alpha.tar.gz").expect("alpha present");
        let i_beta = msg.find("beta.tar.gz").expect("beta present");
        assert!(i_alpha < i_beta, "got: {msg}");

        // Within r2: charlie before delta.
        let i_charlie = msg.find("charlie.tar.gz").expect("charlie present");
        let i_delta = msg.find("delta.tar.gz").expect("delta present");
        assert!(i_charlie < i_delta, "got: {msg}");

        // Outer order: r1's last asset (beta) appears before r2's first
        // asset (charlie), confirming release order is unchanged.
        assert!(i_beta < i_charlie, "got: {msg}");
    }
}
