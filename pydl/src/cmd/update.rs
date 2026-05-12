//! `pydl update`: refresh the local snapshots that `pydl available` and
//! `pydl self-update` read from. This is the single command in the CLI that
//! contacts `api.github.com` for *release listings* — every other command
//! (except `pydl download` for asset bytes and `pydl self-update --online`
//! for an explicit network bypass) is offline by design.
//!
//! Two snapshots are written under `~/.pydl/snapshot/`:
//!
//! - `pbs-releases.json` — full paginated listing of
//!   `astral-sh/python-build-standalone` releases, consumed by `available`.
//! - `pydl-latest.json` — the latest stable `rcook/pydl` release, consumed
//!   by `self-update`.
//!
//! Both writes go through the existing `request_with_retry` + `CachingClient`
//! stack, so the bounded-retry policy that issue #2 introduced still applies.

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use log::{debug, info};
use pydl_cache::{CachingClient, Method, StatusCode};
use pydl_common::filter::Release;
use pydl_common::snapshot::{self, PydlRelease};
use pydl_common::{PER_PAGE, fetch_releases_page, make_client, min_freshness_secs};

const SELF_OWNER: &str = "rcook";
const SELF_REPO: &str = "pydl";

#[derive(Parser, Debug)]
pub struct Args {}

#[allow(clippy::needless_pass_by_value)]
pub async fn run(_args: Args) -> Result<()> {
    let min_freshness = min_freshness_secs()?;
    debug!("cache min-freshness floor: {min_freshness}s");
    let client = make_client(crate::USER_AGENT, min_freshness)?;

    info!("fetching python-build-standalone releases...");
    let releases = fetch_all_pbs_releases(&client).await?;
    snapshot::write_pbs_releases(&releases)
        .with_context(|| "writing pbs-releases snapshot".to_owned())?;
    info!(
        "snapshot: pbs-releases ({} releases) -> {}",
        releases.len(),
        snapshot::pbs_releases_path()?.display()
    );

    info!("fetching latest pydl release...");
    let pydl_latest = fetch_latest_pydl_stable(&client).await?;
    snapshot::write_pydl_latest(&pydl_latest)
        .with_context(|| "writing pydl-latest snapshot".to_owned())?;
    info!(
        "snapshot: pydl-latest ({}) -> {}",
        pydl_latest.tag_name,
        snapshot::pydl_latest_path()?.display()
    );

    Ok(())
}

/// Paginate the Python releases endpoint until upstream returns a partial page.
/// Same loop shape that lived in `pydl available` before this refactor —
/// moved here so the network usage is concentrated in `update`.
async fn fetch_all_pbs_releases(client: &CachingClient) -> Result<Vec<Release>> {
    let mut all = Vec::new();
    let mut page = 1usize;
    loop {
        debug!("fetching releases page {page}");
        let page_releases: Vec<Release> = fetch_releases_page(client, page, PER_PAGE).await?;
        let n = page_releases.len();
        all.extend(page_releases);
        if n < PER_PAGE {
            break;
        }
        page += 1;
    }
    Ok(all)
}

/// Fetch `releases/latest` from `rcook/pydl`. Filters drafts at write time
/// so consumers (i.e. `self-update` reading the snapshot) don't have to.
async fn fetch_latest_pydl_stable(client: &CachingClient) -> Result<PydlRelease> {
    let url = format!("https://api.github.com/repos/{SELF_OWNER}/{SELF_REPO}/releases/latest");
    let (status, body) = client.request(Method::GET, &url).await?;
    if status != StatusCode::OK {
        bail!(
            "GET {url} returned {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    let api: ApiRelease = serde_json::from_slice(&body).map_err(|e| {
        anyhow!(
            "parsing latest pydl release JSON: {e} (body: {})",
            String::from_utf8_lossy(&body)
        )
    })?;
    if api.draft {
        bail!("`releases/latest` returned a draft release — refusing to snapshot");
    }
    Ok(api.into_pydl_release())
}

#[derive(serde::Deserialize, Debug)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<ApiReleaseAsset>,
}

#[derive(serde::Deserialize, Debug)]
struct ApiReleaseAsset {
    name: String,
    browser_download_url: String,
}

impl ApiRelease {
    fn into_pydl_release(self) -> PydlRelease {
        PydlRelease {
            tag_name: self.tag_name,
            assets: self
                .assets
                .into_iter()
                .map(|a| pydl_common::snapshot::PydlReleaseAsset {
                    name: a.name,
                    browser_download_url: a.browser_download_url,
                })
                .collect(),
        }
    }
}
