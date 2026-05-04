//! Shared helpers for the `pydl-*` binaries: cache-dir discovery, the
//! min-freshness env override, a paginated GitHub releases fetcher, asset
//! parsing, host-platform detection and the asset-filter CLI/logic.

pub mod asset;
pub mod checksums;
pub mod config;
pub mod filter;
pub mod install;
pub mod platform;

use std::env;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use pydl_cache::{CachingClient, Method, StatusCode};
use serde::de::DeserializeOwned;

pub const OWNER: &str = "astral-sh";
pub const REPO: &str = "python-build-standalone";

/// Page size for release-list requests. Shared so every binary keys on the
/// same URL and hits the same cache entries.
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

/// Fetch a single page of releases from the GitHub API through the cache,
/// deserializing into the caller's chosen shape.
pub async fn fetch_releases_page<T: DeserializeOwned>(
    client: &CachingClient,
    page: usize,
    per_page: usize,
) -> Result<Vec<T>> {
    let url = format!(
        "https://api.github.com/repos/{OWNER}/{REPO}/releases?per_page={per_page}&page={page}"
    );
    let (status, body) = client.request(Method::GET, &url).await?;
    if status != StatusCode::OK {
        bail!(
            "GET {url} returned {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    serde_json::from_slice(&body).map_err(|e| anyhow!("parsing releases page {page}: {e}"))
}
