use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::auth::{self, CLIENT_ID, format_reqwest_error};
use crate::http_retry::{self, ReplaySafety};

use super::parse::parse_usage_checked;
use super::reset_credits::merge_cached_reset_credits;
use super::{
    ImportValidation, MAX_RETRIES, RETRY_DELAY, Refresh, RefreshedTokens, TerminalAuthError,
    TokenPersistFailure, UsageError, UsageFetchOutcome, UsageInfo,
};

#[derive(Debug)]
struct UsageRateLimited {
    retry_after: Duration,
}

impl std::fmt::Display for UsageRateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Usage API rate limited (HTTP 429; retry after {}s)",
            self.retry_after.as_secs()
        )
    }
}

impl std::error::Error for UsageRateLimited {}

static USAGE_COOLDOWNS: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

fn usage_cooldowns() -> &'static Mutex<HashMap<String, Instant>> {
    USAGE_COOLDOWNS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn usage_cooldown_remaining(key: &str) -> Option<Duration> {
    let now = Instant::now();
    let mut cooldowns = usage_cooldowns()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let until = cooldowns.get(key).copied()?;
    if until <= now {
        cooldowns.remove(key);
        return None;
    }
    Some(until.duration_since(now))
}

fn record_usage_cooldown(key: &str, delay: Duration) {
    let mut cooldowns = usage_cooldowns()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let until = Instant::now() + delay;
    cooldowns
        .entry(key.to_string())
        .and_modify(|saved| *saved = (*saved).max(until))
        .or_insert(until);
}

fn clear_usage_cooldown(key: &str) {
    usage_cooldowns()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(key);
}

fn rate_limited(
    alias: &str,
    account_id: Option<&str>,
    response: &http_retry::BufferedResponse,
) -> anyhow::Error {
    let delay = response.retry_after.unwrap_or(Duration::from_secs(30));
    record_usage_cooldown(account_id.unwrap_or(alias), delay);
    UsageRateLimited { retry_after: delay }.into()
}

pub(crate) fn apply_account_routing_headers(
    mut builder: reqwest::RequestBuilder,
    account_id: Option<&str>,
    is_fedramp: bool,
) -> reqwest::RequestBuilder {
    if let Some(account_id) = account_id.filter(|value| !value.trim().is_empty()) {
        builder = builder.header("ChatGPT-Account-ID", account_id);
    }
    if is_fedramp {
        builder = builder.header("X-OpenAI-Fedramp", "true");
    }
    builder
}

/// The auth server reports failures in two shapes: the OAuth 2.0 standard
/// `{"error": "invalid_grant", "error_description": "..."}` and OpenAI's
/// `{"error": {"code": ..., "message": ..., "type": ...}}`. Accept both, or the
/// whole response fails to deserialize and the actionable server message is
/// replaced by a serde type error.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RefreshError {
    Code(String),
    Detail {
        code: Option<String>,
        message: Option<String>,
        #[serde(rename = "type")]
        kind: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    error: Option<RefreshError>,
    error_description: Option<String>,
}

impl RefreshResponse {
    /// Normalize both wire shapes to `(code, message)`.
    fn error_parts(&self) -> Option<(String, Option<String>)> {
        match self.error.as_ref()? {
            RefreshError::Code(code) => Some((code.clone(), self.error_description.clone())),
            RefreshError::Detail {
                code,
                message,
                kind,
            } => Some((
                code.clone()
                    .or_else(|| kind.clone())
                    .unwrap_or_else(|| "unknown_error".to_string()),
                message.clone().or_else(|| self.error_description.clone()),
            )),
        }
    }
}

/// Auth-server verdicts no retry can change, independent of HTTP status.
const TERMINAL_AUTH_CODES: &[&str] = &[
    "refresh_token_reused",
    "refresh_token_invalidated",
    "invalid_grant",
    "invalid_client",
    "unauthorized_client",
    "access_denied",
];

/// The subset of [`TERMINAL_AUTH_CODES`] that may outlive the invocation.
///
/// Both are OpenAI-specific and say one unambiguous thing: *this* credential is
/// gone, and only signing in again produces another. Everything else in
/// `TERMINAL_AUTH_CODES` is standard OAuth wording that assorted servers and
/// intermediaries also emit for transient conditions — `invalid_grant` for
/// clock skew, `access_denied` from a gateway — and a bare 4xx can as easily be
/// a proxy, a WAF, or a captive portal in front of the real endpoint.
///
/// Guessing wrong in this direction is expensive: a recorded verdict survives
/// until the next sign-in, so a transient cause would leave a working account
/// showing "re-login required" with nothing to suggest that `--force` clears
/// it. Guessing wrong the other way costs one round trip. So only these two are
/// remembered; every code in `TERMINAL_AUTH_CODES` still stops the retry loop
/// within the call it happened in.
const MEMORABLE_AUTH_CODES: &[&str] = &["refresh_token_reused", "refresh_token_invalidated"];

fn is_memorable_auth_verdict(code: &str) -> bool {
    MEMORABLE_AUTH_CODES.contains(&code)
}

