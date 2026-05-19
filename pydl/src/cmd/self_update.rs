//! `pydl self-update`: replace the running binary with the latest released
//! `pydl` version.
//!
//! By default this command is **offline for the version check**: it reads
//! the latest version from the local snapshot written by `pydl update`. The
//! actual binary download still happens over the network — there's no way
//! around that. Pass `--online` to bypass the snapshot and hit
//! `api.github.com` directly, matching the pre-refactor behaviour.
//!
//! Trust model: HTTPS to GitHub plus a `SHA256SUMS` manifest published in the
//! same release. The downloaded archive is hashed and compared against the
//! manifest entry before extraction; a hash mismatch never replaces the
//! binary. A missing manifest is a hard error: `pydl self-update` refuses
//! to self-replace from a release that doesn't publish `SHA256SUMS`. Pass
//! `--allow-missing-checksum` to opt out for the rare case of updating
//! through a pre-manifest release (only relevant for binaries built before
//! v0.1.5).

use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use futures_util::StreamExt;
use log::{debug, warn};
use pydl_cache::{CacheOutcome, CachingClient, Method, StatusCode};
use pydl_common::snapshot::{self, PydlRelease, PydlReleaseAsset};
use pydl_common::{cache_dir, checksums, min_freshness_secs};
use semver::Version;
use serde::Deserialize;

use crate::progress::{self, ProgressMode};

const SELF_OWNER: &str = "rcook";
const SELF_REPO: &str = "pydl";
/// How many recent releases to scan when `--pre` is set. The first page is
/// almost always enough — pydl ships infrequently.
const PRE_PAGE_SIZE: usize = 10;
/// Defensive lower bound on the extracted binary size. The smallest plausible
/// `pydl` build is well over 1 MiB; anything smaller suggests a packaging
/// regression and should not silently overwrite the user's binary.
const MIN_BINARY_BYTES: u64 = 1024 * 1024;

#[derive(Parser, Debug)]
// Each of these flags is independent and toggled by its own `--flag`; the
// "model state as an enum" suggestion from `struct_excessive_bools` doesn't
// apply.
#[allow(clippy::struct_excessive_bools)]
pub struct Args {
    /// Allow updating to the newest pre-release. By default only stable
    /// releases (GitHub's `releases/latest`) are considered.
    #[arg(long)]
    pub pre: bool,

    /// Re-download and re-install even if the running binary is already on
    /// the latest version. Useful for repairing a corrupted install.
    #[arg(long)]
    pub force: bool,

    /// Print what would be done without replacing the binary.
    #[arg(long)]
    pub dry_run: bool,

    /// Allow self-updating to a release that doesn't publish a `SHA256SUMS`
    /// manifest. By default `pydl self-update` refuses to self-replace
    /// without verification. Pre-manifest releases (v0.1.4 and earlier) do
    /// not publish `SHA256SUMS`; this flag lets you update through one of
    /// them at your own risk.
    #[arg(long)]
    pub allow_missing_checksum: bool,

    /// Bypass the snapshot written by `pydl update` and check
    /// `api.github.com` directly for the latest version. Required when
    /// combined with `--pre` (the snapshot only carries the latest stable).
    #[arg(long)]
    pub online: bool,
}

// `Release` and `ReleaseAsset` are now type aliases over the
// snapshot-defined shapes. `--online` paths still deserialize straight from
// the GitHub API, but they go through `ApiRelease` (a private wire-shape
// helper that strips drafts and lifts into `PydlRelease`).
type Release = PydlRelease;
type ReleaseAsset = PydlReleaseAsset;

#[derive(Deserialize, Debug)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<ApiReleaseAsset>,
}

#[derive(Deserialize, Debug)]
struct ApiReleaseAsset {
    name: String,
    browser_download_url: String,
}

