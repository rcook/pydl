use anyhow::{Context, Result};
use clap::Parser;
use pydl_common::config::{find_config, load_config};
use pydl_common::filter::{
    FilterArgs, auto_select_tag_embedded, filter_embedded, pick_single_embedded,
};
use pydl_common::{OWNER, REPO, checksums, install, make_client, min_freshness_secs};

#[derive(Parser, Debug)]
pub struct Args {}

#[allow(clippy::needless_pass_by_value)]
pub fn run(_args: Args) -> Result<()> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let Some(config_path) = find_config(&cwd)? else {
        println!("no .pydl.json found");
        return Ok(());
    };

    let cfg = load_config(&config_path)?;
    println!("pin:       {}", config_path.display());
    println!("version:   {}", cfg.version);

    let tag_was_pinned = cfg.tag.is_some();
    let bare = FilterArgs {
        tag: None,
        version: None,
        platform: false,
        no_platform: false,
        default_attrs: false,
        no_default_attrs: false,
    };
    let mut filter = bare.with_defaults_from(&cfg);

    if let Err(e) = auto_select_tag_embedded(&mut filter) {
        println!("tag:       (resolution failed)");
        println!("status:    {e:#}");
        return Ok(());
    }

    let tag = filter.tag.as_deref().unwrap_or("(unknown)");
    if tag_was_pinned {
        println!("tag:       {tag}");
    } else {
        println!("tag:       {tag} (auto-selected)");
    }

    let hits = match filter_embedded(&filter) {
        Ok(h) => h,
        Err(e) => {
            println!("status:    {e:#}");
            return Ok(());
        }
    };
    let (tag, asset_name) = match pick_single_embedded(&hits) {
        Ok(pair) => pair,
        Err(e) => {
            println!("status:    {e:#}");
            return Ok(());
        }
    };

    println!("asset:     {asset_name}");

    let install_dir = install::install_root()?.join(install::asset_hash(asset_name));
    if install_dir.exists() {
        println!("status:    installed");
        return Ok(());
    }

    if checksums::expected_hash(tag, asset_name).is_err() {
        println!(
            "status:    checksum unavailable \u{2014} run `pydl self-update` for a newer build"
        );
        return Ok(());
    }

    let url = format!("https://github.com/{OWNER}/{REPO}/releases/download/{tag}/{asset_name}");
    let min_freshness = min_freshness_secs()?;
    let client = make_client(crate::USER_AGENT, min_freshness)?;
    if client.cached_body_path(&url)?.is_some() {
        println!("status:    cached \u{2014} run `pydl install` to unpack");
        return Ok(());
    }

    println!("status:    not downloaded \u{2014} run `pydl download` to fetch");
    Ok(())
}
