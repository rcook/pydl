//! `check-checksums <input-dir>` — verify every `<tag>.sha256sums` file in
//! `<input-dir>` against what upstream currently serves.
//!
//! For each `<tag>.sha256sums` file on disc, GETs
//! `https://github.com/astral-sh/python-build-standalone/releases/download/<tag>/SHA256SUMS`
//! through the cache and asserts the bytes match. A mismatch means the
//! committed checksum set disagrees with what GitHub is currently serving —
//! either the file was edited post-release (suspicious) or the committed
//! version was corrupted at PR time (also suspicious).
//!
//! This is a CI trip-wire, not a user-facing install step. It's the formal
//! control the review-at-commit-time argument implies.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use log::{debug, info, warn};
use pydl_cache::{CachingClient, Method, StatusCode};
use pydl_common::{OWNER, REPO, make_client, min_freshness_secs};
use tokio::fs;

/// How many upstream fetches to have in flight concurrently. GitHub's
/// release-asset CDN is fine with a handful of parallel requests; 8 cuts the
/// wall-clock of a ~80-tag check roughly 8x on a cold cache without being
/// aggressive enough to draw rate-limiting attention.
const CONCURRENCY: usize = 8;

#[derive(Parser, Debug)]
#[command(
    name = "check-checksums",
    version,
    about = "Verify every committed <tag>.sha256sums file matches upstream."
)]
struct Cli {
    /// Directory containing `<tag>.sha256sums` files to check.
    input_dir: PathBuf,

    /// Log filter directive (overrides `RUST_LOG`). Accepts the same syntax
    /// as `RUST_LOG`, e.g. `debug`, `pydl_cache=debug`.
    #[arg(short = 'l', long = "log", value_name = "DIRECTIVE")]
    log: Option<String>,
}

