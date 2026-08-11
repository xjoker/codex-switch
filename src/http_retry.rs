use std::time::Duration;

use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde_json::Value;
use tracing::debug;

const LOCAL_RATE_LIMIT_BASE: Duration = Duration::from_secs(30);
const MAX_RATE_LIMIT_DELAY: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DelaySource {
    Header,
    Body,
    LocalBackoff,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RateLimitDecision {
    pub delay: Duration,
    pub source: DelaySource,
    pub limit_type: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplaySafety {
    Idempotent,
    UnsafePost,
}

impl ReplaySafety {
    pub(crate) fn may_replay(self) -> bool {
        matches!(self, Self::Idempotent)
    }
}

pub(crate) struct BufferedResponse {
    pub status: reqwest::StatusCode,
    pub body: Vec<u8>,
}

pub(crate) async fn send(
    request: reqwest::RequestBuilder,
    safety: ReplaySafety,
) -> anyhow::Result<BufferedResponse> {
    const MAX_ATTEMPTS: u32 = 3;
    let mut attempt = 0;
    loop {
        let next = request
            .try_clone()
            .ok_or_else(|| anyhow::anyhow!("HTTP request body is not replayable"))?;
        let response = next.send().await?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.bytes().await?.to_vec();

        if status != reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Ok(BufferedResponse { status, body });
        }

        let decision = rate_limit_decision(&headers, &body, attempt);
        if !safety.may_replay() {
            debug!(
                "HTTP 429 on non-replayable request; waiting {:.3}s before returning ({:?})",
                decision.delay.as_secs_f64(),
                decision.source
            );
            tokio::time::sleep(with_jitter(decision.delay)).await;
            return Ok(BufferedResponse { status, body });
        }
        if attempt + 1 >= MAX_ATTEMPTS {
            return Ok(BufferedResponse { status, body });
        }

        debug!(
            "HTTP 429; retrying replay-safe request in {:.3}s ({:?})",
            decision.delay.as_secs_f64(),
            decision.source
        );
        tokio::time::sleep(with_jitter(decision.delay)).await;
        attempt += 1;
    }
}

pub(crate) async fn wait_after_429_headers(
    status: reqwest::StatusCode,
    headers: &HeaderMap,
    consecutive_429: u32,
) -> bool {
    if status != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return false;
    }
    let decision = rate_limit_decision(headers, &[], consecutive_429);
    debug!(
        "HTTP 429 on non-replayable request; waiting {:.3}s before returning ({:?})",
        decision.delay.as_secs_f64(),
        decision.source
    );
    tokio::time::sleep(with_jitter(decision.delay)).await;
    true
}

fn with_jitter(delay: Duration) -> Duration {
    use rand::RngExt;

    let max_jitter_ms = delay.as_millis().min(u128::from(u64::MAX)) as u64 / 5;
    if max_jitter_ms == 0 {
        return delay;
    }
    delay.saturating_add(Duration::from_millis(
        rand::rng().random_range(0..=max_jitter_ms),
    ))
}

pub(crate) fn rate_limit_decision(
    headers: &HeaderMap,
    body: &[u8],
    consecutive_429: u32,
) -> RateLimitDecision {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let limit_type = parsed
        .as_ref()
        .and_then(|value| value.pointer("/detail/type"))
        .and_then(Value::as_str)
        .map(str::to_string);

    if let Some(delay) = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_delay)
    {
        return RateLimitDecision {
            delay: delay.min(MAX_RATE_LIMIT_DELAY),
            source: DelaySource::Header,
            limit_type,
        };
    }

    if let Some(delay) = parsed.as_ref().and_then(delay_from_body) {
        return RateLimitDecision {
            delay: delay.min(MAX_RATE_LIMIT_DELAY),
            source: DelaySource::Body,
            limit_type,
        };
    }

    let multiplier = 1_u32 << consecutive_429.min(4);
    RateLimitDecision {
        delay: LOCAL_RATE_LIMIT_BASE
            .saturating_mul(multiplier)
            .min(MAX_RATE_LIMIT_DELAY),
        source: DelaySource::LocalBackoff,
        limit_type,
    }
}

