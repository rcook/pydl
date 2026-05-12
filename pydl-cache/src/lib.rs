use std::fs;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt};
use log::{debug, info, warn};
use reqwest::header::{
    CACHE_CONTROL, ETAG, EXPIRES, HeaderMap, HeaderValue, IF_MODIFIED_SINCE, IF_NONE_MATCH,
    LAST_MODIFIED, RETRY_AFTER,
};
use reqwest::{Client, Response};
pub use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio_util::io::ReaderStream;
use url::Url;

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;

#[derive(Serialize, Deserialize, Clone)]
struct EntryMeta {
    status: u16,
    fetched_at: u64,
    expires_at: Option<u64>,
    must_revalidate: bool,
    etag: Option<String>,
    last_modified: Option<String>,
}

struct EntryPaths {
    meta: PathBuf,
    body: PathBuf,
}

impl EntryPaths {
    fn tmp_body(&self) -> PathBuf {
        self.body.with_extension("body.tmp")
    }
}

pub struct CachingClient {
    inner: Client,
    cache_dir: PathBuf,
    min_freshness_secs: u64,
}

impl CachingClient {
    pub fn new(cache_dir: impl Into<PathBuf>) -> Result<Self> {
        Self::with_user_agent(cache_dir, None)
    }

    pub fn with_user_agent(
        cache_dir: impl Into<PathBuf>,
        user_agent: Option<&str>,
    ) -> Result<Self> {
        let cache_dir = cache_dir.into();
        fs::create_dir_all(&cache_dir)
            .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;
        let mut builder = Client::builder();
        if let Some(ua) = user_agent {
            builder = builder.user_agent(ua);
        }
        let inner = builder.build().context("building reqwest client")?;
        Ok(Self {
            inner,
            cache_dir,
            min_freshness_secs: 0,
        })
    }

    /// Set a client-side minimum freshness window. Cache entries are considered
    /// fresh for at least this many seconds past their `fetched_at`, even if
    /// the upstream's `Cache-Control` / `Expires` says otherwise. The server's
    /// TTL wins if it's longer. Entries marked `must-revalidate` or with no
    /// body are always honoured as-is — this only overrides the freshness
    /// check on entries we'd otherwise treat as stale.
    #[must_use]
    pub const fn with_min_freshness_secs(mut self, secs: u64) -> Self {
        self.min_freshness_secs = secs;
        self
    }

    fn canonical_url(url: &Url) -> String {
        let mut pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        pairs.sort();

        let mut canonical = url.clone();
        canonical.set_query(None);
        canonical.set_fragment(None);

        let query = pairs
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");

        if query.is_empty() {
            canonical.to_string()
        } else {
            format!("{canonical}?{query}")
        }
    }

    fn entry_paths(&self, canonical: &str) -> EntryPaths {
        let digest = Sha256::digest(canonical.as_bytes());
        let stem = self.cache_dir.join(format!("{digest:x}"));
        EntryPaths {
            meta: stem.with_extension("meta"),
            body: stem.with_extension("body"),
        }
    }

