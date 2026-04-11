use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::auth::{self, CLIENT_ID, format_reqwest_error};

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

/// All data needed to score an account. Pure data, no I/O.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub alias: String,
    pub used_5h: f64,
    pub resets_at_5h: Option<i64>,
    pub used_7d: f64,
    pub resets_at_7d: Option<i64>,
    pub has_5h_data: bool,
    pub has_7d_data: bool,
    pub is_team: bool,
    pub is_free: bool,
    pub last_used: i64,
    pub now: i64,
    // Pool-level signals (set by caller after building all candidates)
    pub pool_size: usize,
    pub pool_exhausted: usize,
    pub team_priority: bool,
}

impl Candidate {
    /// Build from UsageInfo + metadata. `now` should be shared across all candidates.
    pub fn from_usage(
        alias: String,
        u: &UsageInfo,
        is_team: bool,
        is_free: bool,
        last_used: i64,
        now: i64,
    ) -> Self {
        Self {
            alias,
            used_5h: u.primary.as_ref().and_then(|w| w.used_percent).unwrap_or(0.0),
            resets_at_5h: u.primary.as_ref().and_then(|w| w.resets_at),
            used_7d: u.secondary.as_ref().and_then(|w| w.used_percent).unwrap_or(0.0),
            resets_at_7d: u.secondary.as_ref().and_then(|w| w.resets_at),
            has_5h_data: u.primary.is_some(),
            has_7d_data: u.secondary.is_some(),
            is_team,
            is_free,
            last_used,
            now,
            pool_size: 1,
            pool_exhausted: 0,
            team_priority: false,
        }
    }

    /// Reset-aware effective 5h usage: 0.0 if window has already reset.
    pub fn effective_used_5h(&self) -> f64 {
        if self.resets_at_5h.is_some_and(|ts| ts <= self.now) { 0.0 } else { self.used_5h }
    }

    /// Reset-aware effective 7d usage: 0.0 if window has already reset.
    pub fn effective_used_7d(&self) -> f64 {
        if self.resets_at_7d.is_some_and(|ts| ts <= self.now) { 0.0 } else { self.used_7d }
    }
}

/// Window durations in seconds (used for pace calculation).
pub const WINDOW_5H_SECS: i64 = 5 * 3600;
pub const WINDOW_7D_SECS: i64 = 7 * 86400;

/// Free plan accounts become ineligible below this 5h remaining%.
pub const FREE_FLOOR_PCT: f64 = 35.0;

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

