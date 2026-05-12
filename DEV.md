# Developer Guide

This document describes the workflow for working on the `pydl` workspace: layout, how the crates fit together, how to build and test, how to run the lints and formatter, what the CI-style verification step looks like and the periodic maintenance tasks.

User-facing documentation lives in [`pydl/README.md`](./pydl/README.md). The design rationale for `pydl`'s verification model lives in [`DESIGN.md`](./DESIGN.md).

## Prerequisites

- A recent stable Rust toolchain (the workspace uses edition 2024 — Rust 1.85 or newer). `rust-toolchain.toml` pins the stable channel and pulls in `clippy` and `rustfmt` automatically the first time you run `cargo`.
- `rustfmt` on **nightly** in addition to the pinned stable toolchain (the import-grouping options in `rustfmt.toml` are nightly-only, so `./dev.sh fmt` and `fmt-check` invoke `cargo +nightly fmt`):
  ```
  rustup toolchain install nightly --profile minimal --component rustfmt
  ```
- A Bash-compatible shell for `./dev.sh` (macOS / Linux). On Windows, run the underlying `cargo` commands directly (see below).

## Workspace layout

```
Cargo.toml            workspace manifest + shared deps + lint table + shared version
rust-toolchain.toml   pinned stable toolchain (with clippy + rustfmt)
rustfmt.toml          nightly rustfmt settings (group_imports, imports_granularity)
cliff.toml            git-cliff configuration for CHANGELOG.md generation
README.md             user-facing entry point
DEV.md                this file
DESIGN.md             design rationale for the checksum-verification model
MAINTENANCE.md        recurring maintenance prompts
TODO.md               maintainer backlog (small, dated items)
ISSUES.md             rolling triage of bugs, smells and resolved investigations
dev.sh                developer entry point (see below)
checksums/            committed <tag>.sha256sums files, embedded at build time
pydl/                 the user-facing binary
pydl-cache/           the pull-through HTTP cache library
pydl-common/          shared helpers (internal; no README)
get-checksums/        project-maintenance SHA256SUMS mirror binary
check-checksums/      CI trip-wire that diffs ./checksums/ against upstream
```

Every crate inherits dependency versions, clippy lints, edition and version from the workspace root via `workspace = true` / `version.workspace = true`. The shared version in `[workspace.package]` is the source of truth for release tags (`v<version>`). Each user-facing crate has its own `README.md`; `pydl-cache` additionally has a `DEV.md` with cache-library-specific notes. `pydl-common` is deliberately README-less — it's an internal helper crate and its module-level doc comments are the authoritative reference.

