mod cmd;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "pydl",
    version,
    about = "Download, install and run python-build-standalone distributions.",
    long_about = "Fetch, verify, install and run Python distributions from the \
                  astral-sh/python-build-standalone release set.\n\n\
                  Lifecycle: `pydl available` shows what upstream publishes; \
                  `pydl install`/`download` resolve a single asset through filter \
                  flags (`--tag`, `--version`, `--platform`, `--default-attrs`) and \
                  verify it against SHA-256 checksums embedded in this binary at \
                  build time; `pydl installed` lists what's on disc and \
                  `pydl uninstall` removes one. `pydl python` invokes the installed \
                  interpreter, and `pydl pin` freezes a filter set into a \
                  `.pydl.json` pin that sibling subcommands will pick up \
                  automatically. Responses are served from a disc cache at \
                  `$HOME/.pydl/cache/`; installs live under `$HOME/.pydl/asset/`."
)]
pub struct Cli {
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

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// [network] List releases and their assets that are available upstream.
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

    /// [offline] Inspect or clear the on-disc HTTP cache at ~/.pydl/cache/.
    Cache(cmd::cache::Args),

    /// [offline] Emit a shell-completion script for the given shell to stdout.
    Completions(cmd::completions::Args),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut builder = cli.log.as_deref().map_or_else(
        || env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")),
        |directive| {
            let mut b = env_logger::Builder::new();
            b.parse_filters(directive);
            b
        },
    );
    if !cli.log_timestamps {
        builder.format_timestamp(None);
    }
    builder.init();
    match cli.cmd {
        Cmd::Available(args) => cmd::available::run(args).await,
        Cmd::Download(args) => cmd::download::run(args).await,
        Cmd::Install(args) => cmd::install::run(args),
        Cmd::Installed(args) => cmd::installed::run(args),
        Cmd::Uninstall(args) => cmd::uninstall::run(args),
        Cmd::Python(args) => cmd::python::run(args),
        Cmd::Pin(args) => cmd::pin::run(args),
        Cmd::Cache(args) => cmd::cache::run(args),
        Cmd::Completions(args) => cmd::completions::run(args),
    }
}
