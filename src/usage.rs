use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, info, warn};

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

/// Window durations in seconds (used for pace calculation).
pub const WINDOW_5H_SECS: i64 = 5 * 3600;
pub const WINDOW_7D_SECS: i64 = 7 * 86400;

/// Calculate pace: the expected used_percent if consumption were even across the window.
/// Returns None if resets_at is unavailable.
pub fn pace_percent(w: &WindowUsage, window_secs: i64) -> Option<f64> {
    let resets_at = w.resets_at?;
    let now = auth::now_unix_secs();
    let remaining_secs = (resets_at - now).max(0) as f64;
    let elapsed_secs = (window_secs as f64 - remaining_secs).clamp(0.0, window_secs as f64);
    Some((elapsed_secs / window_secs as f64 * 100.0).clamp(0.0, 100.0))
}

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const MAX_RETRIES: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    error: Option<String>,
}

pub struct RefreshedTokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

/// Structured error for usage fetch failures.
#[derive(Debug, Clone)]
pub struct UsageError {
    /// Short summary for user-facing display (e.g. "HTTP 401 Unauthorized")
    pub summary: String,
    /// Full detail for debug/log (e.g. "Usage API failed (HTTP 401), token refresh also failed: ...")
    pub detail: String,
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}

fn usage_url() -> String {
    std::env::var("CS_USAGE_URL").unwrap_or_else(|_| USAGE_URL.to_string())
}

/// Extract a short summary from an error message for user-facing display.
/// Looks for "HTTP <status>" patterns; falls back to first line truncated.
fn extract_error_summary(err: &str) -> String {
    // Look for "HTTP 4xx ..." or "HTTP 5xx ..." pattern
    if let Some(pos) = err.find("HTTP ") {
        let rest = &err[pos..];
        // Take until comma, closing paren, or end
        let end = rest.find([',', ')']).unwrap_or(rest.len());
        return rest[..end].to_string();
    }
    // Fallback: first line, truncated
    let first_line = err.lines().next().unwrap_or(err);
    let mut chars = first_line.chars();
    let preview: String = chars.by_ref().take(60).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        first_line.to_string()
    }
}

/// High-level: fetch usage with retry, token refresh, and disk cache.
/// Set `force` to true to bypass cache (e.g., manual refresh).
pub async fn fetch_usage_retried(
    alias: &str,
    profile_path: &Path,
    current_alias: &str,
) -> std::result::Result<UsageInfo, UsageError> {
    fetch_usage_retried_inner(alias, profile_path, current_alias, false).await
}

/// Same as `fetch_usage_retried` but with explicit force flag.
pub async fn fetch_usage_retried_force(
    alias: &str,
    profile_path: &Path,
    current_alias: &str,
) -> std::result::Result<UsageInfo, UsageError> {
    fetch_usage_retried_inner(alias, profile_path, current_alias, true).await
}

fn persist_refreshed_tokens(alias: &str, profile_path: &Path, new_tokens: &RefreshedTokens) {
    if let Err(err) = auth::update_tokens(
        profile_path,
        &new_tokens.id_token,
        &new_tokens.access_token,
        &new_tokens.refresh_token,
    ) {
        warn!(
            "[{alias}] Failed to persist refreshed tokens to {}: {err}",
            profile_path.display()
        );
    }

    if crate::profile::read_current() == alias {
        let live = auth::codex_auth_path();
        if let Err(err) = auth::update_tokens(
            &live,
            &new_tokens.id_token,
            &new_tokens.access_token,
            &new_tokens.refresh_token,
        ) {
            warn!(
                "[{alias}] Failed to persist refreshed tokens to {}: {err}",
                live.display()
            );
        }
    }
}

async fn fetch_usage_retried_inner(
    alias: &str,
    profile_path: &Path,
    _current_alias: &str,
    force: bool,
) -> std::result::Result<UsageInfo, UsageError> {
    if !force {
        if let Some(cached) = crate::cache::get(alias) {
            debug!("{alias}: cache hit");
            return Ok(cached);
        }
        debug!("{alias}: cache miss, fetching from API");
    } else {
        debug!("{alias}: force refresh, bypassing cache");
    }

    let val = auth::read_auth(profile_path).map_err(|e| {
        let detail = format!("failed to read auth file {}: {e}", profile_path.display());
        UsageError {
            summary: "auth file unreadable".into(),
            detail,
        }
    })?;
    let (access_token, refresh_token) = auth::extract_tokens(&val);

    let at = match access_token {
        Some(t) => t,
        None => {
            return Err(UsageError {
                summary: "no access_token".into(),
                detail: "no access_token in auth file".into(),
            });
        }
    };

    let mut last_err = String::new();
    let mut last_summary = String::new();
    for attempt in 0..MAX_RETRIES {
        if attempt > 0 {
            debug!("[{alias}] retry attempt {}/{MAX_RETRIES}", attempt + 1);
            tokio::time::sleep(RETRY_DELAY).await;
        }
        match fetch_usage_with_refresh(alias, &at, refresh_token.as_deref()).await {
            Ok((usage, refreshed)) => {
                if let Some(new_tokens) = refreshed {
                    persist_refreshed_tokens(alias, profile_path, &new_tokens);
                }
                crate::cache::put(alias, &usage);
                return Ok(usage);
            }
            Err(e) => {
                let msg = e.to_string();
                warn!(
                    "[{alias}] attempt {}/{MAX_RETRIES} failed: {msg}",
                    attempt + 1
                );
                last_summary = extract_error_summary(&msg);
                last_err = msg;
            }
        }
    }
    Err(UsageError {
        summary: last_summary,
        detail: last_err,
    })
}