#[derive(Debug)]
enum CheckError {
    /// Non-OK HTTP status; caller may treat this as a soft warning rather
    /// than a hard failure (old tags sometimes return 404 because upstream
    /// never shipped a `SHA256SUMS` for that release).
    Status(StatusCode),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for CheckError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

/// Outcome of a single tag check, paired with the tag for reporting.
enum Outcome {
    Match,
    Mismatch,
    FetchFailed(StatusCode),
}

/// Fetch upstream's `SHA256SUMS` for `tag` and compare against `embedded`.
/// Returns `Ok(true)` on match, `Ok(false)` on mismatch (with a log line
/// naming the tag) and `Err(CheckError::Status)` on a non-200 response.
async fn check_one(
    client: &CachingClient,
    tag: &str,
    embedded: &str,
    url: &str,
) -> Result<bool, CheckError> {
    let (status, body, _) = client.request(Method::GET, url).await?;
    if status != StatusCode::OK {
        return Err(CheckError::Status(status));
    }
    let upstream = std::str::from_utf8(&body).map_err(|e| {
        CheckError::Other(anyhow::anyhow!(
            "upstream body for tag {tag} is not valid UTF-8: {e}"
        ))
    })?;
    if upstream == embedded {
        debug!("{tag}: match");
        Ok(true)
    } else {
        log::error!(
            "{tag}: committed checksums DISAGREE with upstream ({} vs {} bytes)",
            embedded.len(),
            upstream.len(),
        );
        Ok(false)
    }
}

fn extract_tag(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let tag = name.strip_suffix(".sha256sums")?;
    if tag.is_empty() {
        return None;
    }
    Some(tag.to_owned())
}

async fn load_entries(input_dir: &Path) -> Result<Vec<(String, String)>> {
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut rd = fs::read_dir(input_dir)
        .await
        .with_context(|| format!("reading input dir {}", input_dir.display()))?;
    while let Some(entry) = rd
        .next_entry()
        .await
        .with_context(|| format!("iterating input dir {}", input_dir.display()))?
    {
        let path = entry.path();
        let Some(tag) = extract_tag(&path) else {
            continue;
        };
        let contents = fs::read_to_string(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        entries.push((tag, contents));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(directive) = cli.log.as_deref() {
        env_logger::Builder::new().parse_filters(directive).init();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    }

    let input_dir = cli.input_dir;

    let min_freshness = min_freshness_secs()?;
    debug!("cache min-freshness floor: {min_freshness}s");
    info!("input dir: {}", input_dir.display());

    let client = make_client("check-checksums/0.1", min_freshness)?;

    let entries = load_entries(&input_dir).await?;
    info!(
        "checking {} committed checksum file(s) with concurrency={CONCURRENCY}",
        entries.len(),
    );

    let mut in_flight = FuturesUnordered::new();
    let mut iter = entries.iter();

    for _ in 0..CONCURRENCY {
        if let Some((tag, body)) = iter.next() {
            in_flight.push(check_one_owned(&client, tag.clone(), body.clone()));
        } else {
            break;
        }
    }

    let mut ok = 0usize;
    let mut mismatched = Vec::<String>::new();
    let mut fetch_failed = Vec::<(String, StatusCode)>::new();

    while let Some(result) = in_flight.next().await {
        match result? {
            (_, Outcome::Match) => ok += 1,
            (tag, Outcome::Mismatch) => mismatched.push(tag),
            (tag, Outcome::FetchFailed(status)) => fetch_failed.push((tag, status)),
        }
        if let Some((tag, body)) = iter.next() {
            in_flight.push(check_one_owned(&client, tag.clone(), body.clone()));
        }
    }

    info!(
        "{ok} matched, {} mismatched, {} fetch-failed (of {} total)",
        mismatched.len(),
        fetch_failed.len(),
        entries.len(),
    );

    if !mismatched.is_empty() {
        mismatched.sort();
        bail!(
            "{} committed checksum file(s) disagree with upstream: {}",
            mismatched.len(),
            mismatched.join(", ")
        );
    }

    Ok(())
}

/// Owned-body wrapper over [`check_one`] so each future is `'static`
/// relative to the caller's stack — required by `FuturesUnordered` when the
/// futures outlive the iterator borrow.
async fn check_one_owned(
    client: &CachingClient,
    tag: String,
    body: String,
) -> Result<(String, Outcome)> {
    let url = format!("https://github.com/{OWNER}/{REPO}/releases/download/{tag}/SHA256SUMS");
    match check_one(client, &tag, &body, &url).await {
        Ok(true) => Ok((tag, Outcome::Match)),
        Ok(false) => Ok((tag, Outcome::Mismatch)),
        Err(CheckError::Status(status)) => {
            warn!("{tag}: GET {url} returned {status}, skipping");
            Ok((tag, Outcome::FetchFailed(status)))
        }
        Err(CheckError::Other(e)) => Err(e).with_context(|| format!("checking tag {tag}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_tag_valid() {
        let path = Path::new("/dir/20260414.sha256sums");
        assert_eq!(extract_tag(path), Some("20260414".to_owned()));
    }

    #[test]
    fn extract_tag_no_suffix() {
        let path = Path::new("/dir/README.md");
        assert_eq!(extract_tag(path), None);
    }

    #[test]
    fn extract_tag_empty_tag() {
        let path = Path::new("/dir/.sha256sums");
        assert_eq!(extract_tag(path), None);
    }

    #[test]
    fn extract_tag_complex_name() {
        let path = Path::new("/some/path/v1.2.3-beta.sha256sums");
        assert_eq!(extract_tag(path), Some("v1.2.3-beta".to_owned()));
    }

    #[tokio::test]
    async fn load_entries_reads_and_sorts() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        tokio::fs::write(dir.join("20260414.sha256sums"), "contents-a")
            .await
            .unwrap();
        tokio::fs::write(dir.join("20260101.sha256sums"), "contents-b")
            .await
            .unwrap();
        tokio::fs::write(dir.join("README.md"), "not a checksum file")
            .await
            .unwrap();

        let entries = load_entries(dir).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "20260101");
        assert_eq!(entries[0].1, "contents-b");
        assert_eq!(entries[1].0, "20260414");
        assert_eq!(entries[1].1, "contents-a");
    }

    #[tokio::test]
    async fn load_entries_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = load_entries(tmp.path()).await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn check_one_matching_body() {
        let cache_dir = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;

        let body = "aaa111  file.tar.gz\n";
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let client = CachingClient::new(cache_dir.path()).unwrap();
        let url = format!("{}/checksums", server.uri());
        let result = check_one(&client, "tag", body, &url).await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn check_one_mismatching_body() {
        let cache_dir = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("upstream\n"))
            .mount(&server)
            .await;

        let client = CachingClient::new(cache_dir.path()).unwrap();
        let url = format!("{}/checksums", server.uri());
        let result = check_one(&client, "tag", "local\n", &url).await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn check_one_non_200_returns_status_error() {
        let cache_dir = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = CachingClient::new(cache_dir.path()).unwrap();
        let url = format!("{}/checksums", server.uri());
        let err = check_one(&client, "tag", "body", &url).await.unwrap_err();
        assert!(matches!(err, CheckError::Status(StatusCode::NOT_FOUND)));
    }
}