/// A 4xx from the token endpoint means the credential itself was rejected, so
/// replaying it only re-triggers reuse detection. 429/408 are load/timing
/// signals and stay retryable.
fn is_terminal_auth_failure(code: &str, status: reqwest::StatusCode) -> bool {
    if matches!(
        status,
        reqwest::StatusCode::TOO_MANY_REQUESTS | reqwest::StatusCode::REQUEST_TIMEOUT
    ) {
        return false;
    }
    TERMINAL_AUTH_CODES.contains(&code) || status.is_client_error()
}

/// Record a verdict against the credential that earned it.
///
/// Keyed by the token rather than the alias so that signing in again clears it
/// without every credential-writing path having to remember to.
async fn remember_terminal_verdict(
    alias: &str,
    code: &str,
    refresh_token: Option<&str>,
    error: &UsageError,
) {
    if !is_memorable_auth_verdict(code) {
        return;
    }
    let Some(refresh_token) = refresh_token else {
        return;
    };
    crate::cache::put_auth_failure_async(alias, refresh_token, error).await;
}

fn format_refresh_error(code: &str, message: Option<&str>) -> String {
    match message {
        Some(message) => format!("{code}: {message}"),
        None => code.to_string(),
    }
}

fn usage_url() -> String {
    std::env::var("CS_USAGE_URL").unwrap_or_else(|_| USAGE_URL.to_string())
}

fn token_needs_refresh(access_token: &str, id_token: Option<&str>, margin_secs: i64) -> bool {
    crate::jwt::is_token_expiring(access_token, margin_secs).unwrap_or(false)
        || id_token
            .is_some_and(|token| crate::jwt::is_token_expiring(token, margin_secs).unwrap_or(false))
}

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// Extract a short summary from an error message for user-facing display.
/// Looks for "HTTP <status>" patterns; falls back to first line truncated.
pub(super) fn extract_error_summary(err: &str) -> String {
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
pub async fn fetch_usage_retried(
    alias: &str,
    profile_path: &Path,
    current_alias: &str,
) -> std::result::Result<UsageInfo, UsageError> {
    fetch_usage_retried_inner(alias, profile_path, current_alias, Refresh::Cached, true).await
}

/// Bypass the usage TTL for current numbers, but leave a recorded auth verdict
/// standing. For callers running on a timer with nobody watching.
pub async fn fetch_usage_retried_unattended(
    alias: &str,
    profile_path: &Path,
    current_alias: &str,
) -> std::result::Result<UsageInfo, UsageError> {
    fetch_usage_retried_inner(
        alias,
        profile_path,
        current_alias,
        Refresh::Unattended,
        true,
    )
    .await
}

/// Daemon batch-refresh variant. Credential rotations are still persisted
/// immediately; only the usage-cache write is deferred to one batch commit.
pub(crate) async fn fetch_usage_retried_unattended_deferred_cache(
    alias: &str,
    profile_path: &Path,
    current_alias: &str,
) -> std::result::Result<UsageInfo, UsageError> {
    fetch_usage_retried_inner(
        alias,
        profile_path,
        current_alias,
        Refresh::Unattended,
        false,
    )
    .await
}

/// Bypass every cache, including a recorded auth verdict. Only for a person
/// explicitly asking again — see [`Refresh::Forced`].
pub async fn fetch_usage_retried_force(
    alias: &str,
    profile_path: &Path,
    current_alias: &str,
) -> std::result::Result<UsageInfo, UsageError> {
    fetch_usage_retried_inner(alias, profile_path, current_alias, Refresh::Forced, true).await
}

/// Write credentials the auth server just rotated back to the profile.
///
/// The previous `refresh_token` is dead the moment these were issued, so a
/// failed write leaves only an in-memory copy of the sole credential the server
/// still accepts. Losing it bricks the account, which makes this a reportable
/// failure rather than something to warn about and walk past.
fn persist_refreshed_tokens(
    alias: &str,
    presented_refresh_token: &str,
    new_tokens: &RefreshedTokens,
) -> std::result::Result<(), UsageError> {
    crate::profile::update_profile_tokens_if_refresh_matches(
        alias,
        presented_refresh_token,
        &new_tokens.id_token,
        &new_tokens.access_token,
        &new_tokens.refresh_token,
    )
    .map(|_| ())
    .map_err(|err| UsageError::token_persist_failed(alias, &err))
}

fn resolve_refreshed_tokens(
    response: RefreshResponse,
    status: reqwest::StatusCode,
    current_id_token: Option<&str>,
    current_access_token: Option<&str>,
    current_refresh_token: &str,
) -> Result<RefreshedTokens> {
    if let Some((code, message)) = response.error_parts() {
        if is_terminal_auth_failure(&code, status) {
            return Err(TerminalAuthError { code, message }.into());
        }
        anyhow::bail!(
            "token refresh failed: {}",
            format_refresh_error(&code, message.as_deref())
        );
    }

    // A non-2xx without a recognizable error body still means no tokens were
    // issued; falling through would "succeed" by echoing the current tokens.
    if !status.is_success() {
        let code = format!("http_{}", status.as_u16());
        if is_terminal_auth_failure(&code, status) {
            return Err(TerminalAuthError {
                code,
                message: None,
            }
            .into());
        }
        anyhow::bail!("token refresh failed: HTTP {status}");
    }

    let id_token = response
        .id_token
        .or_else(|| current_id_token.map(str::to_string))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "token refresh response omitted id_token and no existing id_token is available"
            )
        })?;
    let access_token = response
        .access_token
        .or_else(|| current_access_token.map(str::to_string))
        .ok_or_else(|| anyhow::anyhow!("token refresh response omitted access_token and no existing access_token is available"))?;
    let refresh_token = response
        .refresh_token
        .unwrap_or_else(|| current_refresh_token.to_string());

    Ok(RefreshedTokens {
        id_token,
        access_token,
        refresh_token,
    })
}

