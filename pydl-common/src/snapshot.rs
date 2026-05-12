//! Persistent local snapshots of upstream releases data, written by
//! `pydl update` and read by `pydl available` / `pydl self-update`.
//!
//! Decouples day-to-day commands from `api.github.com` availability and rate
//! limiting: instead of every invocation paginating the releases endpoint
//! through the HTTP cache, all that traffic is consolidated into a single
//! `pydl update` call that writes a snapshot under `~/.pydl/snapshot/`. Other
//! commands read the snapshot synchronously and never touch the network for
//! release listings.
//!
//! Trust model is unchanged: the snapshot is a UX optimization, not a new
//! trust root. SHA-256 verification still flows through the embedded checksum
//! set on `install` and the `SHA256SUMS` manifest on `self-update`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use log::warn;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::filter::Release;
use crate::pydl_root;

/// Schema version baked into every snapshot envelope. Bump on any
/// non-backwards-compatible payload change. Readers treat an unrecognized
/// version the same as a missing file: `Ok(None)` and let the caller surface
/// the "run `pydl update`" hint.
const SCHEMA_VERSION: u32 = 1;

/// A snapshot is considered stale (and worth nudging the user to refresh)
/// once it's older than this. Tracks roughly the upstream release cadence.
pub const STALE_THRESHOLD_SECS: u64 = 7 * 24 * 60 * 60;

/// Wraps a snapshot payload with the metadata callers need to reason about
/// freshness without reaching for filesystem `mtime`s (which can be perturbed
/// by tools like `cp -p` or backup restores).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Envelope<T> {
    pub version: u32,
    pub fetched_at: u64,
    pub payload: T,
}

/// One asset attached to a `rcook/pydl` release.
///
/// Mirrors the small set of fields `self-update` needs from the GitHub API,
/// kept separate from the Python-releases-side `Asset` so the two payloads can evolve
/// independently.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PydlReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
}

/// A `rcook/pydl` release, as consumed by `self-update`. Drafts are filtered
/// out at write time so consumers don't have to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PydlRelease {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<PydlReleaseAsset>,
}

/// `~/.pydl/snapshot/`. Created on demand by [`write_envelope`].
pub fn snapshot_dir() -> Result<PathBuf> {
    Ok(pydl_root()?.join("snapshot"))
}

pub fn pbs_releases_path() -> Result<PathBuf> {
    Ok(snapshot_dir()?.join("pbs-releases.json"))
}

pub fn pydl_latest_path() -> Result<PathBuf> {
    Ok(snapshot_dir()?.join("pydl-latest.json"))
}

pub fn write_pbs_releases(releases: &[Release]) -> Result<()> {
    write_envelope(&pbs_releases_path()?, releases)
}

pub fn read_pbs_releases() -> Result<Option<Envelope<Vec<Release>>>> {
    read_envelope(&pbs_releases_path()?)
}

pub fn write_pydl_latest(release: &PydlRelease) -> Result<()> {
    write_envelope(&pydl_latest_path()?, release)
}

pub fn read_pydl_latest() -> Result<Option<Envelope<PydlRelease>>> {
    read_envelope(&pydl_latest_path()?)
}

/// Atomically write `payload` to `path` wrapped in a versioned envelope.
/// Atomicity follows the same `tmp + rename` shape as
/// `pydl_cache::CachingClient::write_meta`: a partial write never reaches the
/// canonical filename.
fn write_envelope<T: Serialize + ?Sized>(path: &Path, payload: &T) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("snapshot path {} has no parent", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating snapshot dir {}", parent.display()))?;
    let envelope = Envelope {
        version: SCHEMA_VERSION,
        fetched_at: unix_now(),
        payload,
    };
    let bytes =
        serde_json::to_vec_pretty(&envelope).context("serializing snapshot envelope to JSON")?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Read an envelope. Distinguishes three cases:
///
/// - File absent → `Ok(None)`.
/// - File present, schema version unknown to this binary → `Ok(None)` plus a
///   `warn!` so log-level diagnostics still surface the mismatch.
/// - File present, parse error → `Err`. A corrupt snapshot is not the same as
///   "no snapshot"; the caller deserves a real error rather than a silent
///   fallback to the "run `pydl update`" hint.
fn read_envelope<T: DeserializeOwned>(path: &Path) -> Result<Option<Envelope<T>>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("reading snapshot {}", path.display()));
        }
    };
    let envelope: Envelope<T> = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing snapshot {}", path.display()))?;
    if envelope.version != SCHEMA_VERSION {
        warn!(
            "ignoring snapshot {} (schema version {} != expected {SCHEMA_VERSION})",
            path.display(),
            envelope.version
        );
        return Ok(None);
    }
    Ok(Some(envelope))
}

