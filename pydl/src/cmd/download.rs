use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use futures_util::StreamExt;
use log::{debug, info, warn};
use pydl_cache::{CachingClient, StatusCode};
use pydl_common::filter::{
    FilterArgs, apply_config_defaults, auto_select_tag_embedded, filter_embedded,
    pick_single_embedded,
};
use pydl_common::{OWNER, REPO, checksums, make_client, min_freshness_secs};
use sha2::{Digest, Sha256};
use tokio::fs;

#[derive(Parser, Debug)]
pub struct Args {
    #[command(flatten)]
    pub filter: FilterArgs,

    /// Optional directory to *also* copy the asset into after the cache is
    /// warmed. Without this flag the asset still ends up in `~/.pydl/cache/`
    /// (where `pydl install` will find it) — `-o` just writes an additional
    /// user-visible copy.
    #[arg(long, short = 'o', value_name = "DIR")]
    pub output_dir: Option<PathBuf>,
}

pub async fn run(args: Args) -> Result<()> {
    let Args { filter, output_dir } = args;
    let mut filter = apply_config_defaults(filter)?;
    auto_select_tag_embedded(&mut filter)?;

    let hits = filter_embedded(&filter)?;
    let (tag, asset_name) = pick_single_embedded(&hits)?;

    let min_freshness = min_freshness_secs()?;
    debug!("cache min-freshness floor: {min_freshness}s");
    let client = make_client(crate::USER_AGENT, min_freshness)?;

    let url = format!("https://github.com/{OWNER}/{REPO}/releases/download/{tag}/{asset_name}");
    let expected = checksums::expected_hash(tag, asset_name)?;

    // Pull through the cache. The cache tees the upstream stream into its
    // on-disc body; draining the stream here is what "warms" the cache.
    // The stream itself is also SHA-hashed so we can verify before any
    // user-visible file is written.
    let total = stream_through_cache(&client, &url, expected, asset_name).await?;
    let body_path = client.cached_body_path(&url)?.with_context(|| {
        format!("cache body missing for {url} after successful fetch (this is a bug)")
    })?;
    debug!("cached {asset_name} ({total} bytes, sha256 ok)");

    if let Some(out_dir) = output_dir {
        fs::create_dir_all(&out_dir)
            .await
            .with_context(|| format!("creating output dir {}", out_dir.display()))?;
        let out_path = out_dir.join(asset_name);
        fs::copy(&body_path, &out_path).await.with_context(|| {
            format!("copying {} -> {}", body_path.display(), out_path.display())
        })?;
        info!("wrote {} ({total} bytes)", out_path.display());
        // Print the user-visible copy — that's what callers typically want:
        //   path=$(pydl download -t ... -v ... -o ...)
        println!("{}", out_path.display());
    } else {
        // No user-visible file; print the cache path so scripts can still
        // locate the archive (e.g. to pipe straight into `pydl install`).
        println!("{}", body_path.display());
    }

    Ok(())
}

/// Consume the asset body through the cache and verify its SHA-256 against
/// `expected_hex`. On hash mismatch the cache entry is evicted so a re-run
/// will refetch from upstream instead of re-serving the bad bytes.
async fn stream_through_cache(
    client: &CachingClient,
    url: &str,
    expected_hex: &str,
    asset_name: &str,
) -> Result<u64> {
    let (status, mut stream) = client.get_stream(url).await?;
    if status != StatusCode::OK {
        bail!("GET {url} returned {status}");
    }
    let mut hasher = Sha256::new();
    // Drain the stream; the cache writes the tee'd bytes to its body file
    // as a side effect of iterating the stream.
    let mut total: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading chunk from upstream")?;
        hasher.update(&chunk);
        total += chunk.len() as u64;
    }

    let actual = checksums::hex_digest(hasher);
    if !checksums::hashes_match(expected_hex, &actual) {
        // The cache holds the wrong bytes upstream served us. Evict before
        // bailing so a re-run actually refetches instead of serving the
        // poisoned entry as a HIT (or revalidating against the same bad
        // upstream and getting a 304). Eviction failure is logged but does
        // not mask the original mismatch error — the user can still recover
        // with `pydl cache clear --yes`.
        if let Err(e) = client.evict(url) {
            warn!("failed to evict cache entry for {url} after sha256 mismatch: {e:#}");
        }
        bail!(
            "sha256 mismatch for {asset_name}: expected {expected_hex}, got {actual} — \
             cache entry evicted; re-run to refetch"
        );
    }
    Ok(total)
}