impl ApiRelease {
    fn into_release(self) -> Release {
        Release {
            tag_name: self.tag_name,
            assets: self
                .assets
                .into_iter()
                .map(|a| ReleaseAsset {
                    name: a.name,
                    browser_download_url: a.browser_download_url,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ArchiveKind {
    TarGz,
    Zip,
}

impl ArchiveKind {
    const fn binary_name(self) -> &'static str {
        match self {
            Self::TarGz => "pydl",
            Self::Zip => "pydl.exe",
        }
    }
}

pub async fn run(args: Args, progress_mode: ProgressMode) -> Result<()> {
    let current_str = env!("CARGO_PKG_VERSION");
    let current = Version::parse(current_str)
        .with_context(|| format!("parsing CARGO_PKG_VERSION {current_str:?}"))?;
    let target = env!("PYDL_BUILD_TARGET");

    if args.pre && !args.online {
        bail!(
            "--pre requires --online: the snapshot from `pydl update` only \
             carries the latest stable release. Pass --online to consider \
             pre-releases."
        );
    }

    let user_agent = format!("{} (self-update)", crate::USER_AGENT);
    let client = CachingClient::with_user_agent(cache_dir()?, Some(user_agent.as_str()))?
        .with_min_freshness_secs(min_freshness_secs()?);

    let (release, latest) = resolve_target_release(&args, &client, &current).await?;

    if !args.force {
        if latest == current {
            println!(
                "pydl {current} is already the latest{}",
                if args.pre {
                    " (including pre-releases)"
                } else {
                    ""
                }
            );
            return Ok(());
        }
        if latest < current {
            println!(
                "running pydl {current} is newer than the latest published release ({latest}); use --force to downgrade"
            );
            return Ok(());
        }
    }

    let kind = if target.contains("windows") {
        ArchiveKind::Zip
    } else {
        ArchiveKind::TarGz
    };
    let asset = pick_asset(&release, target, kind)?;
    let url = &asset.browser_download_url;

    if args.dry_run {
        println!("dry run: would download {} from {url}", asset.name);
        return Ok(());
    }

    let staging = tempfile::Builder::new()
        .prefix("pydl-self-update.")
        .tempdir()
        .context("creating staging tempdir")?;
    let archive_path = staging.path().join(&asset.name);
    download_archive(&client, url, &asset.name, &archive_path, progress_mode).await?;
    verify_release_checksum(
        &client,
        &release,
        &asset.name,
        &archive_path,
        args.allow_missing_checksum,
    )
    .await?;
    let new_binary = extract_pydl_binary(&archive_path, kind, staging.path())?;

    let size = fs::metadata(&new_binary)
        .with_context(|| format!("stat {}", new_binary.display()))?
        .len();
    if size < MIN_BINARY_BYTES {
        bail!(
            "extracted binary at {} is {size} bytes — refusing to self-replace with a suspiciously small file",
            new_binary.display()
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&new_binary, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("chmod 0755 on {}", new_binary.display()))?;
    }

    self_replace::self_replace(&new_binary)
        .with_context(|| format!("self-replacing with {}", new_binary.display()))?;

    drop(staging);

    println!("pydl updated: {current} -> {latest}");
    Ok(())
}

async fn resolve_target_release(
    args: &Args,
    client: &CachingClient,
    current: &Version,
) -> Result<(Release, Version)> {
    let release = if args.online {
        if args.pre {
            fetch_latest_including_pre(client).await?
        } else {
            fetch_latest_stable(client).await?
        }
    } else {
        let envelope = snapshot::read_pydl_latest()?.ok_or_else(|| {
            let p = snapshot::pydl_latest_path().map_or_else(
                |_| "<snapshot path unavailable>".to_owned(),
                |p| p.display().to_string(),
            );
            anyhow!(
                "no pydl version snapshot found at {p}. Run `pydl update`, or \
                 pass --online to check upstream directly."
            )
        })?;
        println!("{}", snapshot::staleness_report(envelope.fetched_at));
        envelope.payload
    };

    let latest_str = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    let latest = Version::parse(latest_str)
        .with_context(|| format!("parsing release tag {:?} as semver", release.tag_name))?;
    debug!(
        "latest release on GitHub: {} (running {current})",
        release.tag_name
    );

    Ok((release, latest))
}

async fn fetch_latest_stable(client: &CachingClient) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{SELF_OWNER}/{SELF_REPO}/releases/latest");
    let (status, body, _) = client.request(Method::GET, &url).await?;
    if status != StatusCode::OK {
        bail!(
            "GET {url} returned {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    let api: ApiRelease = serde_json::from_slice(&body).map_err(|e| {
        anyhow!(
            "parsing latest release JSON: {e} (body: {})",
            String::from_utf8_lossy(&body)
        )
    })?;
    Ok(api.into_release())
}

async fn fetch_latest_including_pre(client: &CachingClient) -> Result<Release> {
    let url = format!(
        "https://api.github.com/repos/{SELF_OWNER}/{SELF_REPO}/releases?per_page={PRE_PAGE_SIZE}"
    );
    let (status, body, _) = client.request(Method::GET, &url).await?;
    if status != StatusCode::OK {
        bail!(
            "GET {url} returned {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    let releases: Vec<ApiRelease> =
        serde_json::from_slice(&body).map_err(|e| anyhow!("parsing releases JSON: {e}"))?;

    let mut best: Option<(Version, ApiRelease)> = None;
    for r in releases {
        if r.draft {
            continue;
        }
        let tag = r.tag_name.strip_prefix('v').unwrap_or(&r.tag_name);
        let Ok(v) = Version::parse(tag) else {
            debug!("skipping release with non-semver tag: {}", r.tag_name);
            continue;
        };
        match &best {
            None => best = Some((v, r)),
            Some((bv, _)) if v > *bv => best = Some((v, r)),
            _ => {}
        }
    }
    best.map(|(_, r)| r.into_release())
        .context("no usable releases found on GitHub (after filtering drafts and unparseable tags)")
}

fn pick_asset<'a>(
    release: &'a Release,
    target: &str,
    kind: ArchiveKind,
) -> Result<&'a ReleaseAsset> {
    // Asset naming from .github/workflows/release.yaml: the `version` is
    // `${GITHUB_REF_NAME}` which keeps the `v` prefix, so the on-disc name is
    // `pydl-v0.1.1-<target>.<ext>`. If the tagging policy ever changes, this
    // command will silently 404.
    let ext = match kind {
        ArchiveKind::TarGz => "tar.gz",
        ArchiveKind::Zip => "zip",
    };
    let want = format!("pydl-{}-{target}.{ext}", release.tag_name);
    if let Some(a) = release.assets.iter().find(|a| a.name == want) {
        return Ok(a);
    }
    let mut msg = format!(
        "no asset named {want} in release {}. Candidates:\n",
        release.tag_name
    );
    for a in &release.assets {
        msg.push_str("  ");
        msg.push_str(&a.name);
        msg.push('\n');
    }
    bail!("{msg}");
}

/// Stream the asset at `url` directly into `dest`, returning the byte count.
///
/// Earlier revisions warmed `pydl-cache` and then queried `cached_body_path`
/// to find where the body landed. That broke when upstream sent
/// `Cache-Control: no-store`, because the cache then refuses to write a body
/// and `cached_body_path` returns `None`. Writing to a caller-owned tmp file
/// avoids that entirely; the cache will still see the request and may
/// populate its meta, but we don't depend on that here.
async fn download_archive(
    client: &CachingClient,
    url: &str,
    asset_name: &str,
    dest: &Path,
    progress_mode: ProgressMode,
) -> Result<u64> {
    use tokio::io::AsyncWriteExt;

    let (status, outcome, content_length, mut stream) = client.get_stream(url).await?;
    if status != StatusCode::OK {
        bail!("GET {url} returned {status}");
    }

    let pb = if outcome == CacheOutcome::Downloaded {
        progress::download_bar(progress_mode, content_length)
    } else {
        indicatif::ProgressBar::hidden()
    };

    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("creating {}", dest.display()))?;
    let mut total: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading chunk from upstream")?;
        file.write_all(&chunk)
            .await
            .with_context(|| format!("writing {}", dest.display()))?;
        total += chunk.len() as u64;
        pb.inc(chunk.len() as u64);
    }
    file.flush()
        .await
        .with_context(|| format!("flushing {}", dest.display()))?;
    pb.finish_and_clear();
    debug!(
        "downloaded {asset_name} ({total} bytes) -> {}",
        dest.display()
    );
    Ok(total)
}

const CHECKSUM_ASSET_NAME: &str = "SHA256SUMS";

/// The `SHA256SUMS` asset attached to `release`, if present. Returns `None`
/// for releases that predate manifest publishing.
fn pick_checksum_asset(release: &Release) -> Option<&ReleaseAsset> {
    release
        .assets
        .iter()
        .find(|a| a.name == CHECKSUM_ASSET_NAME)
}

/// Fetch the manifest body into `dest`, returning the parsed map of
/// `filename → expected hex hash`.
async fn download_checksums(
    client: &CachingClient,
    asset: &ReleaseAsset,
    dest: &Path,
) -> Result<std::collections::HashMap<String, String>> {
    download_archive(
        client,
        &asset.browser_download_url,
        &asset.name,
        dest,
        ProgressMode::Never,
    )
    .await?;
    let body = tokio::fs::read_to_string(dest)
        .await
        .with_context(|| format!("reading {}", dest.display()))?;
    Ok(checksums::parse_sha256sums_owned(&body))
}

/// Look up `asset_name` in `manifest` and check it against the on-disc bytes
/// at `archive_path`. Errors on mismatch or when the manifest doesn't list
/// our asset (a present-but-incomplete manifest is treated as an attack
/// signal, not a soft warning).
fn verify_checksum(
    manifest: &std::collections::HashMap<String, String>,
    asset_name: &str,
    archive_path: &Path,
) -> Result<()> {
    let expected = manifest.get(asset_name).with_context(|| {
        format!("SHA256SUMS does not list {asset_name:?} — refusing to self-replace")
    })?;
    let actual = checksums::sha256_file(archive_path)?;
    if !checksums::hashes_match(expected, &actual) {
        bail!(
            "sha256 mismatch for {asset_name}: expected {expected}, got {actual} — refusing to self-replace"
        );
    }
    debug!("sha256 verified for {asset_name}: {actual}");
    Ok(())
}

/// Wire the manifest fetch + verify into the update flow. Strict by default;
/// `allow_missing=true` (set by `--allow-missing-checksum`) downgrades a
/// missing manifest from a hard error to a warning.
async fn verify_release_checksum(
    client: &CachingClient,
    release: &Release,
    asset_name: &str,
    archive_path: &Path,
    allow_missing: bool,
) -> Result<()> {
    let Some(checksum_asset) = pick_checksum_asset(release) else {
        if !allow_missing {
            bail!(
                "release {tag} does not publish a SHA256SUMS — refusing to self-update without verification (pass --allow-missing-checksum to override)",
                tag = release.tag_name
            );
        }
        warn!(
            "self-update: no SHA256SUMS in release {} and --allow-missing-checksum was passed; proceeding without verification",
            release.tag_name
        );
        return Ok(());
    };

    // Stage the manifest beside the archive so the staging tempdir cleans it
    // up on drop.
    let manifest_path = archive_path.with_file_name(CHECKSUM_ASSET_NAME);
    let manifest = match download_checksums(client, checksum_asset, &manifest_path).await {
        Ok(m) => m,
        Err(e) => {
            if !allow_missing {
                return Err(e).context("fetching SHA256SUMS");
            }
            warn!(
                "self-update: failed to fetch SHA256SUMS for release {} ({e:#}); --allow-missing-checksum was passed, proceeding without verification",
                release.tag_name
            );
            return Ok(());
        }
    };

    verify_checksum(&manifest, asset_name, archive_path)
}

fn extract_pydl_binary(archive_path: &Path, kind: ArchiveKind, dest_dir: &Path) -> Result<PathBuf> {
    let binary_name = kind.binary_name();
    match kind {
        ArchiveKind::TarGz => extract_from_tar_gz(archive_path, dest_dir, binary_name),
        ArchiveKind::Zip => extract_from_zip(archive_path, dest_dir, binary_name),
    }
}

fn extract_from_tar_gz(archive_path: &Path, dest_dir: &Path, binary_name: &str) -> Result<PathBuf> {
    let file =
        File::open(archive_path).with_context(|| format!("opening {}", archive_path.display()))?;
    let gz = flate2::read::GzDecoder::new(BufReader::new(file));
    let mut archive = tar::Archive::new(gz);
    let mut found: Option<PathBuf> = None;
    for entry in archive
        .entries()
        .with_context(|| format!("reading entries from {}", archive_path.display()))?
    {
        let mut entry = entry.context("reading tar entry")?;
        let entry_path = entry
            .path()
            .context("decoding tar entry path")?
            .into_owned();
        let basename = match entry_path.file_name() {
            Some(n) => n.to_owned(),
            None => continue,
        };
        if basename == binary_name {
            let dest = dest_dir.join(binary_name);
            let mut out =
                File::create(&dest).with_context(|| format!("creating {}", dest.display()))?;
            io::copy(&mut entry, &mut out)
                .with_context(|| format!("writing {}", dest.display()))?;
            found = Some(dest);
            break;
        }
    }
    found.with_context(|| {
        format!(
            "no `{binary_name}` binary found in {}",
            archive_path.display()
        )
    })
}

fn extract_from_zip(archive_path: &Path, dest_dir: &Path, binary_name: &str) -> Result<PathBuf> {
    let file =
        File::open(archive_path).with_context(|| format!("opening {}", archive_path.display()))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file))
        .with_context(|| format!("opening zip {}", archive_path.display()))?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).context("reading zip entry")?;
        let name = entry.name().to_owned();
        let basename = Path::new(&name)
            .file_name()
            .map(std::ffi::OsStr::to_owned)
            .unwrap_or_default();
        if basename == binary_name {
            let dest = dest_dir.join(binary_name);
            let mut out =
                File::create(&dest).with_context(|| format!("creating {}", dest.display()))?;
            io::copy(&mut entry, &mut out)
                .with_context(|| format!("writing {}", dest.display()))?;
            return Ok(dest);
        }
    }
    bail!(
        "no `{binary_name}` binary found in {}",
        archive_path.display()
    );
}

