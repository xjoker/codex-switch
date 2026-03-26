use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, info};

use crate::auth::{self, CLIENT_ID, TOKEN_URL, format_reqwest_error};

#[derive(Debug, Default, Clone)]
pub struct WindowUsage {
    pub used_percent: Option<f64>,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Default, Clone)]
pub struct UsageInfo {
    pub fetched_at: Option<i64>,
    pub primary: Option<WindowUsage>,   // 5h window
    pub secondary: Option<WindowUsage>, // 7d window
    pub credits_balance: Option<f64>,
    pub unlimited_credits: Option<bool>,
}

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const MAX_RETRIES: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    #[allow(dead_code)]
    error: Option<String>,
}

pub struct RefreshedTokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

fn usage_url() -> String {
    std::env::var("CS_USAGE_URL").unwrap_or_else(|_| USAGE_URL.to_string())
}

/// High-level: fetch usage with retry, token refresh, and disk cache.
/// Set `force` to true to bypass cache (e.g., manual refresh).
pub async fn fetch_usage_retried(
    alias: &str,
    profile_path: &Path,
    current_alias: &str,
) -> std::result::Result<UsageInfo, String> {
    fetch_usage_retried_inner(alias, profile_path, current_alias, false).await
}

/// Same as `fetch_usage_retried` but with explicit force flag.
pub async fn fetch_usage_retried_force(
    alias: &str,
    profile_path: &Path,
    current_alias: &str,
) -> std::result::Result<UsageInfo, String> {
    fetch_usage_retried_inner(alias, profile_path, current_alias, true).await
}

async fn fetch_usage_retried_inner(
    alias: &str,
    profile_path: &Path,
    current_alias: &str,
    force: bool,
) -> std::result::Result<UsageInfo, String> {
    if !force {
        if let Some(cached) = crate::cache::get(alias) {
            debug!("{alias}: cache hit");
            return Ok(cached);
        }
        debug!("{alias}: cache miss, fetching from API");
    } else {
        debug!("{alias}: force refresh, bypassing cache");
    }

    let val = auth::read_auth(profile_path).map_err(|e| e.to_string())?;
    let (access_token, refresh_token) = auth::extract_tokens(&val);

    let at = match access_token {
        Some(t) => t,
        None => return Err("no access_token".to_string()),
    };

    let mut last_err = String::new();
    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(RETRY_DELAY).await;
        }
        match fetch_usage_with_refresh(&at, refresh_token.as_deref()).await {
            Ok((usage, refreshed)) => {
                if let Some(new_tokens) = refreshed {
                    let _ = auth::update_tokens(
                        profile_path,
                        &new_tokens.id_token,
                        &new_tokens.access_token,
                        &new_tokens.refresh_token,
                    );
                    if alias == current_alias {
                        let live = auth::codex_auth_path();
                        let _ = auth::update_tokens(
                            &live,
                            &new_tokens.id_token,
                            &new_tokens.access_token,
                            &new_tokens.refresh_token,
                        );
                    }
                }
                crate::cache::put(alias, &usage);
                return Ok(usage);
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(last_err)
}

/// Fetch usage; on 401/403 automatically refresh the token and retry once.
pub async fn fetch_usage_with_refresh(
    access_token: &str,
    refresh_token: Option<&str>,
) -> Result<(UsageInfo, Option<RefreshedTokens>)> {
    let client = auth::build_http_client()?;
    let usage_url = usage_url();

    // Pre-refresh: if access_token expires within 60 seconds, refresh proactively.
    if let Some(rt) = refresh_token
        && crate::jwt::is_token_expiring(access_token, 60).unwrap_or(false)
    {
        info!("Access token expiring soon, proactively refreshing");

        match do_refresh_token(&client, rt).await {
            Ok(new_tokens) => {
                let resp = client
                    .get(&usage_url)
                    .header(
                        "Authorization",
                        format!("Bearer {}", new_tokens.access_token),
                    )
                    .send()
                    .await
                    .map_err(|e| format_reqwest_error("Usage API request failed", &e))?;

                let status = resp.status();
                debug!("Usage API: HTTP {status}");
                if status.is_success() {
                    let body: Value = resp.json().await.map_err(|e| {
                        anyhow::anyhow!("Failed to parse usage response (HTTP {status}): {e}")
                    })?;
                    return Ok((parse_usage(&body), Some(new_tokens)));
                }
                anyhow::bail!("Usage API failed (HTTP {status}) after proactive token refresh");
            }
            Err(e) => {
                info!("Proactive token refresh failed, trying with existing token: {e}");
            }
        }
    }

    let resp = client
        .get(&usage_url)
        .header("Authorization", format!("Bearer {access_token}"))
        .send()
        .await
        .map_err(|e| format_reqwest_error("Usage API request failed", &e))?;

    let status = resp.status();
    debug!("Usage API: HTTP {status}");
    if status.is_success() {
        let body: Value = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse usage response (HTTP {status}): {e}"))?;
        return Ok((parse_usage(&body), None));
    }

    // If 401/403 and we have a refresh_token, try to refresh
    if let Some(rt) = refresh_token
        && (status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN)
    {
        info!("Got HTTP {status}, attempting token refresh");

        match do_refresh_token(&client, rt).await {
            Ok(new_tokens) => {
                let resp2 = client
                    .get(&usage_url)
                    .header(
                        "Authorization",
                        format!("Bearer {}", new_tokens.access_token),
                    )
                    .send()
                    .await
                    .map_err(|e| format_reqwest_error("Usage API retry request failed", &e))?;

                let status2 = resp2.status();
                if status2.is_success() {
                    let body: Value = resp2.json().await.map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to parse usage response after refresh (HTTP {status2}): {e}"
                        )
                    })?;
                    return Ok((parse_usage(&body), Some(new_tokens)));
                }
                anyhow::bail!("Usage API failed (HTTP {status2}) after token refresh");
            }
            Err(e) => {
                info!("Token refresh failed: {e}");
                anyhow::bail!("Usage API failed (HTTP {status}), token refresh also failed: {e}");
            }
        }
    }

    anyhow::bail!("Usage API failed (HTTP {status})");
}