/// Fetch usage; on 401/403 automatically refresh the token and retry once.
pub async fn fetch_usage_with_refresh(
    alias: &str,
    access_token: &str,
    refresh_token: Option<&str>,
) -> Result<(UsageInfo, Option<RefreshedTokens>)> {
    let client = auth::build_http_client()?;
    let usage_url = usage_url();

    // Pre-refresh: if access_token expires within 60 seconds, refresh proactively.
    if let Some(rt) = refresh_token
        && crate::jwt::is_token_expiring(access_token, 60).unwrap_or(false)
    {
        info!("[{alias}] access token expiring soon, proactively refreshing");

        match do_refresh_token(alias, &client, rt).await {
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
                debug!("[{alias}] Usage API (after proactive refresh): HTTP {status}");
                if status.is_success() {
                    let body: Value = resp.json().await.map_err(|e| {
                        anyhow::anyhow!("failed to parse usage response (HTTP {status}): {e}")
                    })?;
                    return Ok((parse_usage(&body), Some(new_tokens)));
                }
                anyhow::bail!("Usage API failed (HTTP {status}) after proactive token refresh");
            }
            Err(e) => {
                info!("[{alias}] proactive token refresh failed, trying with existing token: {e}");
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
    debug!("[{alias}] Usage API: HTTP {status}");
    if status.is_success() {
        let body: Value = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("failed to parse usage response (HTTP {status}): {e}"))?;
        return Ok((parse_usage(&body), None));
    }

    // If 401/403 and we have a refresh_token, try to refresh
    if let Some(rt) = refresh_token
        && (status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN)
    {
        info!("[{alias}] got HTTP {status}, attempting token refresh");

        match do_refresh_token(alias, &client, rt).await {
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
                debug!("[{alias}] Usage API (after token refresh): HTTP {status2}");
                if status2.is_success() {
                    let body: Value = resp2.json().await.map_err(|e| {
                        anyhow::anyhow!(
                            "failed to parse usage response after refresh (HTTP {status2}): {e}"
                        )
                    })?;
                    return Ok((parse_usage(&body), Some(new_tokens)));
                }
                anyhow::bail!("Usage API still failed (HTTP {status2}) after token refresh");
            }
            Err(e) => {
                info!("[{alias}] token refresh failed: {e}");
                anyhow::bail!("Usage API failed (HTTP {status}), token refresh also failed: {e}");
            }
        }
    }

    anyhow::bail!("Usage API failed (HTTP {status}), no refresh_token available");
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

    let alias = "import";
    match (access_token, refresh_token) {
        (Some(at), rt) => {
            let (usage, refreshed) = fetch_usage_with_refresh(alias, &at, rt.as_deref()).await?;
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
            let refreshed = do_refresh_token(alias, &client, &rt).await?;
            auth::apply_tokens(
                val,
                &refreshed.id_token,
                &refreshed.access_token,
                &refreshed.refresh_token,
            )?;
            let (usage, refreshed_again) = fetch_usage_with_refresh(
                alias,
                &refreshed.access_token,
                Some(&refreshed.refresh_token),
            )
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
    alias: &str,
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<RefreshedTokens> {
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        urlencoding::encode(refresh_token),
        urlencoding::encode(CLIENT_ID),
    );

    debug!("[{alias}] sending token refresh request to {TOKEN_URL}");

    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| format_reqwest_error("token refresh request failed", &e))?;

    let status = resp.status();
    debug!("[{alias}] token refresh response: HTTP {status}");

    // Read raw body first so we can log it on parse failure
    let body_text = resp.text().await.map_err(|e| {
        anyhow::anyhow!("failed to read token refresh response body (HTTP {status}): {e}")
    })?;

    let r: RefreshResponse = serde_json::from_str(&body_text).map_err(|e| {
        let preview = if body_text.len() > 200 {
            format!("{}...(truncated)", &body_text[..200])
        } else {
            body_text.clone()
        };
        debug!("[{alias}] token refresh parse failure, raw body: {preview}");
        anyhow::anyhow!("Failed to parse token refresh response (HTTP {status}): {e}")
    })?;

    if let Some(err) = &r.error {
        warn!("[{alias}] token refresh returned error field: {err}");
    }

    match (r.access_token, r.id_token, r.refresh_token) {
        (Some(at), Some(id), Some(rt)) => {
            info!("[{alias}] token refresh succeeded");
            Ok(RefreshedTokens {
                id_token: id,
                access_token: at,
                refresh_token: rt,
            })
        }
        (Some(at), Some(id), None) => {
            debug!("[{alias}] token refresh succeeded (no new refresh_token, reusing old one)");
            Ok(RefreshedTokens {
                id_token: id,
                access_token: at,
                refresh_token: refresh_token.to_string(),
            })
        }
        (at, id, rt) => {
            anyhow::bail!(
                "token refresh HTTP {status}: missing required fields (access_token: {}, id_token: {}, refresh_token: {})",
                at.is_some(),
                id.is_some(),
                rt.is_some(),
            )
        }
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

/// Whether the account should be considered eligible for selection.
///
/// An account is **ineligible** when:
/// - Either window is fully exhausted (>=100%), OR
/// - 7d remaining < `critical_pct` AND 7d resets more than 48h away.
///
/// When ALL accounts are ineligible the caller should fall back to the
/// best-scoring ineligible account rather than giving up entirely.
pub fn is_eligible(u: &UsageInfo, safety_margin_7d: f64) -> bool {
    if !is_available(u) {
        return false;
    }
    // Hard gate: 7d critically low AND reset is far away
    let critical_pct = (safety_margin_7d * 0.25).max(1.0); // 25% of safety margin, min 1%
    if let Some(w7) = &u.secondary {
        let remaining_7d = 100.0 - w7.used_percent.unwrap_or(0.0);
        if remaining_7d < critical_pct {
            let hours_to_reset = w7
                .resets_at
                .map(|ts| ((ts - auth::now_unix_secs()) as f64 / 3600.0).max(0.0))
                .unwrap_or(f64::MAX);
            if hours_to_reset > 48.0 {
                return false;
            }
        }
    }
    true
}

// ── 7d adjustment (shared by max-remaining & drain-first) ──

/// Compute the 7d health adjustment (range: -300 to 0).
///
/// * 7d remaining >= `safety_margin` → 0 (safe zone)
/// * 7d remaining < `safety_margin` → penalty up to -300, reduced when
///   the 7d window resets within 48h.
/// * No 7d data → -50 (mild penalty for unknown state)
fn compute_7d_adjustment(u: &UsageInfo, safety_margin: f64) -> f64 {
    const MAX_PENALTY: f64 = 300.0;
    const RELIEF_WINDOW_HOURS: f64 = 48.0;
    const MAX_RELIEF: f64 = 0.8; // reset time can reduce penalty by up to 80%

    let Some(w7) = &u.secondary else {
        return -50.0; // no 7d data → mild penalty for unknown state
    };
    let used_7d = w7.used_percent.unwrap_or(0.0).clamp(0.0, 100.0);
    let remaining_7d = 100.0 - used_7d;

    if remaining_7d >= safety_margin {
        return 0.0; // safe zone
    }

    // pressure: 0.0 (at safety_margin) → 1.0 (at 0% remaining)
    let pressure = if safety_margin > 0.0 {
        ((safety_margin - remaining_7d) / safety_margin).clamp(0.0, 1.0)
    } else {
        1.0
    };

    // time relief: if 7d resets within 48h, reduce penalty
    let time_relief = w7
        .resets_at
        .map(|ts| {
            let hours = ((ts - auth::now_unix_secs()) as f64 / 3600.0).max(0.0);
            if hours < RELIEF_WINDOW_HOURS {
                (1.0 - hours / RELIEF_WINDOW_HOURS).clamp(0.0, 1.0)
            } else {
                0.0
            }
        })
        .unwrap_or(0.0); // unknown reset time → no relief

    let effective = pressure * (1.0 - time_relief * MAX_RELIEF);
    -MAX_PENALTY * effective
}

// ── scoring functions ───────────────────────────────────────

/// Score an account for **max-remaining** mode.
///
/// Primary: 5h remaining% → higher is better.
/// Secondary: 7d adjustment (additive, -300 to 0).
pub fn score(u: &UsageInfo, safety_margin_7d: f64) -> f64 {
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

    let base = match &u.primary {
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
                            let remaining_min = remaining_secs as f64 / 60.0;
                            (500.0 - remaining_min).max(0.0)
                        }
                    }
                }
            }
        }
    };

    base + compute_7d_adjustment(u, safety_margin_7d)
}

