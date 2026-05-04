use std::ffi::OsString;
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Parser;
use log::debug;
use pydl_cache::CachingClient;
use pydl_common::filter::{
    FilterArgs, apply_config_defaults, auto_select_tag_embedded, filter_embedded,
    pick_single_embedded,
};
use pydl_common::install::{install_from_archive, python_binary};
use pydl_common::{OWNER, REPO, cache_dir, min_freshness_secs};

#[derive(Parser, Debug)]
#[command(
    // Any argument after the last known flag is treated as a Python arg,
    // including `--` so callers can pass it through to Python if needed.
    trailing_var_arg = true,
    allow_hyphen_values = true,
)]
pub struct Args {
    #[command(flatten)]
    pub filter: FilterArgs,

    /// Arguments forwarded verbatim to the installed `python` binary.
    #[arg(value_name = "PYTHON_ARGS")]
    pub python_args: Vec<OsString>,
}

// `args` by value matches the dispatch shape of every other subcommand.
#[allow(clippy::needless_pass_by_value)]
pub fn run(args: Args) -> Result<()> {
    let Args {
        filter,
        python_args,
    } = args;
    let mut filter = apply_config_defaults(filter)?;
    auto_select_tag_embedded(&mut filter)?;

    let hits = filter_embedded(&filter)?;
    let (tag, asset_name) = pick_single_embedded(&hits)?;

    let min_freshness = min_freshness_secs()?;
    debug!("cache min-freshness floor: {min_freshness}s");
    let client = CachingClient::with_user_agent(cache_dir()?, Some("pydl/0.1"))?
        .with_min_freshness_secs(min_freshness);

    let url = format!("https://github.com/{OWNER}/{REPO}/releases/download/{tag}/{asset_name}");
    let Some(archive_path) = client.cached_body_path(&url)? else {
        let hint = filter.version.as_deref().map_or_else(
            || format!("pydl download -t {tag}"),
            |v| format!("pydl download -t {tag} -v {v}"),
        );
        bail!("{asset_name} isn't in the cache — run `{hint}` first");
    };

    let installation = install_from_archive(&archive_path, tag, asset_name)
        .with_context(|| format!("installing {asset_name} from {}", archive_path.display()))?;
    if installation.already_present {
        // For `pydl python` the install directory is an implementation
        // detail — the user is here to run the interpreter, not to see
        // install-cache hit messages. `pydl install` still logs this at
        // info level since there "you are already installed" is the answer.
        debug!(
            "{asset_name} is already installed at {}",
            installation.dir.display()
        );
    }

    let python = python_binary(&installation.dir);
    if !python.exists() {
        bail!(
            "expected python binary at {} but it is missing",
            python.display()
        );
    }

    debug!(
        "exec {} with {} arg(s)",
        python.display(),
        python_args.len()
    );
    let status = Command::new(&python)
        .args(&python_args)
        .status()
        .with_context(|| format!("spawning {}", python.display()))?;

    // Propagate the child's exit status. On Unix we also want to handle
    // signals, but `ExitStatus::code()` returning `None` is rare enough in
    // this context that mapping to 1 is fine.
    let code = status.code().unwrap_or(1);
    std::process::exit(code);
}
