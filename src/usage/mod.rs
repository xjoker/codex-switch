use std::time::Duration;

use serde::{Deserialize, Serialize};

mod api;
mod parse;
mod reset_credits;
mod scoring;

pub(crate) use api::{apply_account_routing_headers, do_refresh_token};
pub use api::{
    fetch_usage_retried, fetch_usage_retried_force, fetch_usage_retried_unattended,
    refresh_expiring_tokens, validate_import_auth,
};
pub(crate) use reset_credits::should_fetch_reset_credit_details;
// Re-exported for the lib target's public API (used by integration tests via
// `codex_switch::usage::X`); the binary target doesn't call these through this
// path itself, so they'd otherwise look unused there.
#[allow(unused_imports)]
pub use api::fetch_usage_with_refresh;
#[allow(unused_imports)]
pub use api::refresh_expiring_tokens_within;
#[allow(unused_imports)]
pub use parse::parse_usage;
pub use reset_credits::{
    consume_reset_credit_by_id, earliest_reset_credit, fetch_earliest_reset_credit,
    refresh_reset_credits_for_profile,
};
pub use scoring::{
    is_available, is_candidate_eligible, pace_percent, pick_switch_target, score_candidates,
    usage_has_active_warmup_window, visible_pace_percent,
};
#[allow(unused_imports)]
pub use scoring::{score_unified, warmup_window_active};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct WindowUsage {
    pub used_percent: Option<f64>,
    pub resets_at: Option<i64>,
    pub window_minutes: Option<i64>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SpendControlLimit {
    pub source: Option<String>,
    pub limit: Option<String>,
    pub used: Option<String>,
    pub remaining: Option<String>,
    pub remaining_percent: Option<f64>,
    pub resets_at: Option<i64>,
}

/// One entry from the `additional_rate_limits` array in the usage API response.
/// Represents a metered feature (e.g. `codex_other`) with its own independent windows.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AdditionalRateLimit {
    pub limit_name: Option<String>,
    pub metered_feature: Option<String>,
    pub allowed: Option<bool>,
    pub limit_reached: Option<bool>,
    pub primary: Option<WindowUsage>,
    pub secondary: Option<WindowUsage>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ResetCredit {
    pub id: String,
    pub granted_at: Option<String>,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ConsumedResetCredit {
    pub credit: ResetCredit,
    pub code: Option<String>,
    pub windows_reset: Option<u64>,
    pub redeemed_at: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct UsageInfo {
    pub fetched_at: Option<i64>,
    pub primary: Option<WindowUsage>,   // 5h window
    pub secondary: Option<WindowUsage>, // 7d window
    pub credits_balance: Option<f64>,
    pub unlimited_credits: Option<bool>,
    /// plan_type from usage API response (authoritative; overrides JWT claims when present)
    pub plan_type: Option<String>,
    pub reset_credits_available_count: Option<u64>,
    pub reset_credits: Vec<ResetCredit>,
    pub reset_credits_error: Option<String>,
    /// Explicit account/workspace-level restriction reported by the API.
    pub account_limited: bool,
    /// Backend-classified limit reason, preserved for detailed diagnostics.
    pub rate_limit_reached_type: Option<String>,
    /// Effective workspace/user spend-control limit, when supplied by the backend.
    pub individual_limit: Option<Box<SpendControlLimit>>,
    /// Per-feature rate limits from `additional_rate_limits[]` (e.g. codex_other).
    pub additional_limits: Vec<AdditionalRateLimit>,
}

/// One assembled display row for an additional-limit pool. Pure data,
/// derived from `AdditionalRateLimit` so CLI and TUI renderers can share
/// the same assembly logic instead of each re-deriving `unavailable`.
#[derive(Debug, Clone)]
pub struct PoolRow {
    pub limit_name: String,
    /// True when the API reports the pool as exhausted or disallowed.
    pub unavailable: bool,
    pub primary: Option<WindowUsage>,
    pub secondary: Option<WindowUsage>,
}

/// Assemble display rows from the raw `additional_limits` array. Returns an
/// empty vec when there are no additional pools (the common case today).
pub fn additional_pool_rows(limits: &[AdditionalRateLimit]) -> Vec<PoolRow> {
    limits
        .iter()
        .map(|l| PoolRow {
            limit_name: l.limit_name.clone().unwrap_or_else(|| "pool".to_string()),
            unavailable: l.limit_reached == Some(true) || l.allowed == Some(false),
            primary: l.primary.clone(),
            secondary: l.secondary.clone(),
        })
        .collect()
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
        let force_exhausted = u.account_limited;
        Self {
            alias,
            used_5h: if force_exhausted {
                100.0
            } else {
                u.primary
                    .as_ref()
                    .and_then(|w| w.used_percent)
                    .unwrap_or(0.0)
            },
            resets_at_5h: (!force_exhausted)
                .then(|| u.primary.as_ref().and_then(|w| w.resets_at))
                .flatten(),
            used_7d: if force_exhausted {
                100.0
            } else {
                u.secondary
                    .as_ref()
                    .and_then(|w| w.used_percent)
                    .unwrap_or(0.0)
            },
            resets_at_7d: (!force_exhausted)
                .then(|| u.secondary.as_ref().and_then(|w| w.resets_at))
                .flatten(),
            has_5h_data: u.primary.is_some() || force_exhausted,
            has_7d_data: u.secondary.is_some() || force_exhausted,
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
        if self.resets_at_5h.is_some_and(|ts| ts <= self.now) {
            0.0
        } else {
            self.used_5h
        }
    }

    /// Reset-aware effective 7d usage: 0.0 if window has already reset.
    pub fn effective_used_7d(&self) -> f64 {
        if self.resets_at_7d.is_some_and(|ts| ts <= self.now) {
            0.0
        } else {
            self.used_7d
        }
    }
}

/// Window durations in seconds (used for pace calculation).
pub const WINDOW_5H_SECS: i64 = 5 * 3600;
pub const WINDOW_7D_SECS: i64 = 7 * 86400;

/// Free plan accounts become ineligible below this 5h remaining%.
pub const FREE_FLOOR_PCT: f64 = 35.0;

/// Minimum elapsed time before a quota window proves that warmup truly stuck.
pub const MIN_WARMUP_ELAPSED_SECS: i64 = 5 * 60;

const MAX_RETRIES: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// How much of the cache a usage fetch may skip.
///
/// One boolean used to cover two unrelated requests: wanting numbers that are
/// not stale, and wanting a verdict the auth server has already given to be
/// asked again. Only a person can mean the second — an unattended timer that
/// re-presents a spent credential every polling interval learns nothing and
/// pays for the rejection every time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refresh {
    /// Serve a fresh cache entry as-is. Everyday reads.
    Cached,
    /// Ignore the usage TTL, but honour a recorded auth verdict. What a timer
    /// with nobody watching wants.
    Unattended,
    /// Ignore both. Reserved for a person explicitly asking again, and the only
    /// way back from a verdict recorded in error.
    Forced,
}

impl Refresh {
    pub(super) fn skips_usage_cache(self) -> bool {
        !matches!(self, Refresh::Cached)
    }

    pub(super) fn may_re_present_a_rejected_credential(self) -> bool {
        matches!(self, Refresh::Forced)
    }
}

pub struct RefreshedTokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

/// A refresh the auth server rejected outright (bad/consumed credential).
///
/// OpenAI rotates `refresh_token` on every use and answers replays with
/// `refresh_token_reused`, so retrying such a failure can never succeed — it
/// only burns round trips. Carried as a typed error so retry loops can
/// recognise it via `anyhow::Error::downcast_ref`.
#[derive(Debug, Clone)]
pub struct TerminalAuthError {
    /// Server-provided error code (or `http_<status>` when the body had none).
    pub code: String,
    /// Server-provided human-readable message, when present.
    pub message: Option<String>,
}

impl TerminalAuthError {
    /// Short, actionable line for list/TUI status columns.
    pub fn summary(&self) -> String {
        format!("re-login required ({})", self.code)
    }
}

impl std::fmt::Display for TerminalAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "token refresh rejected, sign in again — {}", self.code)?;
        if let Some(message) = &self.message {
            write!(f, ": {message}")?;
        }
        Ok(())
    }
}

