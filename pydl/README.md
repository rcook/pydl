# pydl

Download, verify, install and run the standalone Python distributions published by [`astral-sh/python-build-standalone`](https://github.com/astral-sh/python-build-standalone).

```
pydl update
pydl available        [filter flags]
pydl download         [filter flags] [-o DIR]
pydl install          [filter flags]
pydl installed        [filter flags]
pydl uninstall        [filter flags] [--all] [--yes]
pydl python           [filter flags] -- [python args]
pydl pin              [filter flags]
pydl cache            {info,clear [--yes]}
pydl completions      <SHELL>
pydl self-update      [--online] [--pre] [--force] [--dry-run] [--require-checksum]
```

## Subcommands at a glance

| Subcommand            | Network?           | What it does                                                                                |
|-----------------------|--------------------|---------------------------------------------------------------------------------------------|
| `pydl update`         | yes                | Refresh `~/.pydl/snapshot/`: Python releases list + latest pydl version. The only command that fetches release listings. |
| `pydl available`      | no                 | Read the Python releases list from the snapshot. Errors with a hint if the snapshot is missing. |
| `pydl download`       | yes                | Fetch one asset into `~/.pydl/cache/` (and optionally `-o DIR`).                            |
| `pydl install`        | no                 | Verify + unpack a downloaded asset into `~/.pydl/asset/<hash>/`.                            |
| `pydl installed`      | no                 | List installed assets and their paths.                                                      |
| `pydl uninstall`      | no                 | Remove an installed asset (dry-run by default; `--yes` to delete).                          |
| `pydl python -- …`    | no                 | Run a previously-installed interpreter.                                                     |
| `pydl pin`            | no                 | Freeze a filter into `.pydl.json` for the project.                                          |
| `pydl cache`          | no                 | Inspect or clear the on-disc HTTP cache.                                                    |
| `pydl completions`    | no                 | Emit a shell-completion script to stdout.                                                   |
| `pydl self-update`    | always             | Replace the running binary with the latest `rcook/pydl` release. Reads the version from the snapshot by default; `--online` bypasses the snapshot. The binary itself is always downloaded over the network. |

**Only `pydl update` and `pydl self-update --online` contact `api.github.com` for *release listings*.** `pydl download` and `pydl self-update` also touch the network — they download asset bytes from GitHub — but neither pulls a releases-list payload. Everything else is guaranteed offline. Filter resolution (`-v 3.14.4` → concrete tag) runs against the SHA-256 checksum table embedded in the `pydl` binary at build time; asset bytes come from whatever `pydl download` previously placed in the cache; release listings come from the local snapshot written by `pydl update`. If a command needs the snapshot and finds none, it fails with a clear hint to run `pydl update` first; if `pydl install` or `pydl python` is asked to operate on an asset whose archive isn't in the cache, it fails with `… isn't in the cache — run 'pydl download -t X -v Y' first` so you always know when a network round-trip is about to happen.

### The snapshot model

`pydl update` is the single command that fetches release listings from upstream. It writes two files under `~/.pydl/snapshot/`:

- `pbs-releases.json` — the full paginated list of `astral-sh/python-build-standalone` releases, consumed by `pydl available`.
- `pydl-latest.json` — the latest `rcook/pydl` release plus its assets, consumed by `pydl self-update`.

`pydl available` and `pydl self-update` always print the snapshot's age (`snapshot from 3 hours ago`) and add a `run \`pydl update\` to refresh` hint when the snapshot is older than 7 days. With no snapshot present those commands error out — they never silently fall back to the network.

The snapshot is **not** part of the trust model: `install` still resolves against the embedded checksum set, `download` still verifies SHA-256 against committed checksums and `self-update` still verifies the downloaded binary against the release's `SHA256SUMS` manifest. See [`../DESIGN.md`](../DESIGN.md) for details.

## Filter flags

Every subcommand that acts on one asset shares the same filter flags, defined once in `pydl_common::filter::FilterArgs`:

| Flag                   | Effect                                                                                                         |
|------------------------|----------------------------------------------------------------------------------------------------------------|
| `-t, --tag <TAG>`      | Pick a specific release (e.g. `20260414`).                                                                     |
| `-v, --version <VER>`  | Filter assets by Python version (e.g. `3.14.4`, `3.15.0a8`).                                                   |
| `--platform`           | **Default.** Limit to the current OS + baseline arch.                                                          |
| `--no-platform`        | Show every triple.                                                                                             |
| `--default-attrs`      | **Default.** Narrow to the canonical redistributable (flavour `install_only`, preferred triple).               |
| `--no-default-attrs`   | Show every flavour/triple that passes the other filters.                                                       |

## Primary workflow: create a virtual environment

Download once, then pin-and-run offline.

### macOS / Linux (bash, zsh)

```sh
pydl download -t 20260414 -v 3.14.4                # one-time network hit
pydl python   -t 20260414 -v 3.14.4 -- -m venv .venv
. ./.venv/bin/activate
```

After activation, `python` and `pip` resolve to the ones inside `.venv`. `deactivate` returns you to the outer shell.

### Windows (PowerShell)

```powershell
pydl download -t 20260414 -v 3.14.4
pydl python   -t 20260414 -v 3.14.4 -- -m venv .venv
. .\.venv\Scripts\Activate.ps1
```

If PowerShell refuses to run `Activate.ps1`, lift the script-execution restriction for the current user once:

```powershell
Set-ExecutionPolicy -Scope CurrentUser -ExecutionPolicy RemoteSigned
```

### Windows (cmd.exe)

```bat
pydl download -t 20260414 -v 3.14.4
pydl python   -t 20260414 -v 3.14.4 -- -m venv .venv
.\.venv\Scripts\activate.bat
```

### Notes

- **`pydl download` is the only network hit** per tag/version. After that, `pydl install` and `pydl python` verify the cached bytes against the embedded SHA-256 and unpack into `$HOME/.pydl/asset/<hash>/`. Re-running either is a local operation.
- **`--` is required** because Python has its own `-m` and `--version` that would otherwise be consumed by `pydl python`'s argument parser.

## Project-pinned configuration (`.pydl.json`)

`pydl pin` writes a JSON file named `.pydl.json` in the current directory capturing whichever filter flags you used. The `download`, `install`, `python` and `uninstall` subcommands read it back automatically, so collaborators can type the bare invocation and get the same pinned build:

```sh
pydl pin -t 20260414 -v 3.14.4      # pin once, commit the file
pydl python -- -m venv .venv        # any team member; no flags needed
```

### Discovery rules

- **Git-style walk.** Subcommands search the current directory and every ancestor for the nearest `.pydl.json`; the first hit wins.
- **Field-level merge.** CLI flags override the config field-by-field. If you pass `-v 3.14.4` but not `-t`, a pinned `tag` from the config still applies; if you pass `-t 20260414 -v 3.14.4`, both CLI values win and nothing is inherited. The two boolean filters (`--platform`/`--no-platform`, `--default-attrs`/`--no-default-attrs`) only inherit from the config when neither side of the pair was passed on the CLI.
- **Scope.** `download`, `install`, `python` and `uninstall` consume the file. `available`, `installed` and `pin` ignore it (their behaviour is unchanged from releases without the file).
- **Corrupt file is a hard error.** If `.pydl.json` exists but doesn't deserialize, the subcommand fails with a message naming the path. Fix or delete it.
- **No file?** The subcommands behave exactly as they did before the feature existed: `install`/`python`/`download` error out when the filter can't pin a single asset.

### Auto-selecting a tag when only a version is pinned

`pydl pin` omits `tag` from `.pydl.json` unless you passed `--tag` on the CLI. The resulting config has a `version` but no specific tag — it floats. When `download`, `install`, `python` or `uninstall` consume such a config (or you pass `-v <VER>` alone on the CLI without `-t`), they automatically select the **newest embedded tag that carries that version** under the current platform / default-attrs filters. The selection is logged at debug level:

```
[DEBUG pydl_common::filter] loading filter defaults from /path/to/.pydl.json
[DEBUG pydl_common::filter] auto-selected tag=20260414 for version=3.10.20 (no tag specified)
```

If no embedded tag carries the version under the current filters, the subcommand fails with `no embedded release carries version "<VER>" under the current platform / default-attrs filters — widen the filter or pass --tag explicitly`. Want a specific historical tag? Re-run `pydl pin --tag <TAG>` (or hand-edit the file to add `"tag"`), and the auto-select step will no-op.

## Subcommands

### `pydl update`

Refresh the local snapshot under `~/.pydl/snapshot/`: paginate the `astral-sh/python-build-standalone` releases listing and write `pbs-releases.json`, then fetch `releases/latest` for `rcook/pydl` and write `pydl-latest.json`. This is the one command that contacts `api.github.com` for release listings. Run it whenever you want `pydl available` or `pydl self-update` to see fresher upstream state — typically before exploring what's available, or after seeing a `run \`pydl update\` to refresh` hint.

```
pydl update
```

Both writes go through the same retry-with-backoff stack as the rest of `pydl`'s network code, so transient 504s recover automatically within bounds. Output reports the path of each snapshot written and a short summary (release count, latest pydl tag).

### `pydl available`

Read the Python releases snapshot from `~/.pydl/snapshot/pbs-releases.json` and print either an aggregate summary or an asset listing when any filter is set. **No network.** If the snapshot is missing the command errors with `no Python releases snapshot found at … — run \`pydl update\` to fetch one`. Every successful run also prints the snapshot's age (`snapshot from 3 hours ago`) and, if older than 7 days, suggests a refresh.

```
pydl available -t 20260414 -v 3.15.0a8
```

### `pydl download`

Fetch exactly one asset through `~/.pydl/cache/`, verifying its embedded SHA-256 as it streams. The cache entry is the primary output — `pydl install` reads from it later. Pass `-o DIR` to *also* write a user-visible copy into `DIR`. Fails fast if the filter collapses to zero or >1 match. Stdout prints the cache body path when `-o` is absent, and the copy's path when `-o` is present, so either form captures a usable path into a shell variable.

```
pydl download -t 20260414 -v 3.10.20                     # cache-warm only
pydl download -t 20260414 -v 3.10.20 -o ./downloads      # also copy to ./downloads
path=$(pydl download -t 20260414 -v 3.10.20 -o ./downloads)
```

Options:

| Flag                     | Default | Meaning                                                                  |
|--------------------------|---------|--------------------------------------------------------------------------|
| `-o, --output-dir <DIR>` | (none)  | Optional: *additionally* copy the asset into `DIR` (created if missing). |

### `pydl install`

Verify and unpack a previously-downloaded asset into `$HOME/.pydl/asset/<hash>/`, where `<hash>` is `sha256(asset.name)`. Idempotent: if the directory exists, exits without re-unpacking. Bails with `… isn't in the cache — run 'pydl download -t X -v Y' first` when the archive hasn't been downloaded. The install directory is printed on stdout so it can be captured into a shell variable:

```
pydl download -t 20260414 -v 3.14.4      # one-time network hit
pydl install  -t 20260414 -v 3.14.4
path=$(pydl install -t 20260414 -v 3.14.4)   # capture into $path
```

### `pydl uninstall`

Remove an installed asset directory from `$HOME/.pydl/asset/`. Resolves a single target the same way `install` does (same filter flags, same `.pydl.json` merge) and removes that `<hash>/` directory plus any crash-leftover `<hash>.<suffix>` staging siblings. Safety gate: without `--yes` it prints the would-remove paths and exits non-zero, so scripts can dry-run.

```
pydl uninstall -t 20260414 -v 3.14.4          # dry-run (exit 2)
pydl uninstall -t 20260414 -v 3.14.4 --yes    # actually remove
pydl uninstall --all                          # preview every install
pydl uninstall --all --yes                    # wipe every install
```

`--all` is mutually exclusive with `--tag`/`--version`.

### `pydl installed`

Enumerate `$HOME/.pydl/asset/` and label each `<hash>` directory with its original asset name (reverse-looked-up via the embedded checksum table). Skips staging dirs (`<hash>.xxxxxx`) and any non-hex entries. Accepts `--tag` / `--version` filters so you can check whether a specific build is installed:

```
pydl installed
pydl installed -v 3.14.4
pydl installed -t 20260414 -v 3.14.4
```

The `--platform` / `--default-attrs` flags are accepted but intentionally don't narrow the local set — every installed directory already satisfied those filters at install time.

### `pydl python`

Install (or reuse) a previously-downloaded asset, then execute its bundled `python` binary with any trailing arguments. The asset's exit code becomes the tool's exit code. Bails with `… isn't in the cache — run 'pydl download -t X -v Y' first` when the archive isn't cached; first run for a new tag/version looks like `pydl download -t X -v Y && pydl python -t X -v Y -- …`.

```
pydl python -t 20260414 -v 3.10.20 -- -c 'import sys; print(sys.version)'
pydl python -t 20260414 -v 3.10.20 -- --version       # needs -- to disambiguate python's --version from ours
```

### `pydl pin`

Resolve a concrete tag and Python version from the embedded checksum set, report them and write the result to `.pydl.json` in the working directory. Fails if the file already exists; pass `-f` / `--force` to overwrite. The `download`, `install`, `python` and `uninstall` subcommands pick the file up automatically — see [Project-pinned configuration](#project-pinned-configuration-pydljson) for the discovery rules.

#### Resolution

- **`--tag` omitted** → uses the newest tag present in the embedded checksum set (tags are `YYYYMMDD` date stamps, so "newest" is unambiguous). **`--tag <TAG>` given** → verifies that `<TAG>` is in the embedded set, with a hint pointing at `pydl available` + a rebuild if it's not.
- **`--version` omitted** → picks the newest **non-prerelease** Python version among assets in the selected tag that match the current platform / default-attrs filters. Prereleases (`3.15.0a8`, `3.15.0b1`, `3.15.0rc2`, …) are deliberately skipped so the default pin is always a stable Python. If the tag happens to carry *only* prereleases, the command errors out and instructs you to pass `-v` explicitly to opt into one. **`--version <VER>` given** → verifies that at least one asset in the selected tag (under the same filters) carries that version, prerelease or not.

#### What gets written

`version` is mandatory in the file and always written. `tag` is only written when **you passed `--tag` on the CLI**; if you omitted `--tag`, it's omitted from the file too, so future subcommand runs re-resolve to the newest embedded tag that still carries your pinned version. The two boolean filters always appear.

```
# Pin to the latest release and the newest non-prerelease Python — both resolved and reported:
pydl pin
# [INFO  pydl::cmd::pin] selected tag=20260414, version=3.14.4
# [INFO  pydl::cmd::pin] wrote ./.pydl.json
cat .pydl.json
# {
#   "version": "3.14.4",
#   "platform": true,
#   "default_attrs": true
# }

# Opt into a prerelease explicitly:
pydl pin -v 3.15.0a8

# Pin a specific tag + version (both are validated against the embedded set):
pydl pin -t 20260414 -v 3.14.4
cat .pydl.json
# {
#   "tag": "20260414",
#   "version": "3.14.4",
#   "platform": true,
#   "default_attrs": true
# }
```

### `pydl cache`

Inspect or wipe the on-disc HTTP cache at `$HOME/.pydl/cache/`. Clearing is safe at any time — the next `pydl download` will just re-fetch.

```
pydl cache info                 # print path, entry count, total bytes
pydl cache clear                # dry-run: preview what would be removed (exit 2)
pydl cache clear --yes          # actually empty the cache
```

### `pydl self-update`

Replace the running binary with the latest released `pydl`. By default the version check is offline: the latest version comes from `~/.pydl/snapshot/pydl-latest.json`, written by `pydl update`. The actual binary download still goes over the network — there's no way around that. Pass `--online` to bypass the snapshot and check `api.github.com` directly. The compile-time target triple (set by `pydl/build.rs`) selects which release artifact to fetch — there is no `--target` flag.

```
pydl self-update                          # snapshot-driven; errors if no snapshot is present
pydl self-update --online                 # bypass the snapshot, hit api.github.com directly
pydl self-update --online --pre           # consider the newest pre-release too (requires --online)
pydl self-update --force                  # re-download and re-install the same version
pydl self-update --force --dry-run        # report the asset URL without replacing
pydl self-update --require-checksum       # refuse to update without a SHA256SUMS manifest
```

Behaviour:

- **Snapshot-driven by default.** Reads `pydl-latest.json` and prints the same staleness line as `pydl available` (`snapshot from N <units> ago`, with a refresh hint when older than 7 days). With no snapshot the command errors with `no pydl version snapshot found at … — run \`pydl update\`, or pass --online to check upstream directly`.
- **`--online` is the escape hatch.** Hits GitHub's `releases/latest` endpoint exactly as the pre-snapshot implementation did. Useful when you want absolute-latest right now without rebuilding the snapshot.
- **`--pre` requires `--online`.** The snapshot only carries the latest stable; the command errors out if `--pre` is set without `--online` and tells you to add it.
- **No-op when already on the latest version.** Logs `pydl X.Y.Z is already the latest` and exits 0. `--force` overrides this so a corrupted install can be repaired by re-downloading.
- **Refuses to downgrade silently.** If the running binary is newer than the latest published release, the command logs that fact and exits 0 without doing anything; `--force` is required to actually downgrade.
- **SHA-256 verification.** Each release publishes a `SHA256SUMS` manifest alongside the platform archives (see `.github/workflows/release.yaml`). After downloading the archive `self-update` fetches the manifest and verifies the archive's hash before extracting. A hash mismatch — or a manifest that's present but doesn't list this archive — is a hard error and the running binary is never replaced. If the manifest is *absent* (older releases predate this), the command warns and proceeds; pass `--require-checksum` to make that a hard error too. The default will flip to strict in a future release once two consecutive manifest-publishing releases have shipped.
- **The replacement is atomic.** Uses [`self_replace`](https://crates.io/crates/self-replace), which handles the Windows `.exe` rename-then-defer-delete dance; on Unix it's a single `rename(2)`. The currently-running process keeps the old code in memory; the next invocation runs the new binary.

### `pydl completions`

Emit a shell-completion script to stdout. Pipe it into your shell's completion loader.

```
# bash (one-shot)
source <(pydl completions bash)

# zsh, persistent
pydl completions zsh > "${fpath[1]}/_pydl"

# fish, persistent
pydl completions fish > ~/.config/fish/completions/pydl.fish

# powershell
pydl completions powershell | Out-String | Invoke-Expression
```

## State directory

All subcommands share `$HOME/.pydl/` on Unix and `%USERPROFILE%\.pydl\` on Windows:

- `snapshot/` — local copy of upstream release listings, written by `pydl update`; read by `pydl available` and `pydl self-update`. Two files: `pbs-releases.json` and `pydl-latest.json`.
- `cache/` — HTTP cache (populated by `pydl update` and `pydl download`; read by `pydl install` / `python`). See [`../pydl-cache/README.md`](../pydl-cache/README.md).
- `asset/<hash>/` — unpacked distributions.

All three are safe to delete at any time — `pydl update` rebuilds the snapshot, `pydl cache clear --yes` and `pydl uninstall --all --yes` do the corresponding cleanups through the CLI.

## Checksum embedding

`pydl install` verifies every asset against a SHA-256 from `../checksums/<tag>.sha256sums`. Those files are embedded into the binary at build time by `pydl-common`'s `build.rs`. An asset whose release has no committed checksum is rejected at `pydl install` time:

```
no embedded checksums for release "<tag>" — rerun `get-checksums ./checksums` and rebuild
```

This means `pydl` can only install Python builds from releases whose `.sha256sums` files were committed when the binary was built. When upstream cuts a new release you'll want to consume, a maintainer has to refresh the set — see [`../DEV.md` § Refreshing the embedded checksum set](../DEV.md#refreshing-the-embedded-checksum-set).

A companion CI tool, [`check-checksums`](../check-checksums/README.md), re-fetches every committed `.sha256sums` from upstream on every PR and nightly, so the committed set can't silently drift from upstream reality.

For the *why* behind this design (tamper-resistance trade-offs, alternatives considered), see [`../DESIGN.md`](../DESIGN.md).

## Environment and logging

| Variable                  | Default  | Meaning                                                                                                 |
|---------------------------|----------|---------------------------------------------------------------------------------------------------------|
| `PYDL_MIN_FRESHNESS_SECS` | `86400`  | Client-side floor on HTTP-cache freshness, in seconds. Set to `0` to force revalidation every request.  |
| `RUST_LOG`                | `info`   | Log-filter directive. See the [`env_logger` docs](https://docs.rs/env_logger/) for syntax.              |

The `-l` / `--log <DIRECTIVE>` CLI flag accepts the same directive syntax as `RUST_LOG` and **overrides** `RUST_LOG` when both are set. It's global (works before or after the subcommand, e.g. `pydl -l debug available` or `pydl available -l debug`) and lives alongside the environment variable for one-off invocations.

## See also

- [`../README.md`](../README.md) — workspace overview.
- [`../DEV.md`](../DEV.md) — contributor guide.
- [`../DESIGN.md`](../DESIGN.md) — design rationale for the checksum-verification model.
- [`../pydl-cache/README.md`](../pydl-cache/README.md) — the cache library.
- [`../get-checksums/README.md`](../get-checksums/README.md) — populate `./checksums/`.
- [`../check-checksums/README.md`](../check-checksums/README.md) — verify `./checksums/` against upstream (CI trip-wire).
