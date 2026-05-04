# check-checksums

A small console application that verifies every `<tag>.sha256sums` file in a local directory against the upstream [`astral-sh/python-build-standalone`](https://github.com/astral-sh/python-build-standalone/releases) `SHA256SUMS` file for the same tag, fetched through [`pydl-cache`](../pydl-cache).

This is the CI trip-wire counterpart to [`get-checksums`](../get-checksums): `get-checksums` populates `./checksums/`, and `check-checksums` asserts that what's on disc is bit-exact with what upstream is still serving. A mismatch means either the committed file was tampered with / corrupted in a PR, or upstream silently rewrote a published release's `SHA256SUMS` — both worth flagging.

## Running

```
cargo run -p check-checksums -- <input-dir>
```

Or via the `dev.sh` wrapper:

```
./dev.sh check-checksums <input-dir>
```

Example:

```
./dev.sh check-checksums ./checksums
```

Exit status:

- `0` — every committed `.sha256sums` matched upstream (or was served as a non-200 and skipped).
- non-zero — at least one committed file disagreed with upstream, or an error occurred.

## How it works

1. Reads every `<tag>.sha256sums` file from `<input-dir>` (non-matching filenames are ignored).
2. Fans out the per-tag GETs with a small bounded concurrency (8 in flight) so a fresh run against ~80 tags finishes in seconds rather than minutes. Each tag's request goes to `https://github.com/astral-sh/python-build-standalone/releases/download/<tag>/SHA256SUMS` through the cache.
3. Compares each upstream body byte-for-byte against the committed file.
   - Match → counted as OK.
   - Mismatch → logged at `ERROR` and added to the failure list; the process exits non-zero at the end.
   - Non-200 response → logged at `WARN` and counted as "fetch-failed" (some very old tags never shipped a `SHA256SUMS`). Not treated as a hard failure.

The final summary line reports totals: matched / mismatched / fetch-failed.

## Cache location and rate-limit behaviour

Responses are cached alongside the other binaries under:

- **Linux / macOS:** `$HOME/.pydl/cache/`
- **Windows:** `%USERPROFILE%\.pydl\cache\`

The same client-side minimum-freshness floor applies as in the `pydl` binary:

| Variable                   | Default | Meaning                                                                                    |
|----------------------------|---------|--------------------------------------------------------------------------------------------|
| `PYDL_MIN_FRESHNESS_SECS`  | `86400` | Client-side floor on cache freshness, in seconds. `0` disables it. See `pydl/README.md`.   |

For CI verification you almost certainly want `PYDL_MIN_FRESHNESS_SECS=0` so the check is authoritative against the live upstream rather than a stale disc cache. The shipped workflow sets this explicitly.

`-l` / `--log <DIRECTIVE>` accepts the same directive syntax as `RUST_LOG` and overrides it when both are set. `--help` and `--version` are also available.

## See also

- [`../get-checksums/README.md`](../get-checksums/README.md) — the mirror binary that populates `./checksums/`.
- [`../pydl-cache/README.md`](../pydl-cache/README.md) — the cache library (cache key, log vocabulary, streaming, eviction and stale-if-error).
- [`../pydl/README.md`](../pydl/README.md) — the user-facing binary that consumes the committed `./checksums/` files (via a build-time embed).
- [`../README.md`](../README.md) / [`../DEV.md`](../DEV.md) — workspace overview and developer guide.
