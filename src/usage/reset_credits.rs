use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use rand::Rng;
use serde_json::Value;
use tracing::debug;

use crate::auth::{self, format_reqwest_error};

use super::api::extract_error_summary;
use super::parse::parse_optional_u64;
use super::{ConsumedResetCredit, MAX_RETRIES, RETRY_DELAY, ResetCredit, UsageInfo};

const RESET_CREDITS_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
const RESET_CREDITS_CONSUME_URL: &str =
    "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume";
static RESET_CREDITS_FETCH_LIMITER: OnceLock<tokio::sync::Semaphore> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsumeFailureKind {
    DefinitelyNotConsumed,
    OutcomeUnknownAfterRequest,
}

#[derive(Debug)]
pub struct ConsumeResetCreditError {
    kind: ConsumeFailureKind,
    source: anyhow::Error,
}

impl ConsumeResetCreditError {
    fn not_consumed(source: impl Into<anyhow::Error>) -> Self {
        Self {
            kind: ConsumeFailureKind::DefinitelyNotConsumed,
            source: source.into(),
        }
    }

    fn outcome_unknown(source: impl Into<anyhow::Error>) -> Self {
        Self {
            kind: ConsumeFailureKind::OutcomeUnknownAfterRequest,
            source: source.into(),
        }
    }

    pub fn definitely_not_consumed(&self) -> bool {
        self.kind == ConsumeFailureKind::DefinitelyNotConsumed
    }

    pub fn outcome_unknown_after_request(&self) -> bool {
        self.kind == ConsumeFailureKind::OutcomeUnknownAfterRequest
    }

    pub fn user_facing_unknown_message(&self, alias: &str) -> String {
        debug_assert!(self.outcome_unknown_after_request());
        format!("{alias}: reset-card consumption may have occurred; verify before retry")
    }
}

impl std::fmt::Display for ConsumeResetCreditError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for ConsumeResetCreditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

fn reset_credits_url() -> String {
    if let Ok(url) = std::env::var("CS_RESET_CREDITS_URL") {
        return url;
    }
    if let Ok(url) = std::env::var("CS_USAGE_URL")
        && let Some(base) = url.strip_suffix("/usage")
    {
        return format!("{base}/rate-limit-reset-credits");
    }
    RESET_CREDITS_URL.to_string()
}

fn reset_credits_consume_url() -> String {
    if let Ok(url) = std::env::var("CS_RESET_CREDITS_CONSUME_URL") {
        return url;
    }
    if std::env::var("CS_RESET_CREDITS_URL").is_ok() {
        return format!("{}/consume", reset_credits_url().trim_end_matches('/'));
    }
    RESET_CREDITS_CONSUME_URL.to_string()
}

pub(super) async fn enrich_reset_credits(
    alias: &str,
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    usage: &mut UsageInfo,
) {
    if !should_fetch_reset_credit_details(usage) {
        usage.reset_credits_error = None;
        return;
    }

    match fetch_reset_credits(client, access_token, account_id).await {
        Ok((available_count, credits)) => {
            if available_count.is_some() {
                usage.reset_credits_available_count = available_count;
            }
            if !credits.is_empty() {
                usage.reset_credits = credits;
            }
            usage.reset_credits_error = None;
        }
        Err(err) => {
            let msg = err.to_string();
            debug!("[{alias}] reset credits fetch failed: {msg}");
            if let Some(cached) = crate::cache::get_async(alias).await {
                retain_cached_reset_credits(usage, &cached);
            }
            usage.reset_credits_error = Some(extract_error_summary(&msg));
        }
    }
}

fn should_fetch_reset_credit_details(usage: &UsageInfo) -> bool {
    usage
        .reset_credits_available_count
        .is_some_and(|count| count > usage.reset_credits.len() as u64)
}

fn reset_credits_fetch_limiter() -> &'static tokio::sync::Semaphore {
    RESET_CREDITS_FETCH_LIMITER.get_or_init(|| tokio::sync::Semaphore::new(1))
}

fn retry_delay_for_response(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    attempt: u32,
) -> Duration {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        && let Some(seconds) = headers
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_secs(seconds.clamp(1, 30));
    }

    exponential_retry_delay(attempt)
}

fn exponential_retry_delay(attempt: u32) -> Duration {
    Duration::from_secs(RETRY_DELAY.as_secs().saturating_mul(1 << attempt.min(4)))
}