/// Credentials re-read from a profile after a refresh was rejected.
struct ReloadedCredentials {
    id_token: Option<String>,
    access_token: String,
    refresh_token: String,
}

/// Re-read `profile_path` after the auth server rejected a refresh outright.
///
/// The daemon timer and the CLI (`list`, `best`) refresh the same profile from
/// separate processes, so both can read the same `refresh_token` and present
/// it. The server rotates it for exactly one of them and answers the other
/// `refresh_token_reused` — a verdict about that *token*, not about the
/// account, whose live credentials the winner has meanwhile written to disk.
///
/// Returns the stored credentials only when their `refresh_token` differs from
/// `presented`. An unchanged profile means nobody rotated anything, so the
/// rejection is the real thing and the caller must keep reporting it.
fn reload_rotated_credentials(
    profile_path: &Path,
    presented: Option<&str>,
) -> Option<ReloadedCredentials> {
    let val = auth::read_auth(profile_path).ok()?;
    let (access_token, refresh_token) = auth::extract_tokens(&val);
    let refresh_token = refresh_token?;
    if Some(refresh_token.as_str()) == presented {
        return None;
    }
    Some(ReloadedCredentials {
        id_token: auth::extract_id_token(&val),
        access_token: access_token?,
        refresh_token,
    })
}