/// Score an account using the **drain-first** strategy.
///
/// Core idea: prefer accounts whose 5h window resets soonest — use them up
/// before the reset makes the spent quota "free", while preserving accounts
/// with distant resets as a reserve.
///
/// * Accounts below `min_remaining`% are demoted (range 500-600).
/// * 7d window exhausted → heavily penalized.
/// * Among usable accounts, shorter time-to-reset → higher score (1000-1300).
/// * 7d adjustment applied additively (-300 to 0).
pub fn score_drain_first(u: &UsageInfo, min_remaining: f64, safety_margin_7d: f64) -> f64 {
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

    let base = match &u.primary {
        None => 50.0,
        Some(w) => {
            let used = w.used_percent.unwrap_or(100.0);
            let remaining = 100.0 - used;

            if used >= 100.0 {
                // Exhausted: score by time-to-reset (0-500 range)
                match w.resets_at {
                    None => 0.0,
                    Some(reset_ts) => {
                        let remaining_secs = reset_ts - now;
                        if remaining_secs <= 0 {
                            500.0
                        } else {
                            let remaining_min = remaining_secs as f64 / 60.0;
                            (500.0 - remaining_min).max(0.0)
                        }
                    }
                }
            } else if remaining < min_remaining {
                // Below threshold: demoted but still usable (range 500-600)
                match w.resets_at {
                    None => 500.0,
                    Some(reset_ts) => {
                        let remaining_secs = reset_ts - now;
                        if remaining_secs <= 0 {
                            600.0
                        } else {
                            let remaining_min = remaining_secs as f64 / 60.0;
                            (600.0 - (remaining_min / 3.0)).max(500.0)
                        }
                    }
                }
            } else {
                // Usable: base 1000 + reset urgency bonus (0-300)
                let reset_bonus = match w.resets_at {
                    None => 0.0,
                    Some(reset_ts) => {
                        let remaining_secs = reset_ts - now;
                        if remaining_secs <= 0 {
                            300.0
                        } else {
                            let remaining_min = remaining_secs as f64 / 60.0;
                            (300.0 - remaining_min).max(0.0)
                        }
                    }
                };
                1000.0 + reset_bonus
            }
        }
    };

    base + compute_7d_adjustment(u, safety_margin_7d)
}