fn retain_cached_reset_credits(usage: &mut UsageInfo, cached: &UsageInfo) {
    if usage.reset_credits_available_count.is_none() {
        usage.reset_credits_available_count = cached.reset_credits_available_count;
    }
    if usage.reset_credits.is_empty()
        && usage.reset_credits_available_count != Some(0)
        && usage.reset_credits_available_count == cached.reset_credits_available_count
    {
        usage.reset_credits.clone_from(&cached.reset_credits);
    }
}

async fn fetch_reset_credits(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
) -> Result<(Option<u64>, Vec<ResetCredit>)> {
    fetch_reset_credits_at_url(client, access_token, account_id, &reset_credits_url()).await
}

async fn fetch_reset_credits_at_url(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    url: &str,
) -> Result<(Option<u64>, Vec<ResetCredit>)> {
    let _permit = reset_credits_fetch_limiter()
        .acquire()
        .await
        .expect("reset credits fetch limiter is never closed");

    for attempt in 0..MAX_RETRIES {
        let mut req = client
            .get(url)
            .bearer_auth(access_token)
            .header("Accept", "application/json")
            .header("OpenAI-Beta", "codex-1")
            .header("Originator", "Codex Desktop");

        if let Some(account_id) = account_id.filter(|s| !s.trim().is_empty()) {
            req = req.header("Chatgpt-Account-Id", account_id);
        }

        let resp = match req.send().await {
            Ok(resp) => resp,
            Err(error) if attempt + 1 < MAX_RETRIES => {
                debug!(
                    "reset credits fetch attempt {}/{} failed before response: {}",
                    attempt + 1,
                    MAX_RETRIES,
                    format_reqwest_error("request failed", &error)
                );
                tokio::time::sleep(exponential_retry_delay(attempt)).await;
                continue;
            }
            Err(error) => {
                return Err(format_reqwest_error("reset credits request failed", &error));
            }
        };
        let status = resp.status();
        if !status.is_success() {
            if (status.is_server_error() || status.as_u16() == 429) && attempt + 1 < MAX_RETRIES {
                let retry_delay = retry_delay_for_response(status, resp.headers(), attempt);
                debug!(
                    "reset credits fetch attempt {}/{} returned HTTP {status}; retrying in {:.1}s",
                    attempt + 1,
                    MAX_RETRIES,
                    retry_delay.as_secs_f64()
                );
                tokio::time::sleep(retry_delay).await;
                continue;
            }
            anyhow::bail!("reset credits request failed (HTTP {status})");
        }

        let body: Value = match resp.json().await {
            Ok(body) => body,
            Err(error) if attempt + 1 < MAX_RETRIES => {
                debug!(
                    "reset credits fetch attempt {}/{} returned invalid JSON: {error}",
                    attempt + 1,
                    MAX_RETRIES
                );
                tokio::time::sleep(exponential_retry_delay(attempt)).await;
                continue;
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "failed to parse reset credits response: {error}"
                ));
            }
        };
        let (available_count, credits, valid_shape) = parse_reset_credits_summary(&body);
        if !valid_shape {
            anyhow::bail!("reset credits response missing expected fields");
        }
        return Ok((available_count, credits));
    }

    unreachable!("reset credits retry loop always returns on its final attempt")
}

pub fn earliest_reset_credit(credits: &[ResetCredit]) -> Option<&ResetCredit> {
    credits.iter().min_by_key(|credit| {
        credit
            .expires_at
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|dt| dt.timestamp())
            .unwrap_or(i64::MAX)
    })
}

pub async fn fetch_earliest_reset_credit(alias: &str, profile_path: &Path) -> Result<ResetCredit> {
    let val = auth::read_auth(profile_path)?;
    let (access_token, _) = auth::extract_tokens(&val);
    let access_token = access_token
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("{alias}: auth.json missing access_token"))?;
    let account_id = crate::jwt::parse_account_info(&val).account_id;
    let client = auth::build_http_client()?;
    let (_, credits) = fetch_reset_credits(&client, &access_token, account_id.as_deref()).await?;

    earliest_reset_credit(&credits)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{alias}: no available reset cards"))
}