async fn fetch_usage_retried_inner(
    alias: &str,
    profile_path: &Path,
    _current_alias: &str,
    refresh: Refresh,
    write_usage_cache: bool,
) -> std::result::Result<UsageInfo, UsageError> {
    if !refresh.skips_usage_cache() {
        if let Some(cached) = crate::cache::get_async(alias).await {
            debug!("{alias}: cache hit");
            return Ok(cached);
        }
        debug!("{alias}: cache miss, fetching from API");
    } else {
        debug!("{alias}: {refresh:?} refresh, bypassing the usage cache");
    }

    let val = auth::read_auth(profile_path).map_err(|e| {
        let detail = format!("failed to read auth file {}: {e}", profile_path.display());
        UsageError {
            summary: "auth file unreadable".into(),
            detail,
        }
    })?;
    let account_info = crate::jwt::parse_account_info(&val);
    let account_id = account_info.account_id;
    let is_fedramp = account_info.is_fedramp;
    let mut id_token = auth::extract_id_token(&val);
    let (access_token, refresh_token) = auth::extract_tokens(&val);
    let mut refresh_token = refresh_token;

    // A verdict the auth server already named stands until the credential is
    // replaced, so re-presenting it buys nothing but the round trip. Only an
    // explicit user force skips this — see [`Refresh`].
    if !refresh.may_re_present_a_rejected_credential()
        && let Some(rt) = refresh_token.as_deref()
        && let Some(known) = crate::cache::get_auth_failure_async(alias, rt).await
    {
        debug!("{alias}: credential already rejected by the auth server, not retrying");
        return Err(known);
    }
    if !refresh.may_re_present_a_rejected_credential()
        && let Some(remaining) = usage_cooldown_remaining(account_id.as_deref().unwrap_or(alias))
    {
        return Err(UsageError {
            summary: "HTTP 429 rate limited".into(),
            detail: format!(
                "[{alias}] Usage API cooling down for {:.1}s after HTTP 429",
                remaining.as_secs_f64()
            ),
        });
    }

    let mut at = match access_token {
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
    // A rejected refresh may just mean a concurrent refresh of the same profile
    // won the rotation, so one such rejection buys a single extra round in which
    // the winner's stored token is tried. Granted at most once: two peers each
    // re-arming on the other's write would otherwise keep this loop alive
    // without either ever reporting a result.
    let mut recovery_round_used = false;
    // Carries the server's error code alongside the error so the verdict can be
    // recorded if the recovery round confirms it.
    let mut pending_terminal: Option<(UsageError, String)> = None;
    let mut max_attempts = MAX_RETRIES;
    let mut attempt = 0;
    while attempt < max_attempts {
        if attempt > 0 {
            debug!("[{alias}] retry attempt {}/{max_attempts}", attempt + 1);
            tokio::time::sleep(RETRY_DELAY).await;
        }

        // Deliberately *after* the delay. The winner writes the rotated token
        // only once the server has issued it, which is already when our replay
        // starts being refused — reading the profile the instant the rejection
        // arrives can still find the old token and mislabel a healthy account.
        if let Some((terminal, code)) = pending_terminal.take() {
            let Some(stored) = reload_rotated_credentials(profile_path, refresh_token.as_deref())
            else {
                // Nothing else rotated the credential, so the rejection was
                // about the token this profile still holds — final.
                remember_terminal_verdict(alias, &code, refresh_token.as_deref(), &terminal).await;
                return Err(terminal);
            };
            info!(
                "[{alias}] refresh was rejected but the profile now holds a different token; \
                 a concurrent refresh won the rotation, retrying with the stored credentials"
            );
            at = stored.access_token;
            id_token = stored.id_token;
            refresh_token = Some(stored.refresh_token);
        }

        let (outcome, rejected_refresh) = fetch_usage_with_refresh_capturing_rejection(
            alias,
            &at,
            id_token.as_deref(),
            refresh_token.as_deref(),
            account_id.as_deref(),
            is_fedramp,
            true,
        )
        .await;

        if let Some(terminal) = &rejected_refresh
            && let Some(presented) = refresh_token.as_deref()
            && profile_still_holds_refresh_token(profile_path, presented)
        {
            let error = UsageError {
                summary: terminal.summary(),
                detail: terminal.to_string(),
            };
            remember_terminal_verdict(alias, &terminal.code, Some(presented), &error).await;
        }

        // The auth server rotates `refresh_token` on every use and rejects the
        // previous one as reused. Persist and adopt the new credentials before
        // looking at the result, or the next attempt would replay a dead token
        // and turn a transient failure into a permanent lockout.
        //
        // A write failure aborts this account outright: the rotated token lives
        // only in memory while the old one is already dead, and another round
        // would just spend a second single-use token we equally cannot keep.
        // Other aliases refresh in their own calls and are unaffected.
        if let Some(new_tokens) = &outcome.refreshed {
            let presented = refresh_token.as_deref().ok_or_else(|| {
                UsageError::token_persist_failed(
                    alias,
                    &anyhow::anyhow!("refresh response without presented refresh_token"),
                )
            })?;
            persist_refreshed_tokens(alias, presented, new_tokens)?;
            at = new_tokens.access_token.clone();
            id_token = Some(new_tokens.id_token.clone());
            refresh_token = Some(new_tokens.refresh_token.clone());
        }

        match outcome.result {
            Ok(mut usage) => {
                if write_usage_cache {
                    let cached = crate::cache::get_async(alias).await;
                    merge_cached_reset_credits(&mut usage, cached.as_ref(), chrono::Utc::now());
                    crate::cache::put_async(alias, &usage).await;
                }
                return Ok(usage);
            }
            Err(e) => {
                let msg = format!("{e:#}");
                if attempt + 1 < max_attempts {
                    debug!(
                        "[{alias}] attempt {}/{max_attempts} failed: {msg}",
                        attempt + 1
                    );
                }
                if let Some(terminal) = e.downcast_ref::<TerminalAuthError>() {
                    let error = UsageError {
                        summary: terminal.summary(),
                        detail: msg,
                    };
                    let code = terminal.code.clone();
                    if recovery_round_used {
                        remember_terminal_verdict(alias, &code, refresh_token.as_deref(), &error)
                            .await;
                        return Err(error);
                    }
                    recovery_round_used = true;
                    // Add the round rather than spend one of the existing ones,
                    // so a rejection arriving on the final attempt is still
                    // checked against the profile before the account is failed.
                    max_attempts += 1;
                    pending_terminal = Some((error, code));
                    attempt += 1;
                    continue;
                }
                if e.downcast_ref::<UsageRateLimited>().is_some() {
                    return Err(UsageError {
                        summary: "HTTP 429 rate limited".into(),
                        detail: msg,
                    });
                }
                last_summary = extract_error_summary(&msg);
                last_err = msg;
            }
        }
        attempt += 1;
    }
    Err(UsageError {
        summary: last_summary,
        detail: last_err,
    })
}

/// Fetch usage; on 401/403 automatically refresh the token and retry once.
///
/// Returns tokens and result separately: a rotated `refresh_token` is the only
/// credential the auth server will still accept, so it is reported even when
/// the usage call afterwards failed.
pub async fn fetch_usage_with_refresh(
    alias: &str,
    access_token: &str,
    id_token: Option<&str>,
    refresh_token: Option<&str>,
    account_id: Option<&str>,
    is_fedramp: bool,
) -> UsageFetchOutcome {
    fetch_usage_with_refresh_capturing_rejection(
        alias,
        access_token,
        id_token,
        refresh_token,
        account_id,
        is_fedramp,
        false,
    )
    .await
    .0
}

async fn fetch_usage_with_refresh_capturing_rejection(
    alias: &str,
    access_token: &str,
    id_token: Option<&str>,
    refresh_token: Option<&str>,
    account_id: Option<&str>,
    is_fedramp: bool,
    persist_rotated_tokens: bool,
) -> (UsageFetchOutcome, Option<TerminalAuthError>) {
    let mut refreshed = None;
    let mut rejected_refresh = None;
    let result = fetch_usage_capturing_refresh(
        alias,
        access_token,
        id_token,
        refresh_token,
        account_id,
        is_fedramp,
        &mut refreshed,
        &mut rejected_refresh,
        persist_rotated_tokens,
    )
    .await;
    if result.is_ok() {
        clear_usage_cooldown(account_id.unwrap_or(alias));
    }
    (UsageFetchOutcome { refreshed, result }, rejected_refresh)
}

/// Inner body of [`fetch_usage_with_refresh`]. Every successful refresh is
/// written into `refreshed` *before* any further fallible step, so `?`/`bail!`
/// can never discard a rotated token.
#[allow(clippy::too_many_arguments)]
async fn fetch_usage_capturing_refresh(
    alias: &str,
    access_token: &str,
    id_token: Option<&str>,
    refresh_token: Option<&str>,
    account_id: Option<&str>,
    is_fedramp: bool,
    refreshed: &mut Option<RefreshedTokens>,
    terminal_refresh: &mut Option<TerminalAuthError>,
    persist_rotated_tokens: bool,
) -> Result<UsageInfo> {
    let client = auth::build_http_client()?;
    let usage_url = usage_url();
    let mut rejected_refresh: Option<anyhow::Error> = None;

    // Refresh when either JWT is near expiry so account identity metadata does
    // not remain stale while the access token is still usable.
    if let Some(rt) = refresh_token
        && token_needs_refresh(access_token, id_token, OPPORTUNISTIC_REFRESH_MARGIN)
    {
        info!("[{alias}] token expiring soon, proactively refreshing");

        match do_refresh_token(alias, &client, id_token, Some(access_token), rt).await {
            Ok(new_tokens) => {
                let bearer = new_tokens.access_token.clone();
                *refreshed = Some(new_tokens);
                if persist_rotated_tokens {
                    persist_refreshed_tokens(alias, rt, refreshed.as_ref().unwrap())
                        .map_err(|error| anyhow::anyhow!(error.detail))?;
                }

                let resp = apply_account_routing_headers(
                    client
                        .get(&usage_url)
                        .header("Authorization", format!("Bearer {bearer}")),
                    account_id,
                    is_fedramp,
                );
                let resp = http_retry::send(resp, ReplaySafety::DeferredGet)
                    .await
                    .context("Usage API request failed")?;

                let status = resp.status;
                debug!("[{alias}] Usage API (after proactive refresh): HTTP {status}");
                if status.is_success() {
                    let body: Value = serde_json::from_slice(&resp.body).map_err(|e| {
                        anyhow::anyhow!("failed to parse usage response (HTTP {status}): {e}")
                    })?;
                    debug!(alias, status = %status, bytes = resp.body.len(), "Usage API response parsed after proactive refresh");
                    return parse_usage_checked(&body);
                }
                if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    return Err(rate_limited(alias, account_id, &resp));
                }
                anyhow::bail!("Usage API failed (HTTP {status}) after proactive token refresh");
            }
            Err(e) => {
                if let Some(terminal) = e.downcast_ref::<TerminalAuthError>() {
                    warn!(
                        alias,
                        code = terminal.code,
                        "proactive token refresh rejected permanently"
                    );
                    *terminal_refresh = Some(terminal.clone());
                    rejected_refresh = Some(e);
                } else {
                    warn!("[{alias}] proactive token refresh failed, trying with existing token");
                }
            }
        }
    }

    let resp = apply_account_routing_headers(
        client
            .get(&usage_url)
            .header("Authorization", format!("Bearer {access_token}")),
        account_id,
        is_fedramp,
    );
    let resp = http_retry::send(resp, ReplaySafety::DeferredGet)
        .await
        .context("Usage API request failed")?;

    let status = resp.status;
    debug!("[{alias}] Usage API: HTTP {status}");
    if status.is_success() {
        let body: Value = serde_json::from_slice(&resp.body)
            .map_err(|e| anyhow::anyhow!("failed to parse usage response (HTTP {status}): {e}"))?;
        debug!(alias, status = %status, bytes = resp.body.len(), "Usage API response parsed");
        return parse_usage_checked(&body);
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(rate_limited(alias, account_id, &resp));
    }

    // The auth server already rejected this refresh token moments ago; asking
    // again can only re-trigger reuse detection and add a round trip.
    if let Some(e) = rejected_refresh {
        return Err(e.context(format!("Usage API failed (HTTP {status})")));
    }

    // If 401/403 and we have a refresh_token, try to refresh
    if let Some(rt) = refresh_token
        && (status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN)
    {
        info!("[{alias}] got HTTP {status}, attempting token refresh");

        match do_refresh_token(alias, &client, id_token, Some(access_token), rt).await {
            Ok(new_tokens) => {
                let bearer = new_tokens.access_token.clone();
                *refreshed = Some(new_tokens);
                if persist_rotated_tokens {
                    persist_refreshed_tokens(alias, rt, refreshed.as_ref().unwrap())
                        .map_err(|error| anyhow::anyhow!(error.detail))?;
                }

                let resp2 = apply_account_routing_headers(
                    client
                        .get(&usage_url)
                        .header("Authorization", format!("Bearer {bearer}")),
                    account_id,
                    is_fedramp,
                );
                let resp2 = http_retry::send(resp2, ReplaySafety::DeferredGet)
                    .await
                    .context("Usage API retry request failed")?;

                let status2 = resp2.status;
                debug!("[{alias}] Usage API (after token refresh): HTTP {status2}");
                if status2.is_success() {
                    let body: Value = serde_json::from_slice(&resp2.body).map_err(|e| {
                        anyhow::anyhow!(
                            "failed to parse usage response after refresh (HTTP {status2}): {e}"
                        )
                    })?;
                    return parse_usage_checked(&body);
                }
                if status2 == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    return Err(rate_limited(alias, account_id, &resp2));
                }
                anyhow::bail!("Usage API still failed (HTTP {status2}) after token refresh");
            }
            Err(e) => {
                info!("[{alias}] token refresh failed");
                // `.context` (not `bail!`) so the typed terminal-auth error
                // stays downcastable by the retry loop.
                return Err(e.context(format!(
                    "Usage API failed (HTTP {status}), token refresh also failed"
                )));
            }
        }
    }

    anyhow::bail!("Usage API failed (HTTP {status}), no refresh_token available");
}

