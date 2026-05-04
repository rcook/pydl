# get-checksums

A small console application that mirrors the per-release `SHA256SUMS` file for every [`astral-sh/python-build-standalone`](https://github.com/astral-sh/python-build-standalone/releases) release into a local directory, fetched through [`pydl-cache`](../pydl-cache).

For each release tag (e.g. `20260414`), the tool writes `<tag>.sha256sums` into the output directory. Files that already exist are skipped — the tool is idempotent and safe to rerun.

## Running

```
cargo run -p get-checksums -- <output-dir>
```

Or via the `dev.sh` wrapper:

```
./dev.sh get-checksums <output-dir>
```

Example:

```
./dev.sh get-checksums ./checksums
```

## How it works

1. Paginates the GitHub releases API at `per_page=100` through the cache.
2. For each release, checks whether `<output-dir>/<tag>.sha256sums` already exists. If it does, skips the download.
3. Otherwise, GETs `https://github.com/astral-sh/python-build-standalone/releases/download/<tag>/SHA256SUMS` through the cache and writes the body to disc.
4. Releases that don't publish a `SHA256SUMS` asset (a few early tags) log a warning and are counted as "missing".

The final summary line reports the totals: downloaded / already-present / missing.

## Cache location and rate-limit behaviour

Responses are cached alongside the other binaries under:

- **Linux / macOS:** `$HOME/.pydl/cache/`
- **Windows:** `%USERPROFILE%\.pydl\cache\`

The same client-side minimum-freshness floor applies as in the `pydl` binary:

| Variable                   | Default | Meaning                                                                                    |
|----------------------------|---------|--------------------------------------------------------------------------------------------|
| `PYDL_MIN_FRESHNESS_SECS`  | `86400` | Client-side floor on cache freshness, in seconds. `0` disables it. See `pydl/README.md`.    |

`-l` / `--log <DIRECTIVE>` accepts the same directive syntax as `RUST_LOG` and overrides it when both are set. `--help` and `--version` are also available.

On a warm run within the freshness window, the release-list pages are served from disc (no upstream calls), and any previously-downloaded `SHA256SUMS` files are also cache hits — so a full re-run against an already-populated output directory makes zero upstream calls.

## See also

- [`../pydl-cache/README.md`](../pydl-cache/README.md) — the cache library (cache key, log vocabulary, streaming, eviction and stale-if-error).
- [`../pydl/README.md`](../pydl/README.md) — the user-facing binary that consumes the committed `./checksums/` files (via a build-time embed).
- [`../README.md`](../README.md) / [`../DEV.md`](../DEV.md) — workspace overview and developer guide.