    fn read_meta(path: &Path) -> Result<Option<EntryMeta>> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("reading cache meta"),
        };
        let meta: EntryMeta = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing meta at {}", path.display()))?;
        Ok(Some(meta))
    }

    fn write_meta(path: &Path, meta: &EntryMeta) -> Result<()> {
        let tmp = path.with_extension("meta.tmp");
        fs::write(&tmp, serde_json::to_vec(meta)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    fn load_entry(&self, canonical: &str) -> Result<Option<(EntryPaths, EntryMeta)>> {
        let paths = self.entry_paths(canonical);
        let Some(meta) = Self::read_meta(&paths.meta)? else {
            return Ok(None);
        };
        if !paths.body.exists() {
            warn!(
                "cache meta without body at {}, ignoring",
                paths.body.display()
            );
            return Ok(None);
        }
        Ok(Some((paths, meta)))
    }

    /// Return the filesystem path of a cached response body for `url` if one
    /// is stored and the stored meta parses cleanly. Does **not** hit the
    /// network, does not revalidate, does not refetch.
    ///
    /// Callers that need strictly-offline access to a previously-downloaded
    /// asset (e.g. `pydl install` reading an archive that `pydl download`
    /// populated) should prefer this over [`Self::get_stream`] /
    /// [`Self::request`], which will refetch on cache miss or staleness.
    ///
    /// Returns `Ok(None)` when the cache is cold for this URL or the meta
    /// file exists without a body.
    pub fn cached_body_path(&self, url: &str) -> Result<Option<PathBuf>> {
        let parsed = Url::parse(url).with_context(|| format!("invalid url: {url}"))?;
        let canonical = Self::canonical_url(&parsed);
        let body = self.load_entry(&canonical)?.map(|(paths, _)| paths.body);
        match &body {
            Some(p) => debug!("cached_body_path({url}) -> {}", p.display()),
            None => debug!("cached_body_path({url}) -> <none>"),
        }
        Ok(body)
    }

    /// Remove every on-disc artifact for `url`: meta, body, and any tmp body
    /// left behind by an in-progress write.
    ///
    /// Idempotent: a URL that was never cached is a no-op. Use this when a
    /// caller-side integrity check (e.g. SHA-256 verification of a download)
    /// catches that the cached bytes are wrong, so the next request actually
    /// refetches rather than re-serving the bad entry as a HIT.
    pub fn evict(&self, url: &str) -> Result<()> {
        let parsed = Url::parse(url).with_context(|| format!("invalid url: {url}"))?;
        let canonical = Self::canonical_url(&parsed);
        let paths = self.entry_paths(&canonical);
        for p in [&paths.meta, &paths.body, &paths.tmp_body()] {
            match fs::remove_file(p) {
                Ok(()) => debug!("evict({url}) -> removed {}", p.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("removing {}", p.display())),
            }
        }
        Ok(())
    }

    pub async fn get_stream(&self, url: &str) -> Result<(StatusCode, ByteStream)> {
        let parsed = Url::parse(url).with_context(|| format!("invalid url: {url}"))?;
        let canonical = Self::canonical_url(&parsed);
        let existing = self.load_entry(&canonical)?;
        let now = unix_now();

        if let Some((paths, meta)) = &existing
            && !meta.must_revalidate
        {
            let server_expiry = meta.expires_at;
            let floor_expiry = (self.min_freshness_secs > 0)
                .then(|| meta.fetched_at.saturating_add(self.min_freshness_secs));
            let effective_expiry = match (server_expiry, floor_expiry) {
                (Some(s), Some(f)) => Some(s.max(f)),
                (s, f) => s.or(f),
            };
            if let Some(exp) = effective_expiry
                && exp > now
            {
                let status = StatusCode::from_u16(meta.status)?;
                let ttl_remaining = exp - now;
                debug!(
                    "GET {url} -> HIT (status {status}, fresh for {ttl_remaining}s, body {})",
                    paths.body.display()
                );
                return Ok((status, open_stream(&paths.body).await?));
            }
        }

        let mut req = self.inner.get(parsed);
        let has_validators = existing
            .as_ref()
            .is_some_and(|(_, m)| m.etag.is_some() || m.last_modified.is_some());
        if let Some((_, meta)) = &existing {
            if let Some(etag) = &meta.etag {
                req = req.header(IF_NONE_MATCH, etag);
            }
            if let Some(lm) = &meta.last_modified {
                req = req.header(IF_MODIFIED_SINCE, lm);
            }
        }

        let paths = self.entry_paths(&canonical);
        if existing.is_none() {
            debug!(
                "GET {url} -> MISS, fetching upstream (body will be {})",
                paths.body.display()
            );
        } else if has_validators {
            debug!(
                "GET {url} -> STALE, revalidating upstream (body {})",
                paths.body.display()
            );
        } else {
            debug!(
                "GET {url} -> STALE (no validators), refetching upstream (body {})",
                paths.body.display()
            );
        }

        let resp = self.inner.execute(req.build()?).await?;

        if resp.status() == StatusCode::NOT_MODIFIED {
            let (paths, meta) =
                existing.ok_or_else(|| anyhow!("got 304 but have no cached entry for {url}"))?;
            let mut meta = meta;
            update_meta_from_headers(&mut meta, resp.headers(), now);
            Self::write_meta(&paths.meta, &meta)?;
            let status = StatusCode::from_u16(meta.status)?;
            debug!(
                "GET {url} -> HIT (304 Not Modified, revalidated, body {})",
                paths.body.display()
            );
            return Ok((status, open_stream(&paths.body).await?));
        }

        let status = resp.status();
        let headers = resp.headers().clone();

        if !status.is_success() {
            if let Some(msg) = rate_limit_message(status, &headers, now) {
                info!("GET {url} -> {msg}");
            }
            if is_stale_if_error(status)
                && let Some((existing_paths, existing_meta)) = existing
            {
                warn!(
                    "GET {url} -> upstream returned {status}, serving stale entry (stale-if-error, body {})",
                    existing_paths.body.display()
                );
                let cached_status = StatusCode::from_u16(existing_meta.status)?;
                return Ok((cached_status, open_stream(&existing_paths.body).await?));
            }
            if existing.is_some() {
                warn!("GET {url} -> upstream returned {status}, not updating cache");
            }
            return Ok((status, passthrough_stream(resp)));
        }

        if parse_cache_control(&headers).no_store {
            return Ok((status, passthrough_stream(resp)));
        }

        download_to_tmp(&paths, resp).await?;
        let meta = build_meta(status, &headers, now);
        Self::write_meta(&paths.meta, &meta)?;
        Ok((status, open_stream(&paths.body).await?))
    }

    pub async fn request(&self, method: Method, url: &str) -> Result<(StatusCode, Vec<u8>)> {
        if method != Method::GET {
            debug!("{method} {url} -> bypassing cache");
            let parsed = Url::parse(url).with_context(|| format!("invalid url: {url}"))?;
            let resp = self.inner.request(method, parsed).send().await?;
            let status = resp.status();
            let body = resp.bytes().await?.to_vec();
            return Ok((status, body));
        }

        let (status, stream) = self.get_stream(url).await?;
        let body = collect_stream(stream).await?;
        Ok((status, body))
    }
}

async fn collect_stream(mut stream: ByteStream) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
    }
    Ok(buf)
}

async fn open_stream(path: &Path) -> Result<ByteStream> {
    let file = File::open(path)
        .await
        .with_context(|| format!("opening cache body {}", path.display()))?;
    let s = ReaderStream::new(file).map(|r| r.map_err(anyhow::Error::from));
    Ok(Box::pin(s))
}

fn passthrough_stream(resp: Response) -> ByteStream {
    Box::pin(resp.bytes_stream().map_err(anyhow::Error::from))
}