pub async fn consume_earliest_reset_credit(
    alias: &str,
    profile_path: &Path,
) -> std::result::Result<ConsumedResetCredit, ConsumeResetCreditError> {
    let val = auth::read_auth(profile_path).map_err(ConsumeResetCreditError::not_consumed)?;
    let (access_token, _) = auth::extract_tokens(&val);
    let access_token = access_token
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("{alias}: auth.json missing access_token"))
        .map_err(ConsumeResetCreditError::not_consumed)?;
    let account_id = crate::jwt::parse_account_info(&val).account_id;
    let client = auth::build_http_client().map_err(ConsumeResetCreditError::not_consumed)?;

    let (_, credits) = fetch_reset_credits(&client, &access_token, account_id.as_deref())
        .await
        .map_err(ConsumeResetCreditError::not_consumed)?;
    let credit = earliest_reset_credit(&credits)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{alias}: no available reset cards"))
        .map_err(ConsumeResetCreditError::not_consumed)?;

    consume_reset_credit(&client, &access_token, account_id.as_deref(), credit).await
}

async fn consume_reset_credit(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    credit: ResetCredit,
) -> std::result::Result<ConsumedResetCredit, ConsumeResetCreditError> {
    consume_reset_credit_at_url(
        client,
        access_token,
        account_id,
        credit,
        &reset_credits_consume_url(),
    )
    .await
}

async fn consume_reset_credit_at_url(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    credit: ResetCredit,
    url: &str,
) -> std::result::Result<ConsumedResetCredit, ConsumeResetCreditError> {
    // Generate once per user action. Any retry after an ambiguous transport/server
    // failure must identify the same logical redemption to the backend.
    let request_id = redeem_request_id();
    let mut outcome_may_have_changed = false;
    for attempt in 0..MAX_RETRIES {
        let mut req = client
            .post(url)
            .bearer_auth(access_token)
            .header("Accept", "application/json")
            .header("OpenAI-Beta", "codex-1")
            .header("Originator", "Codex Desktop")
            .json(&serde_json::json!({
                "credit_id": &credit.id,
                "redeem_request_id": &request_id,
            }));

        if let Some(account_id) = account_id.filter(|s| !s.trim().is_empty()) {
            req = req.header("Chatgpt-Account-Id", account_id);
        }

        let resp = match req.send().await {
            Ok(resp) => resp,
            Err(error) if attempt + 1 < MAX_RETRIES => {
                outcome_may_have_changed = true;
                debug!(
                    "reset credit consume attempt {}/{} failed before response: {}",
                    attempt + 1,
                    MAX_RETRIES,
                    format_reqwest_error("request failed", &error)
                );
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
            Err(error) => {
                return Err(ConsumeResetCreditError::outcome_unknown(
                    format_reqwest_error("reset credit consume request failed", &error),
                ));
            }
        };
        let status = resp.status();
        if !status.is_success() {
            if (status.is_server_error() || status.as_u16() == 429) && attempt + 1 < MAX_RETRIES {
                outcome_may_have_changed = true;
                debug!(
                    "reset credit consume attempt {}/{} returned HTTP {status}",
                    attempt + 1,
                    MAX_RETRIES
                );
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
            let error = anyhow::anyhow!("reset credit consume request failed (HTTP {status})");
            if status.is_client_error() && status.as_u16() != 429 && !outcome_may_have_changed {
                return Err(ConsumeResetCreditError::not_consumed(error));
            }
            return Err(ConsumeResetCreditError::outcome_unknown(error));
        }

        match resp.json::<Value>().await {
            Ok(body) => {
                return parse_consumed_reset_credit(&body, credit)
                    .map_err(ConsumeResetCreditError::outcome_unknown);
            }
            Err(error) if attempt + 1 < MAX_RETRIES => {
                outcome_may_have_changed = true;
                debug!(
                    "reset credit consume attempt {}/{} returned invalid JSON: {error}",
                    attempt + 1,
                    MAX_RETRIES
                );
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Err(error) => {
                return Err(ConsumeResetCreditError::outcome_unknown(anyhow::anyhow!(
                    "failed to parse reset credit consume response: {error}"
                )));
            }
        }
    }

    unreachable!("reset credit retry loop always returns on its final attempt")
}

fn parse_consumed_reset_credit(body: &Value, credit: ResetCredit) -> Result<ConsumedResetCredit> {
    let code = body
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("reset credit consume response missing code"))?;
    if code != "reset" {
        anyhow::bail!("reset credit was not consumed: {code}");
    }

    Ok(ConsumedResetCredit {
        credit,
        code: Some(code.to_string()),
        windows_reset: parse_optional_u64(body.get("windows_reset")),
        redeemed_at: body
            .get("credit")
            .and_then(|v| v.as_object())
            .and_then(|obj| {
                obj.get("redeemed_at")
                    .or_else(|| obj.get("redeemedAt"))
                    .and_then(|v| v.as_str())
            })
            .map(|s| s.to_string()),
    })
}

fn redeem_request_id() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let value = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &value[0..8],
        &value[8..12],
        &value[12..16],
        &value[16..20],
        &value[20..32]
    )
}

