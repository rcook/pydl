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

use std::future::Future;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use log::{debug, warn};
use owo_colors::OwoColorize;
use owo_colors::Stream::Stdout;
use pydl_cache::{CachingClient, Method, Notice, StatusCode};
use pydl_common::filter::Release;
use pydl_common::snapshot::{self, PydlRelease};
use pydl_common::{PER_PAGE, fetch_releases_page, make_client};
use semver::Version;

use crate::progress::{self, ProgressMode};

const SELF_OWNER: &str = "rcook";
const SELF_REPO: &str = "pydl";

#[derive(Parser, Debug)]
pub struct Args {
    /// Force a complete re-fetch of all releases, bypassing incremental logic.
    #[arg(long)]
    pub full: bool,
}

#[allow(clippy::needless_pass_by_value)]
pub async fn run(args: Args, progress_mode: ProgressMode) -> Result<()> {
    let client = make_client(crate::USER_AGENT, 0)?;

    let pb = progress::spinner(progress_mode, "fetching releases...");
    let (releases, notices) = fetch_pbs_releases(&client, args.full, &pb).await?;
    pb.finish_and_clear();
    print_notices(&notices);
    snapshot::write_pbs_releases(&releases)
        .with_context(|| "writing pbs-releases snapshot".to_owned())?;
    println!(
        "snapshot: pbs-releases ({} releases) -> {}",
        releases.len(),
        snapshot::pbs_releases_path()?.display()
    );

    let pb = progress::spinner(progress_mode, "fetching latest pydl release...");
    let (pydl_latest, notices) = fetch_latest_pydl_stable(&client).await?;
    pb.finish_and_clear();
    print_notices(&notices);
    snapshot::write_pydl_latest(&pydl_latest)
        .with_context(|| "writing pydl-latest snapshot".to_owned())?;
    println!(
        "snapshot: pydl-latest ({}) -> {}",
        pydl_latest.tag_name,
        snapshot::pydl_latest_path()?.display()
    );

    println!(
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
    println!("{}", snapshot::format_pydl_version_line(&running, &latest));
}

fn print_notices(notices: &[Notice]) {
    for notice in notices {
        match notice {
            Notice::RateLimit(msg) => {
                let line = format!("warning: {msg}");
                println!("{}", line.if_supports_color(Stdout, |t| t.yellow()));
            }
            Notice::StaleIfError { upstream_status } => {
                let line =
                    format!("warning: upstream returned {upstream_status}, serving from cache");
                println!("{}", line.if_supports_color(Stdout, |t| t.yellow()));
            }
            Notice::Retry {
                reason,
                attempt,
                max_attempts,
                delay_ms,
            } => {
                let line = format!(
                    "  retrying ({attempt}/{max_attempts}, {reason}, backoff {delay_ms}ms)"
                );
                println!("{}", line.if_supports_color(Stdout, |t| t.dimmed()));
            }
        }
    }
}

/// Fetch PBS releases: incremental when a valid snapshot exists, full otherwise.
async fn fetch_pbs_releases(
    client: &CachingClient,
    force_full: bool,
    pb: &indicatif::ProgressBar,
) -> Result<(Vec<Release>, Vec<Notice>)> {
    let fetch_page = |page: usize| {
        pb.set_message(format!("fetching releases (page {page})..."));
        fetch_releases_page(client, page, PER_PAGE)
    };
    if force_full {
        debug!("--full: performing complete re-fetch");
    } else if let Some(envelope) = snapshot::read_pbs_releases()? {
        let existing = envelope.payload;
        match fetch_incremental(&fetch_page, &existing).await? {
            Some((new_releases, notices)) if new_releases.is_empty() => {
                pb.finish_and_clear();
                println!("already up to date");
                return Ok((existing, notices));
            }
            Some((new_releases, notices)) => {
                pb.finish_and_clear();
                let count = new_releases.len();
                println!("{count} new release(s) found");
                let mut merged = new_releases;
                merged.extend(existing);
                return Ok((merged, notices));
            }
            None => {
                debug!("incremental fetch not possible, performing full refresh");
            }
        }
    } else {
        debug!("no existing snapshot, performing full fetch");
    }
    fetch_all(&fetch_page).await
}

/// Attempt an incremental fetch: only pages newer than the snapshot's
/// most-recent tag. Returns `None` when the caller should fall back to a
/// full fetch (empty snapshot, known tag not found upstream).
async fn fetch_incremental<F, Fut>(
    fetch_page: &F,
    existing: &[Release],
) -> Result<Option<(Vec<Release>, Vec<Notice>)>>
where
    F: Fn(usize) -> Fut + Send + Sync,
    Fut: Future<Output = Result<(Vec<Release>, Vec<Notice>)>> + Send,
{
    let Some(known_newest) = existing.first().map(|r| &r.tag_name) else {
        return Ok(None);
    };

    let mut new_releases: Vec<Release> = Vec::new();
    let mut all_notices: Vec<Notice> = Vec::new();
    let mut page = 1usize;
    loop {
        debug!("incremental: fetching page {page}");
        let (page_releases, notices): (Vec<Release>, _) = fetch_page(page).await?;
        all_notices.extend(notices);
        let n = page_releases.len();

        if let Some(i) = page_releases
            .iter()
            .position(|r| r.tag_name == *known_newest)
        {
            new_releases.extend(page_releases.into_iter().take(i));
            return Ok(Some((new_releases, all_notices)));
        }

        new_releases.extend(page_releases);

        if n < PER_PAGE {
            return Ok(None);
        }
        page += 1;
    }
}

/// Paginate the releases endpoint until upstream returns a partial page.
async fn fetch_all<F, Fut>(fetch_page: &F) -> Result<(Vec<Release>, Vec<Notice>)>
where
    F: Fn(usize) -> Fut + Send + Sync,
    Fut: Future<Output = Result<(Vec<Release>, Vec<Notice>)>> + Send,
{
    let mut all = Vec::new();
    let mut all_notices = Vec::new();
    let mut page = 1usize;
    loop {
        debug!("fetching releases page {page}");
        let (page_releases, notices): (Vec<Release>, _) = fetch_page(page).await?;
        all_notices.extend(notices);
        let n = page_releases.len();
        all.extend(page_releases);
        if n < PER_PAGE {
            break;
        }
        page += 1;
    }
    Ok((all, all_notices))
}

/// Fetch `releases/latest` from `rcook/pydl`. Filters drafts at write time
/// so consumers (i.e. `self-update` reading the snapshot) don't have to.
async fn fetch_latest_pydl_stable(client: &CachingClient) -> Result<(PydlRelease, Vec<Notice>)> {
    let url = format!("https://api.github.com/repos/{SELF_OWNER}/{SELF_REPO}/releases/latest");
    let (status, body, notices) = client.request(Method::GET, &url).await?;
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
    Ok((api.into_pydl_release(), notices))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(tag: &str) -> Release {
        Release {
            tag_name: tag.to_owned(),
            name: Some(tag.to_owned()),
            draft: false,
            prerelease: false,
            published_at: None,
            assets: vec![],
        }
    }

    /// Build a page-fetcher backed by pre-canned pages. Each inner `Vec` is
    /// one page of releases; page indices are 1-based.
    #[allow(clippy::type_complexity)]
    fn make_fetcher(
        pages: Vec<Vec<Release>>,
    ) -> impl Fn(usize) -> futures_util::future::Ready<Result<(Vec<Release>, Vec<Notice>)>> {
        move |page: usize| {
            let result = pages
                .get(page.saturating_sub(1))
                .cloned()
                .unwrap_or_default();
            futures_util::future::ready(Ok((result, vec![])))
        }
    }

    #[tokio::test]
    async fn incremental_already_up_to_date() {
        let existing = vec![rel("20260510"), rel("20260505")];
        let pages = vec![vec![rel("20260510"), rel("20260505")]];
        let fetcher = make_fetcher(pages);

        let result = fetch_incremental(&fetcher, &existing).await.unwrap();
        let (new, _) = result.expect("should return Some for up-to-date");
        assert!(new.is_empty());
    }

    #[tokio::test]
    async fn incremental_some_new_on_first_page() {
        let existing = vec![rel("20260510"), rel("20260505")];
        let pages = vec![vec![
            rel("20260515"),
            rel("20260512"),
            rel("20260510"),
            rel("20260505"),
        ]];
        let fetcher = make_fetcher(pages);

        let result = fetch_incremental(&fetcher, &existing).await.unwrap();
        let (new, _) = result.expect("should find new releases");
        assert_eq!(new.len(), 2);
        assert_eq!(new[0].tag_name, "20260515");
        assert_eq!(new[1].tag_name, "20260512");
    }

    #[tokio::test]
    async fn incremental_spans_two_pages() {
        let existing = vec![rel("20260505"), rel("20260501")];
        // Page 1: PER_PAGE items, all newer than the known tag.
        let page1: Vec<Release> = (0..PER_PAGE)
            .map(|i| rel(&format!("2026060{i:02}")))
            .collect();
        // Page 2: starts with one more new release, then the known tag.
        let page2 = vec![rel("20260520"), rel("20260505")];
        let fetcher = make_fetcher(vec![page1.clone(), page2]);

        let result = fetch_incremental(&fetcher, &existing).await.unwrap();
        let (new, _) = result.expect("should find new releases");
        assert_eq!(new.len(), PER_PAGE + 1);
        assert_eq!(new.last().unwrap().tag_name, "20260520");
    }

    #[tokio::test]
    async fn incremental_tag_not_found_falls_back() {
        let existing = vec![rel("20250101")];
        // Two partial pages that never contain the known tag.
        let pages = vec![vec![rel("20260515"), rel("20260510")]];
        let fetcher = make_fetcher(pages);

        let result = fetch_incremental(&fetcher, &existing).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn incremental_empty_existing_falls_back() {
        let existing: Vec<Release> = vec![];
        let fetcher = make_fetcher(vec![vec![rel("20260510")]]);

        let result = fetch_incremental(&fetcher, &existing).await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn fetch_all_collects_across_pages() {
        let page1: Vec<Release> = (0..PER_PAGE)
            .map(|i| rel(&format!("2026060{i:02}")))
            .collect();
        let page2 = vec![rel("20260505"), rel("20260501")];
        let expected_len = page1.len() + page2.len();
        let fetcher = make_fetcher(vec![page1, page2]);

        let (all, _) = fetch_all(&fetcher).await.unwrap();
        assert_eq!(all.len(), expected_len);
    }
}