pub async fn validate_import_auth(
    val: &mut serde_json::Value,
) -> Result<(UsageInfo, Option<RefreshedTokens>)> {
    if std::env::var("CS_IMPORT_SKIP_USAGE_VALIDATION")
        .ok()
        .as_deref()
        == Some("1")
    {
        return Ok((UsageInfo::default(), None));
    }

    let (access_token, refresh_token) = auth::extract_tokens(val);

    match (access_token, refresh_token) {
        (Some(at), rt) => {
            let (usage, refreshed) = fetch_usage_with_refresh(&at, rt.as_deref()).await?;
            if let Some(tokens) = &refreshed {
                auth::apply_tokens(
                    val,
                    &tokens.id_token,
                    &tokens.access_token,
                    &tokens.refresh_token,
                )?;
            }
            Ok((usage, refreshed))
        }
        (None, Some(rt)) => {
            let client = auth::build_http_client()?;
            let refreshed = do_refresh_token(&client, &rt).await?;
            auth::apply_tokens(
                val,
                &refreshed.id_token,
                &refreshed.access_token,
                &refreshed.refresh_token,
            )?;
            let (usage, refreshed_again) =
                fetch_usage_with_refresh(&refreshed.access_token, Some(&refreshed.refresh_token))
                    .await?;
            if let Some(tokens) = &refreshed_again {
                auth::apply_tokens(
                    val,
                    &tokens.id_token,
                    &tokens.access_token,
                    &tokens.refresh_token,
                )?;
            }
            Ok((usage, refreshed_again.or(Some(refreshed))))
        }
        (None, None) => anyhow::bail!("auth.json missing access_token and refresh_token"),
    }
}

