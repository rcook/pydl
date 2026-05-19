mod client;
mod entry;
mod freshness;
mod rate_limit;
mod stream;

use std::pin::Pin;

use anyhow::Result;
use bytes::Bytes;
use futures_util::Stream;
pub use reqwest::{Method, StatusCode};

pub use crate::client::CachingClient;

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    Hit,
    Revalidated,
    Downloaded,
    StaleIfError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    RateLimit(String),
    StaleIfError {
        upstream_status: StatusCode,
    },
    Retry {
        reason: String,
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
    },
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use reqwest::header::{HeaderMap, HeaderValue};
    use tempfile::TempDir;
    use url::Url;
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::entry::EntryMeta;
    use crate::freshness::{build_meta, parse_cache_control, parse_expires, unix_now};
    use crate::rate_limit::{humanize_duration, is_stale_if_error, rate_limit_message};
    use crate::stream::collect_stream;

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
        let h = headers(&[]);
        assert!(rate_limit_message(StatusCode::FORBIDDEN, &h, 1000).is_none());
    }

    #[test]
    fn rate_limit_message_unspecified_wait() {
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
        crate::freshness::update_meta_from_headers(&mut m, &h, 500);
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
        crate::freshness::update_meta_from_headers(&mut m, &h, 1);
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
        crate::entry::write_meta(&path, &meta).unwrap();
        let got = crate::entry::read_meta(&path).unwrap().unwrap();
        assert_eq!(got.status, 200);
        assert_eq!(got.etag.as_deref(), Some("\"x\""));
    }

    #[test]
    fn read_meta_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        let got = crate::entry::read_meta(&dir.path().join("nope.meta")).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn read_meta_corrupt_errors() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.meta");
        fs::write(&path, b"not valid json").unwrap();
        assert!(crate::entry::read_meta(&path).is_err());
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
        let (s1, b1, _) = client.request(Method::GET, &url).await.unwrap();
        let (s2, b2, _) = client.request(Method::GET, &url).await.unwrap();
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
        let (_, b1, _) = client.request(Method::GET, &url).await.unwrap();
        let (s2, b2, _) = client.request(Method::GET, &url).await.unwrap();
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
        let (_, b1, _) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(b1, b"good");

        let (s2, b2, _) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(s2, 200);
        assert_eq!(b2, b"good");

        let canonical = CachingClient::canonical_url(&Url::parse(&url).unwrap());
        let paths = client.entry_paths(&canonical);
        let meta = crate::entry::read_meta(&paths.meta).unwrap().unwrap();
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
        let (status, body, _) = client.request(Method::GET, &url).await.unwrap();
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
        let (status, body, _) = client.request(Method::GET, &url).await.unwrap();
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
        let (status, body, _) = client.request(Method::GET, &url).await.unwrap();
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
        let (status, body, _) = client.request(Method::GET, &url).await.unwrap();
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
        let (s1, _, _) = client.request(Method::POST, &url).await.unwrap();
        let (s2, _, _) = client.request(Method::POST, &url).await.unwrap();
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
        let (_, b1, _) = client.request(Method::GET, &url).await.unwrap();
        let (_, b2, _) = client.request(Method::GET, &url).await.unwrap();
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
        let (status, outcome, _len, stream) = client.get_stream(&url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(outcome, CacheOutcome::Downloaded);
        let collected = collect_stream(stream).await.unwrap();
        assert_eq!(collected, b"hello tarball");

        let canonical = CachingClient::canonical_url(&Url::parse(&url).unwrap());
        let paths = client.entry_paths(&canonical);
        assert!(paths.body.exists());
        assert!(paths.meta.exists());
        assert!(!paths.tmp_body().exists());
        assert_eq!(fs::read(&paths.body).unwrap(), b"hello tarball");

        let (_, outcome2, _len2, stream2) = client.get_stream(&url).await.unwrap();
        assert_eq!(outcome2, CacheOutcome::Hit);
        let collected2 = collect_stream(stream2).await.unwrap();
        assert_eq!(collected2, b"hello tarball");
    }

    #[tokio::test]
    async fn hit_path_streams_from_disc_without_buffering_all_bytes() {
        use futures_util::StreamExt;

        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        let server = MockServer::start().await;

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
        let (_, _, _, s1) = client.get_stream(&url).await.unwrap();
        assert_eq!(collect_stream(s1).await.unwrap().len(), big_body.len());

        let (status, _, _, mut stream) = client.get_stream(&url).await.unwrap();
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
        let (_, b1, _) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(b1, b"body-v1");

        let (status, b2, _) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(b2, b"body-v2");

        let canonical = CachingClient::canonical_url(&Url::parse(&url).unwrap());
        let paths = client.entry_paths(&canonical);
        assert_eq!(fs::read(&paths.body).unwrap(), b"body-v2");
        let meta = crate::entry::read_meta(&paths.meta).unwrap().unwrap();
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

        let (status, body, _) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"fresh-but-volatile");

        let canonical = CachingClient::canonical_url(&Url::parse(&url).unwrap());
        let paths = client.entry_paths(&canonical);
        assert_eq!(fs::read(&paths.body).unwrap(), b"cached");
        let meta = crate::entry::read_meta(&paths.meta).unwrap().unwrap();
        assert_eq!(meta.etag.as_deref(), Some("\"v1\""));
    }

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
        });

        format!("http://{addr}")
    }

    #[tokio::test]
    async fn truncated_download_on_empty_cache_leaves_no_files_and_errors() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);

        let base = serve_truncated_once(b"0123456789", 100).await;
        let url = format!("{base}/r");

        let result = client.request(Method::GET, &url).await;
        assert!(result.is_err(), "expected error from truncated download");

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
        crate::entry::write_meta(&paths.meta, &meta).unwrap();

        let result = client.request(Method::GET, &url).await;
        assert!(result.is_err(), "expected error from truncated download");

        assert_eq!(fs::read(&paths.body).unwrap(), b"cached-good");
        let meta_after = crate::entry::read_meta(&paths.meta).unwrap().unwrap();
        assert_eq!(meta_after.etag.as_deref(), Some("\"v1\""));
        assert!(!paths.tmp_body().exists(), "tmp body must be cleaned up");
    }

    #[tokio::test]
    async fn connection_refused_returns_error() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let url = format!("http://{addr}/r");
        let result = client.request(Method::GET, &url).await;
        assert!(result.is_err(), "expected connection error, got {result:?}");
    }

    #[tokio::test]
    async fn connection_refused_does_not_stale_if_error() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let url = format!("http://{addr}/r");

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
        crate::entry::write_meta(&paths.meta, &meta).unwrap();

        let result = client.request(Method::GET, &url).await;
        assert!(
            result.is_err(),
            "connection failure must error even when a cached entry exists"
        );
    }

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
        crate::entry::write_meta(&paths.meta, &meta).unwrap();
        assert!(paths.meta.exists());
        assert!(!paths.body.exists());

        let (status, body, _) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"fresh");
        assert!(paths.body.exists());
        let refreshed = crate::entry::read_meta(&paths.meta).unwrap().unwrap();
        assert!(
            refreshed.etag.is_none(),
            "meta should have been overwritten"
        );
    }

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
        let (_, b1, _) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(b1, b"body-v1");
        let (status, b2, _) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(b2, b"body-v1");
    }

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
        let (status, _, _, mut stream) = client.get_stream(&url).await.unwrap();
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
        let (status, body, _) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"ok");
    }

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
        let canonical = CachingClient::canonical_url(&Url::parse(&url).unwrap());
        let paths = client.entry_paths(&canonical);
        let mut meta = crate::entry::read_meta(&paths.meta).unwrap().unwrap();
        let now = unix_now();
        meta.fetched_at = now - 60;
        meta.expires_at = Some(now - 59);
        crate::entry::write_meta(&paths.meta, &meta).unwrap();

        let (status, body, _) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"cached");
    }

    #[tokio::test]
    async fn longer_server_max_age_wins_over_shorter_floor() {
        let dir = TempDir::new().unwrap();
        let client = CachingClient::new(dir.path())
            .unwrap()
            .with_min_freshness_secs(1);
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
        let (status, body, _) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"cached");
    }

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

        Mock::given(method("GET"))
            .and(path("/r"))
            .and(header_exists("if-none-match"))
            .respond_with(ResponseTemplate::new(304))
            .expect(1)
            .mount(&server)
            .await;

        let url = format!("{}/r", server.uri());
        client.request(Method::GET, &url).await.unwrap();
        let (status, body, _) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"cached");
    }

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
        let (status, body, _) = client.request(Method::GET, &url).await.unwrap();
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
        let (status, body, _) = client.request(Method::GET, &url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"payload");
    }

    #[tokio::test]
    async fn evict_is_idempotent_for_uncached_url() {
        let dir = TempDir::new().unwrap();
        let client = make_client(&dir);
        client.evict("https://x.test/never-fetched").unwrap();
    }
}