/// Validate an auth.json being imported, refreshing its credentials if needed.
///
/// Returns the rotation and the validation result as separate fields: the
/// caller's `val` is a local copy, so a rotated `refresh_token` reported only
/// through `Ok(..)` would be dropped by the caller's `?` on the very failures
/// that make it matter. See [`ImportValidation`].
pub async fn validate_import_auth(val: &mut serde_json::Value) -> ImportValidation {
    let mut refreshed = None;
    let mut validated_account_id = None;
    let result = validate_import_auth_capturing_refresh(val, &mut refreshed)
        .await
        .map(|(usage, account_id)| {
            validated_account_id = Some(account_id);
            usage
        });
    ImportValidation {
        refreshed,
        validated_account_id,
        result,
    }
}

/// Record a rotation and write it into the auth value being validated.
///
/// `refreshed` is assigned *before* the fallible write so that a failure to
/// update the value still leaves the caller holding the live credentials.
fn adopt_refreshed_tokens(
    val: &mut serde_json::Value,
    tokens: RefreshedTokens,
    refreshed: &mut Option<RefreshedTokens>,
) -> Result<()> {
    let tokens = refreshed.insert(tokens);
    auth::apply_tokens(
        val,
        &tokens.id_token,
        &tokens.access_token,
        &tokens.refresh_token,
    )
}