async fn do_refresh_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<RefreshedTokens> {
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        urlencoding::encode(refresh_token),
        urlencoding::encode(CLIENT_ID),
    );

    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| format_reqwest_error("Token refresh request failed", &e))?;

    let status = resp.status();
    let r: RefreshResponse = resp.json().await.map_err(|e| {
        anyhow::anyhow!("Failed to parse token refresh response (HTTP {status}): {e}")
    })?;

    match (r.access_token, r.id_token, r.refresh_token) {
        (Some(at), Some(id), Some(rt)) => {
            info!("Token refresh succeeded");
            Ok(RefreshedTokens {
                id_token: id,
                access_token: at,
                refresh_token: rt,
            })
        }
        (Some(at), Some(id), None) => Ok(RefreshedTokens {
            id_token: id,
            access_token: at,
            refresh_token: refresh_token.to_string(),
        }),
        _ => anyhow::bail!("Token refresh HTTP {status}: missing required fields"),
    }
}

fn parse_window(val: &Value) -> Option<WindowUsage> {
    let used_percent = val.get("used_percent").and_then(|v| v.as_f64());
    let resets_at = val.get("reset_at").and_then(|v| v.as_i64());

    if used_percent.is_none() && resets_at.is_none() {
        return None;
    }

    Some(WindowUsage {
        used_percent,
        resets_at,
    })
}

/// Whether an account is currently usable (both windows have remaining quota).
pub fn is_available(u: &UsageInfo) -> bool {
    if let Some(w) = &u.secondary
        && w.used_percent.unwrap_or(0.0) >= 100.0
    {
        return false;
    }
    if let Some(w) = &u.primary
        && w.used_percent.unwrap_or(0.0) >= 100.0
    {
        return false;
    }
    true
}

/// Score an account for `codex-switch use` auto-selection.
pub fn score(u: &UsageInfo) -> f64 {
    let now = auth::now_unix_secs();

    // 7d window exhausted → heavily penalized
    if let Some(w7) = &u.secondary {
        let used_7d = w7.used_percent.unwrap_or(0.0);
        if used_7d >= 100.0 {
            return match w7.resets_at {
                None => 0.0,
                Some(reset_ts) => {
                    let remaining_secs = reset_ts - now;
                    if remaining_secs <= 0 {
                        100.0
                    } else {
                        (100.0 - (remaining_secs as f64 / 60.0)).max(0.0)
                    }
                }
            };
        }
    }

    match &u.primary {
        None => 50.0,
        Some(w) => {
            let used = w.used_percent.unwrap_or(100.0);
            if used < 100.0 {
                1000.0 + (100.0 - used)
            } else {
                match w.resets_at {
                    None => 0.0,
                    Some(reset_ts) => {
                        let remaining_secs = reset_ts - now;
                        if remaining_secs <= 0 {
                            500.0
                        } else {
                            let remaining_min = remaining_secs / 60;
                            (500.0 - remaining_min as f64).max(0.0)
                        }
                    }
                }
            }
        }
    }
}