/// Format a duration in seconds into a short human-readable phrase. The
/// buckets match what users intuitively reach for when scanning a CLI line.
#[must_use]
pub fn humanize_age(now: u64, fetched_at: u64) -> String {
    let secs = now.saturating_sub(fetched_at);
    if secs < 60 {
        return "just now".to_owned();
    }
    if secs < 60 * 60 {
        let mins = secs / 60;
        return if mins == 1 {
            "1 minute ago".to_owned()
        } else {
            format!("{mins} minutes ago")
        };
    }
    if secs < 24 * 60 * 60 {
        let hours = secs / (60 * 60);
        return if hours == 1 {
            "1 hour ago".to_owned()
        } else {
            format!("{hours} hours ago")
        };
    }
    let days = secs / (24 * 60 * 60);
    if days == 1 {
        "1 day ago".to_owned()
    } else {
        format!("{days} days ago")
    }
}

/// One- or two-line human report on a snapshot's age, used identically by
/// both `pydl available` and `pydl self-update` so the wording matches across
/// commands.
///
/// - Always emits a `snapshot from <relative-age>` line.
/// - Adds a `run `pydl update` to refresh` line if the snapshot is older than
///   [`STALE_THRESHOLD_SECS`].
#[must_use]
pub fn staleness_report(fetched_at: u64) -> String {
    let now = unix_now();
    let age = humanize_age(now, fetched_at);
    let stale = now.saturating_sub(fetched_at) > STALE_THRESHOLD_SECS;
    if stale {
        format!("snapshot from {age} — run `pydl update` to refresh")
    } else {
        format!("snapshot from {age}")
    }
}

/// One-line summary of the pydl version snapshot. Used by both
/// `pydl available` and the trailer of `pydl update` so the wording is
/// identical across producer and consumer.
///
/// Three shapes:
/// - `running == latest`: `pydl: latest v0.1.7 (you are up to date)`
/// - `running <  latest`: `pydl: latest v0.1.7 (running v0.1.6 — run `pydl self-update`)`
/// - `running >  latest`: `pydl: latest v0.1.7 (running v0.1.8, ahead of latest)`
#[must_use]
pub fn format_pydl_version_line(running: &semver::Version, latest: &semver::Version) -> String {
    use std::cmp::Ordering;
    match running.cmp(latest) {
        Ordering::Equal => format!("pydl: latest v{latest} (you are up to date)"),
        Ordering::Less => {
            format!("pydl: latest v{latest} (running v{running} — run `pydl self-update`)")
        }
        Ordering::Greater => {
            format!("pydl: latest v{latest} (running v{running}, ahead of latest)")
        }
    }
}

/// One-line short summary of the Python releases snapshot.
///
/// Used by `pydl available` (default mode, no filters) and by the trailer
/// of `pydl update`. The relative-age suffix uses `humanize_age` against
/// the first release's `published_at`; if that field is missing or
/// unparseable the suffix is omitted gracefully.
///
/// Shapes:
/// - With a parseable `published_at`: `Python releases: 113 (latest tag 20260512, 2 days ago upstream)`
/// - Without:                          `Python releases: 113 (latest tag 20260512)`
/// - Empty list:                       `Python releases: 0`
#[must_use]
pub fn format_python_releases_short_summary(releases: &[Release]) -> String {
    let total = releases.len();
    let Some(latest) = releases.first() else {
        return format!("Python releases: {total}");
    };
    let tag = &latest.tag_name;
    let now = unix_now();
    if let Some(published_at) = latest.published_at.as_deref()
        && let Some(published_secs) = parse_iso8601_z(published_at)
    {
        let age = humanize_age(now, published_secs);
        format!("Python releases: {total} (latest tag {tag}, {age} upstream)")
    } else {
        format!("Python releases: {total} (latest tag {tag})")
    }
}

