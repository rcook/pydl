//! Shared helpers for the `pydl-*` binaries: cache-dir discovery, the
//! min-freshness env override, a paginated GitHub releases fetcher, asset
//! parsing, host-platform detection and the asset-filter CLI/logic.

pub mod asset;
pub mod checksums;
pub mod config;
pub mod filter;
pub mod install;
pub mod platform;
pub mod snapshot;

use std::env;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use log::info;
use pydl_cache::{CachingClient, Method, StatusCode};
use serde::de::DeserializeOwned;

pub const OWNER: &str = "astral-sh";
pub const REPO: &str = "python-build-standalone";

/// Page size for release-list requests. Shared so every binary keys on the
/// same URL and hits the same cache entries.
///
/// Set to `10` (down from the API's `100` maximum) because GitHub's edge
/// reliably 504s while assembling the larger payloads for
/// `astral-sh/python-build-standalone` — see `ISSUES.md` § "Open
/// investigations" for the live measurements that motivated the drop. The
/// trade-off is ~9 paginated GETs per `pydl update` instead of 1, which is
/// well within the 60 req/hr unauthenticated budget for a single command.
/// Revisit if the upstream timeout window improves.
pub const PER_PAGE: usize = 10;

/// Default client-side minimum-freshness floor in seconds (24 h). See the
/// `pydl` README for rationale.
pub const DEFAULT_MIN_FRESHNESS_SECS: u64 = 24 * 60 * 60;
pub const MIN_FRESHNESS_ENV: &str = "PYDL_MIN_FRESHNESS_SECS";

/// Read `PYDL_MIN_FRESHNESS_SECS` from the environment, falling back to
/// [`DEFAULT_MIN_FRESHNESS_SECS`] when unset.
pub fn min_freshness_secs() -> Result<u64> {
    match env::var(MIN_FRESHNESS_ENV) {
        Ok(v) => v
            .parse::<u64>()
            .map_err(|e| anyhow!("invalid {MIN_FRESHNESS_ENV}={v:?}: {e}")),
        Err(env::VarError::NotPresent) => Ok(DEFAULT_MIN_FRESHNESS_SECS),
        Err(env::VarError::NotUnicode(_)) => bail!("{MIN_FRESHNESS_ENV} is not valid unicode"),
    }
}

/// Resolve the top-level pydl state directory — `$HOME/.pydl/` on Unix,
/// `%USERPROFILE%\.pydl\` on Windows.
pub fn pydl_root() -> Result<PathBuf> {
    let home = if cfg!(windows) {
        env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("USERPROFILE is not set"))?
    } else {
        env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HOME is not set"))?
    };
    Ok(home.join(".pydl"))
}

/// Shared HTTP cache directory: `$HOME/.pydl/cache/`.
pub fn cache_dir() -> Result<PathBuf> {
    Ok(pydl_root()?.join("cache"))
}

/// Build a `CachingClient` against [`cache_dir`] with the given user agent
/// and min-freshness floor.
pub fn make_client(user_agent: &str, min_freshness: u64) -> Result<CachingClient> {
    Ok(
        CachingClient::with_user_agent(cache_dir()?, Some(user_agent))?
            .with_min_freshness_secs(min_freshness),
    )
}

/// HTTP statuses we treat as transient and worth retrying. 500 is excluded —
/// a real server bug shouldn't be papered over by retries — and 429 / 403
/// (rate-limit shapes) are surfaced cleanly by the cache's existing
/// `rate_limit_message` and won't recover within a short backoff window.
const RETRY_STATUSES: &[StatusCode] = &[
    StatusCode::BAD_GATEWAY,
    StatusCode::SERVICE_UNAVAILABLE,
    StatusCode::GATEWAY_TIMEOUT,
];

const MAX_ATTEMPTS: u32 = 5;
const BASE_BACKOFF_MS: u64 = 1000;

/// Run a GET through the cache with bounded retries on transient upstream
/// failures (network errors and the statuses in [`RETRY_STATUSES`]).
async fn request_with_retry(client: &CachingClient, url: &str) -> Result<(StatusCode, Vec<u8>)> {
    let mut attempt: u32 = 1;
    loop {
        let outcome = client.request(Method::GET, url).await;
        let retry_reason = match &outcome {
            Ok((status, _)) if RETRY_STATUSES.contains(status) => Some(format!("{status}")),
            Err(e) => Some(format!("network error: {e}")),
            _ => None,
        };
        match retry_reason {
            Some(reason) if attempt < MAX_ATTEMPTS => {
                let delay = backoff_ms(attempt);
                info!(
                    "GET {url} -> {reason}, retrying in {delay}ms (attempt {attempt}/{MAX_ATTEMPTS})"
                );
                tokio::time::sleep(Duration::from_millis(delay)).await;
                attempt += 1;
            }
            _ => return outcome,
        }
    }
}

/// Backoff for retry `attempt` (1-indexed), with ±25% jitter. `1000 * 2^(n-1)`
/// before jitter: 1s, 2s, 4s, 8s. Jitter source is the unix time's low bits —
/// good enough to spread retries across processes without taking on a `rand`
/// dependency.
fn backoff_ms(attempt: u32) -> u64 {
    let base = BASE_BACKOFF_MS.saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1)));
    let jitter_seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    // Map seed to [-25%, +25%] of base.
    let span = base / 2;
    let offset = jitter_seed % span.max(1);
    base.saturating_sub(span / 2).saturating_add(offset)
}