fn usage_url() -> &'static str {
    static CELL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        std::env::var("CS_USAGE_URL").unwrap_or_else(|_| USAGE_URL.to_string())
    })
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
        match auth::codex_auth_path() {
            Ok(live) => {
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
            Err(err) => {
                warn!("[{alias}] Failed to determine codex auth path: {err}");
            }
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
                    .get(usage_url)
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
        .get(usage_url)
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
                    .get(usage_url)
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

pub(crate) async fn do_refresh_token(
    alias: &str,
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<RefreshedTokens> {
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        urlencoding::encode(refresh_token),
        urlencoding::encode(CLIENT_ID),
    );

    let token_url = auth::token_url();
    debug!("[{alias}] sending token refresh request to {token_url}");

    let resp = client
        .post(token_url)
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
        anyhow::bail!("[{alias}] token refresh failed: {err}");
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


/// Eligibility check on a Candidate (reset-aware).
pub fn is_candidate_eligible(c: &Candidate, safety_margin_7d: f64) -> bool {
    let used_5h = c.effective_used_5h();
    let used_7d = c.effective_used_7d();

    // Gate 1: 5h exhausted (and not past reset)
    if used_5h >= 100.0 { return false; }
    // Gate 2: 7d exhausted (and not past reset)
    if used_7d >= 100.0 { return false; }
    // Gate 3: 7d critically low and reset far away
    if c.has_7d_data {
        let remaining_7d = 100.0 - used_7d;
        let critical_pct = (safety_margin_7d * 0.25_f64).max(1.0);
        if remaining_7d < critical_pct {
            let hours_to_reset = c.resets_at_7d
                .map(|ts| ((ts - c.now) as f64 / 3600.0).max(0.0))
                .unwrap_or(f64::MAX);
            if hours_to_reset > 48.0 { return false; }
        }
    }
    // Gate 4: Free plan safety floor
    if c.is_free && c.has_5h_data {
        let remaining_5h = 100.0 - used_5h;
        if remaining_5h < FREE_FLOOR_PCT { return false; }
    }
    true
}

// ── adaptive scoring algorithm ─────────────────────────────

/// Adaptive scoring algorithm. Pure function, no I/O.
///
/// Automatically adjusts strategy based on pool state. No mode selection needed.
///
/// Components:
///   tier_bonus   — Team priority (0 or 500, configurable)
///   headroom     — Pace-aware effective remaining time (0..1100)
///   drain_value  — Quota that will be wasted if not used before reset (0..300)
///   sustain      — 7d budget-per-window sustainability (-800..0)
///   recency      — Spread usage across accounts (-60..0)
///
/// Pool-adaptive: drain_weight scales with pool_size and exhausted ratio.
pub fn score_unified(c: &Candidate, safety_margin_7d: f64) -> f64 {
    let used_5h = c.effective_used_5h();
    let used_7d = c.effective_used_7d();

    // ── Component A: tier_bonus (0 or 500) ──
    let tier_bonus = if c.is_team && c.team_priority { 500.0 } else { 0.0 };

    // ── Component B: headroom (0..1100) ──
    // Pace-aware: uses burn rate to project effective remaining time,
    // not just static remaining%.
    let headroom = if !c.has_5h_data {
        50.0
    } else if used_5h >= 100.0 {
        // Exhausted: score by time-to-reset (closer = higher, range 0..500)
        match c.resets_at_5h {
            None => 0.0,
            Some(reset_ts) => {
                let remaining_secs = (reset_ts - c.now).max(0) as f64;
                (500.0 - remaining_secs / 60.0).max(0.0)
            }
        }
    } else {
        // Pace-aware headroom: project remaining minutes using burn rate
        let remaining_pct = 100.0 - used_5h;
        match c.resets_at_5h {
            Some(reset_ts) => {
                let remaining_secs = (reset_ts - c.now).max(0) as f64;
                let elapsed_secs = (WINDOW_5H_SECS as f64 - remaining_secs).max(1.0);
                let burn_rate = used_5h / elapsed_secs; // %/sec

                if burn_rate > 0.001 {
                    // Project minutes until exhaustion at current rate
                    let projected_min = (remaining_pct / burn_rate) / 60.0;
                    // Cap at 300 min (5h), normalize to 0..100, add base 1000
                    1000.0 + (projected_min.min(300.0) / 300.0 * 100.0)
                } else {
                    // Near-zero burn rate → effectively full capacity
                    1000.0 + remaining_pct
                }
            }
            None => 1000.0 + remaining_pct,
        }
    };

    // ── Component C: sustain — 7d sustainability (-800..0) ──
    // Uses budget-per-window: how much 7d quota is available per remaining 5h window.
    const RELIEF_WINDOW_HOURS: f64 = 48.0;
    const MAX_RELIEF: f64 = 0.8;

    let sustain = if !c.has_7d_data {
        -50.0
    } else if used_7d >= 100.0 {
        // 7d exhausted: heavy penalty, relieved as reset approaches
        match c.resets_at_7d {
            None => -800.0, // no reset info: maximum penalty
            Some(reset_ts) => {
                let remaining_min = ((reset_ts - c.now).max(0) as f64) / 60.0;
                let relief = (1.0 - remaining_min / 10080.0).clamp(0.0, 1.0);
                -800.0 * (1.0 - relief)
            }
        }
    } else {
        let remaining_7d = 100.0 - used_7d;
        if remaining_7d >= safety_margin_7d {
            0.0
        } else {
            // Compute budget per remaining 5h window
            let budget_penalty = if let Some(reset_ts_7d) = c.resets_at_7d {
                let hours_to_7d_reset = ((reset_ts_7d - c.now) as f64 / 3600.0).max(0.0);
                let remaining_windows = (hours_to_7d_reset / 5.0).max(1.0);
                let budget_per_window = remaining_7d / remaining_windows;
                // If each window gets ≥ safety_margin worth of budget, it's fine
                if budget_per_window >= safety_margin_7d {
                    0.0
                } else {
                    // Shortfall: 0..1, higher = more pressure
                    ((safety_margin_7d - budget_per_window) / safety_margin_7d).clamp(0.0, 1.0)
                }
            } else {
                // No reset time: use simple pressure
                let pressure = if safety_margin_7d > 0.0 {
                    ((safety_margin_7d - remaining_7d) / safety_margin_7d).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                pressure
            };

            // Time relief: if 7d resets within 48h, reduce penalty
            let time_relief = c.resets_at_7d
                .map(|ts| {
                    let hours = ((ts - c.now) as f64 / 3600.0).max(0.0);
                    if hours < RELIEF_WINDOW_HOURS {
                        (1.0 - hours / RELIEF_WINDOW_HOURS).clamp(0.0, 1.0)
                    } else {
                        0.0
                    }
                })
                .unwrap_or(0.0);

            let effective = budget_penalty * (1.0 - time_relief * MAX_RELIEF);
            -800.0 * effective
        }
    };

    // ── Component D: drain_value (0..300) ──
    // Only activates when 5h reset is within 60 minutes AND there's quota to waste.
    // Pool-adaptive: larger pools with more available accounts → more aggressive drain.
    const DRAIN_WINDOW_MIN: f64 = 60.0;

    let raw_drain = if c.has_5h_data && used_5h < 100.0 {
        if let Some(reset_ts) = c.resets_at_5h {
            let remaining_min = ((reset_ts - c.now).max(0) as f64) / 60.0;
            if remaining_min <= DRAIN_WINDOW_MIN {
                let remaining_pct = 100.0 - used_5h;
                let urgency = ((DRAIN_WINDOW_MIN - remaining_min) / DRAIN_WINDOW_MIN).clamp(0.0, 1.0);
                // waste = remaining quota × urgency, scaled to 0..300
                (remaining_pct * urgency * 3.0).min(300.0)
            } else {
                0.0
            }
        } else {
            0.0
        }
    } else {
        0.0
    };

    // Pool-adaptive drain weight
    let drain_weight = if c.pool_size <= 2 {
        0.5 // Few accounts: be conservative, don't chase drain
    } else {
        let exhausted_ratio = c.pool_exhausted as f64 / c.pool_size as f64;
        if exhausted_ratio > 0.7 {
            0.3 // Most accounts exhausted: conserve what we have
        } else if c.pool_size >= 5 && exhausted_ratio < 0.3 {
            1.5 // Plenty of backup: drain aggressively
        } else {
            1.0
        }
    };

    let drain_value = raw_drain * drain_weight;

    // ── Component E: recency (-60..0) ──
    // Light spread penalty to avoid hammering the same account
    let recency = if c.last_used == 0 {
        0.0
    } else {
        let seconds_ago = (c.now - c.last_used).max(0) as f64;
        -(60.0 - (seconds_ago / 30.0)).clamp(0.0, 60.0)
    };

    tier_bonus + headroom + sustain + drain_value + recency
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
        let path = match crate::profile::profile_auth_path(alias) {
            Ok(p) => p,
            Err(_) => continue,
        };
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



    // ── adaptive scoring tests ──

    fn make_candidate(alias: &str, used_5h: f64, reset_5h: Option<i64>, used_7d: f64, reset_7d: Option<i64>) -> Candidate {
        Candidate {
            alias: alias.to_string(),
            used_5h, resets_at_5h: reset_5h,
            used_7d, resets_at_7d: reset_7d,
            has_5h_data: true, has_7d_data: true,
            is_team: false, is_free: false,
            last_used: 0, now: 1_000_000,
            pool_size: 5, pool_exhausted: 0,
            team_priority: true,
        }
    }

    #[test]
    fn test_adaptive_prefers_more_remaining() {
        let now = 1_000_000i64;
        let a = make_candidate("a", 30.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        let b = make_candidate("b", 60.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        assert!(score_unified(&a, 20.0) > score_unified(&b, 20.0));
    }

    #[test]
    fn test_adaptive_team_priority_dominates() {
        let now = 1_000_000i64;
        // Non-team with 0% used vs Team with 50% used → Team wins with priority
        let a = make_candidate("a", 0.0, Some(now + 18000), 10.0, Some(now + 5 * 86400));
        let mut b = make_candidate("b", 50.0, Some(now + 7200), 10.0, Some(now + 5 * 86400));
        b.is_team = true;
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(sb > sa, "team account should beat non-team even with worse 5h: {sb} > {sa}");
    }

    #[test]
    fn test_adaptive_team_priority_disabled() {
        let now = 1_000_000i64;
        // With team_priority=false, Team should not get +500 bonus
        let mut a = make_candidate("a", 0.0, Some(now + 18000), 10.0, Some(now + 5 * 86400));
        a.team_priority = false;
        let mut b = make_candidate("b", 50.0, Some(now + 7200), 10.0, Some(now + 5 * 86400));
        b.is_team = true;
        b.team_priority = false;
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(sa > sb, "without team_priority, more remaining should win: {sa} > {sb}");
    }

    #[test]
    fn test_adaptive_drain_near_reset() {
        let now = 1_000_000i64;
        // Account A: 40% used, resets in 30 min (within drain window)
        let a = make_candidate("a", 40.0, Some(now + 1800), 20.0, Some(now + 5 * 86400));
        // Account B: 40% used, resets in 4h (outside drain window)
        let b = make_candidate("b", 40.0, Some(now + 14400), 20.0, Some(now + 5 * 86400));
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(sa > sb, "near-reset account should score higher due to drain: {sa} > {sb}");
    }

    #[test]
    fn test_adaptive_no_drain_outside_window() {
        let now = 1_000_000i64;
        // Both accounts reset in 2h+ (outside 60-min drain window)
        // A: 40% used, resets in 2h → elapsed 3h → burn=40/3h → low rate, more headroom
        // B: 40% used, resets in 4h → elapsed 1h → burn=40/1h → high rate, less headroom
        let a = make_candidate("a", 40.0, Some(now + 7200), 20.0, Some(now + 5 * 86400));
        let b = make_candidate("b", 40.0, Some(now + 14400), 20.0, Some(now + 5 * 86400));
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(sa > 1000.0 && sb > 1000.0, "both should be usable: {sa}, {sb}");
        // A consumed 40% over 3h (lower burn rate) → more projected headroom
        assert!(sa > sb, "lower burn rate gives more headroom: {sa} > {sb}");
    }

    #[test]
    fn test_adaptive_7d_critical_overrides_5h() {
        let now = 1_000_000i64;
        let a = make_candidate("a", 0.0, Some(now + 18000), 95.0, Some(now + 6 * 86400));
        let b = make_candidate("b", 50.0, Some(now + 7200), 30.0, Some(now + 5 * 86400));
        assert!(score_unified(&b, 20.0) > score_unified(&a, 20.0), "7d-critical should lose");
    }

    #[test]
    fn test_adaptive_7d_budget_per_window() {
        let now = 1_000_000i64;
        // Account A: 7d 15% remaining, resets in 3 windows (15h) → 5%/window (tight)
        let a = make_candidate("a", 30.0, Some(now + 3600), 85.0, Some(now + 15 * 3600));
        // Account B: 7d 15% remaining, resets in 1 window (5h) → 15%/window (ok)
        let b = make_candidate("b", 30.0, Some(now + 3600), 85.0, Some(now + 5 * 3600));
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(sb > sa, "higher budget-per-window should score better: {sb} > {sa}");
    }

    #[test]
    fn test_adaptive_recency_breaks_tie() {
        let now = 1_000_000i64;
        let mut a = make_candidate("a", 40.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        a.last_used = now - 5; // used 5 seconds ago
        let mut b = make_candidate("b", 40.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        b.last_used = now - 1200; // used 20 minutes ago
        assert!(score_unified(&b, 20.0) > score_unified(&a, 20.0), "recently-used should score lower");
    }

    #[test]
    fn test_adaptive_reset_aware() {
        let now = 1_000_000i64;
        let a = make_candidate("a", 80.0, Some(now - 600), 20.0, Some(now + 5 * 86400));
        let score = score_unified(&a, 20.0);
        assert!(score > 1000.0, "past-reset account should score as fully available, got {score}");
    }

    #[test]
    fn test_adaptive_exhausted_scores_low() {
        let now = 1_000_000i64;
        let a = make_candidate("a", 100.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        let b = make_candidate("b", 50.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(sb > sa, "exhausted should score much lower: {sb} > {sa}");
        assert!(sa < 500.0, "exhausted score should be low: {sa}");
    }

    #[test]
    fn test_adaptive_pool_exhausted_conservative_drain() {
        let now = 1_000_000i64;
        // Most accounts exhausted → drain weight should be low
        let mut a = make_candidate("a", 40.0, Some(now + 1800), 20.0, Some(now + 5 * 86400));
        a.pool_size = 10;
        a.pool_exhausted = 8; // 80% exhausted
        let mut b = make_candidate("b", 40.0, Some(now + 1800), 20.0, Some(now + 5 * 86400));
        b.pool_size = 10;
        b.pool_exhausted = 1; // 10% exhausted
        // Both should have drain but b's pool allows more aggressive drain
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        assert!(sb > sa, "healthy pool should allow more drain: {sb} > {sa}");
    }

    #[test]
    fn test_adaptive_free_floor_ineligible() {
        let now = 1_000_000i64;
        let mut c = make_candidate("free1", 70.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        c.is_free = true;
        assert!(!is_candidate_eligible(&c, 20.0));
    }

    #[test]
    fn test_adaptive_no_data_low_score() {
        let c = Candidate {
            alias: "unknown".to_string(),
            used_5h: 0.0, resets_at_5h: None,
            used_7d: 0.0, resets_at_7d: None,
            has_5h_data: false, has_7d_data: false,
            is_team: false, is_free: false,
            last_used: 0, now: 1_000_000,
            pool_size: 1, pool_exhausted: 0,
            team_priority: true,
        };
        // headroom=50 (no 5h data) + sustain=-50 (no 7d data) = 0
        assert_eq!(score_unified(&c, 20.0), 0.0, "no-data account should score exactly 0");
    }

    #[test]
    fn test_adaptive_both_windows_exhausted() {
        let now = 1_000_000i64;
        // 5h exhausted (no reset info) + 7d exhausted (resets in 7 days)
        let mut c = make_candidate("both_dead", 100.0, None, 100.0, Some(now + 7 * 86400));
        c.has_5h_data = true;
        c.has_7d_data = true;
        let s = score_unified(&c, 20.0);
        // headroom=0 (exhausted, no reset), sustain should still be heavily negative
        assert!(s < -700.0, "doubly-exhausted account must score very low, got {s}");
    }

    #[test]
    fn test_adaptive_both_windows_exhausted_no_reset_info() {
        // Worst case: both exhausted, no reset info at all
        let c = Candidate {
            alias: "dead".to_string(),
            used_5h: 100.0, resets_at_5h: None,
            used_7d: 100.0, resets_at_7d: None,
            has_5h_data: true, has_7d_data: true,
            is_team: false, is_free: false,
            last_used: 0, now: 1_000_000,
            pool_size: 1, pool_exhausted: 1,
            team_priority: false,
        };
        let s = score_unified(&c, 20.0);
        assert!(s < -700.0, "doubly-exhausted no-reset account must score very low, got {s}");
    }

    #[test]
    fn test_adaptive_pace_aware_headroom() {
        let now = 1_000_000i64;
        // Account A: 30% used, resets in 4h → elapsed 1h → burn=30%/3600s (fast)
        // projected exhaustion = 70 / (30/3600) / 60 ≈ 140 min
        let a = make_candidate("a", 30.0, Some(now + 4 * 3600), 20.0, Some(now + 5 * 86400));
        // Account B: 30% used, resets in 1h → elapsed 4h → burn=30%/14400s (slow)
        // projected exhaustion = 70 / (30/14400) / 60 ≈ 560 min → capped 300 min
        let b = make_candidate("b", 30.0, Some(now + 1 * 3600), 20.0, Some(now + 5 * 86400));
        let sa = score_unified(&a, 20.0);
        let sb = score_unified(&b, 20.0);
        // B has slower burn rate → higher projected exhaustion → higher headroom
        assert!(sb > sa, "slower burn rate should give higher headroom: {sb} > {sa}");
    }

    #[test]
    fn test_candidate_eligible_basic() {
        let now = 1_000_000i64;
        let c = make_candidate("ok", 30.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        assert!(is_candidate_eligible(&c, 20.0));
    }

    #[test]
    fn test_candidate_ineligible_5h_exhausted() {
        let now = 1_000_000i64;
        let c = make_candidate("ex", 100.0, Some(now + 3600), 20.0, Some(now + 5 * 86400));
        assert!(!is_candidate_eligible(&c, 20.0));
    }

    #[test]
    fn test_candidate_ineligible_7d_critical_far() {
        let now = 1_000_000i64;
        // 7d at 97% (3% remaining < critical 5%), resets in 5 days
        let c = make_candidate("crit", 30.0, Some(now + 3600), 97.0, Some(now + 5 * 86400));
        assert!(!is_candidate_eligible(&c, 20.0));
    }

    #[test]
    fn test_candidate_eligible_7d_critical_near_reset() {
        let now = 1_000_000i64;
        // 7d at 97%, but resets in 12h → still eligible
        let c = make_candidate("near", 30.0, Some(now + 3600), 97.0, Some(now + 12 * 3600));
        assert!(is_candidate_eligible(&c, 20.0));
    }
}