/// Inner body of [`validate_import_auth`]. Every rotation reaches `refreshed`
/// before any further fallible step, so `?`/`bail!` can never discard one.
async fn validate_import_auth_capturing_refresh(
    val: &mut serde_json::Value,
    refreshed: &mut Option<RefreshedTokens>,
) -> Result<(UsageInfo, String)> {
    let (access_token, refresh_token) = auth::extract_tokens(val);
    let id_token = auth::extract_id_token(val);
    let account_info = crate::jwt::parse_account_info(val);
    let account_id = account_info.account_id;
    let is_fedramp = account_info.is_fedramp;

    let alias = "import";
    match (access_token, refresh_token) {
        (Some(at), rt) => {
            let validated_account_id = account_id
                .filter(|id| !id.is_empty())
                .ok_or_else(|| anyhow::anyhow!("imported auth must contain an account_id"))?;
            let outcome = fetch_usage_with_refresh(
                alias,
                &at,
                id_token.as_deref(),
                rt.as_deref(),
                Some(&validated_account_id),
                is_fedramp,
            )
            .await;
            if let Some(tokens) = outcome.refreshed {
                adopt_refreshed_tokens(val, tokens, refreshed)?;
            }
            let usage = outcome.result?;
            if let Err(err) = crate::workspace::refresh_for_auth(val).await {
                debug!("workspace metadata unavailable while importing: {err}");
            }
            Ok((usage, validated_account_id))
        }
        (None, Some(rt)) => {
            let client = auth::build_http_client()?;
            let first = do_refresh_token(alias, &client, id_token.as_deref(), None, &rt).await?;
            let (access_token, id_token, refresh_token) = (
                first.access_token.clone(),
                first.id_token.clone(),
                first.refresh_token.clone(),
            );
            adopt_refreshed_tokens(val, first, refreshed)?;

            let validated_account_id = crate::jwt::parse_account_info(val)
                .account_id
                .filter(|id| !id.is_empty())
                .ok_or_else(|| anyhow::anyhow!("refreshed auth must contain an account_id"))?;
            let outcome = fetch_usage_with_refresh(
                alias,
                &access_token,
                Some(&id_token),
                Some(&refresh_token),
                Some(&validated_account_id),
                is_fedramp,
            )
            .await;
            if let Some(tokens) = outcome.refreshed {
                adopt_refreshed_tokens(val, tokens, refreshed)?;
            }
            let usage = outcome.result?;
            if let Err(err) = crate::workspace::refresh_for_auth(val).await {
                debug!("workspace metadata unavailable while importing: {err}");
            }
            Ok((usage, validated_account_id))
        }
        (None, None) => anyhow::bail!("auth.json missing access_token and refresh_token"),
    }
}

/// Build the token refresh request. Codex 0.144.1 sends a JSON body
/// ({client_id, grant_type, refresh_token}) — keep the same shape so the
/// auth server sees requests identical to the real client's.
pub(crate) fn build_refresh_request(
    client: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
) -> reqwest::RequestBuilder {
    client.post(token_url).json(&serde_json::json!({
        "client_id": CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
    }))
}