/// Parse the narrow RFC 3339 shape GitHub returns for `published_at`,
/// e.g. `2026-05-12T00:00:00Z`. Returns the corresponding unix seconds, or
/// `None` if the input doesn't match the expected shape. Deliberately
/// minimal — we don't depend on `chrono` and only need the GitHub-shaped
/// subset.
fn parse_iso8601_z(s: &str) -> Option<u64> {
    // Expect exactly: YYYY-MM-DDTHH:MM:SSZ (20 chars).
    let bytes = s.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    let minute: u32 = s.get(14..16)?.parse().ok()?;
    let second: u32 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Days from 1970-01-01 to the given (year, month, day), via Howard
    // Hinnant's date algorithm (civil_from_days, inverted). Handles all the
    // calendar quirks without bringing in chrono.
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    // `y - era * 400` is in `[0, 400)` by construction of `div_euclid`, so
    // the i64→u64 cast is lossless.
    let yoe = (y - era * 400).cast_unsigned();
    let m = u64::from(month);
    let d = u64::from(day);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    // `doe` fits in 32 bits (146,097 max); the cast is lossless.
    let days_since_epoch = era * 146_097 + doe.cast_signed() - 719_468;
    if days_since_epoch < 0 {
        return None;
    }
    // Guarded above; cast is lossless.
    let secs = days_since_epoch.cast_unsigned() * 86_400
        + u64::from(hour) * 3_600
        + u64::from(minute) * 60
        + u64::from(second);
    Some(secs)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn fake_release(tag: &str) -> Release {
        Release {
            tag_name: tag.to_owned(),
            name: Some(tag.to_owned()),
            draft: false,
            prerelease: false,
            published_at: Some("2026-05-01T00:00:00Z".to_owned()),
            assets: vec![],
        }
    }

    #[test]
    fn roundtrip_pbs_releases() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("pbs.json");
        let releases = vec![fake_release("20260512"), fake_release("20260505")];
        write_envelope(&path, releases.as_slice()).unwrap();
        let env: Envelope<Vec<Release>> = read_envelope(&path).unwrap().expect("envelope present");
        assert_eq!(env.version, SCHEMA_VERSION);
        assert_eq!(env.payload, releases);
    }

    #[test]
    fn roundtrip_pydl_latest() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("pydl.json");
        let release = PydlRelease {
            tag_name: "v0.2.0".to_owned(),
            assets: vec![PydlReleaseAsset {
                name: "pydl-v0.2.0-x86_64-unknown-linux-musl.tar.gz".to_owned(),
                browser_download_url: "https://example.test/asset".to_owned(),
            }],
        };
        write_envelope(&path, &release).unwrap();
        let env: Envelope<PydlRelease> = read_envelope(&path).unwrap().expect("envelope present");
        assert_eq!(env.payload, release);
    }

    #[test]
    fn read_returns_none_when_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing.json");
        let res: Option<Envelope<Vec<Release>>> = read_envelope(&path).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn read_returns_none_on_schema_version_mismatch() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("future.json");
        let payload = r#"{"version":999,"fetched_at":0,"payload":[]}"#;
        fs::write(&path, payload).unwrap();
        let res: Option<Envelope<Vec<Release>>> = read_envelope(&path).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn read_errors_on_corrupt_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("corrupt.json");
        fs::write(&path, b"not json at all").unwrap();
        let res: Result<Option<Envelope<Vec<Release>>>> = read_envelope(&path);
        assert!(res.is_err());
    }

    #[test]
    fn atomic_write_does_not_leave_tmp_on_success() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("snap.json");
        write_envelope(&path, [fake_release("20260512")].as_slice()).unwrap();
        let tmp = path.with_extension("json.tmp");
        assert!(path.exists(), "canonical path must exist");
        assert!(!tmp.exists(), "tmp file must be cleaned up by rename");
    }

    #[test]
    fn humanize_age_buckets() {
        assert_eq!(humanize_age(1000, 1000), "just now");
        assert_eq!(humanize_age(1000, 970), "just now"); // < 1 min
        assert_eq!(humanize_age(1060, 1000), "1 minute ago");
        assert_eq!(humanize_age(1180, 1000), "3 minutes ago");
        assert_eq!(humanize_age(4600, 1000), "1 hour ago");
        assert_eq!(humanize_age(11800, 1000), "3 hours ago");
        assert_eq!(humanize_age(87400, 1000), "1 day ago");
        assert_eq!(humanize_age(700_000, 1000), "8 days ago");
    }

    #[test]
    fn staleness_report_includes_refresh_hint_when_stale() {
        // 14 days ago.
        let stale_at = unix_now().saturating_sub(14 * 24 * 60 * 60);
        let s = staleness_report(stale_at);
        assert!(s.contains("pydl update"), "got: {s}");
    }

    #[test]
    fn staleness_report_omits_refresh_hint_when_fresh() {
        let fresh_at = unix_now().saturating_sub(60); // 1 minute ago
        let s = staleness_report(fresh_at);
        assert!(!s.contains("pydl update"), "got: {s}");
        assert!(s.contains("snapshot from"), "got: {s}");
    }

    fn v(s: &str) -> semver::Version {
        semver::Version::parse(s).unwrap()
    }

    #[test]
    fn format_pydl_version_line_up_to_date() {
        let s = format_pydl_version_line(&v("0.1.7"), &v("0.1.7"));
        assert_eq!(s, "pydl: latest v0.1.7 (you are up to date)");
    }

    #[test]
    fn format_pydl_version_line_newer_available() {
        let s = format_pydl_version_line(&v("0.1.6"), &v("0.1.7"));
        assert_eq!(
            s,
            "pydl: latest v0.1.7 (running v0.1.6 — run `pydl self-update`)"
        );
    }

    #[test]
    fn format_pydl_version_line_running_newer() {
        let s = format_pydl_version_line(&v("0.1.8"), &v("0.1.7"));
        assert_eq!(s, "pydl: latest v0.1.7 (running v0.1.8, ahead of latest)");
    }

    fn release_with_published(tag: &str, published_at: Option<&str>) -> Release {
        Release {
            tag_name: tag.to_owned(),
            name: Some(tag.to_owned()),
            draft: false,
            prerelease: false,
            published_at: published_at.map(ToOwned::to_owned),
            assets: vec![],
        }
    }

    #[test]
    fn format_python_releases_short_summary_basic() {
        // Use a date close to "now" so the relative-age suffix is stable in
        // shape regardless of when the test is run.
        let now = unix_now();
        let one_day_ago = now.saturating_sub(24 * 60 * 60);
        let day_iso = unix_to_iso8601_z(one_day_ago);
        let releases = vec![
            release_with_published("20260512", Some(&day_iso)),
            release_with_published("20260505", Some(&day_iso)),
        ];
        let s = format_python_releases_short_summary(&releases);
        assert!(
            s.starts_with("Python releases: 2 (latest tag 20260512,"),
            "got: {s}"
        );
        assert!(s.ends_with("upstream)"), "got: {s}");
    }

    #[test]
    fn format_python_releases_short_summary_handles_unparseable_date() {
        let releases = vec![release_with_published("20260512", Some("not a date"))];
        let s = format_python_releases_short_summary(&releases);
        assert_eq!(s, "Python releases: 1 (latest tag 20260512)");
    }

    #[test]
    fn format_python_releases_short_summary_no_published_at() {
        let releases = vec![release_with_published("20260512", None)];
        let s = format_python_releases_short_summary(&releases);
        assert_eq!(s, "Python releases: 1 (latest tag 20260512)");
    }

    #[test]
    fn format_python_releases_short_summary_empty() {
        let s = format_python_releases_short_summary(&[]);
        assert_eq!(s, "Python releases: 0");
    }

    #[test]
    fn parse_iso8601_z_known_anchor() {
        // 2026-05-12T00:00:00Z should be exactly (2026-1970)*365.25 days,
        // adjusted for actual leap years. Cross-check by round-tripping
        // through the inverse helper used in this test suite.
        let s = "2026-05-12T00:00:00Z";
        let secs = parse_iso8601_z(s).expect("parses");
        assert_eq!(unix_to_iso8601_z(secs), s);
    }

    #[test]
    fn parse_iso8601_z_rejects_bad_shape() {
        assert!(parse_iso8601_z("not a date").is_none());
        assert!(parse_iso8601_z("2026/05/12T00:00:00Z").is_none());
        assert!(parse_iso8601_z("2026-05-12T00:00:00").is_none()); // no Z
        assert!(parse_iso8601_z("2026-13-12T00:00:00Z").is_none()); // bad month
    }

    /// Inverse of `parse_iso8601_z` for test fixtures only. Same Howard
    /// Hinnant date algorithm, run forward.
    fn unix_to_iso8601_z(secs: u64) -> String {
        // For any plausible test input this fits in i64 trivially.
        let days = (secs / 86_400).cast_signed();
        let time_of_day = secs % 86_400;
        let hour = time_of_day / 3_600;
        let minute = (time_of_day % 3_600) / 60;
        let second = time_of_day % 60;
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = (z - era * 146_097).cast_unsigned();
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe.cast_signed() + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if m <= 2 { y + 1 } else { y };
        format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
    }
}
