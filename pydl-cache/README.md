# pydl-cache

A disc-backed pull-through HTTP cache over [`reqwest`](https://crates.io/crates/reqwest). GET responses are cached to disc, keyed by URL, with support for `Cache-Control: max-age`, `Expires`, `ETag` and `Last-Modified` revalidation. Non-GET requests bypass the cache.

## Contents

- [API](#api)
- [On-disc format](#on-disc-format)
- [MISS flow: download-to-tmp, then stream](#miss-flow-download-to-tmp-then-stream)
- [Future enhancement: tee-on-the-fly](#future-enhancement-tee-on-the-fly)
- [Cache key](#cache-key)
- [Log lines](#log-lines)
- [Stale-if-error](#stale-if-error)
- [Eviction](#eviction)

## API

Two entry points on `CachingClient`:

- **`get_stream(url) -> (StatusCode, impl Stream<Item = Result<Bytes>>)`** — the streaming primitive. Suitable for multi-MB payloads (tarballs, archives) where buffering the whole body in memory would be a real cost.
- **`request(method, url) -> (StatusCode, Vec<u8>)`** — buffered convenience. For GETs, this is `get_stream` collected into a `Vec`. For non-GET methods, it forwards to `reqwest` and skips the cache.

## On-disc format

Each cached response is stored as a pair of files in `cache_dir`, named by the SHA-256 of the canonical URL:

- **`<hash>.meta`** — a small JSON file with status, `fetched_at`, `expires_at`, `must_revalidate`, `etag`, `last_modified`.
- **`<hash>.body`** — the raw response bytes. No framing, no header — `File::open` and stream.

Writes are atomic: each file is written to a `.tmp` sibling then renamed into place. A partially-downloaded body never becomes visible as `<hash>.body`.

## MISS flow: download-to-tmp, then stream

On a MISS, the cache takes option (1) of a choice we had to make: how to tee the upstream body to both disc and caller. We download the full body to `<hash>.body.tmp` first, rename it to `<hash>.body` once the transfer is complete, then open that finished file and hand the caller a stream from it.

Consequences:

- **Memory-bounded.** The body never lives in a `Vec<u8>` in memory — it flows straight to disc, and the caller's stream reads back from the file.
- **Atomic.** If the upstream stream errors mid-transfer or the caller drops, the tmp file is removed and nothing pollutes the cache.
- **Simple cancellation semantics.** "The entry on disc is either complete or absent" — no third state to reason about.
- **Trade-off: no early bytes.** The caller sees no bytes until the whole download completes. Fine for tarballs that can't be consumed partially anyway; less good if you wanted byte-level progress output *during* the download.

## Future enhancement: tee-on-the-fly

A different MISS strategy — option (2) — is to wrap the upstream stream so each chunk is written to `<hash>.body.tmp` *and* yielded to the caller immediately. Same memory bound, but bytes flow through as they arrive rather than only after the download completes.

Reasons it isn't currently implemented:

- **Cancellation semantics are hairy.** If the caller drops the stream partway through, do we abandon the tmp file or keep writing to complete the cache entry? Each answer has a cost.
- **Error paths multiply.** A failure can happen on the read side (upstream), the write side (disc) or the caller side (dropped). Option (1) collapses these into "did the download complete or not"; tee-on-the-fly has to handle each independently.
- **The UX win is narrow for this use case.** Tarballs and similar archives can't be consumed partially, so showing early bytes to the caller doesn't help. The win is mostly about byte-level progress reporting during the download, which can be layered on later via a small reporter wrapper over option (1) without committing to tee-on-the-fly at the cache boundary.

Worth revisiting if a caller appears that genuinely needs streaming-while-downloading behaviour (e.g. a streaming parser consuming JSON lines as they arrive).

## Cache key

The cache key is currently **URL origin + path + sorted query pairs**. Mostly yes this is a reasonable design choice, with a few real caveats worth flagging.

### What's right about it

- **Sorting the query pairs** handles the common case where `?a=1&b=2` and `?b=2&a=1` are semantically identical. Without this you'd get duplicate entries for the same resource.
- **Stripping the fragment** is correct — fragments are never sent to the server, so they can't affect the response.
- **Including the origin** (scheme + host + port) means `http://` and `https://` get separate entries, and two hosts can't collide.

### Where it's wrong or risky

1. **HTTP's rule is that the URL is opaque.** From the server's perspective, `?a=1&b=2` and `?b=2&a=1` are *not required* to return the same response. Most servers treat query order as irrelevant, but some (signed URLs, certain REST APIs, query-string caches behind a CDN) don't. Reordering the keys when computing the cache key is a correctness bet that's almost always safe but not formally guaranteed.

2. **Duplicate keys get silently collapsed.** `?tag=a&tag=b` is a valid repeated key. Sorting flattens it correctly (both pairs survive), but if you later swapped to a `HashMap<String, String>` representation you'd lose one — worth knowing the shape matters.

3. **Host casing and default ports.** `HTTPS://Example.com/` and `https://example.com:443/` are equivalent by RFC 3986 but produce different strings. `url::Url` normalizes host case and strips default ports on parse, so you get this for free here — but it's a property of the library, not of your key function.

4. **Percent-encoding.** `?q=hello%20world` vs `?q=hello+world` — in a query string these mean the same thing, but your key treats them as different. Also `?q=%2F` vs `?q=/` in the path. `url::Url` does *some* normalization but isn't exhaustive.

5. **Case sensitivity in the path.** `/Foo` ≠ `/foo` by the spec, and your key respects that, which is correct. But if you're caching for a case-insensitive server (IIS defaults, some S3 setups) you could over-fetch.

6. **The `Vary` header is the big one.** A response can say `Vary: Accept-Language, Authorization` meaning "the body depends on these request headers." A URL-only key will serve an English response to a French caller, or one user's data to another. Real HTTP caches key on URL + the subset of request headers listed in `Vary`. For a naive pull-through talking to simple APIs this rarely bites, but it's the biggest correctness gap.

7. **Auth headers and cookies.** Related to `Vary`: if your requests carry a bearer token or session cookie, responses are often per-user. Without `Vary` handling you'd leak one user's data to another. Fine if this client is single-user; dangerous otherwise.

8. **Method isn't in the key.** You only cache GETs today, so this is moot — but if you extended to cache `HEAD` or conditional `GET`s as separate things, you'd want method in the key.

### Practical verdict

For a single-user CLI hitting well-behaved public APIs: the current key is fine and the `Vary`/auth risks don't materialize. For anything multi-user, anything behind auth or anything where correctness matters more than hit rate, the honest next step is honoring `Vary` — that's the one gap that can produce *wrong* answers rather than just *missed* cache opportunities.

## Log lines

The cache emits a single log line per request describing what it did. The vocabulary:

- **`MISS, fetching upstream`** — no entry on disc at all. Do a full `GET` and store the result.
- **`HIT (status ..., fresh for Xs)`** — entry is within its TTL. Return it with no network call.
- **`STALE, revalidating upstream`** — stale, but the entry has an `ETag` or `Last-Modified`, so we send a conditional request. A `304` reuses the stored body for free; a `200` replaces it.
- **`HIT (304 Not Modified, revalidated)`** — the outcome when a revalidation succeeds.
- **`STALE (no validators), refetching upstream`** — we have a cached entry, but it isn't fresh *and* has no cheap way to confirm it's still current, so we do a full refetch. See below.
- **`upstream returned <status>, serving stale entry (stale-if-error)`** — upstream failed with a transient error but we have a cached entry, so we serve it instead of propagating the failure. See [Stale-if-error](#stale-if-error).
- **`upstream returned <status>, not updating cache`** — upstream returned a non-success status we don't recover from (e.g. `404`, `400`); the error is passed through to the caller and the existing cache entry is preserved but not updated.
- **`<METHOD> <url> -> bypassing cache`** — non-GET request; the cache is skipped entirely.

### `STALE (no validators), refetching upstream`

This log line is the one worth explaining in detail. Breaking it down:

- **STALE** — we have a cached entry, but it isn't "fresh" by our rules. An entry is fresh only if it has an `expires_at` in the future *and* `must_revalidate` is false. Any other state is stale.
- **(no validators)** — the cached response didn't include an `ETag` or a `Last-Modified` header. Those are what make a cheap revalidation possible: `If-None-Match: "<etag>"` or `If-Modified-Since: <date>` lets the server respond with a bodyless `304 Not Modified`, and we keep using the stored body. Without them, there's no conditional request to send.
- **refetching upstream** — so we do the only thing left: a plain `GET` and pull the whole body down again, then overwrite the entry.

#### When you'd see it in practice

Endpoints that don't send `Cache-Control: max-age=...`, `Expires`, `ETag` or `Last-Modified`. Every subsequent call is a full refetch, because the server gave the cache nothing to work with. In that situation the cache has essentially zero value beyond letting the program keep running offline with a stale body (which this cache doesn't currently do).

If you want to extract *some* value from those responses, you have two options: serve the stale entry for a configured grace period (a client-side "heuristic freshness" or `stale-while-revalidate`), or impose a client-side default TTL for origins you trust. Both are deliberate correctness-for-hit-rate trades, which is why the current implementation is pessimistic and just refetches.

## Stale-if-error

When the cache attempts to revalidate (or refetch) a stale entry and the upstream returns a transient failure, the cache falls back to serving the stale entry rather than propagating the error. This is RFC 5861's `stale-if-error` behaviour, enabled unconditionally — a caller hitting the library is far better served by a slightly-out-of-date response than by a hard failure.

**Which statuses trigger the fallback:**

- **`5xx`** — `500 Internal Server Error`, `502 Bad Gateway`, `503 Service Unavailable`, `504 Gateway Timeout`, etc. Upstream outages.
- **`429 Too Many Requests`** — explicit rate limit.
- **`403 Forbidden`** — included because some upstreams (notably GitHub's REST API) use `403` for rate-limit refusals, not just genuine auth failures. Misclassifying a real auth failure as a transient error is the cost; for the workloads this library is aimed at (public APIs, no per-user auth), the exchange is worth it.

**Which statuses don't:**

- **`4xx` other than `429`/`403`** — `400`, `401`, `404`, etc. These represent a deliberate response about the resource itself (it doesn't exist, the request is malformed, etc.), so substituting a stale copy would be misleading. The error is passed through to the caller.

**Interaction with revalidation.** When the cache has an `ETag` and the upstream answers `304 Not Modified`, that's a revalidation success, not an error — the normal revalidation path runs and we refresh `fetched_at` on the existing entry. Stale-if-error only kicks in for genuine failure responses.

**Observability.** Every fallback emits a `WARN`-level log line (`serving stale entry (stale-if-error)`) so you can tell the cache masked an error. The log line names the upstream status, so rate-limit refusals are still visible in the output.

**No cached entry, no fallback.** On a MISS that hits one of these errors, the cache has nothing to serve, so the error is returned to the caller as normal.

## Eviction

**`pydl-cache` intentionally does not implement eviction.** The cache grows without bound. Deciding this deliberately, rather than drifting into it, keeps the library's behaviour simple and predictable: an entry, once written, stays on disc until it's overwritten by a successful refetch of the same URL, or until the user deletes the cache directory.

What the current code *does* do that you might confuse with eviction:

- **Overwrite on refetch.** When a URL is refetched, the existing entry is replaced by `rename(tmp, path)`. That keeps a single URL's entry from growing, but doesn't remove anything else.
- **Skip caching for `no-store`.** Responses with `Cache-Control: no-store` are returned to the caller but never written. That's avoidance, not eviction.

What it does *not* do:

- **No TTL-based cleanup.** An expired entry stays on disc forever. The freshness check (`expires_at > now`) only decides whether to *use* the entry; a stale one is either overwritten by a successful refetch or ignored, not deleted.
- **No size or count cap.** There's no LRU, no max-bytes, no max-entries bookkeeping. The cache dir can grow to whatever the sum of all distinct-URL responses adds up to.
- **No index.** We'd need one to evict without scanning every file.
- **No lifetime bound.** Even a URL you hit once and never again persists indefinitely.

### Possible strategies

If eviction is added later, the usual options in rough order of complexity:

1. **Opportunistic TTL sweep on startup** — walk `cache_dir`, read each meta, unlink anything where `expires_at < now`. Cheap, but doesn't bound size for entries that keep revalidating.
2. **Size cap with LRU** — track access time (stat `mtime` or a field in meta), and on each write, if total size > budget, delete oldest until under. Needs either a full dir scan or a sidecar index.
3. **Max age sweep** — a hard "nothing older than N days" rule regardless of `expires_at`. Simplest size bound for long-lived caches.

Each is a different trade-off between implementation cost, runtime overhead and how tightly you can bound disc usage. For the current use case (a single-user CLI with bounded upstream URL variety) none have been needed.

## See also

- [`../README.md`](../README.md) — workspace overview.
- [`../DEV.md`](../DEV.md) — workspace-level developer guide.
- [`DEV.md`](./DEV.md) — crate-specific developer notes.
- [`../pydl/README.md`](../pydl/README.md) — the user-facing binary that exercises this cache across all of its subcommands.
