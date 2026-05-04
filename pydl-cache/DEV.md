# pydl-cache — Developer Notes

Crate-specific notes for working on the cache library. Workspace-wide build/lint/test conventions live in the [workspace `DEV.md`](../DEV.md); this file covers what's specific to `pydl-cache`.

## Source layout

Everything is currently in a single file: `src/lib.rs`. It's organized top-to-bottom as:

1. Public types (`CachingClient`, `ByteStream`, re-exports of `reqwest::Method` / `StatusCode`).
2. Private metadata types (`EntryMeta`, `EntryPaths`).
3. The request path — `get_stream`, `request`, the freshness check, the revalidation path.
4. The download / tee-to-disc helpers.
5. Freshness and cache-control parsing.
6. `#[cfg(test)] mod tests` with ~50 tests.

There's no module split yet because the file is still small enough to navigate. If it grows, the natural fault lines are: metadata (`EntryMeta` + on-disc I/O), request logic (freshness, revalidation, stale-if-error) and parsing (`Cache-Control`, `Expires`).

## Running tests

```
cargo test -p pydl-cache                     # all tests
cargo test -p pydl-cache <pattern>           # scope to matching tests
cargo test -p pydl-cache -- --nocapture      # show log output
RUST_LOG=debug cargo test -p pydl-cache      # verbose library logs
```

Each test creates its own `tempfile::TempDir` cache and, where an upstream is needed, its own `wiremock::MockServer`. Tests are hermetic and run in parallel.

## Test taxonomy

- **Freshness** — `max-age`, `Expires`, `must-revalidate`, `no-store`, the `min_freshness_secs` client floor.
- **Revalidation** — `ETag` / `If-None-Match`, `Last-Modified` / `If-Modified-Since`, `304` vs `200` branches.
- **Stale-if-error** — the 4xx/5xx/429/403 classification and the "no cached entry" fallthrough.
- **Streaming** — memory bounds, cancellation (caller drops the stream mid-read), large payloads.
- **On-disc layout** — atomic writes, `.tmp` cleanup, JSON round-trips for `EntryMeta`.
- **Network-edge** — a few tests bind a raw `tokio::net::TcpListener` to reproduce scenarios `wiremock` can't express (mid-stream truncation, connection refused).

When adding a test, prefer `wiremock`; only drop to a raw `TcpListener` when you need to model a pathology the mock server can't simulate.

## Adding a new test

1. Add a `#[tokio::test]` in the `tests` module.
2. Create `TempDir::new().unwrap()` for the cache.
3. Start a `MockServer::start().await` if an upstream is needed.
4. Mount your mocks with `Mock::given(...)` and `.expect(N)` where N asserts the expected call count (important for HIT/MISS assertions).
5. Assert on the returned `StatusCode`, body, *and* where meaningful the disc state (`<hash>.meta` / `<hash>.body` presence).

Count-based assertions (`.expect(N)` plus `MockServer::verify` on drop) are the single best way to catch regressions where the cache "works" but silently makes extra upstream calls.

## Publishing

`pydl-cache` is not currently published. If it ever is:

- Remove the workspace-level `missing_errors_doc = "allow"` / `missing_panics_doc = "allow"` from the root `Cargo.toml` (or override in this crate's `[lints.clippy]`) and add `# Errors` / `# Panics` sections to public `Result`-returning functions.
- Decide whether `reqwest::Method` / `StatusCode` should remain re-exported at the crate root or move behind a `pub mod reexports`.
- Pin `reqwest`/`tokio` minor versions explicitly in `[dependencies]` rather than relying solely on the workspace table.

## See also

- [`README.md`](./README.md) — user-facing design documentation: API, cache key, log-line vocabulary, stale-if-error, eviction.
- [`../DEV.md`](../DEV.md) — workspace-wide developer guide.
