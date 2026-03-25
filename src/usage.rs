use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use tracing::info;

use crate::auth::{self, CLIENT_ID, TOKEN_URL};

#[derive(Debug, Default, Clone)]
pub struct WindowUsage {
    pub used_percent: Option<f64>,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Default, Clone)]
pub struct UsageInfo {
    pub primary: Option<WindowUsage>,   // 5h window
    pub secondary: Option<WindowUsage>, // 7d window
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

/// High-level: fetch usage with retry (up to 3 attempts) and auto token refresh.
/// Persists refreshed tokens back to profile and live auth.json if applicable.
pub async fn fetch_usage_retried(
    alias: &str,
    profile_path: &Path,
    current_alias: &str,
) -> std::result::Result<UsageInfo, String> {
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

    let resp = client
        .get(USAGE_URL)
        .header("Authorization", format!("Bearer {access_token}"))
        .send()
        .await?;

    let status = resp.status();
    if status.is_success() {
        let body: Value = resp.json().await?;
        return Ok((parse_usage(&body), None));
    }

    // If 401/403 and we have a refresh_token, try to refresh
    if let Some(rt) = refresh_token {
        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            info!("Got HTTP {status}, attempting token refresh");

            match do_refresh_token(&client, rt).await {
                Ok(new_tokens) => {
                    let resp2 = client
                        .get(USAGE_URL)
                        .header(
                            "Authorization",
                            format!("Bearer {}", new_tokens.access_token),
                        )
                        .send()
                        .await?;

                    if resp2.status().is_success() {
                        let body: Value = resp2.json().await?;
                        return Ok((parse_usage(&body), Some(new_tokens)));
                    }
                    anyhow::bail!("HTTP {} (after token refresh)", resp2.status());
                }
                Err(e) => {
                    info!("Token refresh failed: {e}");
                    anyhow::bail!("HTTP {status} (token refresh failed: {e})");
                }
            }
        }
    }

    anyhow::bail!("HTTP {status}");
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
        .await?;

    let status = resp.status();
    let r: RefreshResponse = resp.json().await?;

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
    if let Some(w) = &u.secondary {
        if w.used_percent.unwrap_or(0.0) >= 100.0 {
            return false;
        }
    }
    if let Some(w) = &u.primary {
        if w.used_percent.unwrap_or(0.0) >= 100.0 {
            return false;
        }
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

    UsageInfo { primary, secondary }
}