/// Score an account using the **round-robin** strategy.
///
/// Two-tier comparison: (is_team, -last_used_ts).
/// Team accounts are preferred; within the same tier, least-recently-used wins.
/// Unavailable or 7d-critical accounts get `f64::NEG_INFINITY`.
pub fn score_round_robin(u: &UsageInfo, last_used_ts: i64, is_team: bool, safety_margin_7d: f64) -> f64 {
    if !is_eligible(u, safety_margin_7d) {
        return f64::NEG_INFINITY;
    }
    // Team tier: team accounts get an offset that guarantees they sort above
    // non-team accounts. We use 4e9 (not 1e18) to stay well within f64's
    // exact-integer range (~2^53), preserving 1-second timestamp precision.
    // Non-team scores: 0 to -2e9.  Team scores: 4e9 to ~2.3e9.  Always team > non-team.
    let team_tier: f64 = if is_team { 4e9 } else { 0.0 };
    // Within tier, prefer least recently used (negate timestamp).
    team_tier - (last_used_ts as f64)
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

/// Max number of tokens to refresh opportunistically per CLI invocation.
const OPPORTUNISTIC_REFRESH_LIMIT: usize = 3;
/// Refresh tokens expiring within this many seconds.
const OPPORTUNISTIC_REFRESH_MARGIN: i64 = 1800; // 30 minutes
/// Total wall-clock timeout for all opportunistic refreshes (concurrent).
const OPPORTUNISTIC_TOTAL_TIMEOUT: Duration = Duration::from_secs(8);

/// Opportunistically refresh tokens that are about to expire.
/// Runs concurrently with a bounded total timeout.
/// Errors are logged, not propagated — safe to await at end of CLI commands.
pub async fn refresh_expiring_tokens() {
    let profiles = match crate::profile::list_profiles() {
        Ok(p) => p,
        Err(_) => return,
    };

    let now = auth::now_unix_secs();

    // Collect (alias, profile_path, refresh_token, expires_at) for tokens expiring soon
    let mut candidates: Vec<(String, std::path::PathBuf, String, i64)> = Vec::new();
    for alias in &profiles {
        let path = crate::profile::profile_auth_path(alias);
        let val = match auth::read_auth(&path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let (access_token, refresh_token) = auth::extract_tokens(&val);
        let Some(at) = access_token else { continue };
        let Some(rt) = refresh_token else { continue };
        let Some(exp) = crate::jwt::token_expires_at(&at) else {
            continue;
        };
        let remaining = exp - now;
        if remaining < OPPORTUNISTIC_REFRESH_MARGIN {
            candidates.push((alias.clone(), path, rt, exp));
        }
    }

    if candidates.is_empty() {
        return;
    }

    // Sort by expiration: soonest first
    candidates.sort_by_key(|c| c.3);
    candidates.truncate(OPPORTUNISTIC_REFRESH_LIMIT);

    let count = candidates.len();
    debug!(
        "opportunistic refresh: {count} token(s) expiring within {}s",
        OPPORTUNISTIC_REFRESH_MARGIN
    );

    // Spawn all refreshes concurrently, bounded by total timeout
    let mut tasks = tokio::task::JoinSet::new();
    for (alias, path, rt, exp) in candidates {
        tasks.spawn(async move {
            let remaining = exp - auth::now_unix_secs();
            debug!("[{alias}] token expires in {remaining}s, refreshing");

            let client = match auth::build_http_client() {
                Ok(c) => c,
                Err(e) => {
                    debug!("[{alias}] skipping refresh: {e}");
                    return;
                }
            };

            match do_refresh_token(&alias, &client, &rt).await {
                Ok(new_tokens) => {
                    persist_refreshed_tokens(&alias, &path, &new_tokens);
                    info!("[{alias}] opportunistic token refresh succeeded");
                }
                Err(e) => {
                    debug!("[{alias}] opportunistic token refresh failed: {e}");
                }
            }
        });
    }

    // Wait for all with total timeout — don't block CLI too long
    let _ = tokio::time::timeout(OPPORTUNISTIC_TOTAL_TIMEOUT, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
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

    // ── max-remaining tests ────────────────────────────────

    #[test]
    fn test_score_available_account() {
        // No 7d data → -50 penalty: 1070 - 50 = 1020
        let usage = usage_with(Some(window(30.0, Some(1_000))), None);
        assert_eq!(score(&usage, 20.0), 1_020.0);
    }

    #[test]
    fn test_score_available_with_healthy_7d() {
        // 7d at 50% used (50% remaining > 20% safety) → no penalty
        let usage = usage_with(Some(window(30.0, Some(1_000))), Some(window(50.0, Some(9_999))));
        assert_eq!(score(&usage, 20.0), 1_070.0);
    }

    #[test]
    fn test_score_7d_penalty_applied() {
        let now = auth::now_unix_secs();
        // 5h: 30% used → base 1070
        // 7d: 90% used (10% remaining), resets in 6d → no time relief
        // pressure = (20-10)/20 = 0.5, adj = -300 × 0.5 = -150 → 920
        let usage = usage_with(
            Some(window(30.0, Some(now + 3_600))),
            Some(window(90.0, Some(now + 6 * 86_400))),
        );
        assert_eq!(score(&usage, 20.0), 920.0);
    }

    #[test]
    fn test_score_7d_penalty_with_time_relief() {
        let now = auth::now_unix_secs();
        // 7d: 90% used (10% remaining), resets in 12h
        // pressure = 0.5, time_relief = 1 - 12/48 = 0.75
        // effective = 0.5 × (1 - 0.75×0.8) = 0.5 × 0.4 = 0.2
        // adj = -300 × 0.2 = -60 → 1070 - 60 = 1010
        let usage = usage_with(
            Some(window(30.0, Some(now + 3_600))),
            Some(window(90.0, Some(now + 12 * 3_600))),
        );
        assert_eq!(score(&usage, 20.0), 1_010.0);
    }

    #[test]
    fn test_score_no_primary() {
        // No data at all: base 50 + no-7d penalty (-50) = 0
        assert_eq!(score(&UsageInfo::default(), 20.0), 0.0);
    }

    #[test]
    fn test_score_primary_exhausted_7d_ok() {
        let now = auth::now_unix_secs();
        // 5h exhausted, resets in 60 min → base = 500 - 60 = 440
        // 7d at 50% (healthy) → adj = 0 → final = 440
        let usage = usage_with(
            Some(window(100.0, Some(now + 3_600))),
            Some(window(50.0, Some(now + 86_400))),
        );
        assert_eq!(score(&usage, 20.0), 440.0);
    }

    #[test]
    fn test_score_multi_account_ordering() {
        let now = auth::now_unix_secs();
        // A: 5h 10% used, 7d healthy → highest
        let a = usage_with(Some(window(10.0, Some(now + 14_400))), Some(window(20.0, Some(now + 86_400))));
        // B: 5h 50% used, 7d healthy → medium
        let b = usage_with(Some(window(50.0, Some(now + 7_200))), Some(window(20.0, Some(now + 86_400))));
        // C: 5h 10% used, 7d 95% (critical), far reset → 7d penalty drags it down
        let c = usage_with(Some(window(10.0, Some(now + 14_400))), Some(window(95.0, Some(now + 6 * 86_400))));
        // D: 5h exhausted, 7d healthy → lowest usable
        let d = usage_with(Some(window(100.0, Some(now + 1_800))), Some(window(20.0, Some(now + 86_400))));

        let sa = score(&a, 20.0);
        let sb = score(&b, 20.0);
        let sc = score(&c, 20.0);
        let sd = score(&d, 20.0);

        assert!(sa > sb, "A > B: {sa} > {sb}");
        assert!(sb > sc, "B > C (7d penalty): {sb} > {sc}");
        assert!(sc > sd, "C > D (exhausted): {sc} > {sd}");
    }

    // ── eligibility tests ───────────────────────────────────

    #[test]
    fn test_eligible_healthy_account() {
        let usage = usage_with(
            Some(window(30.0, Some(1_000))),
            Some(window(50.0, Some(9_999))),
        );
        assert!(is_eligible(&usage, 20.0));
    }

    #[test]
    fn test_ineligible_7d_critical_far_reset() {
        let now = auth::now_unix_secs();
        // 7d at 97% (3% remaining < critical 5%), resets in 5 days
        let usage = usage_with(
            Some(window(30.0, Some(now + 3_600))),
            Some(window(97.0, Some(now + 5 * 86_400))),
        );
        assert!(!is_eligible(&usage, 20.0));
    }

    #[test]
    fn test_eligible_7d_critical_but_near_reset() {
        let now = auth::now_unix_secs();
        // 7d at 97% (3% remaining), but resets in 12h → still eligible
        let usage = usage_with(
            Some(window(30.0, Some(now + 3_600))),
            Some(window(97.0, Some(now + 12 * 3_600))),
        );
        assert!(is_eligible(&usage, 20.0));
    }

    #[test]
    fn test_ineligible_exhausted() {
        let usage = usage_with(Some(window(100.0, Some(1_000))), None);
        assert!(!is_eligible(&usage, 20.0));
    }

    // ── drain-first tests ───────────────────────────────────

    #[test]
    fn test_drain_first_prefers_sooner_reset() {
        let now = auth::now_unix_secs();
        let a = usage_with(Some(window(50.0, Some(now + 1_800))), Some(window(10.0, Some(now + 86_400))));
        let b = usage_with(Some(window(0.0, Some(now + 14_400))), Some(window(10.0, Some(now + 86_400))));

        let sa = score_drain_first(&a, 5.0, 20.0);
        let sb = score_drain_first(&b, 5.0, 20.0);

        assert!(sa > sb, "drain-first should prefer sooner reset: {sa} > {sb}");
    }

    #[test]
    fn test_drain_first_demotes_below_threshold() {
        let now = auth::now_unix_secs();
        let a = usage_with(Some(window(97.0, Some(now + 600))), Some(window(10.0, Some(now + 86_400))));
        let b = usage_with(Some(window(50.0, Some(now + 7_200))), Some(window(10.0, Some(now + 86_400))));

        let sa = score_drain_first(&a, 5.0, 20.0);
        let sb = score_drain_first(&b, 5.0, 20.0);

        assert!(sb > sa, "drain-first should demote below-threshold: {sb} > {sa}");
        assert!((500.0..=600.0).contains(&sa), "demoted score in 500-600 range: {sa}");
    }

    #[test]
    fn test_drain_first_below_threshold_beats_exhausted() {
        let now = auth::now_unix_secs();
        let a = usage_with(Some(window(97.0, Some(now + 600))), Some(window(10.0, Some(now + 86_400))));
        let b = usage_with(Some(window(100.0, Some(now + 300))), Some(window(10.0, Some(now + 86_400))));

        let sa = score_drain_first(&a, 5.0, 20.0);
        let sb = score_drain_first(&b, 5.0, 20.0);

        assert!(sa > sb, "below-threshold must beat exhausted: {sa} > {sb}");
    }

    #[test]
    fn test_drain_first_7d_exhausted() {
        let now = auth::now_unix_secs();
        // 7d exhausted, resets in 1 day (1440 min) → score = 100 - 1440 → clamped to 0
        let usage = usage_with(
            Some(window(50.0, Some(now + 1_800))),
            Some(window(100.0, Some(now + 86_400))),
        );
        let scored = score_drain_first(&usage, 5.0, 20.0);
        assert_eq!(scored, 0.0);
    }

    #[test]
    fn test_drain_first_no_primary() {
        assert_eq!(score_drain_first(&UsageInfo::default(), 5.0, 20.0), 0.0);
    }

    #[test]
    fn test_drain_first_exact_min_remaining_boundary() {
        let now = auth::now_unix_secs();
        // remaining = 5.0, min_remaining = 5.0 → NOT demoted (code uses `<`, not `<=`)
        let usage = usage_with(
            Some(window(95.0, Some(now + 1_800))),
            Some(window(10.0, Some(now + 86_400))),
        );
        let scored = score_drain_first(&usage, 5.0, 20.0);
        assert!(scored >= 1000.0, "exact boundary should be in usable tier: {scored}");
    }

    #[test]
    fn test_drain_first_7d_penalty_overrides_5h_advantage() {
        let now = auth::now_unix_secs();
        // Account A: 5h 0% used (full), 7d 95% used (5% remaining), resets in 6d
        let a = usage_with(
            Some(window(0.0, Some(now + 1_800))),
            Some(window(95.0, Some(now + 6 * 86_400))),
        );
        // Account B: 5h 50% used, 7d 30% used (70% remaining)
        let b = usage_with(
            Some(window(50.0, Some(now + 7_200))),
            Some(window(30.0, Some(now + 5 * 86_400))),
        );

        let sa = score_drain_first(&a, 5.0, 20.0);
        let sb = score_drain_first(&b, 5.0, 20.0);

        assert!(sb > sa, "7d-endangered account should lose to 7d-healthy one: {sb} > {sa}");
    }

    // ── round-robin tests ───────────────────────────────────

    #[test]
    fn test_round_robin_prefers_least_recent() {
        let a = usage_with(Some(window(30.0, Some(1_000))), Some(window(10.0, Some(9_999))));
        let b = usage_with(Some(window(30.0, Some(1_000))), Some(window(10.0, Some(9_999))));

        let sa = score_round_robin(&a, 100, false, 20.0);
        let sb = score_round_robin(&b, 200, false, 20.0);

        assert!(sa > sb, "round-robin should prefer least recently used: {sa} > {sb}");
    }

    #[test]
    fn test_round_robin_team_beats_non_team() {
        let a = usage_with(Some(window(30.0, Some(1_000))), Some(window(10.0, Some(9_999))));
        let b = usage_with(Some(window(30.0, Some(1_000))), Some(window(10.0, Some(9_999))));

        let sa = score_round_robin(&a, 100, true, 20.0);   // team, used at t=100
        let sb = score_round_robin(&b, 50, false, 20.0);    // non-team, used earlier

        assert!(sa > sb, "team should beat non-team in round-robin: {sa} > {sb}");
    }

    #[test]
    fn test_round_robin_excludes_unavailable() {
        let exhausted = usage_with(Some(window(100.0, Some(1_000))), None);
        let scored = score_round_robin(&exhausted, 0, false, 20.0);
        assert_eq!(scored, f64::NEG_INFINITY);
    }

    #[test]
    fn test_round_robin_excludes_7d_critical() {
        let now = auth::now_unix_secs();
        // 7d at 98%, resets in 5 days → ineligible
        let usage = usage_with(
            Some(window(30.0, Some(now + 3_600))),
            Some(window(98.0, Some(now + 5 * 86_400))),
        );
        let scored = score_round_robin(&usage, 0, false, 20.0);
        assert_eq!(scored, f64::NEG_INFINITY);
    }

    #[test]
    fn test_round_robin_real_epoch_timestamps_preserve_1s_ordering() {
        let healthy = usage_with(Some(window(30.0, Some(1_000))), Some(window(10.0, Some(9_999))));

        // Real-world epoch timestamps ~2024, differing by 1 second
        let ts_a: i64 = 1_700_000_000;
        let ts_b: i64 = 1_700_000_001;

        let sa = score_round_robin(&healthy, ts_a, true, 20.0);
        let sb = score_round_robin(&healthy, ts_b, true, 20.0);

        assert!(sa > sb, "1-second difference must be distinguishable at real epoch: {sa} vs {sb}");
        assert_ne!(sa, sb);
    }

    #[test]
    fn test_round_robin_same_last_used_is_stable() {
        let a = usage_with(Some(window(30.0, Some(1_000))), Some(window(10.0, Some(9_999))));
        let b = usage_with(Some(window(60.0, Some(1_000))), Some(window(10.0, Some(9_999))));

        // Same last_used → same score regardless of usage differences
        let sa = score_round_robin(&a, 100, false, 20.0);
        let sb = score_round_robin(&b, 100, false, 20.0);

        assert_eq!(sa, sb, "same last_used should produce same score");
    }

    // ── eligibility boundary tests ──────────────────────────

    #[test]
    fn test_eligible_at_exact_safety_margin() {
        let now = auth::now_unix_secs();
        // 7d at 80% (remaining = 20% = safety_margin) → eligible (>= check)
        let usage = usage_with(
            Some(window(30.0, Some(now + 3_600))),
            Some(window(80.0, Some(now + 5 * 86_400))),
        );
        assert!(is_eligible(&usage, 20.0));
    }

    #[test]
    fn test_eligible_with_safety_margin_zero() {
        let now = auth::now_unix_secs();
        // safety_margin = 0 → critical_pct = max(0*0.25, 1.0) = 1.0
        // 7d at 99.5% (remaining 0.5% < 1%), far reset → ineligible
        let usage = usage_with(
            Some(window(30.0, Some(now + 3_600))),
            Some(window(99.5, Some(now + 5 * 86_400))),
        );
        assert!(!is_eligible(&usage, 0.0));

        // 7d at 98% (remaining 2% > 1%) → eligible
        let usage2 = usage_with(
            Some(window(30.0, Some(now + 3_600))),
            Some(window(98.0, Some(now + 5 * 86_400))),
        );
        assert!(is_eligible(&usage2, 0.0));
    }

    #[test]
    fn test_eligible_no_7d_data() {
        // No secondary window → eligible (no 7d data = no 7d gate)
        let usage = usage_with(Some(window(30.0, Some(1_000))), None);
        assert!(is_eligible(&usage, 20.0));
    }

    // ── 7d adjustment tests ─────────────────────────────────

    #[test]
    fn test_7d_adjustment_safe_zone() {
        let usage = usage_with(
            Some(window(30.0, Some(1_000))),
            Some(window(50.0, Some(9_999))), // 50% remaining > 20% safety
        );
        assert_eq!(compute_7d_adjustment(&usage, 20.0), 0.0);
    }

    #[test]
    fn test_7d_adjustment_no_data() {
        assert_eq!(compute_7d_adjustment(&UsageInfo::default(), 20.0), -50.0);
    }

    #[test]
    fn test_7d_adjustment_critical_far_reset() {
        let now = auth::now_unix_secs();
        // 7d at 95% (5% remaining), resets in 6 days
        let usage = usage_with(
            Some(window(30.0, Some(now + 3_600))),
            Some(window(95.0, Some(now + 6 * 86_400))),
        );
        let adj = compute_7d_adjustment(&usage, 20.0);
        // pressure = (20-5)/20 = 0.75, no time relief → adj = -225
        assert!(
            (-230.0..=-220.0).contains(&adj),
            "expected ~-225: {adj}"
        );
    }

    #[test]
    fn test_7d_adjustment_critical_near_reset() {
        let now = auth::now_unix_secs();
        // 7d at 95% (5% remaining), resets in 12h
        // pressure = 0.75, time_relief = 1-12/48 = 0.75
        // effective = 0.75 × (1 - 0.75×0.8) = 0.75 × 0.4 = 0.3
        // adj = -300 × 0.3 = -90
        let usage = usage_with(
            Some(window(30.0, Some(now + 3_600))),
            Some(window(95.0, Some(now + 12 * 3_600))),
        );
        let adj = compute_7d_adjustment(&usage, 20.0);
        assert!((adj - (-90.0)).abs() < 0.01, "expected -90.0, got {adj}");
    }

    #[test]
    fn test_7d_adjustment_at_exact_safety_margin() {
        // 7d at 80% (remaining = 20% = safety_margin) → 0 (boundary is >=)
        let usage = usage_with(
            Some(window(30.0, Some(1_000))),
            Some(window(80.0, Some(9_999))),
        );
        assert_eq!(compute_7d_adjustment(&usage, 20.0), 0.0);
    }

    #[test]
    fn test_7d_adjustment_reset_in_past() {
        let now = auth::now_unix_secs();
        // 7d at 95% but reset time is in the past → time_relief = max(0, ...) = 0
        // pressure = 0.75, no relief → adj = -225
        let usage = usage_with(
            Some(window(30.0, Some(now + 3_600))),
            Some(window(95.0, Some(now - 3_600))),
        );
        // resets_at in past → remaining_hours = max(0, negative) = 0 → within 48h → time_relief = 1.0
        // effective = 0.75 × (1 - 1.0 × 0.8) = 0.75 × 0.2 = 0.15
        // adj = -300 × 0.15 = -45
        let adj = compute_7d_adjustment(&usage, 20.0);
        assert!((adj - (-45.0)).abs() < 0.01, "expected -45.0, got {adj}");
    }

    #[test]
    fn test_7d_adjustment_exactly_48h() {
        let now = auth::now_unix_secs();
        // 7d at 95%, resets in exactly 48h → time_relief = 1 - 48/48 = 0 → no relief
        let usage = usage_with(
            Some(window(30.0, Some(now + 3_600))),
            Some(window(95.0, Some(now + 48 * 3_600))),
        );
        // pressure = 0.75, time_relief = 0 → adj = -225
        assert_eq!(compute_7d_adjustment(&usage, 20.0), -225.0);
    }

    #[test]
    fn test_7d_adjustment_cannot_make_usable_score_negative() {
        let now = auth::now_unix_secs();
        // Worst case: 7d at 100% is handled before adjustment, but 99.9% with far reset:
        // pressure = (20-0.1)/20 ≈ 0.995, adj ≈ -298.5
        // base score for 5h 0% = 1100, final ≈ 801.5 → still positive
        let usage = usage_with(
            Some(window(0.0, Some(now + 3_600))),
            Some(window(99.9, Some(now + 7 * 86_400))),
        );
        let scored = score(&usage, 20.0);
        assert!(scored > 700.0, "usable account should never go very low: {scored}");
    }

    #[test]
    fn test_7d_adjustment_safety_margin_zero() {
        // safety_margin = 0 → pressure = 1.0 for any remaining < 0 (impossible),
        // but remaining = 50 >= 0 → safe zone → adj = 0
        let usage = usage_with(
            Some(window(30.0, Some(1_000))),
            Some(window(50.0, Some(9_999))),
        );
        assert_eq!(compute_7d_adjustment(&usage, 0.0), 0.0);
    }
}
