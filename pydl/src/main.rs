mod cmd;
pub mod progress;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::progress::ProgressMode;

/// HTTP `User-Agent` for outbound requests from `pydl`. Subcommands that mint
/// their own UA (e.g. `self-update`, which appends a suffix) build on this.
pub const USER_AGENT: &str = concat!("pydl/", env!("CARGO_PKG_VERSION"));

/// Multi-line `--version` output. The build script (`pydl/build.rs`) populates
/// the four `PYDL_BUILD_*` env vars at compile time; `CARGO_PKG_VERSION` comes
/// straight from `Cargo.toml`. `PYDL_BUILD_SOURCE` is `"official"` only when
/// `release.yaml` sets `PYDL_RELEASE_BUILD=1`; everything else is `"local"`.
const VERSION_STRING: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("PYDL_BUILD_PROFILE"),
    " build, ",
    env!("PYDL_BUILD_SOURCE"),
    ")\n",
    "commit: ",
    env!("PYDL_BUILD_COMMIT"),
    "\n",
    "built:  ",
    env!("PYDL_BUILD_TIMESTAMP"),
    "\n",
    "target: ",
    env!("PYDL_BUILD_TARGET"),
);

#[derive(Parser, Debug)]
#[allow(clippy::struct_excessive_bools)]
#[command(
    name = "pydl",
    version = VERSION_STRING,
    about = "Download, install and run python-build-standalone distributions.",
    long_about = "Fetch, verify, install and run Python distributions from the \
                  astral-sh/python-build-standalone release set.\n\n\
                  Network model: `pydl update` refreshes a local snapshot of the \
                  upstream releases list and the latest pydl version. `pydl available` \
                  reads from that snapshot and never touches the network; `pydl self-update` \
                  reads from that snapshot too but still downloads the binary itself \
                  over the network (pass `--online` to bypass the snapshot for the \
                  version check as well). Only `pydl update` and `pydl self-update --online` \
                  contact `api.github.com` for *release listings*; `pydl download` and \
                  `pydl self-update` reach out for asset bytes. Every other command is \
                  guaranteed offline. Run `pydl update` periodically to stay current.\n\n\
                  Lifecycle: `pydl available` shows what upstream publishes; \
                  `pydl install`/`download` resolve a single asset through filter \
                  flags (`--tag`, `--version`, `--platform`, `--default-attrs`) and \
                  verify it against SHA-256 checksums embedded in this binary at \
                  build time; `pydl installed` lists what's on disc and \
                  `pydl uninstall` removes one. `pydl python` invokes the installed \
                  interpreter, and `pydl pin` freezes a filter set into a \
                  `.pydl.json` pin that sibling subcommands will pick up \
                  automatically. Responses are served from a disc cache at \
                  `$HOME/.pydl/cache/`; the snapshot lives at `$HOME/.pydl/snapshot/`; \
                  installs live under `$HOME/.pydl/asset/`."
)]
pub struct Cli {
    /// Run as if pydl was started in <DIR> instead of the current working
    /// directory. Affects config discovery, pin output and resolution of
    /// relative paths in other flags.
    #[arg(short = 'C', global = true, value_name = "DIR")]
    directory: Option<PathBuf>,

    /// Log filter directive (overrides `RUST_LOG`). Accepts the same syntax
    /// as `RUST_LOG`, e.g. `debug`, `pydl=trace,warn`, `pydl_cache=debug`.
    #[arg(short = 'l', long = "log", global = true, value_name = "DIRECTIVE")]
    log: Option<String>,

    /// Prefix log lines with a timestamp. Disabled by default; pass
    /// `--log-timestamps` to opt in (useful when capturing logs to a file).
    #[arg(long, global = true, overrides_with = "no_log_timestamps")]
    log_timestamps: bool,