#[cfg(test)]
mod tests {
    use pydl_cache::CachingClient;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// Regression for the no-store branch: with the old "warm cache, then ask
    /// for the body path" approach, an upstream `Cache-Control: no-store`
    /// caused `pydl-cache` to skip writing a body, and `cached_body_path`
    /// returned `None`, and the helper bailed with "this is a bug". The
    /// rewrite writes directly to a caller-owned path, so `no-store` is
    /// irrelevant.
    #[tokio::test]
    async fn download_archive_handles_no_store() {
        let cache_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/pydl.tar.gz"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "no-store")
                    .set_body_bytes(b"hello pydl".as_slice()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = CachingClient::new(cache_dir.path()).unwrap();
        let url = format!("{}/pydl.tar.gz", server.uri());
        let dest = dest_dir.path().join("pydl.tar.gz");

        let total = download_archive(&client, &url, "pydl.tar.gz", &dest, ProgressMode::Never)
            .await
            .expect(
                "download_archive must succeed even when upstream sets Cache-Control: no-store",
            );

        assert_eq!(total, 10);
        let bytes = std::fs::read(&dest).unwrap();
        assert_eq!(bytes, b"hello pydl");
    }

    /// Builds a manifest body in canonical `sha256sum -b` format.
    fn manifest_body(entries: &[(&str, &str)]) -> String {
        let mut s = String::new();
        for (hash, name) in entries {
            s.push_str(hash);
            s.push_str("  ");
            s.push_str(name);
            s.push('\n');
        }
        s
    }

    fn write_archive(dir: &TempDir, name: &str, body: &[u8]) -> std::path::PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn verify_checksum_matches() {
        let dir = TempDir::new().unwrap();
        let archive = write_archive(&dir, "pydl.tar.gz", b"the bytes");
        // Pre-compute the expected hash with the same routine the verifier uses.
        let expected = checksums::sha256_file(&archive).unwrap();
        let manifest =
            checksums::parse_sha256sums_owned(&manifest_body(&[(&expected, "pydl.tar.gz")]));
        verify_checksum(&manifest, "pydl.tar.gz", &archive).expect("matching hash should pass");
    }

    #[test]
    fn verify_checksum_mismatch() {
        let dir = TempDir::new().unwrap();
        let archive = write_archive(&dir, "pydl.tar.gz", b"the bytes");
        // Manifest lists a wrong (but well-formed) hash.
        let bogus = "0".repeat(64);
        let manifest =
            checksums::parse_sha256sums_owned(&manifest_body(&[(&bogus, "pydl.tar.gz")]));
        let err = verify_checksum(&manifest, "pydl.tar.gz", &archive)
            .expect_err("mismatched hash must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("sha256 mismatch"), "got: {msg}");
        assert!(msg.contains("pydl.tar.gz"), "got: {msg}");
    }

    #[test]
    fn verify_checksum_asset_not_in_manifest() {
        let dir = TempDir::new().unwrap();
        let archive = write_archive(&dir, "pydl.tar.gz", b"the bytes");
        // Manifest lists *some* file but not ours.
        let other = "1".repeat(64);
        let manifest = checksums::parse_sha256sums_owned(&manifest_body(&[(&other, "other.zip")]));
        let err = verify_checksum(&manifest, "pydl.tar.gz", &archive)
            .expect_err("missing entry must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("does not list"), "got: {msg}");
        assert!(msg.contains("pydl.tar.gz"), "got: {msg}");
    }

    fn fake_release(asset_names: &[&str]) -> Release {
        Release {
            tag_name: "v0.0.0".to_owned(),
            assets: asset_names
                .iter()
                .map(|n| ReleaseAsset {
                    name: (*n).to_owned(),
                    browser_download_url: format!("https://example.test/{n}"),
                })
                .collect(),
        }
    }

    #[test]
    fn pick_checksum_asset_finds_sha256sums() {
        let release = fake_release(&[
            "pydl-v0.0.0-aarch64-apple-darwin.tar.gz",
            "pydl-v0.0.0-x86_64-pc-windows-msvc.zip",
            "pydl-v0.0.0-x86_64-unknown-linux-musl.tar.gz",
            "SHA256SUMS",
        ]);
        let asset = pick_checksum_asset(&release).expect("manifest must be found");
        assert_eq!(asset.name, "SHA256SUMS");
    }

    #[test]
    fn pick_checksum_asset_returns_none_when_absent() {
        let release = fake_release(&["pydl-v0.0.0-aarch64-apple-darwin.tar.gz"]);
        assert!(pick_checksum_asset(&release).is_none());
    }

    /// Strict default: a release without a `SHA256SUMS` asset must error,
    /// and the error must name the new opt-out flag so the user knows the
    /// path forward.
    #[tokio::test]
    async fn verify_release_checksum_strict_by_default_errors_on_missing_manifest() {
        let dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let archive = write_archive(&dir, "pydl-v0.0.0-x86_64-unknown-linux-musl.tar.gz", b"x");
        let release = fake_release(&["pydl-v0.0.0-x86_64-unknown-linux-musl.tar.gz"]);
        let client = CachingClient::new(cache_dir.path()).unwrap();

        let err = verify_release_checksum(
            &client,
            &release,
            "pydl-v0.0.0-x86_64-unknown-linux-musl.tar.gz",
            &archive,
            /* allow_missing */ false,
        )
        .await
        .expect_err("strict mode must reject a release without SHA256SUMS");
        let msg = format!("{err:#}");
        assert!(msg.contains("does not publish a SHA256SUMS"), "got: {msg}");
        assert!(
            msg.contains("--allow-missing-checksum"),
            "error should name the opt-out flag, got: {msg}"
        );
    }

    /// Opt-in escape hatch: with `--allow-missing-checksum`, the same
    /// missing-manifest case downgrades to a warning and returns Ok.
    #[tokio::test]
    async fn verify_release_checksum_allow_missing_skips_verification() {
        let dir = TempDir::new().unwrap();
        let cache_dir = TempDir::new().unwrap();
        let archive = write_archive(&dir, "pydl-v0.0.0-x86_64-unknown-linux-musl.tar.gz", b"x");
        let release = fake_release(&["pydl-v0.0.0-x86_64-unknown-linux-musl.tar.gz"]);
        let client = CachingClient::new(cache_dir.path()).unwrap();

        verify_release_checksum(
            &client,
            &release,
            "pydl-v0.0.0-x86_64-unknown-linux-musl.tar.gz",
            &archive,
            /* allow_missing */ true,
        )
        .await
        .expect("--allow-missing-checksum must downgrade missing manifest to a warning");
    }

    fn make_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        let buf = Vec::new();
        let enc = GzEncoder::new(buf, Compression::fast());
        let mut ar = tar::Builder::new(enc);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            ar.append_data(&mut header, name, &data[..]).unwrap();
        }
        ar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn extract_from_tar_gz_finds_pydl_binary() {
        let archive_bytes = make_tar_gz(&[
            ("some-dir/README", b"readme"),
            ("some-dir/pydl", b"the pydl binary content"),
        ]);
        let dir = TempDir::new().unwrap();
        let archive_path = dir.path().join("pydl.tar.gz");
        std::fs::write(&archive_path, &archive_bytes).unwrap();

        let result = extract_from_tar_gz(&archive_path, dir.path(), "pydl").unwrap();
        assert_eq!(result, dir.path().join("pydl"));
        assert_eq!(
            std::fs::read_to_string(&result).unwrap(),
            "the pydl binary content"
        );
    }

    #[test]
    fn extract_from_tar_gz_errors_when_no_pydl() {
        let archive_bytes = make_tar_gz(&[("some-dir/other", b"not pydl")]);
        let dir = TempDir::new().unwrap();
        let archive_path = dir.path().join("pydl.tar.gz");
        std::fs::write(&archive_path, &archive_bytes).unwrap();

        let err = extract_from_tar_gz(&archive_path, dir.path(), "pydl")
            .expect_err("missing pydl binary must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("no `pydl` binary"), "got: {msg}");
    }

    #[test]
    fn extract_from_zip_finds_pydl_exe() {
        use std::io::Write;
        let dir = TempDir::new().unwrap();
        let archive_path = dir.path().join("pydl.zip");
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("pydl-v0.0.0/pydl.exe", opts).unwrap();
            zip.write_all(b"the pydl exe content").unwrap();
            zip.finish().unwrap();
        }

        let result = extract_from_zip(&archive_path, dir.path(), "pydl.exe").unwrap();
        assert_eq!(result, dir.path().join("pydl.exe"));
        assert_eq!(
            std::fs::read_to_string(&result).unwrap(),
            "the pydl exe content"
        );
    }

    #[test]
    fn extract_from_zip_errors_when_no_pydl_exe() {
        use std::io::Write;
        let dir = TempDir::new().unwrap();
        let archive_path = dir.path().join("pydl.zip");
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("other.txt", opts).unwrap();
            zip.write_all(b"not pydl").unwrap();
            zip.finish().unwrap();
        }

        let err = extract_from_zip(&archive_path, dir.path(), "pydl.exe")
            .expect_err("missing pydl.exe must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("no `pydl.exe` binary"), "got: {msg}");
    }
}