pub(crate) async fn do_refresh_token(
    alias: &str,
    client: &reqwest::Client,
    current_id_token: Option<&str>,
    current_access_token: Option<&str>,
    refresh_token: &str,
) -> Result<RefreshedTokens> {
    let token_url = auth::token_url();
    debug!("[{alias}] sending token refresh request to {token_url}");

    let resp = build_refresh_request(client, &token_url, refresh_token)
        .send()
        .await
        .map_err(|e| format_reqwest_error("token refresh request failed", &e))?;

    let status = resp.status();
    debug!("[{alias}] token refresh response: HTTP {status}");

    // Read the body once for parsing, but never log its contents: unknown
    // server error bodies can carry credentials outside our known schema.
    let body_text = resp.text().await.map_err(|e| {
        anyhow::anyhow!("failed to read token refresh response body (HTTP {status}): {e}")
    })?;

    let r: RefreshResponse = serde_json::from_str(&body_text).map_err(|e| {
        debug!(
            "[{alias}] token refresh parse failure (HTTP {status}, {} bytes)",
            body_text.len()
        );
        anyhow::anyhow!("Failed to parse token refresh response (HTTP {status}): {e}")
    })?;

    let refreshed = resolve_refreshed_tokens(
        r,
        status,
        current_id_token,
        current_access_token,
        refresh_token,
    )
    .with_context(|| format!("[{alias}] token refresh HTTP {status}"))?;
    info!("[{alias}] token refresh succeeded");
    Ok(refreshed)
}

/// Max number of tokens to refresh opportunistically per CLI invocation.
const OPPORTUNISTIC_REFRESH_LIMIT: usize = 3;
/// Refresh tokens expiring within this many seconds.
const OPPORTUNISTIC_REFRESH_MARGIN: i64 = 1800; // 30 minutes
/// How many rotations may be in flight at once. Each in-flight request holds a
/// credential that only exists in its own response, so this also bounds how
/// much can be lost if the process dies mid-batch.
const OPPORTUNISTIC_REFRESH_CONCURRENCY: usize = 2;
/// Wall-clock budget for *starting* opportunistic refreshes. It never cancels
/// one — see [`refresh_expiring_tokens_within`].
const OPPORTUNISTIC_START_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

fn profile_still_holds_refresh_token(profile_path: &Path, presented: &str) -> bool {
    auth::read_auth(profile_path)
        .ok()
        .and_then(|value| auth::extract_tokens(&value).1)
        .as_deref()
        == Some(presented)
}

/// Opportunistically refresh tokens that are about to expire.
///
/// Refresh *failures* are logged, not propagated. A memorable terminal
/// rejection is cached against the presented credential so the next background
/// pass does not replay it. Failures to **save** a rotated token are returned
/// instead: the old credential is already dead server-side, so a lost write
/// silently bricks that profile and the caller has to tell someone.
pub async fn refresh_expiring_tokens() -> Vec<TokenPersistFailure> {
    refresh_expiring_tokens_within(OPPORTUNISTIC_START_BUDGET).await
}

