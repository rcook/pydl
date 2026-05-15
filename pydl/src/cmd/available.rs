use std::cmp::Ordering;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use log::warn;
use owo_colors::Stream::Stdout;
use owo_colors::{OwoColorize, Style};
use pydl_cache::CachingClient;
use pydl_common::asset::asset_sort_key;
use pydl_common::filter::{Asset, FilterArgs, Release, filter_releases};
use pydl_common::format::humanize_bytes;
use pydl_common::{OWNER, REPO, cache_dir, checksums, install, snapshot};
use semver::Version;

#[derive(Parser, Debug)]
pub struct Args {
    #[command(flatten)]
    pub filter: FilterArgs,

    /// Show a compact one-line overview instead of the per-asset listing.
    #[arg(long, conflicts_with = "pydl")]
    pub summary: bool,

    /// Show only the latest-pydl-version line. Mutually exclusive with
    /// `--summary` and with any filter flag (the filter flags only narrow
    /// the Python section, which `--pydl` suppresses).
    #[arg(
        long,
        conflicts_with_all = [
            "summary", "tag", "version",
            "platform", "no_platform",
            "default_attrs", "no_default_attrs",
            "all_tags",
        ],
    )]
    pub pydl: bool,

    /// Show all release tags. Without this flag only the newest tag is shown.
    #[arg(long)]
    pub all_tags: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AssetStatus {
    Installed,
    Cached,
    NoChecksum,
    Available,
}

fn asset_status(
    tag: &str,
    asset_name: &str,
    client: Option<&CachingClient>,
    install_root: Option<&Path>,
) -> AssetStatus {
    if let Some(root) = install_root {
        let hash = install::asset_hash(asset_name);
        if root.join(&hash).exists() {
            return AssetStatus::Installed;
        }
    }

    if checksums::expected_hash(tag, asset_name).is_err() {
        return AssetStatus::NoChecksum;
    }

    if let Some(c) = client {
        let url = format!("https://github.com/{OWNER}/{REPO}/releases/download/{tag}/{asset_name}");
        if c.cached_body_path(&url).ok().flatten().is_some() {
            return AssetStatus::Cached;
        }
    }

    AssetStatus::Available
}

fn print_detailed(
    groups: &[(&Release, Vec<&Asset>)],
    client: Option<&CachingClient>,
    install_root: Option<&Path>,
) -> bool {
    if groups.is_empty() {
        println!("(no assets matched the filter)");
        return false;
    }
    let mut saw_no_checksum = false;
    for (release, assets) in groups {
        let tag_header = format!("{}:", release.tag_name);
        let tag_style = Style::new().bold().blue();
        println!(
            "{}",
            tag_header.if_supports_color(Stdout, |t| t.style(tag_style))
        );
        let mut sorted: Vec<&Asset> = assets
            .iter()
            .filter(|a| !a.name.ends_with(".sha256"))
            .copied()
            .collect();
        sorted.sort_by_cached_key(|a| asset_sort_key(&a.name));
        for asset in sorted {
            let status = asset_status(&release.tag_name, &asset.name, client, install_root);
            if status == AssetStatus::NoChecksum {
                saw_no_checksum = true;
            }
            let size = humanize_bytes(asset.size);
            let size_str = format!("({size})");
            let dim_size = size_str.if_supports_color(Stdout, |t| t.dimmed());
            match status {
                AssetStatus::Installed => {
                    let marker = "[installed]".if_supports_color(Stdout, |t| t.green());
                    println!("  {} {dim_size} {marker}", asset.name);
                }
                AssetStatus::Cached => {
                    let marker = "[cached]".if_supports_color(Stdout, |t| t.cyan());
                    println!("  {} {dim_size} {marker}", asset.name);
                }
                AssetStatus::NoChecksum => {
                    let marker = "[checksum unavailable]".if_supports_color(Stdout, |t| t.yellow());
                    println!("  {} {dim_size} {marker}", asset.name);
                }
                AssetStatus::Available => {
                    println!("  {} {dim_size}", asset.name);
                }
            }
        }
    }
    saw_no_checksum
}

#[allow(clippy::cast_precision_loss)]
// `args` by value matches the dispatch shape of every other subcommand.
#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> Result<()> {
    let resolved = args.filter.resolve();
    let any_explicit_filter = args.filter.tag.is_some()
        || args.filter.version.is_some()
        || args.filter.platform
        || args.filter.no_platform
        || args.filter.default_attrs
        || args.filter.no_default_attrs;
    let show_python = !args.pydl;
    let show_pydl = !args.summary && !any_explicit_filter;

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

    let staleness_basis = match (&pbs_env, &pydl_env) {
        (Some(p), Some(d)) => Some(p.fetched_at.min(d.fetched_at)),
        (Some(p), None) => Some(p.fetched_at),
        (None, Some(d)) => Some(d.fetched_at),
        (None, None) => None,
    };
    if let Some(fetched_at) = staleness_basis {
        println!(
            "{}",
            snapshot::staleness_report(fetched_at).if_supports_color(Stdout, |t| t.dimmed())
        );
    }

    if let Some(env) = pydl_env {
        emit_pydl_line(&env.payload);
    }

    if let Some(env) = pbs_env {
        let releases = &env.payload;
        if args.summary && !any_explicit_filter {
            println!(
                "{}",
                snapshot::format_python_releases_short_summary(releases)
            );
            println!("(pass -t/-v to filter or omit --summary for per-asset detail)");
        } else {
            let groups = filter_releases(releases, resolved)?;
            let show_all = args.all_tags || args.filter.tag.is_some();
            let (visible, hidden_count) = if show_all || groups.len() <= 1 {
                (groups.as_slice(), 0)
            } else {
                (&groups[..1], groups.len() - 1)
            };
            let client = cache_dir().ok().and_then(|d| CachingClient::new(d).ok());
            let install_root = install::install_root().ok();
            let saw_no_checksum = print_detailed(visible, client.as_ref(), install_root.as_deref());
            if saw_no_checksum {
                println!();
                let note = "note: some assets are marked [checksum unavailable] because this \
                            build of pydl\ndoes not include checksums for their release. \
                            Run `pydl self-update` to install a newer\nversion that may include them.";
                println!("{}", note.if_supports_color(Stdout, |t| t.yellow()));
            }
            if hidden_count > 0 {
                let noun = if hidden_count == 1 { "tag" } else { "tags" };
                let hint = format!(
                    "({hidden_count} older {noun} not shown \u{2014} pass --all-tags to list all)"
                );
                println!("{}", hint.if_supports_color(Stdout, |t| t.dimmed()));
            }
        }
    }

    Ok(())
}

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
    let line = snapshot::format_pydl_version_line(&running, &latest);
    match running.cmp(&latest) {
        Ordering::Less => {
            println!("{}", line.if_supports_color(Stdout, |t| t.yellow()));
        }
        _ => {
            println!("{}", line.if_supports_color(Stdout, |t| t.green()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_status_no_checksum() {
        let status = asset_status("nonexistent_tag", "fake_asset.tar.gz", None, None);
        assert_eq!(status, AssetStatus::NoChecksum);
    }

    #[test]
    fn asset_status_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let hash = install::asset_hash("fake_asset.tar.gz");
        std::fs::create_dir(tmp.path().join(&hash)).unwrap();
        let status = asset_status(
            "nonexistent_tag",
            "fake_asset.tar.gz",
            None,
            Some(tmp.path()),
        );
        assert_eq!(status, AssetStatus::Installed);
    }
}
