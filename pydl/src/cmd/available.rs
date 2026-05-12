use anyhow::{Result, anyhow};
use clap::Parser;
use log::info;
use pydl_common::asset::asset_sort_key;
use pydl_common::filter::{Asset, FilterArgs, Release, filter_releases};
use pydl_common::snapshot;

#[derive(Parser, Debug)]
pub struct Args {
    #[command(flatten)]
    pub filter: FilterArgs,
}

fn print_summary(all_releases: &[Release]) {
    let total = all_releases.len();
    let drafts = all_releases.iter().filter(|r| r.draft).count();
    let prereleases = all_releases.iter().filter(|r| r.prerelease).count();
    let total_assets: usize = all_releases.iter().map(|r| r.assets.len()).sum();
    let total_asset_bytes: u64 = all_releases
        .iter()
        .flat_map(|r| r.assets.iter().map(|a| a.size))
        .sum();

    info!("snapshot has {total} release(s)");
    info!("  drafts: {drafts}, prereleases: {prereleases}");
    info!("  assets: {total_assets}, total size: {total_asset_bytes} bytes");

    if let Some(latest) = all_releases.first() {
        info!(
            "most recent release: tag={}, name={:?}, published_at={:?}, assets={}",
            latest.tag_name,
            latest.name,
            latest.published_at,
            latest.assets.len()
        );
        // Sort so the "first 5" is deterministic across runs regardless of the
        // order GitHub returned assets in.
        let mut sorted: Vec<&Asset> = latest.assets.iter().collect();
        sorted.sort_by_cached_key(|a| asset_sort_key(&a.name));
        for asset in sorted.iter().take(5) {
            info!("  asset: {} ({} bytes)", asset.name, asset.size);
        }
        if sorted.len() > 5 {
            info!("  ...and {} more", sorted.len() - 5);
        }
    }
}

fn print_filtered(groups: &[(&Release, Vec<&Asset>)]) {
    if groups.is_empty() {
        info!("(no assets matched the filter)");
        return;
    }
    for (release, assets) in groups {
        info!(
            "release: tag={}, name={:?}, published_at={:?}, draft={}, prerelease={}, assets={}",
            release.tag_name,
            release.name,
            release.published_at,
            release.draft,
            release.prerelease,
            assets.len()
        );
        let mut sorted: Vec<&Asset> = assets.clone();
        sorted.sort_by_cached_key(|a| asset_sort_key(&a.name));
        for asset in sorted {
            info!("  asset: {} ({} bytes)", asset.name, asset.size);
        }
    }
}

// `args` by value matches the dispatch shape of every other subcommand.
#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> Result<()> {
    let envelope = snapshot::read_pbs_releases()?.ok_or_else(|| {
        let p = snapshot::pbs_releases_path()
            .map_or_else(|_| "<snapshot path unavailable>".to_owned(), |p| p.display().to_string());
        anyhow!("no PBS releases snapshot found at {p}. Run `pydl update` to fetch one.")
    })?;
    info!("{}", snapshot::staleness_report(envelope.fetched_at));
    let releases = &envelope.payload;

    let resolved = args.filter.resolve();
    if args.filter.any_asset_filter(&resolved) {
        let groups = filter_releases(releases, resolved)?;
        print_filtered(&groups);
    } else {
        print_summary(releases);
    }

    Ok(())
}
