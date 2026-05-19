use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use log::debug;
use reqwest::StatusCode;
use reqwest::header::{CACHE_CONTROL, ETAG, EXPIRES, HeaderMap, HeaderValue, LAST_MODIFIED};

use crate::entry::{EntryMeta, EntryPaths, file_len};

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheControl {
    pub max_age: Option<u64>,
    pub must_revalidate: bool,
    pub no_store: bool,
}

pub fn parse_cache_control(headers: &HeaderMap) -> CacheControl {
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

pub fn parse_expires(headers: &HeaderMap) -> Option<u64> {
    let v = headers.get(EXPIRES)?.to_str().ok()?;
    httpdate::parse_http_date(v)
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

fn compute_expires_at(cc: &CacheControl, headers: &HeaderMap, now: u64) -> Option<u64> {
    cc.max_age
        .map(|s| now.saturating_add(s))
        .or_else(|| parse_expires(headers))
}

pub fn build_meta(status: StatusCode, headers: &HeaderMap, now: u64) -> EntryMeta {
    let cc = parse_cache_control(headers);
    EntryMeta {
        status: status.as_u16(),
        fetched_at: now,
        expires_at: compute_expires_at(&cc, headers, now),
        must_revalidate: cc.must_revalidate,
        etag: header_string(headers, ETAG),
        last_modified: header_string(headers, LAST_MODIFIED),
    }
}

pub fn update_meta_from_headers(meta: &mut EntryMeta, headers: &HeaderMap, now: u64) {
    let cc = parse_cache_control(headers);
    meta.fetched_at = now;
    meta.expires_at = compute_expires_at(&cc, headers, now).or(meta.expires_at);
    meta.must_revalidate = cc.must_revalidate;
    if let Some(v) = header_string(headers, ETAG) {
        meta.etag = Some(v);
    }
    if let Some(v) = header_string(headers, LAST_MODIFIED) {
        meta.last_modified = Some(v);
    }
}

pub fn header_string(headers: &HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string)
}

pub fn check_fresh_hit(
    url: &str,
    min_freshness_secs: u64,
    existing: Option<&(EntryPaths, EntryMeta)>,
    now: u64,
) -> Result<Option<(StatusCode, Option<u64>, std::path::PathBuf)>> {
    if let Some((paths, meta)) = existing
        && !meta.must_revalidate
    {
        let server_expiry = meta.expires_at;
        let floor_expiry =
            (min_freshness_secs > 0).then(|| meta.fetched_at.saturating_add(min_freshness_secs));
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
            let len = file_len(&paths.body);
            return Ok(Some((status, len, paths.body.clone())));
        }
    }
    Ok(None)
}
