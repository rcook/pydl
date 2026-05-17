//! `pydl update`: refresh the local snapshots that `pydl available` and
//! `pydl self-update` read from. This is the single command in the CLI that
//! contacts `api.github.com` for *release listings*. The other network-using
//! commands (`pydl download`, `pydl self-update`) reach out only for asset
//! bytes; `pydl self-update --online` adds a network round-trip for the
//! version check as an explicit escape hatch. Every other subcommand is
//! offline by design.
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
use log::{debug, info, warn};
use pydl_cache::{CachingClient, Method, StatusCode};
use pydl_common::filter::Release;
use pydl_common::snapshot::{self, PydlRelease};
use pydl_common::{PER_PAGE, fetch_releases_page, make_client, min_freshness_secs};
use semver::Version;

const SELF_OWNER: &str = "rcook";
const SELF_REPO: &str = "pydl";

#[derive(Parser, Debug)]
pub struct Args {
    /// Force a complete re-fetch of all releases, bypassing incremental logic.
    #[arg(long)]
    pub full: bool,
}

#[allow(clippy::needless_pass_by_value)]
pub async fn run(args: Args) -> Result<()> {
    let min_freshness = min_freshness_secs()?;
    debug!("cache min-freshness floor: {min_freshness}s");
    let client = make_client(crate::USER_AGENT, min_freshness)?;

    info!("fetching python-build-standalone releases...");
    let releases = fetch_pbs_releases(&client, args.full).await?;
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

    // Trailer: print exactly what `pydl available` will print after this
    // run, so the user sees the same wording from producer and consumer.
    info!("");
    info!(
        "{}",
        snapshot::format_python_releases_short_summary(&releases)
    );
    emit_pydl_trailer(&pydl_latest);

    Ok(())
}

/// Print the canonical `pydl: latest …` line for a freshly-written
/// snapshot. Non-fatal: a non-semver running version or upstream tag logs
/// a warning and skips the line — the snapshot itself is already on disc.
fn emit_pydl_trailer(release: &PydlRelease) {
    let running_str = env!("CARGO_PKG_VERSION");
    let Ok(running) = Version::parse(running_str) else {
        warn!("could not parse running version {running_str:?} as semver; skipping pydl trailer");
        return;
    };
    let latest_str = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    let Ok(latest) = Version::parse(latest_str) else {
        warn!(
            "could not parse snapshot latest tag {:?} as semver; skipping pydl trailer",
            release.tag_name
        );
        return;
    };
    info!("{}", snapshot::format_pydl_version_line(&running, &latest));
}

/// Fetch PBS releases: incremental when a valid snapshot exists, full otherwise.
async fn fetch_pbs_releases(client: &CachingClient, force_full: bool) -> Result<Vec<Release>> {
    if !force_full {
        if let Some(envelope) = snapshot::read_pbs_releases()? {
            let existing = envelope.payload;
            match fetch_pbs_releases_incremental(client, &existing).await? {
                Some(new_releases) if new_releases.is_empty() => {
                    info!("already up to date (0 new releases)");
                    return Ok(existing);
                }
                Some(new_releases) => {
                    let count = new_releases.len();
                    info!("{count} new release(s) found, prepending to snapshot");
                    let mut merged = new_releases;
                    merged.extend(existing);
                    return Ok(merged);
                }
                None => {
                    info!("incremental fetch not possible, performing full refresh");
                }
            }
        } else {
            debug!("no existing snapshot, performing full fetch");
        }
    } else {
        info!("--full: performing complete re-fetch");
    }
    fetch_all_pbs_releases(client).await
}

/// Attempt an incremental fetch: only pages newer than the snapshot's
/// most-recent tag. Returns `None` when the caller should fall back to a
/// full fetch (empty snapshot, known tag not found upstream).
async fn fetch_pbs_releases_incremental(
    client: &CachingClient,
    existing: &[Release],
) -> Result<Option<Vec<Release>>> {
    let Some(known_newest) = existing.first().map(|r| &r.tag_name) else {
        return Ok(None);
    };

    let mut new_releases: Vec<Release> = Vec::new();
    let mut page = 1usize;
    loop {
        debug!("incremental: fetching page {page}");
        let page_releases: Vec<Release> = fetch_releases_page(client, page, PER_PAGE).await?;
        let n = page_releases.len();

        if let Some(i) = page_releases
            .iter()
            .position(|r| r.tag_name == *known_newest)
        {
            new_releases.extend(page_releases.into_iter().take(i));
            return Ok(Some(new_releases));
        }

        new_releases.extend(page_releases);

        if n < PER_PAGE {
            return Ok(None);
        }
        page += 1;
    }
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