Two crates carry a `build.rs`: `pydl-common` embeds the SHA-256 checksum table (see [Refreshing the embedded checksum set](#refreshing-the-embedded-checksum-set)); `pydl` re-exports several compile-time facts that surface in `pydl --version` and drive `pydl self-update`:

- `PYDL_BUILD_TARGET` — cargo's `TARGET`, so `self-update` knows which release artifact to fetch.
- `PYDL_BUILD_PROFILE` — cargo's `PROFILE` (`debug` / `release`).
- `PYDL_BUILD_SOURCE` — `"official"` when `PYDL_RELEASE_BUILD=1` is set (only by `release.yaml`); `"local"` otherwise.
- `PYDL_BUILD_COMMIT` — short git SHA, with a `-dirty` suffix when the working tree is uncommitted; `unknown` outside a checkout.
- `PYDL_BUILD_TIMESTAMP` — HTTP-date string. Honours `SOURCE_DATE_EPOCH` for reproducible builds.

## Architecture at a glance

```
user invocation
      │
      ▼
   pydl (main.rs + cmd/*.rs)            ← subcommand dispatch
      │
      ▼
   pydl-common::filter                  ← resolve flag bundle → (tag, asset_name)
      │   (runs against the embedded
      │    checksum table for offline
      │    subcommands)
      │
      ▼ (network-facing path used by    pydl-common::snapshot     ← ~/.pydl/snapshot/
        `update` and `download`;        (written by `update`,
        `update` also writes the         read by `available` /
        snapshot consumed by             `self-update`)
        `available` / `self-update`)
   pydl-cache::CachingClient            ← HTTP cache at ~/.pydl/cache/
      │   (populated by `update` /
      │    `download` / `self-update`;
      │    read by `install` / `python`
      │    via cached_body_path)
      ▼
   pydl-common::install                 ← verify SHA-256, unpack into ~/.pydl/asset/<hash>/
```

Key invariants:

- **Network usage is a subcommand-level property.** Only `update`, `download` and `self-update` (always for the binary download; for the version check only with `--online`) construct network-facing paths. All other subcommands enforce this by construction. Filter resolution against the embedded checksum table — and release-list resolution against the local snapshot — is what makes this possible.
- **The snapshot consolidates release-listing traffic.** `pydl update` paginates `astral-sh/python-build-standalone` releases and fetches `releases/latest` for `rcook/pydl`, then writes both as JSON under `~/.pydl/snapshot/`. `pydl available` and the default `pydl self-update` never touch the network for these listings; they read the snapshot.
- **The cache is the handoff point for asset bytes.** `download` warms it, `install`/`python` read from it via `cached_body_path`. `update` also flows through the cache, but its product is the snapshot file, not a cached body.
- **The embedded checksum table is the canonical "what exists" index** for offline asset resolution. Asset names encode version, triple, flavour — enough for `filter_embedded` to resolve any `FilterArgs` without the network and without the snapshot.

## Self-update

`pydl self-update` is the one subcommand that operates on the `pydl` binary itself rather than on python-build-standalone assets. By default it reads the latest version from the snapshot written by `pydl update` (under `~/.pydl/snapshot/pydl-latest.json`); pass `--online` to bypass the snapshot and hit `releases/latest` for [`rcook/pydl`](https://github.com/rcook/pydl) directly (or the paged `releases?per_page=10` list when `--pre` is set, which requires `--online`). It compares the running `CARGO_PKG_VERSION` against the latest tag via [`semver`](https://crates.io/crates/semver), downloads the matching archive through the existing `pydl-cache::CachingClient`, extracts the single `pydl` / `pydl.exe` binary and atomically swaps the running executable via [`self_replace`](https://crates.io/crates/self-replace).

A few mechanics worth knowing when modifying this code:

- **Target detection.** `pydl/build.rs` re-exports cargo's `TARGET` environment variable as `PYDL_BUILD_TARGET` so `cmd/self_update.rs` can read it via `env!("PYDL_BUILD_TARGET")` at compile time. Runtime `std::env::consts::{OS, ARCH}` would not distinguish, e.g., `x86_64-unknown-linux-gnu` from `x86_64-unknown-linux-musl`.
- **Asset naming.** Release artifacts are named `pydl-${GITHUB_REF_NAME}-${target}.{tar.gz|zip}` (see `.github/workflows/release.yaml`). The leading `v` from the tag is preserved in the asset name; `cmd/self_update.rs` matches on the tag string verbatim. Changing the tagging policy or the release packaging filename will silently break self-update.
- **Verification.** `release.yaml` writes a `SHA256SUMS` manifest alongside the per-target archives. `self-update` fetches it and hashes the downloaded archive against the matching entry before extracting. A missing manifest currently warns and proceeds (so a binary built before manifest publishing can still update through a manifest-publishing release); pass `--require-checksum` to make a missing manifest a hard error. The default flips to strict in a future release once two consecutive manifest-publishing releases have shipped — see the rollout note in `pydl/src/cmd/self_update.rs`.
- **Manifest format.** Plain `sha256sum`-style: `<hex>  <filename>` per line. Generated in the `publish` job with `sha256sum pydl-* > SHA256SUMS`. Filenames only — no path prefix — so the verifier consumes lines verbatim.
- **No other release-pipeline coupling.** `release.yaml` publishes everything `self-update` consumes; no changes are required there to support a new pydl release once the manifest step is in place.

When cutting a release that bumps pydl's version, end-to-end testing is a useful smoke check: temporarily roll workspace `Cargo.toml` back one patch version, `cargo build -p pydl`, run the resulting `target/debug/pydl self-update`, then verify `target/debug/pydl --version` reports the new version. The output is multi-line: a `pydl X.Y.Z (<profile> build, <source>)` header followed by `commit:`, `built:`, `target:` lines. Revert the Cargo.toml edit afterwards.

## `./dev.sh` / `./dev.ps1`

A thin wrapper over the usual `cargo` invocations. Run `./dev.sh help` for the current command list. On Windows use the PowerShell port: `./dev.ps1 help` exposes the same commands and the same arguments — substitute `./dev.ps1` for `./dev.sh` in any example below. The PowerShell version expects PowerShell 7 (`pwsh`); Windows PowerShell 5.1 also works for everything except some edge-case argument quoting.

| Command                            | What it does                                                     |
|------------------------------------|------------------------------------------------------------------|
| `./dev.sh build`                   | `cargo build --workspace --all-targets`                          |
| `./dev.sh test`                    | `cargo test --workspace --all-targets`                           |
| `./dev.sh lint`                    | `cargo clippy --workspace --all-targets -- -D warnings`          |
| `./dev.sh fmt`                     | `cargo +nightly fmt --all` (rewrites files in place)             |
| `./dev.sh fmt-check`               | `cargo +nightly fmt --all -- --check`                            |
| `./dev.sh check`                   | `fmt-check` + `lint` + `test` — the CI-equivalent gate           |
| `./dev.sh pydl <args>`             | `cargo run -p pydl --quiet -- <args>`                            |
| `./dev.sh get-checksums <dir>`     | `cargo run -p get-checksums -- <dir>`                            |
| `./dev.sh check-checksums <dir>`   | `cargo run -p check-checksums -- <dir>`                          |
| `./dev.sh install-pydl`            | build `pydl` in release mode and copy it to `~/.local/bin/pydl`  |
| `./dev.sh clean`                   | `cargo clean`                                                    |

`dev.sh` forwards extra arguments to `cargo` where sensible, e.g. `./dev.sh test my_test_name -- --nocapture`. Set `CARGO=/path/to/cargo` to override the cargo binary for the non-fmt commands (fmt always uses nightly — see [Formatting](#formatting)).

## Day-to-day workflow

1. Edit code.
2. `./dev.sh fmt` — format in place.
3. `./dev.sh check` — the gate. If this is clean, your change is ready for review.

[`ISSUES.md`](./ISSUES.md) tracks open bugs, smells and resolved investigations across the workspace. Skim it before starting a non-trivial change so you know whether the area you're touching has known caveats; add a new entry there (rather than fixing on the side) if you spot a problem outside your immediate task.

Tighter iteration:

- `./dev.sh test some_test_name` to run a specific test.
- `cargo test -p pydl-cache <pattern>` to scope to a single crate.
- `./dev.sh pydl -l debug <subcommand>` for chatty logs on a one-off invocation. The `-l` / `--log` flag accepts the same directive syntax as `RUST_LOG` (`debug`, `pydl=trace,warn`, `pydl_cache=debug`, …) and overrides `RUST_LOG` when both are set. `RUST_LOG=debug ./dev.sh pydl <subcommand>` still works for the env-style case.
- `./dev.sh pydl --log-timestamps <subcommand>` prepends timestamps to log lines — handy when capturing them to a file. Off by default since interactive terminal output rarely needs them.

## Lints

The workspace enables clippy's `pedantic` and `nursery` groups at `warn` level, plus `unsafe_code = "forbid"` and `rust_2018_idioms`. Two doc-specific pedantic lints (`missing_errors_doc`, `missing_panics_doc`) are explicitly allowed because this workspace isn't a published crate; if `pydl-cache` is ever published, remove those allows and add `# Errors` / `# Panics` sections to public `Result`-returning functions.

To add a local override, use `#[allow(clippy::xxx)]` on the offending item with a comment explaining why, or add the lint to a crate's own `[lints.clippy]` table in its `Cargo.toml` for crate-scoped overrides.

`./dev.sh lint` uses `-D warnings` so new warnings fail CI rather than silently accumulating.

## Formatting

Run `./dev.sh fmt` before committing, or configure your editor to format on save. `./dev.sh fmt-check` is part of `./dev.sh check` and fails if anything is mis-formatted.

`rustfmt.toml` opts into:

- `group_imports = "StdExternalCrate"` — splits `use` statements into three blocks (std → external crates → `crate::`/`super::`/`self::`) separated by blank lines, alphabetical within each.
- `imports_granularity = "Module"` — merges sibling `use` lines from the same module into a single brace group (e.g. `use std::{fs, io};`).
- `reorder_imports = true` — sorts imports alphabetically within each group (stable default; stated explicitly).

Both grouping options are nightly-only rustfmt features, which is why `./dev.sh fmt` invokes `cargo +nightly fmt`. Stable `cargo fmt` ignores them silently with a warning — it won't corrupt the layout, but it also won't enforce the grouping.

## Testing

All tests live inline (`#[cfg(test)] mod tests`) in the crate they belong to. Integration tests that need a mock HTTP server use [`wiremock`](https://crates.io/crates/wiremock); a few network-edge tests bind a raw `tokio::net::TcpListener` directly to reproduce scenarios wiremock can't (mid-stream truncation, connection refused).

Tests run with `./dev.sh test` (everything in the workspace) or `cargo test -p <crate>` when you want to scope. Tests are hermetic — they use `tempfile::TempDir` for cache directories — so they can run in parallel.

The cache library carries the bulk of the test suite, covering freshness, revalidation, stale-if-error, streaming, on-disc layout, error recovery and network failure classification. See [`pydl-cache/README.md`](./pydl-cache/README.md) for the design being tested and [`pydl-cache/DEV.md`](./pydl-cache/DEV.md) for crate-specific developer notes.

## Running the binaries

- **`./dev.sh pydl <subcommand> [args]`** — the user-facing binary. Subcommands are `update`, `available`, `download`, `install`, `installed`, `uninstall`, `python`, `pin`, `cache`, `completions`, `self-update`. See [`pydl/README.md`](./pydl/README.md) for full details.
- **`./dev.sh get-checksums <dir>`** — project maintenance: mirrors every per-release `SHA256SUMS` into `<dir>` (by convention `./checksums/`). Idempotent; the results are embedded into `pydl` at build time for offline verification.
- **`./dev.sh check-checksums <dir>`** — CI trip-wire: verifies every `<dir>/<tag>.sha256sums` file is bit-exact with what upstream currently serves. Exits non-zero on any mismatch. Used by `.github/workflows/check-checksums.yml`. See [`check-checksums/README.md`](./check-checksums/README.md).
- **`./dev.sh install-pydl`** — builds `pydl` in release mode and copies the resulting binary to `~/.local/bin/pydl`, creating the directory if needed. Warns if `~/.local/bin` is not on your `PATH`. Re-run after any change you want to exercise via a `pydl` on `PATH` rather than through `cargo run`.

The HTTP cache (`~/.pydl/cache/` on Unix, `%USERPROFILE%\.pydl\cache\` on Windows) is shared across all binaries that touch it: `pydl update`, `pydl download`, `pydl self-update`, `get-checksums` and `check-checksums` all populate it; `pydl install` and `pydl python` read from it via `pydl-cache::CachingClient::cached_body_path` without revalidating. Debug-level logs from `pydl_cache` surface the per-request HIT/MISS/STALE transitions.

Separate from the HTTP cache, `~/.pydl/snapshot/` holds the JSON snapshots written by `pydl update`. The cache is keyed by URL and revalidates against upstream; the snapshot is opaque to the cache and is rewritten wholesale on every `pydl update`. Clearing one does not affect the other.

## Refreshing the embedded checksum set

`pydl` verifies every asset against an embedded SHA-256, keyed by `(tag, asset_name)`. The embedded table is generated at build time by `pydl-common/build.rs` from the committed `./checksums/<tag>.sha256sums` files. A missing checksum is a hard error, not a silent pass-through — [`DESIGN.md`](./DESIGN.md) explains why.

The practical consequence: **`pydl` can only install or run Python builds from releases whose `.sha256sums` files are committed at the time the binary was built.** When upstream cuts a new release, the embedded set goes stale. Two user-facing failure modes flag this:

- **Filter doesn't resolve** → `no embedded release carries version "<VER>" …` (from `pydl install` / `python` / `pin` / `uninstall`). The embedded table doesn't have an entry matching the filter.
- **Tag-specific lookup fails** → `tag "<TAG>" isn't in the embedded checksum set …` (from `pydl pin`). The user named a tag explicitly that isn't embedded.

In either case, the fix is the same:

1. Run `./dev.sh get-checksums ./checksums` — the binary paginates releases through the cache, downloads any `SHA256SUMS` that aren't already on disc and writes each as `./checksums/<tag>.sha256sums`. Idempotent; already-present files are left alone.
2. Commit the new `./checksums/*.sha256sums` files.
3. Rebuild the `pydl` binary (`./dev.sh build` or `./dev.sh pydl …` — the build script sees the new files via its `cargo:rerun-if-changed` declarations and re-embeds).

Do this periodically (before a release, or when a user reports a staleness error for a tag they want). There's no automation today; it's a deliberate human step so the committed checksum list is always a reviewable artifact.

A few releases upstream don't ship a `SHA256SUMS` asset at all (mostly very old ones) — `get-checksums` logs a `404 Not Found, skipping` warning for those and they remain uncovered. That's expected; the reporting line at the end breaks the total into `downloaded / already present / missing SHA256SUMS` so the gap is visible.

## Adding a new crate to the workspace

1. `cargo new --lib <name>` or `--bin <name>` at the workspace root.
2. Add it to `members = [...]` in the root `Cargo.toml`.
3. In the new crate's `Cargo.toml`, inherit shared metadata and lints:
   ```toml
   [package]
   name = "<name>"
   version.workspace = true
   edition.workspace = true

   [lints]
   workspace = true
   ```
4. Pull shared deps with `{ workspace = true }` and add new deps to the workspace's `[workspace.dependencies]` table.
5. If the crate produces a binary that should ship with releases, add a build step for it to `.github/workflows/release.yaml` (today only `pydl` is published).
6. Run `./dev.sh check` to confirm it builds cleanly under the workspace's lint policy.

## CI and release

Four GitHub Actions workflows live under `.github/workflows/`:

| Workflow                                                              | Trigger                            | What it does                                                                                                                       |
|-----------------------------------------------------------------------|------------------------------------|------------------------------------------------------------------------------------------------------------------------------------|
| [`ci.yaml`](./.github/workflows/ci.yaml)                              | push/PR to `main`, nightly cron    | `fmt` (nightly), build/lint/test matrix on Ubuntu/macOS/Windows, plus per-crate isolation builds.                                  |
| [`check-checksums.yml`](./.github/workflows/check-checksums.yml)      | PRs touching the checksum pipeline; nightly | `check-checksums ./checksums` — diffs the committed checksum files against upstream so a tampered or rewritten file fails CI.    |
| [`release.yaml`](./.github/workflows/release.yaml)                    | tag push matching `v*.*.*`         | Builds release binaries of `pydl` for `aarch64-apple-darwin`, `x86_64-unknown-linux-musl`, `x86_64-pc-windows-msvc`; archives them; creates a GitHub Release with auto-generated notes. |
| [`changelog.yaml`](./.github/workflows/changelog.yaml)                | tag push matching `v*.*.*`         | Regenerates `CHANGELOG.md` via [`git-cliff`](https://git-cliff.org) (config: `cliff.toml`) and commits to `main`.                  |

Caching is provided by [`Swatinem/rust-cache`](https://github.com/Swatinem/rust-cache). The `fmt` job in `ci.yaml` installs nightly via `dtolnay/rust-toolchain@nightly` without changing the default toolchain, so the build/lint/test legs still use stable.

To reproduce the gate locally:

```
./dev.sh check
```

This runs `fmt-check` + `clippy` + `test` on the host platform. The full multi-OS matrix only runs in CI.

### Cutting a release

The shared workspace version lives in `[workspace.package]` of the root `Cargo.toml`. To cut a release:

1. Bump the version: edit `[workspace.package].version` in `Cargo.toml` and run `cargo update --workspace` so `Cargo.lock` reflects the new value.
2. Commit (`Bump version to <X.Y.Z>` — `cliff.toml` skips this commit when generating the changelog).
3. Tag: `git tag v<X.Y.Z> && git push origin main v<X.Y.Z>`.
4. Both `release.yaml` and `changelog.yaml` fire on the tag push. The release workflow attaches three archives to the GitHub Release; the changelog workflow commits a refreshed `CHANGELOG.md` back to `main`.

If the first tag misbehaves (wrong artefact name, broken archive), delete the tag and the corresponding release, fix, retag.
