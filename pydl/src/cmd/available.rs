use anyhow::{Context, Result, anyhow};
use clap::Parser;
use log::{info, warn};
use pydl_common::asset::asset_sort_key;
use pydl_common::filter::{Asset, FilterArgs, Release, filter_releases};
use pydl_common::snapshot;
use semver::Version;

#[derive(Parser, Debug)]
pub struct Args {
    #[command(flatten)]
    pub filter: FilterArgs,

    /// Show only the Python releases section. Implied when any filter flag
    /// (`-t`, `-v`, `--platform`, `--default-attrs`) is set. Mutually
    /// exclusive with `--pydl`.
    #[arg(long, conflicts_with = "pydl")]
    pub python: bool,

    /// Show only the latest-pydl-version line. Mutually exclusive with
    /// `--python` and with any filter flag (the filter flags only narrow
    /// the Python section, which `--pydl` suppresses).
    #[arg(
        long,
        conflicts_with_all = [
            "python", "tag", "version",
            "platform", "no_platform",
            "default_attrs", "no_default_attrs",
        ],
    )]
    pub pydl: bool,
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
    let resolved = args.filter.resolve();
    // `any_explicit_filter` is true only if the user passed at least one
    // filter flag on the command line. We can't use
    // `FilterArgs::any_asset_filter` here because that returns true on a
    // bare `pydl available` invocation (the default `--platform` filter is
    // already on). For routing the new `--pydl` / `--python` semantics we
    // need to know what the user *typed*, not what defaults resolve to.
    let any_explicit_filter = args.filter.tag.is_some()
        || args.filter.version.is_some()
        || args.filter.platform
        || args.filter.no_platform
        || args.filter.default_attrs
        || args.filter.no_default_attrs;
    // `--python` is implied (rather than required) when an explicit filter
    // is set — filters only ever narrow the Python section.
    let show_python = !args.pydl;
    let show_pydl = !args.python && !any_explicit_filter;

    // Read whichever snapshots we'll actually need. A missing required
    // snapshot is the only fatal case; an unrelated missing snapshot is
    // silently skipped.
    let pbs_env = if show_python {
        Some(snapshot::read_pbs_releases()?.ok_or_else(|| {
            let p = snapshot::pbs_releases_path().map_or_else(
                |_| "<snapshot path unavailable>".to_owned(),
                |p| p.display().to_string(),
            );
            anyhow!("no Python releases snapshot found at {p}. Run `pydl update` to fetch one.")
        })?)
    } else {
        None
    };
    let pydl_env = if show_pydl {
        Some(snapshot::read_pydl_latest()?.ok_or_else(|| {
            let p = snapshot::pydl_latest_path().map_or_else(
                |_| "<snapshot path unavailable>".to_owned(),
                |p| p.display().to_string(),
            );
            anyhow!("no pydl version snapshot found at {p}. Run `pydl update` to fetch one.")
        })?)
    } else {
        None
    };

    // Print the staleness line once. Prefer the older snapshot's age when
    // both are available — conservative: a refresh-suggestion fires as
    // soon as either half is stale.
    let staleness_basis = match (&pbs_env, &pydl_env) {
        (Some(p), Some(d)) => Some(p.fetched_at.min(d.fetched_at)),
        (Some(p), None) => Some(p.fetched_at),
        (None, Some(d)) => Some(d.fetched_at),
        (None, None) => None,
    };
    if let Some(fetched_at) = staleness_basis {
        info!("{}", snapshot::staleness_report(fetched_at));
    }

    if let Some(env) = pydl_env {
        emit_pydl_line(&env.payload);
    }

    if let Some(env) = pbs_env {
        let releases = &env.payload;
        if any_explicit_filter {
            // Filters always run through the resolved (post-default) filter
            // set, including the implicit platform filter.
            let groups = filter_releases(releases, resolved)?;
            print_filtered(&groups);
        } else if args.python {
            // Explicit `--python` with no filter: full detailed listing.
            print_summary(releases);
        } else {
            // Default mode (no filter, no scope flag): one-line summary so
            // both sections fit on three lines.
            info!(
                "{}",
                snapshot::format_python_releases_short_summary(releases)
            );
        }
    }

    Ok(())
}

/// Print the one-line pydl version summary. A non-semver running version
/// or latest tag downgrades to a `warn!` and skips the line — never fatal.
fn emit_pydl_line(release: &pydl_common::snapshot::PydlRelease) {
    let running_str = env!("CARGO_PKG_VERSION");
    let Ok(running) = Version::parse(running_str)
        .with_context(|| format!("parsing running version {running_str:?} as semver"))
    else {
        warn!("could not parse running version {running_str:?} as semver; skipping pydl line");
        return;
    };
    let latest_str = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    let Ok(latest) = Version::parse(latest_str).with_context(|| {
        format!(
            "parsing snapshot latest tag {:?} as semver",
            release.tag_name
        )
    }) else {
        warn!(
            "could not parse snapshot latest tag {:?} as semver; skipping pydl line",
            release.tag_name
        );
        return;
    };
    info!("{}", snapshot::format_pydl_version_line(&running, &latest));
}