impl std::error::Error for TerminalAuthError {}

/// Outcome of one usage fetch attempt.
///
/// `refreshed` is populated whenever the auth server issued new tokens during
/// the attempt — **including when `result` is an error**. The rotated
/// `refresh_token` is the only one the server will still accept, so callers
/// must persist it before propagating the failure.
pub struct UsageFetchOutcome {
    pub refreshed: Option<RefreshedTokens>,
    pub result: anyhow::Result<UsageInfo>,
}

/// Outcome of validating an auth.json on the `import` path.
///
/// Same split as [`UsageFetchOutcome`], and for the same reason: validation
/// refreshes the credential before it calls the usage API, so `refreshed` is
/// populated **even when `result` is an error**. `import` owns a local copy of
/// the auth value, so returning only the error would drop the single credential
/// the auth server still accepts and brick the account being imported.
pub struct ImportValidation {
    pub refreshed: Option<RefreshedTokens>,
    /// Account id that the Usage API accepted for these credentials.
    pub validated_account_id: Option<String>,
    pub result: anyhow::Result<UsageInfo>,
}

/// Structured error for usage fetch failures.
#[derive(Debug, Clone)]
pub struct UsageError {
    /// Short summary for user-facing display (e.g. "HTTP 401 Unauthorized")
    pub summary: String,
    /// Full detail for debug/log (e.g. "Usage API failed (HTTP 401), token refresh also failed: ...")
    pub detail: String,
}