async fn download_to_tmp(paths: &EntryPaths, resp: Response) -> Result<()> {
    let tmp = paths.tmp_body();
    match stream_response_to_path(resp, &tmp).await {
        Ok(()) => tokio::fs::rename(&tmp, &paths.body)
            .await
            .with_context(|| format!("renaming {} -> {}", tmp.display(), paths.body.display())),
        Err(e) => {
            // Body may be partially written; remove the tmp file so the next
            // call has a clean slate. A stray tmp from a crash is harmless —
            // it's only reused on a successful write of the same URL — but
            // cleaning up here keeps `cache info` byte counts honest.
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

/// Stream the response body into `dest`, flushing before returning. The file
/// is created (and on success, fully populated) but not renamed — the caller
/// owns the final-name semantics.
async fn stream_response_to_path(resp: Response, dest: &Path) -> Result<()> {
    let file = File::create(dest)
        .await
        .with_context(|| format!("creating {}", dest.display()))?;
    let mut writer = BufWriter::new(file);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading upstream body")?;
        writer
            .write_all(&chunk)
            .await
            .context("writing cache body")?;
    }
    writer.flush().await.context("flushing cache body")?;
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn build_meta(status: StatusCode, headers: &HeaderMap, now: u64) -> EntryMeta {
    let cc = parse_cache_control(headers);
    let expires_at = cc
        .max_age
        .map(|s| now.saturating_add(s))
        .or_else(|| parse_expires(headers));

    EntryMeta {
        status: status.as_u16(),
        fetched_at: now,
        expires_at,
        must_revalidate: cc.must_revalidate,
        etag: header_string(headers, ETAG),
        last_modified: header_string(headers, LAST_MODIFIED),
    }
}

fn update_meta_from_headers(meta: &mut EntryMeta, headers: &HeaderMap, now: u64) {
    let cc = parse_cache_control(headers);
    meta.fetched_at = now;
    meta.expires_at = cc
        .max_age
        .map(|s| now.saturating_add(s))
        .or_else(|| parse_expires(headers))
        .or(meta.expires_at);
    meta.must_revalidate = cc.must_revalidate;
    if let Some(v) = header_string(headers, ETAG) {
        meta.etag = Some(v);
    }
    if let Some(v) = header_string(headers, LAST_MODIFIED) {
        meta.last_modified = Some(v);
    }
}

fn header_string(headers: &HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string)
}

/// Parsed view of a `Cache-Control` header. All fields default to "absent" /
/// `false` when the header is missing or unparseable.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CacheControl {
    max_age: Option<u64>,
    must_revalidate: bool,
    no_store: bool,
}

fn parse_cache_control(headers: &HeaderMap) -> CacheControl {
    let mut cc = CacheControl::default();
    let Some(v) = headers
        .get(CACHE_CONTROL)
        .and_then(|v: &HeaderValue| v.to_str().ok())
    else {
        return cc;
    };
    for part in v.split(',').map(str::trim) {
        let lower = part.to_ascii_lowercase();
        if lower == "no-store" {
            cc.no_store = true;
            cc.must_revalidate = true;
        } else if lower == "no-cache" || lower == "must-revalidate" {
            cc.must_revalidate = true;
        } else if let Some(rest) = lower.strip_prefix("max-age=")
            && let Ok(n) = rest.trim().parse::<u64>()
        {
            cc.max_age = Some(n);
        }
    }
    cc
}

fn parse_expires(headers: &HeaderMap) -> Option<u64> {
    let v = headers.get(EXPIRES)?.to_str().ok()?;
    httpdate::parse_http_date(v)
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// Whether an upstream error is the kind we'd rather hide behind a cached
/// response if we have one. 5xx outages, rate limiting (429) and 403s that
/// look like rate-limit refusals all qualify.
fn is_stale_if_error(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::FORBIDDEN
}

/// If `status` + `headers` look like a rate-limit refusal (mostly GitHub-shaped:
/// 403/429 carrying `X-RateLimit-Remaining: 0` plus `X-RateLimit-Reset`, or any
/// response with `Retry-After`), return a one-line description that includes an
/// estimated wait time. `now` is the current unix-epoch timestamp.
fn rate_limit_message(status: StatusCode, headers: &HeaderMap, now: u64) -> Option<String> {
    let is_rate_status = status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::FORBIDDEN;
    let remaining_zero = headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        == Some(0);
    let reset_at = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let retry_after_secs = headers
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .and_then(|v| {
            // `Retry-After` is either delta-seconds or an HTTP-date.
            v.parse::<u64>().ok().or_else(|| {
                httpdate::parse_http_date(v)
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs().saturating_sub(now))
            })
        });

    let wait_secs = match (retry_after_secs, reset_at) {
        (Some(s), _) => Some(s),
        (None, Some(r)) => Some(r.saturating_sub(now)),
        _ => None,
    };

    let looks_rate_limited = (is_rate_status && remaining_zero)
        || status == StatusCode::TOO_MANY_REQUESTS
        || retry_after_secs.is_some();

    if !looks_rate_limited {
        return None;
    }

    let scope = headers
        .get("x-ratelimit-resource")
        .and_then(|v| v.to_str().ok())
        .map(|s| format!(" (scope: {s})"))
        .unwrap_or_default();

    Some(wait_secs.map_or_else(
        || format!("rate-limited (HTTP {status}){scope} — retry-after unspecified"),
        |s| {
            format!(
                "rate-limited (HTTP {status}){scope} — try again in ~{}",
                humanize_duration(s)
            )
        },
    ))
}

fn humanize_duration(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m{:02}s", secs % 60);
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    format!("{hours}h{rem_mins:02}m")
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;
    use tempfile::TempDir;
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn rate_limit_message_github_403_with_reset() {
        let h = headers(&[
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset", "1700"),
            ("x-ratelimit-resource", "core"),
        ]);
        let msg =
            rate_limit_message(StatusCode::FORBIDDEN, &h, 1000).expect("should detect rate limit");
        assert!(msg.contains("rate-limited"), "got: {msg}");
        assert!(msg.contains("HTTP 403"), "got: {msg}");
        assert!(msg.contains("scope: core"), "got: {msg}");
        // 700 seconds = 11m40s
        assert!(msg.contains("11m40s"), "got: {msg}");
    }

    #[test]
    fn rate_limit_message_429_with_retry_after_seconds() {
        let h = headers(&[("retry-after", "45")]);
        let msg = rate_limit_message(StatusCode::TOO_MANY_REQUESTS, &h, 1000)
            .expect("should detect rate limit");
        assert!(msg.contains("rate-limited"), "got: {msg}");
        assert!(msg.contains("45s"), "got: {msg}");
    }

    #[test]
    fn rate_limit_message_403_without_remaining_zero_is_ignored() {
        // Plain 403 (e.g. private repo, bad token) — no rate-limit headers.
        let h = headers(&[]);
        assert!(rate_limit_message(StatusCode::FORBIDDEN, &h, 1000).is_none());
    }

    #[test]
    fn rate_limit_message_unspecified_wait() {
        // Hit the rate-limit shape but no reset/retry-after info.
        let h = headers(&[("x-ratelimit-remaining", "0")]);
        let msg =
            rate_limit_message(StatusCode::FORBIDDEN, &h, 1000).expect("should detect rate limit");
        assert!(msg.contains("retry-after unspecified"), "got: {msg}");
    }

    #[test]
    fn rate_limit_message_500_is_ignored() {
        let h = headers(&[]);
        assert!(rate_limit_message(StatusCode::INTERNAL_SERVER_ERROR, &h, 1000).is_none());
    }

    #[test]
    fn humanize_duration_buckets() {
        assert_eq!(humanize_duration(0), "0s");
        assert_eq!(humanize_duration(45), "45s");
        assert_eq!(humanize_duration(60), "1m00s");
        assert_eq!(humanize_duration(125), "2m05s");
        assert_eq!(humanize_duration(3600), "1h00m");
        assert_eq!(humanize_duration(3725), "1h02m");
    }

    #[test]
    fn canonical_url_sorts_query_pairs() {
        let a = CachingClient::canonical_url(&Url::parse("https://x.test/p?b=2&a=1").unwrap());
        let b = CachingClient::canonical_url(&Url::parse("https://x.test/p?a=1&b=2").unwrap());
        assert_eq!(a, b);
        assert_eq!(a, "https://x.test/p?a=1&b=2");
    }

    #[test]
    fn canonical_url_strips_fragment() {
        let u = CachingClient::canonical_url(&Url::parse("https://x.test/p?a=1#frag").unwrap());
        assert_eq!(u, "https://x.test/p?a=1");
    }

    #[test]
    fn canonical_url_without_query() {
        let u = CachingClient::canonical_url(&Url::parse("https://x.test/p").unwrap());
        assert_eq!(u, "https://x.test/p");
    }

    #[test]
    fn canonical_url_preserves_repeated_keys() {
        let u = CachingClient::canonical_url(&Url::parse("https://x.test/p?t=b&t=a").unwrap());
        assert_eq!(u, "https://x.test/p?t=a&t=b");
    }

    #[test]
    fn canonical_url_different_origins_differ() {
        let a = CachingClient::canonical_url(&Url::parse("http://x.test/p").unwrap());
        let b = CachingClient::canonical_url(&Url::parse("https://x.test/p").unwrap());
        assert_ne!(a, b);
    }

    #[test]
    fn entry_paths_are_deterministic_and_unique() {
        let dir = TempDir::new().unwrap();
        let c = CachingClient::new(dir.path()).unwrap();
        let p1 = c.entry_paths("https://x.test/a");
        let p2 = c.entry_paths("https://x.test/a");
        let p3 = c.entry_paths("https://x.test/b");
        assert_eq!(p1.meta, p2.meta);
        assert_eq!(p1.body, p2.body);
        assert_ne!(p1.meta, p3.meta);
        assert!(p1.meta.starts_with(dir.path()));
        assert_eq!(p1.meta.extension().and_then(|s| s.to_str()), Some("meta"));
        assert_eq!(p1.body.extension().and_then(|s| s.to_str()), Some("body"));
    }

    #[test]
    fn parse_cache_control_max_age() {
        let h = headers(&[("cache-control", "public, max-age=120")]);
        let cc = parse_cache_control(&h);
        assert_eq!(cc.max_age, Some(120));
        assert!(!cc.must_revalidate);
        assert!(!cc.no_store);
    }

    #[test]
    fn parse_cache_control_no_cache() {
        let h = headers(&[("cache-control", "no-cache")]);
        let cc = parse_cache_control(&h);
        assert_eq!(cc.max_age, None);
        assert!(cc.must_revalidate);
        assert!(!cc.no_store);
    }

    #[test]
    fn parse_cache_control_must_revalidate() {
        let h = headers(&[("cache-control", "max-age=60, must-revalidate")]);
        let cc = parse_cache_control(&h);
        assert_eq!(cc.max_age, Some(60));
        assert!(cc.must_revalidate);
        assert!(!cc.no_store);
    }

    #[test]
    fn parse_cache_control_no_store_sets_flag_and_forces_revalidate() {
        let h = headers(&[("cache-control", "no-store, max-age=60")]);
        let cc = parse_cache_control(&h);
        // `no-store` forces revalidation and is reported on its own field.
        // `max-age` is still parsed verbatim — call sites consult `no_store`
        // to decide whether to persist the body, regardless of `max_age`.
        assert!(cc.no_store);
        assert!(cc.must_revalidate);
        assert_eq!(cc.max_age, Some(60));
    }

    #[test]
    fn parse_cache_control_absent() {
        let h = headers(&[]);
        let cc = parse_cache_control(&h);
        assert_eq!(cc.max_age, None);
        assert!(!cc.must_revalidate);
        assert!(!cc.no_store);
    }

    #[test]
    fn parse_cache_control_ignores_garbage_max_age() {
        let h = headers(&[("cache-control", "max-age=not-a-number")]);
        let cc = parse_cache_control(&h);
        assert_eq!(cc.max_age, None);
        assert!(!cc.must_revalidate);
        assert!(!cc.no_store);
    }

    #[test]
    fn parse_expires_valid_http_date() {
        let h = headers(&[("expires", "Thu, 01 Jan 1970 00:01:00 GMT")]);
        assert_eq!(parse_expires(&h), Some(60));
    }

    #[test]
    fn parse_expires_invalid_date() {
        let h = headers(&[("expires", "not a date")]);
        assert_eq!(parse_expires(&h), None);
    }

    #[test]
    fn parse_expires_absent() {
        let h = headers(&[]);
        assert_eq!(parse_expires(&h), None);
    }

    #[test]
    fn no_store_detection_via_parse_cache_control() {
        assert!(parse_cache_control(&headers(&[("cache-control", "no-store")])).no_store);
        assert!(
            parse_cache_control(&headers(&[(
                "cache-control",
                "public, no-store, max-age=0"
            )]))
            .no_store
        );
        assert!(!parse_cache_control(&headers(&[("cache-control", "max-age=60")])).no_store);
        assert!(!parse_cache_control(&headers(&[])).no_store);
    }

    #[test]
    fn build_meta_prefers_max_age_over_expires() {
        let h = headers(&[
            ("cache-control", "max-age=30"),
            ("expires", "Thu, 01 Jan 1970 00:01:00 GMT"),
        ]);
        let m = build_meta(StatusCode::OK, &h, 1000);
        assert_eq!(m.expires_at, Some(1030));
        assert!(!m.must_revalidate);
    }

    #[test]
    fn build_meta_falls_back_to_expires() {
        let h = headers(&[("expires", "Thu, 01 Jan 1970 00:01:00 GMT")]);
        let m = build_meta(StatusCode::OK, &h, 1000);
        assert_eq!(m.expires_at, Some(60));
    }

    #[test]
    fn build_meta_saturates_on_pathological_max_age() {
        let h = headers(&[("cache-control", &format!("max-age={}", u64::MAX))]);
        let m = build_meta(StatusCode::OK, &h, 1000);
        assert_eq!(m.expires_at, Some(u64::MAX));
    }

    #[test]
    fn build_meta_captures_validators() {
        let h = headers(&[
            ("etag", "\"abc\""),
            ("last-modified", "Thu, 01 Jan 1970 00:00:00 GMT"),
        ]);
        let m = build_meta(StatusCode::OK, &h, 1000);
        assert_eq!(m.etag.as_deref(), Some("\"abc\""));
        assert_eq!(
            m.last_modified.as_deref(),
            Some("Thu, 01 Jan 1970 00:00:00 GMT")
        );
        assert_eq!(m.expires_at, None);
        assert_eq!(m.fetched_at, 1000);
        assert_eq!(m.status, 200);
    }

    #[test]
    fn update_meta_refreshes_fetched_at_and_keeps_old_etag_when_absent() {
        let mut m = EntryMeta {
            status: 200,
            fetched_at: 100,
            expires_at: Some(200),
            must_revalidate: false,
            etag: Some("\"old\"".into()),
            last_modified: None,
        };
        let h = headers(&[("cache-control", "max-age=50")]);
        update_meta_from_headers(&mut m, &h, 500);
        assert_eq!(m.fetched_at, 500);
        assert_eq!(m.expires_at, Some(550));
        assert_eq!(m.etag.as_deref(), Some("\"old\""));
    }

    #[test]
    fn update_meta_overrides_etag_when_present() {
        let mut m = EntryMeta {
            status: 200,
            fetched_at: 0,
            expires_at: None,
            must_revalidate: false,
            etag: Some("\"old\"".into()),
            last_modified: None,
        };
        let h = headers(&[("etag", "\"new\"")]);
        update_meta_from_headers(&mut m, &h, 1);
        assert_eq!(m.etag.as_deref(), Some("\"new\""));
    }

    #[test]
    fn meta_read_write_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("entry.meta");
        let meta = EntryMeta {
            status: 200,
            fetched_at: 10,
            expires_at: Some(20),
            must_revalidate: false,
            etag: Some("\"x\"".into()),
            last_modified: None,
        };
        CachingClient::write_meta(&path, &meta).unwrap();
        let got = CachingClient::read_meta(&path).unwrap().unwrap();
        assert_eq!(got.status, 200);
        assert_eq!(got.etag.as_deref(), Some("\"x\""));
    }

    #[test]
    fn read_meta_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        let got = CachingClient::read_meta(&dir.path().join("nope.meta")).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn read_meta_corrupt_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.meta");
        fs::write(&path, b"not valid json").unwrap();
        assert!(CachingClient::read_meta(&path).is_err());
    }

    fn make_client(dir: &TempDir) -> CachingClient {
        CachingClient::new(dir.path()).unwrap()
    }

    #[tokio::test]
    async fn miss_then_hit_with_max_age() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "max-age=60")
                    .set_body_bytes(b"payload".as_slice()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        let (s1, b1) = client.request(Method::GET, &url).await.unwrap();
        let (s2, b2) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(s1, 200);
        assert_eq!(s2, 200);
        assert_eq!(b1, b"payload");
        assert_eq!(b2, b"payload");
    }

    #[tokio::test]
    async fn query_order_does_not_cause_refetch() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "max-age=60")
                    .set_body_bytes(b"ok".as_slice()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let u1 = format!("{}/r?a=1&b=2", server.uri());
        let u2 = format!("{}/r?b=2&a=1", server.uri());
        client.request(Method::GET, &u1).await.unwrap();
        client.request(Method::GET, &u2).await.unwrap();
    }

    #[tokio::test]
    async fn etag_revalidation_serves_stored_body_on_304() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .and(header("if-none-match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1\"")
                    .set_body_bytes(b"body-v1".as_slice()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        let (_, b1) = client.request(Method::GET, &url).await.unwrap();
        let (s2, b2) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(b1, b"body-v1");
        assert_eq!(s2, 200);
        assert_eq!(b2, b"body-v1");
    }

    #[tokio::test]
    async fn no_store_response_is_not_cached() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "no-store")
                    .set_body_bytes(b"x".as_slice()),
            )
            .expect(2)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        client.request(Method::GET, &url).await.unwrap();
        client.request(Method::GET, &url).await.unwrap();
    }

    #[tokio::test]
    async fn server_error_serves_stale_if_error() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1\"")
                    .set_body_bytes(b"good".as_slice()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(ResponseTemplate::new(500).set_body_bytes(b"bad".as_slice()))
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        let (_, b1) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(b1, b"good");

        let (s2, b2) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(s2, 200);
        assert_eq!(b2, b"good");

        let canonical = CachingClient::canonical_url(&Url::parse(&url).unwrap());
        let paths = client.entry_paths(&canonical);
        let meta = CachingClient::read_meta(&paths.meta).unwrap().unwrap();
        assert_eq!(meta.etag.as_deref(), Some("\"v1\""));
        let body = fs::read(&paths.body).unwrap();
        assert_eq!(body, b"good");
    }

    #[tokio::test]
    async fn rate_limited_429_serves_stale_if_error() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1\"")
                    .set_body_bytes(b"cached".as_slice()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(ResponseTemplate::new(429).set_body_bytes(b"slow down".as_slice()))
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        client.request(Method::GET, &url).await.unwrap();
        let (status, body) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"cached");
    }

    #[tokio::test]
    async fn forbidden_403_serves_stale_if_error() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1\"")
                    .set_body_bytes(b"cached".as_slice()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(403).set_body_bytes(b"rate limit exceeded".as_slice()),
            )
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        client.request(Method::GET, &url).await.unwrap();
        let (status, body) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"cached");
    }

    #[tokio::test]
    async fn not_found_404_bubbles_through_without_serving_stale() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1\"")
                    .set_body_bytes(b"good".as_slice()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(ResponseTemplate::new(404).set_body_bytes(b"gone".as_slice()))
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        client.request(Method::GET, &url).await.unwrap();
        let (status, body) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 404);
        assert_eq!(body, b"gone");
    }

    #[tokio::test]
    async fn server_error_with_no_cached_entry_bubbles_through() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(ResponseTemplate::new(500).set_body_bytes(b"boom".as_slice()))
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        let (status, body) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 500);
        assert_eq!(body, b"boom");
    }

    #[test]
    fn is_stale_if_error_classifies() {
        assert!(is_stale_if_error(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_stale_if_error(StatusCode::BAD_GATEWAY));
        assert!(is_stale_if_error(StatusCode::GATEWAY_TIMEOUT));
        assert!(is_stale_if_error(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_stale_if_error(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_stale_if_error(StatusCode::FORBIDDEN));
        assert!(!is_stale_if_error(StatusCode::NOT_FOUND));
        assert!(!is_stale_if_error(StatusCode::BAD_REQUEST));
        assert!(!is_stale_if_error(StatusCode::UNAUTHORIZED));
        assert!(!is_stale_if_error(StatusCode::OK));
    }

    #[tokio::test]
    async fn post_bypasses_cache() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/r"))
            .respond_with(ResponseTemplate::new(201).set_body_bytes(b"created".as_slice()))
            .expect(2)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        let (s1, _) = client.request(Method::POST, &url).await.unwrap();
        let (s2, _) = client.request(Method::POST, &url).await.unwrap();
        assert_eq!(s1, 201);
        assert_eq!(s2, 201);

        let canonical = CachingClient::canonical_url(&Url::parse(&url).unwrap());
        let paths = client.entry_paths(&canonical);
        assert!(!paths.meta.exists());
        assert!(!paths.body.exists());
    }

    #[tokio::test]
    async fn no_cache_directive_forces_revalidation() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "no-cache")
                    .insert_header("etag", "\"v1\"")
                    .set_body_bytes(b"data".as_slice()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .and(header("if-none-match", "\"v1\""))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        let (_, b1) = client.request(Method::GET, &url).await.unwrap();
        let (_, b2) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(b1, b"data");
        assert_eq!(b2, b"data");
    }

    #[tokio::test]
    async fn response_without_freshness_is_refetched() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"x".as_slice()))
            .expect(2)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        client.request(Method::GET, &url).await.unwrap();
        client.request(Method::GET, &url).await.unwrap();
    }

    #[tokio::test]
    async fn get_stream_emits_bytes_and_persists_body_file() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "max-age=60")
                    .set_body_bytes(b"hello tarball".as_slice()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        let (status, stream) = client.get_stream(&url).await.unwrap();
        assert_eq!(status, 200);
        let collected = collect_stream(stream).await.unwrap();
        assert_eq!(collected, b"hello tarball");

        let canonical = CachingClient::canonical_url(&Url::parse(&url).unwrap());
        let paths = client.entry_paths(&canonical);
        assert!(paths.body.exists());
        assert!(paths.meta.exists());
        assert!(!paths.tmp_body().exists());
        assert_eq!(fs::read(&paths.body).unwrap(), b"hello tarball");

        let (_, stream2) = client.get_stream(&url).await.unwrap();
        let collected2 = collect_stream(stream2).await.unwrap();
        assert_eq!(collected2, b"hello tarball");
    }

    /// Verifies the HIT path streams bytes without buffering the whole body
    /// up-front. We poll the stream once and assert a chunk arrives; if the
    /// implementation regressed to reading the whole file into memory before
    /// yielding the first item, the stream would still work but a much larger
    /// body would sit in the buffer. To keep the test deterministic we use a
    /// small body and rely on `StreamExt::next` returning some chunk rather
    /// than None.
    #[tokio::test]
    async fn hit_path_streams_from_disc_without_buffering_all_bytes() {
        use futures_util::StreamExt;

        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        // ~128 KiB body — large enough that ReaderStream yields multiple chunks.
        let big_body = vec![b'x'; 128 * 1024];

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "max-age=60")
                    .set_body_bytes(big_body.clone()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        // Cold run populates the cache.
        let (_, s1) = client.get_stream(&url).await.unwrap();
        assert_eq!(collect_stream(s1).await.unwrap().len(), big_body.len());

        // Warm run — must be served from disc. Poll once and assert we receive
        // a non-terminal chunk, then drain the rest.
        let (status, mut stream) = client.get_stream(&url).await.unwrap();
        assert_eq!(status, 200);

        let first = stream
            .next()
            .await
            .expect("stream should yield at least one chunk")
            .expect("first chunk should not be an error");
        assert!(!first.is_empty(), "first chunk should carry bytes");
        assert!(
            first.len() < big_body.len(),
            "first chunk ({}) should be smaller than full body ({}) — if equal, the stream buffered everything",
            first.len(),
            big_body.len()
        );

        let mut total = first.len();
        while let Some(chunk) = stream.next().await {
            total += chunk.unwrap().len();
        }
        assert_eq!(total, big_body.len());
    }

    #[tokio::test]
    async fn revalidation_200_replaces_body_and_meta() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        // First call: returns v1 with an ETag. Marked must-revalidate so the
        // second call goes back to upstream rather than staying fresh.
        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "must-revalidate")
                    .insert_header("etag", "\"v1\"")
                    .set_body_bytes(b"body-v1".as_slice()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second call arrives with If-None-Match: "v1"; upstream returns a
        // fresh 200 with a new body and new ETag (i.e. the resource changed).
        Mock::given(method("GET"))
            .and(path("/r"))
            .and(header("if-none-match", "\"v1\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v2\"")
                    .set_body_bytes(b"body-v2".as_slice()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        let (_, b1) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(b1, b"body-v1");

        let (status, b2) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(b2, b"body-v2");

        let canonical = CachingClient::canonical_url(&Url::parse(&url).unwrap());
        let paths = client.entry_paths(&canonical);
        assert_eq!(fs::read(&paths.body).unwrap(), b"body-v2");
        let meta = CachingClient::read_meta(&paths.meta).unwrap().unwrap();
        assert_eq!(meta.etag.as_deref(), Some("\"v2\""));
    }

    #[tokio::test]
    async fn no_store_on_revalidation_preserves_existing_entry() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "must-revalidate")
                    .insert_header("etag", "\"v1\"")
                    .set_body_bytes(b"cached".as_slice()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .and(header("if-none-match", "\"v1\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "no-store")
                    .set_body_bytes(b"fresh-but-volatile".as_slice()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        client.request(Method::GET, &url).await.unwrap();

        // Caller gets the fresh-but-volatile body back.
        let (status, body) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"fresh-but-volatile");

        // On-disc entry is unchanged: original body and meta both preserved.
        let canonical = CachingClient::canonical_url(&Url::parse(&url).unwrap());
        let paths = client.entry_paths(&canonical);
        assert_eq!(fs::read(&paths.body).unwrap(), b"cached");
        let meta = CachingClient::read_meta(&paths.meta).unwrap().unwrap();
        assert_eq!(meta.etag.as_deref(), Some("\"v1\""));
    }

    /// Handcrafted TCP "server" that accepts one connection, reads the request
    /// line + headers, sends back a 200 response whose `Content-Length` header
    /// promises `promised_len` bytes but whose body only contains `actual_body`
    /// before closing the connection. Returns the bound address so the test can
    /// point reqwest at it.
    async fn serve_truncated_once(actual_body: &'static [u8], promised_len: usize) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let mut total = 0usize;
            loop {
                let n = socket.read(&mut buf[total..]).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                total += n;
                if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if total == buf.len() {
                    break;
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {promised_len}\r\nContent-Type: application/octet-stream\r\n\r\n"
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(actual_body).await;
            // Drop the socket: body stream ends before Content-Length is met.
        });

        format!("http://{addr}")
    }

    #[tokio::test]
    async fn truncated_download_on_empty_cache_leaves_no_files_and_errors() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);

        // Promise 100 bytes, send 10, close. reqwest must surface this as an
        // error when the stream is consumed.
        let base = serve_truncated_once(b"0123456789", 100).await;
        let url = format!("{base}/r");

        let result = client.request(Method::GET, &url).await;
        assert!(result.is_err(), "expected error from truncated download");

        // No meta, no body, no tmp body.
        let canonical = CachingClient::canonical_url(&Url::parse(&url).unwrap());
        let paths = client.entry_paths(&canonical);
        assert!(!paths.meta.exists(), "meta must not exist");
        assert!(!paths.body.exists(), "body must not exist");
        assert!(!paths.tmp_body().exists(), "tmp body must be cleaned up");
    }

    #[tokio::test]
    async fn truncated_download_preserves_existing_entry() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);

        let base = serve_truncated_once(b"0123456789", 100).await;
        let url = format!("{base}/r");

        // Seed an on-disc entry for this exact URL by writing meta + body
        // directly, marked stale so the next call revalidates.
        let canonical = CachingClient::canonical_url(&Url::parse(&url).unwrap());
        let paths = client.entry_paths(&canonical);
        fs::write(&paths.body, b"cached-good").unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let meta = EntryMeta {
            status: 200,
            fetched_at: now - 10_000,
            expires_at: Some(now - 1), // stale — forces revalidation
            must_revalidate: true,
            etag: Some("\"v1\"".into()),
            last_modified: None,
        };
        CachingClient::write_meta(&paths.meta, &meta).unwrap();

        // Call the cache. It'll send a conditional GET; our truncator ignores
        // it and returns the partial-200 response. The download should fail.
        let result = client.request(Method::GET, &url).await;
        assert!(result.is_err(), "expected error from truncated download");

        // Existing body and meta must remain untouched, and no tmp leftover.
        assert_eq!(fs::read(&paths.body).unwrap(), b"cached-good");
        let meta_after = CachingClient::read_meta(&paths.meta).unwrap().unwrap();
        assert_eq!(meta_after.etag.as_deref(), Some("\"v1\""));
        assert!(!paths.tmp_body().exists(), "tmp body must be cleaned up");
    }

    #[tokio::test]
    async fn connection_refused_returns_error() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);

        // Bind then immediately drop, freeing the port. The follow-up connect
        // attempt should fail with ECONNREFUSED on most platforms (or at worst
        // a routing/timeout error, which we still count as an error).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let url = format!("http://{addr}/r");
        let result = client.request(Method::GET, &url).await;
        assert!(result.is_err(), "expected connection error, got {result:?}");
    }

    #[tokio::test]
    async fn connection_refused_does_not_stale_if_error() {
        // Network-level failures (not HTTP status errors) bypass stale-if-error:
        // we never get a chance to see a status, so the cached entry — even if
        // present — is not served. This locks that behaviour in.
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let url = format!("http://{addr}/r");

        // Pre-seed a stale cache entry for this URL.
        let canonical = CachingClient::canonical_url(&Url::parse(&url).unwrap());
        let paths = client.entry_paths(&canonical);
        fs::write(&paths.body, b"cached-good").unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let meta = EntryMeta {
            status: 200,
            fetched_at: now - 10_000,
            expires_at: Some(now - 1),
            must_revalidate: true,
            etag: Some("\"v1\"".into()),
            last_modified: None,
        };
        CachingClient::write_meta(&paths.meta, &meta).unwrap();

        let result = client.request(Method::GET, &url).await;
        assert!(
            result.is_err(),
            "connection failure must error even when a cached entry exists"
        );
    }

    /// Gap 1: orphan-meta recovery. If `<hash>.meta` exists on disc but
    /// `<hash>.body` is missing, `load_entry` should warn and return `None`.
    /// The next call then treats this as a MISS and refetches cleanly.
    #[tokio::test]
    async fn orphan_meta_without_body_treated_as_miss() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "max-age=60")
                    .set_body_bytes(b"fresh".as_slice()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        let canonical = CachingClient::canonical_url(&Url::parse(&url).unwrap());
        let paths = client.entry_paths(&canonical);

        // Plant an orphan meta pointing at a body that doesn't exist.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let meta = EntryMeta {
            status: 200,
            fetched_at: now,
            expires_at: Some(now + 600),
            must_revalidate: false,
            etag: Some("\"stale\"".into()),
            last_modified: None,
        };
        CachingClient::write_meta(&paths.meta, &meta).unwrap();
        assert!(paths.meta.exists());
        assert!(!paths.body.exists());

        // Request should succeed, hitting the mock exactly once and populating
        // both files from the real response.
        let (status, body) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"fresh");
        assert!(paths.body.exists());
        let refreshed = CachingClient::read_meta(&paths.meta).unwrap().unwrap();
        assert!(
            refreshed.etag.is_none(),
            "meta should have been overwritten"
        );
    }

    /// Gap 2: revalidation path with only `Last-Modified`, no `ETag`.
    /// The cache should send `If-Modified-Since` and honour a 304.
    #[tokio::test]
    async fn last_modified_only_revalidation_sends_if_modified_since() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        let lm = "Wed, 01 Jan 2020 00:00:00 GMT";

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "must-revalidate")
                    .insert_header("last-modified", lm)
                    .set_body_bytes(b"body-v1".as_slice()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .and(header_exists("if-modified-since"))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        let (_, b1) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(b1, b"body-v1");
        let (status, b2) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(b2, b"body-v1");
    }

    /// Gap 3: verify the MISS path also streams from the finished body file
    /// rather than buffering the entire download in memory before returning
    /// the stream. Poll one chunk and assert it's strictly smaller than the
    /// full body — same shape as the HIT-side streaming test.
    #[tokio::test]
    async fn miss_path_streams_from_disc_without_buffering_all_bytes() {
        use futures_util::StreamExt;

        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        let big_body = vec![b'y'; 128 * 1024];

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "max-age=60")
                    .set_body_bytes(big_body.clone()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        let (status, mut stream) = client.get_stream(&url).await.unwrap();
        assert_eq!(status, 200);

        let first = stream
            .next()
            .await
            .expect("stream should yield at least one chunk")
            .expect("first chunk should not be an error");
        assert!(!first.is_empty());
        assert!(
            first.len() < big_body.len(),
            "first chunk ({}) should be smaller than full body ({}) — if equal, the MISS path buffered everything",
            first.len(),
            big_body.len()
        );

        let mut total = first.len();
        while let Some(chunk) = stream.next().await {
            total += chunk.unwrap().len();
        }
        assert_eq!(total, big_body.len());
    }

    /// Gap 4: `with_user_agent` should attach the provided UA to outbound
    /// requests. Verified by wiremock's `header` matcher — if the UA is wrong
    /// or absent, the mock won't match and the request will 404.
    #[tokio::test]
    async fn with_user_agent_sets_outbound_header() {
        let dir = TempDir::new().unwrap();
        let client = CachingClient::with_user_agent(dir.path(), Some("pydl-test/1.2.3")).unwrap();
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .and(header("user-agent", "pydl-test/1.2.3"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ok".as_slice()))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        let (status, body) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"ok");
    }

    /// The client-side min-freshness floor keeps an entry usable past the
    /// server's `max-age`. Here the server says 1 second but the client asks
    /// for a day — the second call, well past 1 s, should still HIT.
    #[tokio::test]
    async fn min_freshness_floor_extends_past_server_max_age() {
        let dir = TempDir::new().unwrap();
        let client = CachingClient::new(dir.path())
            .unwrap()
            .with_min_freshness_secs(24 * 60 * 60);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "max-age=1")
                    .set_body_bytes(b"cached".as_slice()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        client.request(Method::GET, &url).await.unwrap();
        // Manually rewind fetched_at so "now" is past the server's max-age
        // without actually waiting.
        let canonical = CachingClient::canonical_url(&Url::parse(&url).unwrap());
        let paths = client.entry_paths(&canonical);
        let mut meta = CachingClient::read_meta(&paths.meta).unwrap().unwrap();
        let now = unix_now();
        meta.fetched_at = now - 60;
        meta.expires_at = Some(now - 59); // server TTL already expired
        CachingClient::write_meta(&paths.meta, &meta).unwrap();

        // Floor-based freshness must keep this a HIT.
        let (status, body) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"cached");
    }

    /// When the server's TTL is *longer* than the client's floor, we keep the
    /// server TTL. This is tested by asserting the second call is a HIT with
    /// only the server's max-age driving it.
    #[tokio::test]
    async fn longer_server_max_age_wins_over_shorter_floor() {
        let dir = TempDir::new().unwrap();
        let client = CachingClient::new(dir.path())
            .unwrap()
            .with_min_freshness_secs(1); // 1-second floor
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "max-age=3600")
                    .set_body_bytes(b"cached".as_slice()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        client.request(Method::GET, &url).await.unwrap();
        // Second call is a HIT because of the server's 1-hour max-age, not
        // our tiny floor. We verify this indirectly via the single-call mock.
        let (status, body) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"cached");
    }

    /// `must_revalidate` is a correctness signal from the server and is never
    /// overridden by the client-side floor.
    #[tokio::test]
    async fn min_freshness_does_not_override_must_revalidate() {
        let dir = TempDir::new().unwrap();
        let client = CachingClient::new(dir.path())
            .unwrap()
            .with_min_freshness_secs(24 * 60 * 60);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "must-revalidate")
                    .insert_header("etag", "\"v1\"")
                    .set_body_bytes(b"cached".as_slice()),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Second call must revalidate (we answer 304).
        Mock::given(method("GET"))
            .and(path("/r"))
            .and(header_exists("if-none-match"))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        client.request(Method::GET, &url).await.unwrap();
        let (status, body) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"cached");
    }

    /// Floor-only freshness: the server sends no TTL at all, but the client
    /// sets a floor. The entry should be HIT within the window.
    #[tokio::test]
    async fn floor_alone_grants_freshness_when_server_sends_no_cache_headers() {
        let dir = TempDir::new().unwrap();
        let client = CachingClient::new(dir.path())
            .unwrap()
            .with_min_freshness_secs(24 * 60 * 60);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"cached".as_slice()))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        client.request(Method::GET, &url).await.unwrap();
        let (status, body) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"cached");
    }

    #[tokio::test]
    async fn cached_body_path_is_none_when_cold() {
        let dir = TempDir::new().unwrap();
        let client = CachingClient::new(dir.path()).unwrap();
        let path = client
            .cached_body_path("https://x.test/never-fetched")
            .unwrap();
        assert!(path.is_none());
    }

    #[tokio::test]
    async fn cached_body_path_returns_body_after_fetch() {
        let dir = TempDir::new().unwrap();
        let client = CachingClient::new(dir.path()).unwrap();
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/asset.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"payload".as_slice()))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/asset.bin", server.uri());
        client.request(Method::GET, &url).await.unwrap();

        let body_path = client
            .cached_body_path(&url)
            .unwrap()
            .expect("body present after fetch");
        let bytes = std::fs::read(&body_path).unwrap();
        assert_eq!(bytes, b"payload");
        // The read must not have triggered a refetch; the mock is `expect(1)`.
    }

    #[tokio::test]
    async fn evict_removes_meta_and_body() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "max-age=60")
                    .set_body_bytes(b"payload".as_slice()),
            )
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        client.request(Method::GET, &url).await.unwrap();

        let canonical = CachingClient::canonical_url(&Url::parse(&url).unwrap());
        let paths = client.entry_paths(&canonical);
        assert!(paths.meta.exists());
        assert!(paths.body.exists());

        client.evict(&url).unwrap();
        assert!(!paths.meta.exists(), "meta must be gone after evict");
        assert!(!paths.body.exists(), "body must be gone after evict");
    }

    /// Locks in the property that motivated `evict`: a poisoned cache entry
    /// can be cleared so the next request actually hits upstream again,
    /// rather than re-serving the bad bytes.
    #[tokio::test]
    async fn evict_then_request_refetches() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/r"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "max-age=3600")
                    .set_body_bytes(b"payload".as_slice()),
            )
            .expect(2)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        client.request(Method::GET, &url).await.unwrap();
        client.evict(&url).unwrap();
        // Without the evict, this would be a HIT (server's max-age=3600); the
        // `expect(2)` on the mock asserts upstream was actually re-hit.
        let (status, body) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"payload");
    }

    #[tokio::test]
    async fn evict_is_idempotent_for_uncached_url() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        // Never fetched — eviction should be a clean no-op.
        client.evict("https://x.test/never-fetched").unwrap();
    }
}
