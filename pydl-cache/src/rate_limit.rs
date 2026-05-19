use std::time::UNIX_EPOCH;

use reqwest::StatusCode;
use reqwest::header::{HeaderMap, RETRY_AFTER};

pub fn is_stale_if_error(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::FORBIDDEN
}

pub fn rate_limit_message(status: StatusCode, headers: &HeaderMap, now: u64) -> Option<String> {
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

pub fn humanize_duration(secs: u64) -> String {
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