impl UsageError {
    /// The auth server issued rotated credentials but they could not be written
    /// to disk.
    ///
    /// This is *not* a rejected refresh: the new tokens are valid, they simply
    /// never reached the profile, while the previous `refresh_token` is already
    /// dead server-side. Continuing would leave the user with an account that
    /// silently stops working at the next start, so the wording has to point at
    /// the local write failure and carry the underlying IO/permission cause.
    pub fn token_persist_failed(alias: &str, cause: &anyhow::Error) -> Self {
        Self {
            summary: "refreshed token not saved".to_string(),
            detail: format!(
                "[{alias}] token refresh succeeded but the rotated credentials could not be saved: \
                 {cause:#}. The auth server has already invalidated the previous refresh token, so \
                 this profile may need to sign in again once the write problem is fixed."
            ),
        }
    }
}

/// One profile whose rotated credentials could not be written to disk during an
/// opportunistic refresh.
///
/// Opportunistic refresh is a batch, and the daemon runs it on a timer, so a
/// single failure must neither abort the remaining profiles nor disappear into
/// a log line: it is collected and handed back for the caller to surface.
#[derive(Debug, Clone)]
pub struct TokenPersistFailure {
    pub alias: String,
    pub error: UsageError,
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}

/// One scored candidate. Pure data, no I/O.
pub struct ScoredCandidate {
    pub candidate: Candidate,
    pub usage: UsageInfo,
    pub score: f64,
}

#[cfg(test)]
mod pool_row_tests {
    use super::*;

    #[test]
    fn empty_additional_limits_yields_no_rows() {
        assert!(additional_pool_rows(&[]).is_empty());
    }

    #[test]
    fn pool_with_both_windows_produces_one_row() {
        let limits = vec![AdditionalRateLimit {
            limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
            metered_feature: Some("codex_other".to_string()),
            allowed: Some(true),
            limit_reached: Some(false),
            primary: Some(WindowUsage {
                used_percent: Some(42.0),
                resets_at: Some(1000),
                window_minutes: Some(300),
            }),
            secondary: Some(WindowUsage {
                used_percent: Some(10.0),
                resets_at: Some(2000),
                window_minutes: Some(10_080),
            }),
        }];

        let rows = additional_pool_rows(&limits);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].limit_name, "GPT-5.3-Codex-Spark");
        assert!(!rows[0].unavailable);
        assert_eq!(rows[0].primary.as_ref().unwrap().used_percent, Some(42.0));
        assert_eq!(rows[0].secondary.as_ref().unwrap().used_percent, Some(10.0));
    }

    #[test]
    fn limit_reached_pool_is_marked_unavailable() {
        let limits = vec![AdditionalRateLimit {
            limit_name: Some("exhausted-pool".to_string()),
            limit_reached: Some(true),
            allowed: Some(true),
            ..Default::default()
        }];

        let rows = additional_pool_rows(&limits);
        assert!(rows[0].unavailable);
    }

    #[test]
    fn disallowed_pool_is_marked_unavailable() {
        let limits = vec![AdditionalRateLimit {
            limit_name: Some("disallowed-pool".to_string()),
            allowed: Some(false),
            limit_reached: Some(false),
            ..Default::default()
        }];

        let rows = additional_pool_rows(&limits);
        assert!(rows[0].unavailable);
    }
}