fn parse_reset_credit(value: &Value) -> Option<ResetCredit> {
    let obj = value.as_object()?;

    let reset_type = obj
        .get("reset_type")
        .or_else(|| obj.get("resetType"))
        .and_then(|v| v.as_str())
        .map(str::trim);
    if let Some(reset_type) = reset_type
        && reset_type != "codex_rate_limits"
    {
        return None;
    }

    let status = obj.get("status").and_then(|v| v.as_str()).map(str::trim);
    if let Some(status) = status
        && status != "available"
    {
        return None;
    }

    let expires_at = obj
        .get("expires_at")
        .or_else(|| obj.get("expiresAt"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let id = obj
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)?;
    let granted_at = obj
        .get("granted_at")
        .or_else(|| obj.get("grantedAt"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    Some(ResetCredit {
        id,
        granted_at,
        expires_at,
    })
}

pub(super) fn parse_reset_credits_summary(body: &Value) -> (Option<u64>, Vec<ResetCredit>, bool) {
    let Some(obj) = body.as_object() else {
        return (None, vec![], false);
    };

    let available_count = parse_optional_u64(
        obj.get("available_count")
            .or_else(|| obj.get("availableCount")),
    );
    let credits = obj
        .get("credits")
        .and_then(|v| v.as_array())
        .map(|items| items.iter().filter_map(parse_reset_credit).collect())
        .unwrap_or_default();
    let valid_shape = obj.contains_key("credits")
        || obj.contains_key("available_count")
        || obj.contains_key("availableCount");

    (available_count, credits, valid_shape)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn local_http_client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    #[test]
    fn test_reset_credit_without_expiry_is_preserved_and_sorted_last() {
        let expiring = parse_reset_credit(&json!({
            "id": "expiring",
            "status": "available",
            "expires_at": "2026-07-08T00:00:00Z"
        }))
        .unwrap();
        let no_expiry = parse_reset_credit(&json!({
            "id": "no-expiry",
            "status": "available",
            "expires_at": null
        }))
        .unwrap();
        let credits = vec![no_expiry, expiring];

        assert_eq!(credits[0].expires_at, None);
        assert_eq!(earliest_reset_credit(&credits).unwrap().id, "expiring");
    }

    #[test]
    fn empty_credit_id_is_filtered_before_earliest_credit_is_selected() {
        let (_, credits, valid_shape) = parse_reset_credits_summary(&json!({
            "credits": [
                {
                    "id": "  ",
                    "status": "available",
                    "expires_at": "2026-07-01T00:00:00Z"
                },
                {
                    "id": "valid-credit",
                    "status": "available",
                    "expires_at": "2026-08-01T00:00:00Z"
                }
            ]
        }));

        assert!(valid_shape);
        assert_eq!(earliest_reset_credit(&credits).unwrap().id, "valid-credit");
    }

    #[test]
    fn test_consume_outcome_only_accepts_reset() {
        let credit = ResetCredit {
            id: "credit-1".to_string(),
            granted_at: None,
            expires_at: None,
        };

        let consumed = parse_consumed_reset_credit(
            &json!({"code": "reset", "windows_reset": 2}),
            credit.clone(),
        )
        .unwrap();
        assert_eq!(consumed.code.as_deref(), Some("reset"));

        for code in ["nothing_to_reset", "no_credit", "already_redeemed"] {
            let error =
                parse_consumed_reset_credit(&json!({"code": code}), credit.clone()).unwrap_err();
            assert!(error.to_string().contains(code));
        }
    }

    #[tokio::test]
    async fn reset_credit_fetch_retries_a_transient_server_error() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&attempts);
        let app = axum::Router::new().route(
            "/credits",
            get(move || {
                let captured = Arc::clone(&captured);
                async move {
                    if captured.fetch_add(1, Ordering::SeqCst) == 0 {
                        StatusCode::SERVICE_UNAVAILABLE.into_response()
                    } else {
                        Json(json!({
                            "available_count": 1,
                            "credits": [{
                                "id": "credit-1",
                                "status": "available",
                                "expires_at": "2026-08-12T00:00:00Z"
                            }]
                        }))
                        .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = local_http_client();
        let result = fetch_reset_credits_at_url(
            &client,
            "access-token",
            None,
            &format!("http://{address}/credits"),
        )
        .await;
        server.abort();

        assert_eq!(attempts.load(Ordering::SeqCst), 2, "result: {result:?}");
        let (count, credits) = result.unwrap();
        assert_eq!(count, Some(1));
        assert_eq!(credits.len(), 1);
    }

    #[test]
    fn reset_credit_details_are_only_fetched_for_a_positive_reported_count() {
        let missing = UsageInfo::default();
        let zero = UsageInfo {
            reset_credits_available_count: Some(0),
            ..Default::default()
        };
        let positive = UsageInfo {
            reset_credits_available_count: Some(1),
            ..Default::default()
        };

        assert!(!should_fetch_reset_credit_details(&missing));
        assert!(!should_fetch_reset_credit_details(&zero));
        assert!(should_fetch_reset_credit_details(&positive));
    }

    #[tokio::test]
    async fn reset_credit_detail_fetches_are_serialized_across_accounts() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new().route(
            "/credits",
            get({
                let active = Arc::clone(&active);
                let maximum = Arc::clone(&maximum);
                move || {
                    let active = Arc::clone(&active);
                    let maximum = Arc::clone(&maximum);
                    async move {
                        let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
                        maximum.fetch_max(now_active, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Json(json!({"available_count": 1, "credits": []}))
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = format!("http://{address}/credits");
        let client = local_http_client();

        let (first, second) = tokio::join!(
            fetch_reset_credits_at_url(&client, "token-1", None, &url),
            fetch_reset_credits_at_url(&client, "token-2", None, &url),
        );
        server.abort();

        first.unwrap();
        second.unwrap();
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reset_credit_fetch_honors_retry_after_on_rate_limit() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&attempts);
        let app = axum::Router::new().route(
            "/credits",
            get(move || {
                let captured = Arc::clone(&captured);
                async move {
                    if captured.fetch_add(1, Ordering::SeqCst) == 0 {
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            [(reqwest::header::RETRY_AFTER.as_str(), "2")],
                            "rate limited",
                        )
                            .into_response()
                    } else {
                        Json(json!({"available_count": 1, "credits": []})).into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let started = Instant::now();
        let result = fetch_reset_credits_at_url(
            &local_http_client(),
            "access-token",
            None,
            &format!("http://{address}/credits"),
        )
        .await;
        server.abort();

        result.unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(started.elapsed() >= Duration::from_millis(1_900));
    }

    #[test]
    fn failed_reset_credit_refresh_retains_the_last_known_cards() {
        let cached = UsageInfo {
            reset_credits_available_count: Some(1),
            reset_credits: vec![ResetCredit {
                id: "cached-credit".to_string(),
                granted_at: None,
                expires_at: Some("2026-08-12T00:00:00Z".to_string()),
            }],
            ..Default::default()
        };
        let mut refreshed = UsageInfo::default();

        retain_cached_reset_credits(&mut refreshed, &cached);

        assert_eq!(refreshed.reset_credits_available_count, Some(1));
        assert_eq!(refreshed.reset_credits[0].id, "cached-credit");
    }

    #[tokio::test]
    async fn test_consume_retry_reuses_redeem_request_id() {
        let request_ids = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured = Arc::clone(&request_ids);
        let app = axum::Router::new().route(
            "/consume",
            post(move |Json(body): Json<Value>| {
                let captured = Arc::clone(&captured);
                async move {
                    let request_id = body
                        .get("redeem_request_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let attempt = {
                        let mut ids = captured.lock().unwrap();
                        ids.push(request_id);
                        ids.len()
                    };
                    if attempt == 1 {
                        StatusCode::INTERNAL_SERVER_ERROR.into_response()
                    } else {
                        Json(json!({"code": "reset", "windows_reset": 2})).into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let result = consume_reset_credit_at_url(
            &local_http_client(),
            "access-token",
            Some("workspace-123"),
            ResetCredit {
                id: "credit-1".to_string(),
                granted_at: None,
                expires_at: None,
            },
            &format!("http://{address}/consume"),
        )
        .await
        .unwrap();
        server.abort();

        assert_eq!(result.code.as_deref(), Some("reset"));
        let ids = request_ids.lock().unwrap();
        assert_eq!(ids.len(), 2);
        assert!(!ids[0].is_empty());
        assert_eq!(ids[0], ids[1]);
    }

    #[tokio::test]
    async fn success_with_invalid_json_is_classified_as_outcome_unknown() {
        let app =
            axum::Router::new().route("/consume", post(|| async { (StatusCode::OK, "not-json") }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let error = consume_reset_credit_at_url(
            &local_http_client(),
            "access-token",
            None,
            ResetCredit {
                id: "credit-1".to_string(),
                granted_at: None,
                expires_at: None,
            },
            &format!("http://{address}/consume"),
        )
        .await
        .unwrap_err();
        server.abort();

        assert!(error.outcome_unknown_after_request());
    }

    #[tokio::test]
    async fn explicit_client_error_is_classified_as_not_consumed() {
        let app = axum::Router::new().route("/consume", post(|| async { StatusCode::BAD_REQUEST }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let error = consume_reset_credit_at_url(
            &local_http_client(),
            "access-token",
            None,
            ResetCredit {
                id: "credit-1".to_string(),
                granted_at: None,
                expires_at: None,
            },
            &format!("http://{address}/consume"),
        )
        .await
        .unwrap_err();
        server.abort();

        assert!(error.definitely_not_consumed());
    }

    #[tokio::test]
    async fn first_success_with_non_reset_code_is_outcome_unknown() {
        let app = axum::Router::new().route(
            "/consume",
            post(|| async { Json(json!({"code": "already_redeemed"})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let error = consume_reset_credit_at_url(
            &local_http_client(),
            "access-token",
            None,
            ResetCredit {
                id: "credit-1".to_string(),
                granted_at: None,
                expires_at: None,
            },
            &format!("http://{address}/consume"),
        )
        .await
        .unwrap_err();
        server.abort();

        assert!(error.outcome_unknown_after_request());
        let message = error.user_facing_unknown_message("account");
        assert_eq!(
            message,
            "account: reset-card consumption may have occurred; verify before retry"
        );
        assert!(!message.contains("already_redeemed"));
    }

    #[tokio::test]
    async fn invalid_response_followed_by_conflict_remains_outcome_unknown() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&attempts);
        let app = axum::Router::new().route(
            "/consume",
            post(move || {
                let captured = Arc::clone(&captured);
                async move {
                    if captured.fetch_add(1, Ordering::SeqCst) == 0 {
                        (StatusCode::OK, "not-json").into_response()
                    } else {
                        StatusCode::CONFLICT.into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let error = consume_reset_credit_at_url(
            &local_http_client(),
            "access-token",
            None,
            ResetCredit {
                id: "credit-1".to_string(),
                granted_at: None,
                expires_at: None,
            },
            &format!("http://{address}/consume"),
        )
        .await
        .unwrap_err();
        server.abort();

        assert!(error.outcome_unknown_after_request());
    }

    #[tokio::test]
    async fn invalid_response_followed_by_already_redeemed_remains_outcome_unknown() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&attempts);
        let app = axum::Router::new().route(
            "/consume",
            post(move || {
                let captured = Arc::clone(&captured);
                async move {
                    if captured.fetch_add(1, Ordering::SeqCst) == 0 {
                        (StatusCode::OK, "not-json").into_response()
                    } else {
                        Json(json!({"code": "already_redeemed"})).into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let error = consume_reset_credit_at_url(
            &local_http_client(),
            "access-token",
            None,
            ResetCredit {
                id: "credit-1".to_string(),
                granted_at: None,
                expires_at: None,
            },
            &format!("http://{address}/consume"),
        )
        .await
        .unwrap_err();
        server.abort();

        assert!(error.outcome_unknown_after_request());
    }
}
