# pydl

**Py**thon **D**own**l**oader (pronounced *"piddle"*). A small CLI that fetches, verifies and runs the standalone Python distributions published by [`astral-sh/python-build-standalone`](https://github.com/astral-sh/python-build-standalone) — no system Python, no `apt`/`brew`, no PPA.

Pick a tag and version, download it once, use it forever:

```sh
pydl update                                        # one-time refresh of the local snapshot
pydl available -v 3.14.4                           # offline: read from the snapshot
pydl download -t 20260414 -v 3.14.4                # one-time network hit for the asset
pydl python   -t 20260414 -v 3.14.4 -- -m venv .venv
. ./.venv/bin/activate
```

## Why `pydl`

- **No system Python required.** Downloads self-contained distributions that run without package-manager coordination. Useful for CI images, air-gapped machines or just avoiding conflicts with your OS Python.
- **Reproducible.** Every asset is verified against a SHA-256 baked into the `pydl` binary at build time. Checksums are committed alongside the code, so two people running the same `pydl` on the same `(tag, version)` get bit-identical bytes.
- **Predictable network usage.** Only `pydl update`, `pydl download` and `pydl self-update` touch the network. Of those, `pydl update` is the only one that contacts `api.github.com` for *release listings* (which `pydl self-update --online` also does); `pydl download` and `pydl self-update` always reach out for asset bytes. Everything else — `available`, `install`, `python`, `pin`, `uninstall`, `installed`, `cache`, `completions` — is guaranteed offline and fast.
- **Project-pinned builds.** `pydl pin` writes a `.pydl.json` file you can commit; teammates who run `pydl python -- …` in the repo pick up the same Python build automatically.

### The `update` model

`pydl update` is the single command that fetches the upstream releases list and the latest pydl version, writing both to `~/.pydl/snapshot/`. `pydl available` and `pydl self-update` read from that snapshot — they never contact `api.github.com`. If no snapshot is present, those commands error with a hint to run `pydl update` first; if the snapshot is older than 7 days they additionally suggest a refresh. Pass `--online` to `pydl self-update` for a one-off bypass that hits the network directly.

## Install

### From a release binary

**macOS / Linux / WSL**

```sh
curl -fsSL https://raw.githubusercontent.com/rcook/pydl/main/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/rcook/pydl/main/install.ps1 | iex
```

**Windows (CMD)**

```cmd
curl -fsSL https://raw.githubusercontent.com/rcook/pydl/main/install.ps1 -o %TEMP%\pydl-install.ps1 && powershell -ExecutionPolicy Bypass -File %TEMP%\pydl-install.ps1
```

Default install location is `~/.local/bin` (Unix) or `%LOCALAPPDATA%\Programs\pydl` (Windows); override with the `PYDL_INSTALL_DIR` environment variable. Make sure the install directory is on your `PATH`.

You can also grab a release archive manually from the [Releases page](https://github.com/rcook/pydl/releases). Once installed, `pydl self-update` keeps the binary current.

### From source

#### Prerequisites

- `rustup` (which provides `cargo`). The pinned toolchain in [`rust-toolchain.toml`](./rust-toolchain.toml) installs the right stable channel — including `clippy` and `rustfmt` — automatically the first time you run `cargo`.
  - **macOS:** `brew install rustup` (or use the script from <https://rustup.rs>).
  - **Debian/Ubuntu:** `sudo apt install pkg-config rustup && rustup default stable`.
- `nightly` rustfmt for the import-grouping options in `rustfmt.toml` — only needed if you'll run `./dev.sh fmt` / `fmt-check`:
  ```sh
  rustup toolchain install nightly --profile minimal --component rustfmt
  ```

For full contributor setup see [`DEV.md`](./DEV.md).

#### Build

```sh
git clone <this-repo>
cd pydl
./dev.sh install-pydl     # builds in release mode, copies to ~/.local/bin/pydl
```

`~/.local/bin` on `PATH` → `pydl` is available in any shell. If `~/.local/bin` isn't on your `PATH`, the installer warns you and the copy still succeeds — add the dir to your shell config (`export PATH="$HOME/.local/bin:$PATH"`) and reload.

## Usage

See [`pydl/README.md`](./pydl/README.md) for the full user guide — subcommands, filter flags, the `.pydl.json` pin file and every supported workflow.

Quick orientation:

| Command               | Network?       | What it does                                                                                        |
|-----------------------|----------------|-----------------------------------------------------------------------------------------------------|
| `pydl update`         | yes            | Refresh `~/.pydl/snapshot/` (Python releases list + latest pydl version). The only listings hit.    |
| `pydl available`      | no             | Show what's available from the local snapshot — Python releases plus the latest pydl version.       |
| `pydl download`       | yes            | Fetch one asset into `~/.pydl/cache/`.                                                              |
| `pydl install`        | no             | Verify + unpack a downloaded asset into `~/.pydl/asset/<hash>`.                                     |
| `pydl installed`      | no             | List what's installed locally.                                                                      |
| `pydl uninstall`      | no             | Remove an installed asset.                                                                          |
| `pydl python -- …`    | no             | Run a previously-installed interpreter.                                                             |
| `pydl pin`            | no             | Freeze tag/version into `.pydl.json` for the project.                                               |
| `pydl cache`          | no             | Inspect or clear the HTTP cache.                                                                    |
| `pydl completions`    | no             | Emit a shell-completion script.                                                                     |
| `pydl self-update`    | always         | Replace the running binary with the latest GitHub release. Reads the version from the snapshot by default; `--online` bypasses it. The binary itself is always downloaded over the network. |

## State

Everything `pydl` writes lives under `~/.pydl/` (Unix) or `%USERPROFILE%\.pydl\` (Windows):

- `snapshot/` — JSON snapshots written by `pydl update` (`pbs-releases.json`, `pydl-latest.json`).
- `cache/` — HTTP-cache bodies for downloaded assets.
- `asset/<hash>/` — unpacked Python distributions.

All three are safe to delete; the next `pydl update` rebuilds the snapshot, and the next `pydl download` / `pydl install` rebuilds the cache and asset directory. `pydl cache clear --yes` and `pydl uninstall --all --yes` do the corresponding cleanups through the CLI.

## See also

- [`pydl/README.md`](./pydl/README.md) — the user guide (subcommands, flags, `.pydl.json`, examples).
- [`DEV.md`](./DEV.md) — contributor guide (workspace layout, build/test/lint workflow, CI, how to refresh the embedded checksum set, how to add a new crate).
- [`DESIGN.md`](./DESIGN.md) — why `pydl` verifies assets the way it does (threat model, alternatives considered).
- [`ISSUES.md`](./ISSUES.md) — rolling triage of bugs, smells and resolved investigations across the workspace.
- [`TODO.md`](./TODO.md) — small dated items the maintainer plans to revisit.
- Library READMEs: [`pydl-cache`](./pydl-cache/README.md) (the pull-through HTTP cache), [`get-checksums`](./get-checksums/README.md) (maintenance tool to populate `./checksums/`), [`check-checksums`](./check-checksums/README.md) (CI trip-wire).

## Licence

[MIT License](LICENSE)