fn delay_from_body(body: &Value) -> Option<Duration> {
    for pointer in [
        "/retry_after",
        "/retry_after_seconds",
        "/error/retry_after",
        "/error/retry_after_seconds",
    ] {
        if let Some(value) = body.pointer(pointer) {
            if let Some(seconds) = value.as_f64() {
                return checked_duration(seconds);
            }
            if let Some(value) = value.as_str().and_then(parse_delay) {
                return Some(value);
            }
        }
    }

    find_message(body).and_then(parse_message_delay)
}

fn find_message(value: &Value) -> Option<&str> {
    match value {
        Value::Object(map) => map
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| map.values().find_map(find_message)),
        Value::Array(values) => values.iter().find_map(find_message),
        _ => None,
    }
}

fn parse_message_delay(message: &str) -> Option<Duration> {
    let lowercase = message.to_ascii_lowercase();
    let start = lowercase.find("try again in ")? + "try again in ".len();
    let mut parts = message[start..].split_whitespace();
    let value = clean_delay_token(parts.next()?);
    if let Some(delay) = parse_delay(value) {
        return Some(delay);
    }
    let unit = clean_delay_token(parts.next()?);
    parse_delay(&format!("{value} {unit}"))
}

fn clean_delay_token(value: &str) -> &str {
    value
        .trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '.')
        .trim_end_matches('.')
}

fn parse_delay(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    if let Some((number, unit)) = value.split_once(' ')
        && matches!(unit.to_ascii_lowercase().as_str(), "second" | "seconds")
    {
        return checked_duration(number.parse().ok()?);
    }
    if let Some(milliseconds) = value.strip_suffix("ms") {
        return checked_duration(milliseconds.parse::<f64>().ok()? / 1_000.0);
    }
    let seconds = value.strip_suffix('s')?;
    checked_duration(seconds.parse().ok()?)
}

fn checked_duration(seconds: f64) -> Option<Duration> {
    Duration::try_from_secs_f64(seconds).ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    use super::*;

    #[test]
    fn server_retry_after_takes_precedence() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("45"));

        let decision = rate_limit_decision(&headers, b"{}", 0);

        assert_eq!(decision.delay, Duration::from_secs(45));
        assert_eq!(decision.source, DelaySource::Header);
    }

    #[test]
    fn connector_rate_limit_without_hint_uses_exponential_fallback() {
        let body = br#"{"detail":{"type":"connector_rate_limit","message":"Connector rate limit exceeded"}}"#;

        let delays: Vec<_> = (0..5)
            .map(|attempt| rate_limit_decision(&HeaderMap::new(), body, attempt).delay)
            .collect();

        assert_eq!(delays, [30, 60, 120, 240, 300].map(Duration::from_secs));
    }

    #[test]
    fn response_message_wait_hint_is_parsed() {
        let body =
            br#"{"error":{"code":"rate_limit_exceeded","message":"Please try again in 1.898s."}}"#;

        let decision = rate_limit_decision(&HeaderMap::new(), body, 0);

        assert_eq!(decision.delay, Duration::from_secs_f64(1.898));
        assert_eq!(decision.source, DelaySource::Body);
    }

    #[test]
    fn retry_safety_never_replays_unsafe_post() {
        assert!(!ReplaySafety::UnsafePost.may_replay());
        assert!(ReplaySafety::Idempotent.may_replay());
    }

    #[test]
    fn invalid_server_delay_falls_back_without_panicking() {
        for body in [
            br#"{"retry_after":-1}"#.as_slice(),
            br#"{"retry_after":"NaNs"}"#.as_slice(),
            br#"{"retry_after":"1e999s"}"#.as_slice(),
        ] {
            let decision = rate_limit_decision(&HeaderMap::new(), body, 0);
            assert_eq!(decision.delay, Duration::from_secs(30));
            assert_eq!(decision.source, DelaySource::LocalBackoff);
        }
    }
}