/// Format a non-2xx response body for inclusion in an error message. Keeps
/// short text/JSON bodies verbatim, summarizes HTML (typical for upstream
/// error pages) and other large bodies as a one-line `(N-byte ... body)`.
fn format_error_body(body: &[u8]) -> String {
    const MAX: usize = 200;
    let leading_ws = body.iter().take_while(|b| b.is_ascii_whitespace()).count();
    let rest = &body[leading_ws..];
    if rest.is_empty() {
        return String::new();
    }
    if rest.starts_with(b"<") {
        return format!(
            ": ({}-byte HTML body — likely an upstream error page)",
            body.len()
        );
    }
    let take = body.len().min(MAX);
    let s = String::from_utf8_lossy(&body[..take]);
    if body.len() > MAX {
        format!(": {s}…")
    } else {
        format!(": {s}")
    }
}

/// Fetch a single page of releases from the GitHub API through the cache.
///
/// Deserializes into the caller's chosen shape, and retries transient
/// upstream failures with bounded exponential backoff via
/// [`request_with_retry`].
pub async fn fetch_releases_page<T: DeserializeOwned>(
    client: &CachingClient,
    page: usize,
    per_page: usize,
) -> Result<Vec<T>> {
    let url = format!(
        "https://api.github.com/repos/{OWNER}/{REPO}/releases?per_page={per_page}&page={page}"
    );
    let (status, body) = request_with_retry(client, &url).await?;
    if status != StatusCode::OK {
        bail!("GET {url} returned {status}{}", format_error_body(&body));
    }
    serde_json::from_slice(&body).map_err(|e| anyhow!("parsing releases page {page}: {e}"))
}

#[cfg(test)]
mod tests {
    use pydl_cache::CachingClient;
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn make_test_client(dir: &TempDir) -> CachingClient {
        CachingClient::new(dir.path()).unwrap()
    }

    #[test]
    fn format_error_body_empty() {
        assert_eq!(format_error_body(b""), "");
    }

    #[test]
    fn format_error_body_html_is_summarized() {
        let body = b"<!DOCTYPE html><html><body>504 Gateway Timeout, lots of content here that should not appear in the error message</body></html>";
        let out = format_error_body(body);
        assert!(out.contains("HTML body"), "got: {out}");
        assert!(out.contains(&format!("{}-byte", body.len())), "got: {out}");
        assert!(!out.contains("DOCTYPE"), "got: {out}");
    }

    #[test]
    fn format_error_body_html_with_leading_whitespace_still_summarized() {
        let body = b"\n  <!DOCTYPE html><html></html>";
        let out = format_error_body(body);
        assert!(out.contains("HTML body"), "got: {out}");
    }

    #[test]
    fn format_error_body_short_json_passes_through() {
        let body = br#"{"message":"Not Found"}"#;
        let out = format_error_body(body);
        assert_eq!(out, r#": {"message":"Not Found"}"#);
    }

    #[test]
    fn format_error_body_long_text_truncated() {
        let body = vec![b'x'; 500];
        let out = format_error_body(&body);
        assert!(out.ends_with('…'), "got: {out}");
        assert!(out.len() < 500, "got len {}: {out}", out.len());
    }

    // `start_paused = true` so the multi-second backoff sleeps in
    // `request_with_retry` advance via tokio's virtual clock instead of
    // blocking the test on wall time.
    #[tokio::test(start_paused = true)]
    async fn fetch_releases_page_retries_on_504_then_succeeds() {
        let dir = TempDir::new().unwrap();
        let client = make_test_client(&dir);
        let server = MockServer::start().await;

        // First two calls return 504, third returns a valid empty-array body.
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(504).set_body_bytes(b"<!DOCTYPE html>".as_slice()))
            .up_to_n_times(2)
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"[]".as_slice()))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        let (status, body) = request_with_retry(&client, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"[]");
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_releases_page_gives_up_after_max_attempts() {
        let dir = TempDir::new().unwrap();
        let client = make_test_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(504).set_body_bytes(
                b"<!DOCTYPE html><html><body>Gateway Timeout</body></html>".as_slice(),
            ))
            .expect(u64::from(MAX_ATTEMPTS))
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        let (status, body) = request_with_retry(&client, &url).await.unwrap();
        assert_eq!(status, 504);

        // The error message produced by the call site must not include the
        // raw HTML — only the summary.
        let msg = format!("GET {url} returned {status}{}", format_error_body(&body));
        assert!(!msg.contains("DOCTYPE"), "leaked HTML: {msg}");
        assert!(msg.contains("HTML body"), "missing summary: {msg}");
    }

    #[tokio::test]
    async fn fetch_releases_page_does_not_retry_4xx() {
        let dir = TempDir::new().unwrap();
        let client = make_test_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_bytes(b"{\"message\":\"Not Found\"}".as_slice()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        let (status, _body) = request_with_retry(&client, &url).await.unwrap();
        assert_eq!(status, 404);
    }

    #[test]
    fn backoff_ms_grows_with_attempt() {
        // ±25% jitter on a 1000ms base doubling per attempt means attempt 1
        // lies in [750, 1250], attempt 2 in [1500, 2500], attempt 3 in
        // [3000, 5000]. Just check the bands don't overlap.
        let a1 = backoff_ms(1);
        let a2 = backoff_ms(2);
        assert!(
            (750..=1250).contains(&a1),
            "attempt 1 jitter out of band: {a1}"
        );
        assert!(
            (1500..=2500).contains(&a2),
            "attempt 2 jitter out of band: {a2}"
        );
    }
}
