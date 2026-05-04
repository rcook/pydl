use anyhow::{Context, Result, bail};
use clap::Parser;
use log::{debug, info};
use pydl_cache::CachingClient;
use pydl_common::filter::{
    FilterArgs, apply_config_defaults, auto_select_tag_embedded, filter_embedded,
    pick_single_embedded,
};
use pydl_common::install::install_from_archive;
use pydl_common::{OWNER, REPO, cache_dir, min_freshness_secs};

#[derive(Parser, Debug)]
pub struct Args {
    #[command(flatten)]
    pub filter: FilterArgs,
}

// `args` by value matches the dispatch shape of every other subcommand.
#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> Result<()> {
    let mut filter = apply_config_defaults(args.filter)?;
    auto_select_tag_embedded(&mut filter)?;

    let hits = filter_embedded(&filter)?;
    let (tag, asset_name) = pick_single_embedded(&hits)?;

    let min_freshness = min_freshness_secs()?;
    debug!("cache min-freshness floor: {min_freshness}s");
    // `install` never hits the network, but we still need a `CachingClient`
    // to look up cache entries on disc. The client is constructed without
    // touching the wire.
    let client = CachingClient::with_user_agent(cache_dir()?, Some("pydl/0.1"))?
        .with_min_freshness_secs(min_freshness);

    let url = format!("https://github.com/{OWNER}/{REPO}/releases/download/{tag}/{asset_name}");
    let Some(archive_path) = client.cached_body_path(&url)? else {
        // If the user passed `-v`, include it in the hint — that's the
        // exact incantation they'll need to rerun `pydl download` with.
        // Otherwise (`-t` only), `-t` alone is enough.
        let hint = filter.version.as_deref().map_or_else(
            || format!("pydl download -t {tag}"),
            |v| format!("pydl download -t {tag} -v {v}"),
        );
        bail!("{asset_name} isn't in the cache — run `{hint}` first");
    };

    let installation = install_from_archive(&archive_path, tag, asset_name)
        .with_context(|| format!("installing {asset_name} from {}", archive_path.display()))?;
    if installation.already_present {
        info!(
            "{asset_name} is already installed at {}",
            installation.dir.display()
        );
    }
    // Print the install dir on stdout so callers can capture it:
    //   path=$(pydl install -t ... -v ...)
    // Logs go to stderr; this line is the "answer."
    println!("{}", installation.dir.display());
    Ok(())
}
