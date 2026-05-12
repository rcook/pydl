# pydl — bugs and code quality review

A rolling triage of bugs, smells and open questions across the workspace, sibling to [`TODO.md`](./TODO.md). New items go under **Open**; resolved items move to **Resolved** with a one-line note. The historical investigation that motivated the snapshot model lives at the bottom under **Archived investigations** and is kept verbatim for reference.

Lints (`cargo clippy --workspace --all-targets -- -D warnings`) are clean and the workspace test suite (252 tests across the four crates) passes. Items below are findings from a rolling re-scan, not a backlog of regressions.

Tags:

- **[easy fix]** — mechanical and low-risk.
- **[design]** — needs a judgement call.
- **[low priority]** — real but not pressing.

Item numbers are stable across edits; gaps (e.g. between #1 and #3 below) are previously-resolved items kept out of the live list. See **Resolved** for the cumulative history.

## Open — bugs / correctness

### 1. `verify_checksum` runs `sha256_file` on the async runtime thread
**File:** `pydl/src/cmd/self_update.rs:403`

`verify_checksum` is called from inside `run` (an `async fn`) and synchronously hashes a multi-MB archive. With Tokio's default multi-thread runtime this only blocks one worker, but it can starve other tasks and breaks the convention that compute-bound work in async code goes through `tokio::task::spawn_blocking`.

Fix: wrap the call site in `spawn_blocking`, or move the hash off the runtime entirely by structuring the verify step as sync code at the top of `run`. **[low priority]** — the self-update path runs once per upgrade and there are no concurrent tasks competing for the runtime.

### 3. `unix_now` silently returns 0 on clock-error
**File:** `pydl-cache/src/lib.rs:379-383`

```rust
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
```

A pre-1970 system clock would make every cache entry look maximally expired (`expires_at > 0` is unlikely; `floor_expiry` saturates to `min_freshness_secs`). In practice no production host has a pre-epoch clock, but the silent fallback is a footgun for tests or VMs with broken time. **[low priority]** — propagate the error or at least `warn!` once.

### 5. `install_from_archive` has a TOCTOU race on `final_dir.exists()`
**File:** `pydl-common/src/install.rs:105, 141`

Two concurrent `pydl install` invocations on the same `(tag, asset)` could both observe `!final_dir.exists()`, both `unpack` into staging and one of the `fs::rename` calls would then fail with `EEXIST` (Linux) or `ERROR_ALREADY_EXISTS` (Windows). The losing process aborts mid-install with a confusing rename error. Real but pathological — running two `pydl install`s in parallel on the same asset is unusual.

Fix: catch `AlreadyExists` on rename, treat as success (the winner's directory is correct) and clean up the loser's staging dir. **[low priority]** — costs a half-installed staging dir cleanup on a rare race.

### 6. `pydl python` maps signal-killed children to exit code 1
**File:** `pydl/src/cmd/python.rs:90`

```rust
let code = status.code().unwrap_or(1);
std::process::exit(code);
```

On Unix, `ExitStatus::code()` returns `None` when the child was killed by a signal. The convention is `128 + signum`, which preserves "the python process was killed by SIGTERM" information. The current `unwrap_or(1)` flattens that to "something failed". The comment at the call site acknowledges the trade-off. **[low priority]** — consider `std::os::unix::process::ExitStatusExt::signal()` on Unix to recover the signal number.

## Open — code quality / smells

### 8. `download.rs` and `self_update.rs::download_archive` have near-duplicate stream-into-file loops
**Files:** `pydl/src/cmd/download.rs:92`, `pydl/src/cmd/self_update.rs:357`

Both drain a `ByteStream` while doing something with each chunk: `download.rs` updates a `Sha256` hasher and `self_update.rs` writes to a tmp file. The shapes are similar enough that a small `process_stream` helper (taking a closure called per chunk) would dedup them. Not load-bearing — it's two short loops. **[low priority]**.

### 9. `pydl/src/main.rs` keeps the redundant `format_timestamp(None)` branch
**File:** `pydl/src/main.rs:130-131`

```rust
if !cli.log_timestamps {
    builder.format_timestamp(None);
}
```

`Builder::from_env` already initializes the format from env defaults. The override is correct (we want the CLI flag to win over env), but it would be more explicit to set the format unconditionally based on the resolved CLI choice (`if cli.log_timestamps { Default } else { None }`). The current form is fine; just a thought. **[low priority]**.

### 11. Symlink-following in `pydl cache info` byte counts
**File:** `pydl/src/cmd/cache.rs:111, 116`

`walk` calls `entry.file_type()` which doesn't follow symlinks, but `is_file()` then follows for `metadata()`. A symlink in `~/.pydl/cache/` pointing outside would inflate the byte count. The cache directory is exclusively populated by `pydl-cache` writing real files, so this only matters if a user manually drops a symlink. **[low priority]** — switch to `entry.metadata()`-based file-type checks if it ever becomes an issue.

## Out of scope

- Honouring HTTP `Vary` headers in the cache (already discussed in `pydl-cache/README.md` — design-level, not exercised by the workloads `pydl` actually runs).
- Cosign / sigstore on top of the `SHA256SUMS` manifest (mentioned as the long-term path in [`DESIGN.md`](./DESIGN.md)).
- Eviction policy beyond the existing caller-driven `evict(url)` (an explicit non-goal in `pydl-cache/README.md`).

## Resolved

Most-recent-first.

- **A. `pydl available` / `pydl update` 504s on the GitHub releases endpoint.** Resolved structurally by gating release-list traffic behind `pydl update` (so `available` no longer makes the call) and by Method B — dropping `PER_PAGE` from `100` to `10` in `pydl-common/src/lib.rs` after live measurement showed only `per_page=10` reliably succeeded against the upstream's degraded edge for `astral-sh/python-build-standalone`. Original investigation preserved under **Archived investigations** below.
- **#10. `pydl-cache::CachingClient::request` for non-GET methods builds a fresh `reqwest::RequestBuilder` ignoring user-agent.** Re-checked at `pydl-cache/src/lib.rs:306`: the user-agent lives on `self.inner` (the `Client`), so it is applied to non-GET requests too. False alarm; no change needed.
- **#4. `build_meta` / `update_meta_from_headers`: `now + max_age` could overflow.** Fixed by replacing the `+` with `saturating_add` at both sites in `pydl-cache/src/lib.rs`. Test: `build_meta_saturates_on_pathological_max_age`.
- **#2 (and a long tail of earlier review items).** From an older 18-item review: eviction on hash-mismatch in `pydl download`, `SHA256SUMS` verification on self-update, the no-store / `cached_body_path` interaction and others. All fixed before this file was first authored; mentioned here only to explain why the live-list numbering starts at #1 and skips around.

## Archived investigations

Kept verbatim for reference. The current state of the world is summarized under **Resolved**; the notes below trace how we got here and may be useful if upstream's edge behaviour changes again.

### A (archived). `pydl available` still hits 504 from the GitHub releases API on cold cache
**Files:** `pydl-common/src/lib.rs` (retry policy in `request_with_retry`, `PER_PAGE`)

After issue #2 shipped retry-with-backoff and HTML-body trimming, a fresh user run still failed:

```
pydl available
[INFO  pydl_common] GET https://api.github.com/repos/astral-sh/python-build-standalone/releases?per_page=100&page=1 -> 504 Gateway Timeout, retrying in 425ms (attempt 1/3)
[INFO  pydl_common] GET ... -> 504 Gateway Timeout, retrying in 1225ms (attempt 2/3)
Error: GET ... returned 504 Gateway Timeout: (92-byte HTML body — likely an upstream error page)
```

In response, the retry policy was loosened to **5 attempts × 1000 ms base × 2^n growth** (≈15 s of total backoff). That still wasn't enough during sustained slow windows.

**Hypothesis (later confirmed):** GitHub's edge struggles to assemble the full per-page payload for `astral-sh/python-build-standalone`, and retries within a single client invocation don't outlast the upstream slow period.

#### How to test manually

The procedure is unchanged after the snapshot refactor — only the *command* the user runs changes. Substitute `pydl update` for `pydl available` in step 1 below.

1. **Reproduce the error**, capturing the log output:

   ```sh
   rm -rf ~/.pydl/snapshot ~/.pydl/cache       # macOS / Linux
   # or:
   Remove-Item -Recurse -Force "$env:USERPROFILE\.pydl\snapshot","$env:USERPROFILE\.pydl\cache"   # Windows
   RUST_LOG=info pydl update 2>&1 | tee /tmp/pydl-update.log
   ```

   Save the log. The retry-banner lines record the exact backoff sequence, which tells us whether the policy has been applied and how the failures distribute.

2. **Probe the API directly** to characterise the failure shape independent of `pydl`. Run several iterations against each page size and record latency + status:

   ```sh
   for n in 100 50 30 10; do
     echo "=== per_page=$n ==="
     for i in 1 2 3 4 5; do
       curl -sS -o /dev/null \
         -w "  attempt %{http_code} time=%{time_total}s size=%{size_download}\n" \
         -H 'User-Agent: pydl-debug' \
         "https://api.github.com/repos/astral-sh/python-build-standalone/releases?per_page=$n&page=1"
       sleep 1
     done
   done
   ```

   **Note:** this loop costs ≤20 unauthenticated requests against `api.github.com`. If you also run `pydl update` in the same hour you can exhaust the 60 req/h anonymous budget and start seeing 403s instead of 504s — wait for the bucket to refill (the response headers include a `Retry-After` and pydl logs an estimated wait time) before drawing further conclusions.

   Two questions answer themselves from the data:
   - **Does smaller `per_page` help?** If `per_page=30` succeeds while `per_page=100` 504s, the timeout correlates with response size. (When this was last measured on 2026-05-12, only `per_page=10` succeeded — Method B below codifies that finding.)
   - **Is this a sustained slow period or a flap?** If 504s come in clusters with successes between, longer retries should suffice; if every attempt 504s for several minutes, retries within a single invocation can't help and we need a different strategy.

3. **Time the upstream**: `time curl -sSL -H 'User-Agent: pydl-debug' 'https://api.github.com/repos/astral-sh/python-build-standalone/releases?per_page=100&page=1' > /dev/null`. Anything north of ~20 s is the edge timing out and likely surfacing as a 504 to other clients on the same window.

4. **Check `gh api` as a sanity baseline** — `gh` uses an authenticated client and a higher rate-limit budget, but if `gh api 'repos/astral-sh/python-build-standalone/releases?per_page=100' | wc -c` succeeds while curl 504s, that suggests the failure correlates with anonymous-traffic shaping rather than payload assembly.

5. **Capture the HTML error body** if it ever varies — `pydl` already trims it to a length summary, but the raw body may carry a `request-id` header useful for an upstream report:

   ```sh
   curl -sSi -H 'User-Agent: pydl-debug' 'https://api.github.com/repos/astral-sh/python-build-standalone/releases?per_page=100&page=1' | head -40
   ```

   The `x-github-request-id` and `x-served-by` headers are the ones to record.

Paste the gathered data into a follow-up issue so the next round of triage has actual numbers, not just a single failure log.

#### Method B (applied 2026-05-12)

Live measurement showed every `per_page=100` request 504-ing in <300 ms (edge negative-cache); `per_page=30` and `per_page=50` 504-ing partway through assembly after ~11 s; only `per_page=10` returning a clean 200 (~7 s cold, ~0.8 s warm). `PER_PAGE` was dropped from `100` to `10` in `pydl-common/src/lib.rs`. Trade-offs:

- **Pro:** smaller responses are cheap for GitHub's edge to assemble; fixes the only failure mode users hit.
- **Con:** ~9 paginated requests per `pydl update` instead of 1. Each consumes one unauthenticated rate-limit token — the unauthenticated budget is 60 req/h, so even worst-case the cost is well within budget for a single `pydl update` invocation.
- **Con:** the cache layer keys on full URL including `per_page`, so changing this value invalidates every existing cached releases-list entry. Acceptable cost — the cache is best-effort and the entries were going to revalidate anyway under their min-freshness floor.

A future refinement might fall back to a smaller page size *automatically* on 504 (try `per_page=N`, on `[502, 503, 504]` retry with a smaller `N`), but that's a more invasive change and shouldn't be designed without fresh data confirming the larger page sizes have started succeeding again.