/// As [`refresh_expiring_tokens`], with an explicit start budget.
///
/// `budget` bounds how long this keeps **opening** new rotations; it is never a
/// deadline for the ones already open. `refresh_token` is single-use: as soon as
/// a request reaches the auth server the presented token is dead and its
/// replacement exists only in that one response. Abandoning the request — which
/// is what a `timeout` around the join loop does, since `JoinSet::drop` aborts
/// every unfinished task — would therefore leave the profile holding a
/// credential nothing will ever accept again. So every started refresh is
/// awaited to completion, and the budget only decides whether the *next*
/// candidate is contacted at all. A candidate that is never contacted loses
/// nothing: it keeps its working token for the next invocation.
///
/// Residual window we cannot close: the HTTP client in `auth::build_http_client`
/// carries its own total timeout, and if that fires the server may already have
/// rotated the credential while we never read the answer. Nothing on this side
/// can prevent that — the loss is decided by whether the request reached the
/// server, not by how long we wait. Shortening either timeout only *widens* the
/// window (more rotations cut off mid-flight), so neither is tuned for latency.
///
/// Worst-case wall clock for a synchronous caller (`list`, `best`) is therefore
/// HTTP client construction + `budget` + one HTTP client timeout. Client
/// construction is deliberately outside the start budget; a refresh started
/// just before the budget expired may still hang for the client's full timeout.
pub async fn refresh_expiring_tokens_within(
    budget: std::time::Duration,
) -> Vec<TokenPersistFailure> {
    let profiles = match crate::profile::list_profiles() {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let now = auth::now_unix_secs();

    // Collect current tokens for profiles expiring soon.
    let mut candidates: Vec<(
        String,
        std::path::PathBuf,
        Option<String>,
        String,
        String,
        i64,
    )> = Vec::new();
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
        let id_token = auth::extract_id_token(&val);
        let Some(at) = access_token else { continue };
        let Some(rt) = refresh_token else { continue };
        // Expiry alone says nothing about whether the credential can still be
        // rotated. Without this, every dead profile is refreshed again here —
        // after `list` has already printed its final screen, so the user waits
        // on a request whose answer is known and not even displayed.
        if crate::cache::get_auth_failure(alias, &rt).is_some() {
            debug!("[{alias}] skipping opportunistic refresh: credential already rejected");
            continue;
        }
        let expiry = [
            crate::jwt::token_expires_at(&at),
            id_token.as_deref().and_then(crate::jwt::token_expires_at),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(exp) = expiry else {
            continue;
        };
        let remaining = exp - now;
        if remaining < OPPORTUNISTIC_REFRESH_MARGIN {
            candidates.push((alias.clone(), path, id_token, at, rt, exp));
        }
    }

    if candidates.is_empty() {
        return Vec::new();
    }

    // Sort by expiration: soonest first
    candidates.sort_by_key(|c| c.5);
    candidates.truncate(OPPORTUNISTIC_REFRESH_LIMIT);

    let count = candidates.len();
    debug!(
        "opportunistic refresh: {count} token(s) expiring within {}s",
        OPPORTUNISTIC_REFRESH_MARGIN
    );

    // Build before starting the budget: client construction can synchronously
    // initialize TLS state, but the budget is only for opening rotations.
    let client = match auth::build_http_client() {
        Ok(client) => client,
        Err(error) => {
            warn!(
                stage = "client_build_failed",
                "opportunistic token refresh unavailable: {error:#}"
            );
            return Vec::new();
        }
    };

    // Start refreshes while the budget lasts, then wait for every started one:
    // an in-flight rotation is not cancellable without losing the credential.
    let started_at = std::time::Instant::now();
    let mut queued = candidates.into_iter();
    let mut tasks = tokio::task::JoinSet::new();
    let mut failures = Vec::new();

    loop {
        while tasks.len() < OPPORTUNISTIC_REFRESH_CONCURRENCY && started_at.elapsed() < budget {
            let Some((alias, path, id_token, access_token, rt, exp)) = queued.next() else {
                break;
            };
            let client = client.clone();
            tasks.spawn(async move {
                let remaining = exp - auth::now_unix_secs();
                debug!("[{alias}] token expires in {remaining}s, refreshing");

                match do_refresh_token(
                    &alias,
                    &client,
                    id_token.as_deref(),
                    Some(&access_token),
                    &rt,
                )
                .await
                {
                    Ok(new_tokens) => match persist_refreshed_tokens(&alias, &rt, &new_tokens) {
                        Ok(()) => {
                            info!("[{alias}] opportunistic token refresh succeeded");
                            None
                        }
                        // Report rather than abort: the remaining profiles still
                        // deserve their refresh, and this one is only recoverable
                        // once a human hears about it.
                        Err(error) => Some(TokenPersistFailure { alias, error }),
                    },
                    Err(e) => {
                        let detail = format!("{e:#}");
                        if let Some(terminal) = e.downcast_ref::<TerminalAuthError>() {
                            let error = UsageError {
                                summary: terminal.summary(),
                                detail: detail.clone(),
                            };
                            if profile_still_holds_refresh_token(&path, &rt) {
                                remember_terminal_verdict(
                                    &alias,
                                    &terminal.code,
                                    Some(&rt),
                                    &error,
                                )
                                .await;
                            } else {
                                debug!(
                                    "[{alias}] not caching terminal verdict for a superseded credential"
                                );
                            }
                        }
                        debug!("[{alias}] opportunistic token refresh failed: {detail}");
                        None
                    }
                }
            });
        }

        // No timeout here on purpose: this awaits requests the auth server has
        // already been told about.
        let Some(joined) = tasks.join_next().await else {
            break;
        };
        if let Ok(Some(failure)) = joined {
            failures.push(failure);
        }
    }

    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::json;

    fn jwt_with_exp(exp: i64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(serde_json::json!({"exp": exp}).to_string());
        format!("header.{payload}.signature")
    }

    #[test]
    fn terminal_verdict_guard_rejects_a_superseded_refresh_token() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("auth.json");
        crate::auth::write_auth(
            &path,
            &json!({
                "tokens": {
                    "id_token": "id",
                    "access_token": "access",
                    "refresh_token": "refresh_new"
                }
            }),
        )
        .unwrap();

        assert!(!profile_still_holds_refresh_token(&path, "refresh_old"));
        assert!(profile_still_holds_refresh_token(&path, "refresh_new"));
    }

    #[test]
    fn expired_id_token_triggers_refresh_before_access_token_expires() {
        let now = crate::auth::now_unix_secs();
        let access = jwt_with_exp(now + 86_400);
        let id = jwt_with_exp(now - 60);

        assert!(token_needs_refresh(&access, Some(&id), 60));
    }

    #[test]
    fn test_refresh_request_uses_json_body_like_codex() {
        let request = build_refresh_request(
            &reqwest::Client::new(),
            "https://auth.openai.com/oauth/token",
            "refresh-token-value",
        )
        .build()
        .unwrap();

        assert_eq!(
            request
                .headers()
                .get("Content-Type")
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let body: serde_json::Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(
            body,
            json!({
                "client_id": crate::auth::CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": "refresh-token-value",
            })
        );
    }

    #[test]
    fn test_account_routing_headers_include_workspace_and_fedramp() {
        let request = apply_account_routing_headers(
            reqwest::Client::new().get("https://example.invalid/usage"),
            Some("workspace-123"),
            true,
        )
        .build()
        .unwrap();

        assert_eq!(
            request
                .headers()
                .get("ChatGPT-Account-ID")
                .and_then(|value| value.to_str().ok()),
            Some("workspace-123")
        );
        assert_eq!(
            request
                .headers()
                .get("X-OpenAI-Fedramp")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
    }

    #[test]
    fn test_refresh_without_id_token_preserves_existing_id_token() {
        let refreshed = resolve_refreshed_tokens(
            RefreshResponse {
                id_token: None,
                access_token: Some("new-access".to_string()),
                refresh_token: None,
                error: None,
                error_description: None,
            },
            reqwest::StatusCode::OK,
            Some("existing-id"),
            Some("existing-access"),
            "existing-refresh",
        )
        .unwrap();

        assert_eq!(refreshed.id_token, "existing-id");
        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token, "existing-refresh");
    }
}