pub fn parse_usage(body: &Value) -> UsageInfo {
    let primary = body
        .pointer("/rate_limit/primary_window")
        .and_then(|v| if v.is_null() { None } else { Some(v) })
        .and_then(parse_window);

    let secondary = body
        .pointer("/rate_limit/secondary_window")
        .and_then(|v| if v.is_null() { None } else { Some(v) })
        .and_then(parse_window);

    let credits_balance = body.pointer("/credits/balance").and_then(|v| v.as_f64());

    let unlimited_credits = body.pointer("/credits/unlimited").and_then(|v| v.as_bool());

    UsageInfo {
        fetched_at: Some(auth::now_unix_secs()),
        primary,
        secondary,
        credits_balance,
        unlimited_credits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use serde_json::json;

    fn usage_with(primary: Option<WindowUsage>, secondary: Option<WindowUsage>) -> UsageInfo {
        UsageInfo {
            fetched_at: None,
            primary,
            secondary,
            credits_balance: None,
            unlimited_credits: None,
        }
    }

    fn window(used_percent: f64, resets_at: Option<i64>) -> WindowUsage {
        WindowUsage {
            used_percent: Some(used_percent),
            resets_at,
        }
    }

    #[test]
    fn test_parse_usage_full_response() {
        let primary_reset = DateTime::parse_from_rfc3339("2026-03-26T10:00:00Z")
            .unwrap()
            .timestamp();
        let secondary_reset = DateTime::parse_from_rfc3339("2026-03-30T00:00:00Z")
            .unwrap()
            .timestamp();
        let body = json!({
            "rate_limit": {
                "primary_window": {
                    "remaining_seconds": 3600,
                    "requests_remaining": 50,
                    "requests_limit": 100,
                    "reset_time": "2026-03-26T10:00:00Z",
                    "used_percent": 50.0,
                    "reset_at": primary_reset
                },
                "secondary_window": {
                    "remaining_seconds": 86400,
                    "requests_remaining": 200,
                    "requests_limit": 500,
                    "reset_time": "2026-03-30T00:00:00Z",
                    "used_percent": 60.0,
                    "reset_at": secondary_reset
                }
            },
            "credits": {
                "balance": 15.50,
                "unlimited": false
            }
        });

        let before = auth::now_unix_secs();
        let usage = parse_usage(&body);
        let after = auth::now_unix_secs();

        assert!(matches!(usage.fetched_at, Some(ts) if ts >= before && ts <= after));
        assert_eq!(
            usage.primary.as_ref().and_then(|w| w.used_percent),
            Some(50.0)
        );
        assert_eq!(
            usage.primary.as_ref().and_then(|w| w.resets_at),
            Some(primary_reset)
        );
        assert_eq!(
            usage.secondary.as_ref().and_then(|w| w.used_percent),
            Some(60.0)
        );
        assert_eq!(
            usage.secondary.as_ref().and_then(|w| w.resets_at),
            Some(secondary_reset)
        );
        assert_eq!(usage.credits_balance, Some(15.5));
        assert_eq!(usage.unlimited_credits, Some(false));
    }

    #[test]
    fn test_parse_usage_unlimited_credits() {
        let usage = parse_usage(&json!({
            "credits": {
                "balance": 15.50,
                "unlimited": true
            }
        }));

        assert_eq!(usage.credits_balance, Some(15.5));
        assert_eq!(usage.unlimited_credits, Some(true));
    }

    #[test]
    fn test_parse_usage_no_credits() {
        let usage = parse_usage(&json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 25.0,
                    "reset_at": 123
                }
            }
        }));

        assert_eq!(usage.credits_balance, None);
        assert_eq!(usage.unlimited_credits, None);
    }

    #[test]
    fn test_parse_usage_null_windows() {
        let usage = parse_usage(&json!({
            "rate_limit": {
                "primary_window": null,
                "secondary_window": null
            }
        }));

        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn test_parse_usage_empty_response() {
        let usage = parse_usage(&json!({}));

        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_none());
        assert_eq!(usage.credits_balance, None);
        assert_eq!(usage.unlimited_credits, None);
    }

    #[test]
    fn test_is_available_both_under_100() {
        let usage = usage_with(
            Some(window(50.0, Some(1_000))),
            Some(window(30.0, Some(2_000))),
        );

        assert!(is_available(&usage));
    }

    #[test]
    fn test_is_available_primary_exhausted() {
        let usage = usage_with(
            Some(window(100.0, Some(1_000))),
            Some(window(30.0, Some(2_000))),
        );

        assert!(!is_available(&usage));
    }

    #[test]
    fn test_is_available_secondary_exhausted() {
        let usage = usage_with(
            Some(window(50.0, Some(1_000))),
            Some(window(100.0, Some(2_000))),
        );

        assert!(!is_available(&usage));
    }

    #[test]
    fn test_is_available_no_data() {
        assert!(is_available(&UsageInfo::default()));
    }

    #[test]
    fn test_score_available_account() {
        let usage = usage_with(Some(window(30.0, Some(1_000))), None);

        assert_eq!(score(&usage), 1_070.0);
    }

    #[test]
    fn test_score_no_primary() {
        assert_eq!(score(&UsageInfo::default()), 50.0);
    }

    #[test]
    fn test_score_primary_exhausted_7d_ok() {
        let now = auth::now_unix_secs();
        let usage = usage_with(
            Some(window(100.0, Some(now + 3_600))),
            Some(window(50.0, Some(now + 86_400))),
        );

        let scored = score(&usage);

        assert!((0.0..=500.0).contains(&scored));
    }
}