    /// Suppress log line timestamps. Inverse of `--log-timestamps`; matches
    /// the default and exists so an explicit `--log-timestamps` can be
    /// overridden later in the same command line.
    #[arg(
        long = "no-log-timestamps",
        global = true,
        overrides_with = "log_timestamps"
    )]
    no_log_timestamps: bool,

    /// Show progress indicators during network operations. Default: auto-detect
    /// based on whether stderr is a terminal.
    #[arg(long, global = true, overrides_with = "no_progress")]
    progress: bool,

    /// Suppress progress indicators unconditionally.
    #[arg(long = "no-progress", global = true, overrides_with = "progress")]
    no_progress: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// [network] Refresh local snapshots of upstream releases (Python releases list + latest pydl). Incremental by default — fetches only new releases when a snapshot exists; pass `--full` to re-fetch everything. The only command that fetches release listings; everything else either runs from the snapshot or hits the network only for asset bytes (`download`, `self-update`).
    Update(cmd::update::Args),

    /// [offline] List releases and their assets from the local snapshot. Run `pydl update` first.
    Available(cmd::available::Args),

    /// [network] Fetch a single asset into ~/.pydl/cache/ (and optionally copy it to --output-dir).
    Download(cmd::download::Args),

    /// [offline] Verify and unpack a previously-downloaded asset into ~/.pydl/asset/<hash>/.
    Install(cmd::install::Args),

    /// [offline] List every installed asset and its directory.
    Installed(cmd::installed::Args),

    /// [offline] Remove an installed asset directory. Requires `--yes` to actually delete.
    Uninstall(cmd::uninstall::Args),

    /// [offline] Install (or reuse) a single asset and run its bundled python binary.
    Python(cmd::python::Args),

    /// [offline] Write the current filter flags to a `.pydl.json` in the working directory.
    Pin(cmd::pin::Args),

    /// [offline] Report the pin and asset state for the current working directory.
    Status(cmd::status::Args),

    /// [offline] Inspect or clear the on-disc HTTP cache at ~/.pydl/cache/.
    Cache(cmd::cache::Args),

    /// [offline] Emit a shell-completion script for the given shell to stdout.
    Completions(cmd::completions::Args),

    /// [network] Self-replace the running binary with the latest released `pydl`. Reads the new version from the snapshot written by `pydl update` by default; `--online` bypasses the snapshot. The binary itself is always downloaded over the network.
    SelfUpdate(cmd::self_update::Args),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Build from env first so that `RUST_LOG_STYLE` and other env-driven
    // `env_logger` knobs are honoured. `--log` then overrides the filter
    // directive on top of that, matching the CLI doc's promise that the flag
    // overrides `RUST_LOG` rather than wiping the slate.
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if let Some(directive) = cli.log.as_deref() {
        builder.parse_filters(directive);
    }
    if !cli.log_timestamps {
        builder.format_timestamp(None);
    }
    builder.init();
    if let Some(dir) = &cli.directory {
        std::env::set_current_dir(dir)
            .with_context(|| format!("changing to directory {}", dir.display()))?;
    }
    let progress_mode = ProgressMode::from_flags(cli.progress, cli.no_progress);
    match cli.cmd {
        Cmd::Update(args) => cmd::update::run(args, progress_mode).await,
        Cmd::Available(args) => cmd::available::run(args),
        Cmd::Download(args) => cmd::download::run(args, progress_mode).await,
        Cmd::Install(args) => cmd::install::run(args),
        Cmd::Installed(args) => cmd::installed::run(args),
        Cmd::Uninstall(args) => cmd::uninstall::run(args),
        Cmd::Python(args) => cmd::python::run(args),
        Cmd::Pin(args) => cmd::pin::run(args),
        Cmd::Status(args) => cmd::status::run(args),
        Cmd::Cache(args) => cmd::cache::run(args),
        Cmd::Completions(args) => cmd::completions::run(args),
        Cmd::SelfUpdate(args) => cmd::self_update::run(args, progress_mode).await,
    }
}
