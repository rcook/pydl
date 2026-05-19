use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use log::{debug, info, warn};
use pydl_cache::{CachingClient, Method, StatusCode};
use pydl_common::{OWNER, PER_PAGE, REPO, fetch_releases_page, make_client, min_freshness_secs};
use serde::Deserialize;
use tokio::fs;

#[derive(Deserialize, Debug)]
struct Release {
    tag_name: String,
}

#[derive(Parser, Debug)]
#[command(
    name = "get-checksums",
    version,
    about = "Mirror every upstream release's SHA256SUMS into a local directory."
)]
struct Cli {
    /// Output directory for `<tag>.sha256sums` files. Created if missing.
    output_dir: PathBuf,

    /// Log filter directive (overrides `RUST_LOG`). Accepts the same syntax
    /// as `RUST_LOG`, e.g. `debug`, `pydl_cache=debug`.
    #[arg(short = 'l', long = "log", value_name = "DIRECTIVE")]
    log: Option<String>,
}

async fn download_checksums(client: &CachingClient, tag: &str, out_path: &Path) -> Result<bool> {
    let url = format!("https://github.com/{OWNER}/{REPO}/releases/download/{tag}/SHA256SUMS");
    let (status, body, _) = client.request(Method::GET, &url).await?;
    if status != StatusCode::OK {
        warn!("{tag}: {url} returned {status}, skipping");
        return Ok(false);
    }
    fs::write(out_path, &body)
        .await
        .with_context(|| format!("writing {}", out_path.display()))?;
    info!("{tag}: wrote {} ({} bytes)", out_path.display(), body.len());
    Ok(true)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(directive) = cli.log.as_deref() {
        env_logger::Builder::new().parse_filters(directive).init();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    }

    let out_dir = cli.output_dir;
    fs::create_dir_all(&out_dir)
        .await
        .with_context(|| format!("creating output dir {}", out_dir.display()))?;

    let min_freshness = min_freshness_secs()?;
    debug!("cache min-freshness floor: {min_freshness}s");
    info!("output dir: {}", out_dir.display());

    let client = make_client("get-checksums/0.1", min_freshness)?;

    let mut page = 1usize;
    let mut total = 0usize;
    let mut downloaded = 0usize;
    let mut skipped = 0usize;
    let mut missing = 0usize;
    loop {
        debug!("fetching releases page {page}");
        let (releases, _): (Vec<Release>, _) = fetch_releases_page(&client, page, PER_PAGE).await?;
        let got = releases.len();
        for release in &releases {
            total += 1;
            let out_path = out_dir.join(format!("{}.sha256sums", release.tag_name));
            if fs::try_exists(&out_path).await.unwrap_or(false) {
                skipped += 1;
                continue;
            }
            if download_checksums(&client, &release.tag_name, &out_path).await? {
                downloaded += 1;
            } else {
                missing += 1;
            }
        }
        if got < PER_PAGE {
            break;
        }
        page += 1;
    }

    info!(
        "{total} release(s): {downloaded} downloaded, {skipped} already present, {missing} missing SHA256SUMS"
    );
    Ok(())
}
