use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use log::debug;
use reqwest::header::{IF_MODIFIED_SINCE, IF_NONE_MATCH};
use reqwest::{Client, Method, Response, StatusCode};
use url::Url;

use crate::entry::{self, EntryMeta, EntryPaths, file_len, write_meta};
use crate::freshness::{
    self, build_meta, check_fresh_hit, parse_cache_control, update_meta_from_headers,
};
use crate::rate_limit::{is_stale_if_error, rate_limit_message};
use crate::stream::{collect_stream, download_to_tmp, open_stream, passthrough_stream};
use crate::{ByteStream, CacheOutcome, Notice};

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

    pub(crate) fn canonical_url(url: &Url) -> String {
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

    pub(crate) fn entry_paths(&self, canonical: &str) -> EntryPaths {
        entry::entry_paths(&self.cache_dir, canonical)
    }

    /// Return the filesystem path of a cached response body for `url` if one
    /// is stored and the stored meta parses cleanly. Does **not** hit the
    /// network, does not revalidate, does not refetch.
    pub fn cached_body_path(&self, url: &str) -> Result<Option<PathBuf>> {
        let parsed = Url::parse(url).with_context(|| format!("invalid url: {url}"))?;
        let canonical = Self::canonical_url(&parsed);
        let body = entry::load_entry(&self.cache_dir, &canonical)?.map(|(paths, _)| paths.body);
        match &body {
            Some(p) => debug!("cached_body_path({url}) -> {}", p.display()),
            None => debug!("cached_body_path({url}) -> <none>"),
        }
        Ok(body)
    }

    /// Remove every on-disc artifact for `url`: meta, body and any tmp body
    /// left behind by an in-progress write.
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

    pub async fn get_stream(
        &self,
        url: &str,
    ) -> Result<(StatusCode, CacheOutcome, Option<u64>, ByteStream)> {
        let mut notices = Vec::new();
        self.get_stream_inner(url, &mut notices).await
    }

    async fn get_stream_inner(
        &self,
        url: &str,
        notices: &mut Vec<Notice>,
    ) -> Result<(StatusCode, CacheOutcome, Option<u64>, ByteStream)> {
        let parsed = Url::parse(url).with_context(|| format!("invalid url: {url}"))?;
        let canonical = Self::canonical_url(&parsed);
        let existing = entry::load_entry(&self.cache_dir, &canonical)?;
        let now = freshness::unix_now();

        if let Some((status, len, body)) =
            check_fresh_hit(url, self.min_freshness_secs, existing.as_ref(), now)?
        {
            return Ok((status, CacheOutcome::Hit, len, open_stream(&body).await?));
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
            write_meta(&paths.meta, &meta)?;
            let status = StatusCode::from_u16(meta.status)?;
            debug!(
                "GET {url} -> HIT (304 Not Modified, revalidated, body {})",
                paths.body.display()
            );
            let len = file_len(&paths.body);
            return Ok((
                status,
                CacheOutcome::Revalidated,
                len,
                open_stream(&paths.body).await?,
            ));
        }

        let status = resp.status();
        let headers = resp.headers().clone();

        if !status.is_success() {
            return self
                .handle_upstream_error(url, status, &headers, now, existing, resp, notices)
                .await;
        }

        if parse_cache_control(&headers).no_store {
            let content_length = resp.content_length();
            return Ok((
                status,
                CacheOutcome::Downloaded,
                content_length,
                passthrough_stream(resp),
            ));
        }

        let content_length = resp.content_length();
        download_to_tmp(&paths, resp).await?;
        let meta = build_meta(status, &headers, now);
        write_meta(&paths.meta, &meta)?;
        Ok((
            status,
            CacheOutcome::Downloaded,
            content_length,
            open_stream(&paths.body).await?,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_upstream_error(
        &self,
        url: &str,
        status: StatusCode,
        headers: &reqwest::header::HeaderMap,
        now: u64,
        existing: Option<(EntryPaths, EntryMeta)>,
        resp: Response,
        notices: &mut Vec<Notice>,
    ) -> Result<(StatusCode, CacheOutcome, Option<u64>, ByteStream)> {
        if let Some(msg) = rate_limit_message(status, headers, now) {
            debug!("GET {url} -> {msg}");
            notices.push(Notice::RateLimit(msg));
        }
        if is_stale_if_error(status)
            && let Some((existing_paths, existing_meta)) = existing
        {
            debug!(
                "GET {url} -> upstream returned {status}, serving stale entry (stale-if-error, body {})",
                existing_paths.body.display()
            );
            notices.push(Notice::StaleIfError {
                upstream_status: status,
            });
            let cached_status = StatusCode::from_u16(existing_meta.status)?;
            let len = file_len(&existing_paths.body);
            return Ok((
                cached_status,
                CacheOutcome::StaleIfError,
                len,
                open_stream(&existing_paths.body).await?,
            ));
        }
        if existing.is_some() {
            debug!("GET {url} -> upstream returned {status}, not updating cache");
        }
        let content_length = resp.content_length();
        Ok((
            status,
            CacheOutcome::Downloaded,
            content_length,
            passthrough_stream(resp),
        ))
    }

    pub async fn request(
        &self,
        method: Method,
        url: &str,
    ) -> Result<(StatusCode, Vec<u8>, Vec<Notice>)> {
        if method != Method::GET {
            debug!("{method} {url} -> bypassing cache");
            let parsed = Url::parse(url).with_context(|| format!("invalid url: {url}"))?;
            let resp = self.inner.request(method, parsed).send().await?;
            let status = resp.status();
            let body = resp.bytes().await?.to_vec();
            return Ok((status, body, vec![]));
        }

        let mut notices = Vec::new();
        let (status, _outcome, _len, stream) = self.get_stream_inner(url, &mut notices).await?;
        let body = collect_stream(stream).await?;
        Ok((status, body, notices))
    }
}
