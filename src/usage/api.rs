use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::auth::{self, CLIENT_ID};
use crate::http_retry::{self, ReplaySafety};

use super::parse::parse_usage_checked;
use super::reset_credits::merge_cached_reset_credits;
use super::{
    ImportValidation, MAX_RETRIES, ProfileTokens, RETRY_DELAY, Refresh, RefreshedTokens,
    TerminalAuthError, TokenPersistFailure, UsageError, UsageFetchOutcome, UsageInfo,
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

struct UsageFetchTiming<'a> {
    alias: &'a str,
    started: Instant,
    attempts: u32,
    cache: &'static str,
    outcome: &'static str,
}

impl<'a> UsageFetchTiming<'a> {
    fn new(alias: &'a str) -> Self {
        Self {
            alias,
            started: Instant::now(),
            attempts: 0,
            cache: "checked",
            outcome: "error",
        }
    }
}

impl Drop for UsageFetchTiming<'_> {
    fn drop(&mut self) {
        info!(
            profile_alias = diagnostic_alias(self.alias),
            phase = "usage_total",
            elapsed_ms = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            attempts = self.attempts,
            cache = self.cache,
            outcome = self.outcome,
            "usage fetch finished"
        );
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn log_local_phase(alias: &str, phase: &'static str, started: Instant, outcome: &'static str) {
    info!(
        profile_alias = diagnostic_alias(alias),
        phase,
        elapsed_ms = elapsed_ms(started),
        outcome,
        "usage local phase finished"
    );
}

fn diagnostic_alias(alias: &str) -> &str {
    if alias.contains('@') {
        "<redacted-email-alias>"
    } else {
        alias
    }
}

async fn send_usage_request(
    alias: &str,
    phase: &'static str,
    request: reqwest::RequestBuilder,
) -> Result<http_retry::BufferedResponse> {
    let started = Instant::now();
    info!(
        profile_alias = diagnostic_alias(alias),
        phase = "usage_http_started",
        request_phase = phase,
        outcome = "started",
        "usage HTTP phase started"
    );
    match http_retry::send(request, ReplaySafety::DeferredGet).await {
        Ok(response) => {
            info!(
                profile_alias = diagnostic_alias(alias),
                phase,
                elapsed_ms = elapsed_ms(started),
                status = %response.status,
                response_bytes = response.body.len(),
                outcome = "response",
                "usage HTTP phase finished"
            );
            Ok(response)
        }
        Err(error) => {
            // Do not log the error string: some transport errors include URL data.
            info!(
                profile_alias = diagnostic_alias(alias),
                phase,
                elapsed_ms = elapsed_ms(started),
                outcome = "transport_error",
                "usage HTTP phase finished"
            );
            Err(error)
        }
    }
}

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
/// `{"error": {"code": ..., "message": ..., "type": ...}}`. Accept both,
/// but never surface server-provided descriptions: a gateway can echo request
/// credentials into them.
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

fn usage_url() -> String {
    std::env::var("CS_USAGE_URL").unwrap_or_else(|_| USAGE_URL.to_string())
}

fn token_needs_refresh(access_token: &str, margin_secs: i64) -> bool {
    crate::jwt::is_token_expiring(access_token, margin_secs).unwrap_or(false)
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
    fetch_usage_retried_inner(alias, profile_path, current_alias, Refresh::Cached).await
}

/// Bypass the usage TTL for current numbers, but leave a recorded auth verdict
/// standing. Used by background refreshes and one-time warmup operations.
pub async fn fetch_usage_retried_unattended(
    alias: &str,
    profile_path: &Path,
    current_alias: &str,
) -> std::result::Result<UsageInfo, UsageError> {
    fetch_usage_retried_inner(alias, profile_path, current_alias, Refresh::Unattended).await
}

/// Bypass every cache, including a recorded auth verdict. Only for a person
/// explicitly asking again — see [`Refresh::Forced`].
pub async fn fetch_usage_retried_force(
    alias: &str,
    profile_path: &Path,
    current_alias: &str,
) -> std::result::Result<UsageInfo, UsageError> {
    fetch_usage_retried_inner(alias, profile_path, current_alias, Refresh::Forced).await
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
    operation: &'static str,
) -> std::result::Result<(), UsageError> {
    let started = Instant::now();
    info!(
        profile_alias = diagnostic_alias(alias),
        phase = "token_persist_started",
        operation,
        outcome = "started",
        "rotated credential persistence started"
    );
    let persisted = crate::profile::update_profile_tokens_if_refresh_matches(
        alias,
        presented_refresh_token,
        &new_tokens.id_token,
        &new_tokens.access_token,
        &new_tokens.refresh_token,
    )
    .map_err(|err| UsageError::token_persist_failed(alias, &err));
    let persisted = match persisted {
        Ok(persisted) => persisted,
        Err(error) => {
            log_local_phase(alias, operation, started, "error");
            return Err(error);
        }
    };
    if !persisted {
        // The outer retry loop also owns persistence for the captured rotation,
        // while the inner request may already have persisted before replaying
        // the new access token. Treat that exact byte-for-byte credential set
        // as an idempotent success; any different profile state belongs to a
        // concurrent winner and must abort this response.
        let already_persisted = crate::profile::profile_auth_path(alias)
            .ok()
            .and_then(|path| auth::read_auth(&path).ok())
            .is_some_and(|stored| {
                auth::extract_id_token(&stored).as_deref() == Some(new_tokens.id_token.as_str())
                    && auth::extract_tokens(&stored).0.as_deref()
                        == Some(new_tokens.access_token.as_str())
                    && auth::extract_tokens(&stored).1.as_deref()
                        == Some(new_tokens.refresh_token.as_str())
            });
        if already_persisted {
            log_local_phase(alias, operation, started, "already_persisted");
            return Ok(());
        }
        log_local_phase(alias, operation, started, "superseded");
        return Err(UsageError {
            summary: "refresh superseded by a concurrent profile update".into(),
            detail: format!(
                "[{alias}] token refresh completed, but the profile no longer contains that exact rotated credential set; the response will not be used for another API request"
            ),
        });
    }
    log_local_phase(alias, operation, started, "saved");
    Ok(())
}

/// Async-safe wrapper: persistence polls a file lock for up to 15 s and does
/// auth I/O and ACL calls, none of which may stall a runtime worker. The
/// blocking task runs to completion even if the caller is dropped, which is
/// what we want for a credential the server has already rotated.
async fn persist_refreshed_tokens_blocking(
    alias: &str,
    presented_refresh_token: &str,
    new_tokens: &RefreshedTokens,
    operation: &'static str,
) -> std::result::Result<(), UsageError> {
    let owned_alias = alias.to_owned();
    let presented = presented_refresh_token.to_owned();
    let tokens = new_tokens.clone();
    match tokio::task::spawn_blocking(move || {
        persist_refreshed_tokens(&owned_alias, &presented, &tokens, operation)
    })
    .await
    {
        Ok(result) => result,
        Err(join_error) => Err(UsageError::token_persist_failed(
            alias,
            &anyhow::anyhow!("token persistence task failed: {join_error}"),
        )),
    }
}

fn resolve_refreshed_tokens(
    response: RefreshResponse,
    status: reqwest::StatusCode,
    current_id_token: Option<&str>,
    current_access_token: Option<&str>,
    current_refresh_token: &str,
) -> Result<RefreshedTokens> {
    if let Some((code, _untrusted_message)) = response.error_parts() {
        if is_terminal_auth_failure(&code, status) {
            return Err(TerminalAuthError {
                code,
                message: None,
            }
            .into());
        }
        anyhow::bail!("token refresh failed: {code}");
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
/// Separate CLI operations such as `list` and `best` can refresh the same
/// profile from different processes, so both can read the same `refresh_token`
/// and present it. The server rotates it for exactly one of them and answers
/// the other `refresh_token_reused` — a verdict about that *token*, not about
/// the account, whose live credentials the winner has meanwhile written to disk.
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

fn read_profile_tokens(profile_path: &Path) -> Result<(Value, ProfileTokens)> {
    let value = auth::read_auth(profile_path)
        .with_context(|| format!("reading auth file {}", profile_path.display()))?;
    let tokens = profile_tokens_from_auth(&value)?;
    Ok((value, tokens))
}

fn profile_tokens_from_auth(value: &Value) -> Result<ProfileTokens> {
    let (access_token, refresh_token) = auth::extract_tokens(value);
    let access_token = access_token
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("auth file missing access_token"))?;
    let info = crate::jwt::parse_account_info(value);
    Ok(ProfileTokens {
        id_token: auth::extract_id_token(value).filter(|token| !token.trim().is_empty()),
        access_token,
        refresh_token: refresh_token.filter(|token| !token.trim().is_empty()),
        account_id: info.account_id,
        email: info.email.map(|email| email.to_lowercase()),
        is_fedramp: info.is_fedramp,
    })
}

fn profile_tokens_changed(left: &ProfileTokens, right: &ProfileTokens) -> bool {
    left != right
}

fn same_profile_identity(left: &ProfileTokens, right: &ProfileTokens) -> bool {
    crate::profile::identities_compatible(
        left.account_id.as_deref(),
        left.email.as_deref(),
        right.account_id.as_deref(),
        right.email.as_deref(),
    )
}

fn ensure_profile_identity(
    alias: &str,
    expected: &ProfileTokens,
    current: &ProfileTokens,
) -> Result<()> {
    if !same_profile_identity(expected, current) {
        return Err(crate::usage::RefreshSafetyError::new(format!(
            "authenticated account does not match profile '{alias}'"
        ))
        .into());
    }
    Ok(())
}

/// Read credentials written by another process without starting another token
/// refresh. A different identity is an error, even when the credentials did
/// change, because the caller's account routing headers belong to the old one.
pub(crate) fn reload_profile_tokens_if_changed(
    alias: &str,
    profile_path: &Path,
    expected: &ProfileTokens,
) -> Result<Option<ProfileTokens>> {
    let (_, current) = read_profile_tokens(profile_path)?;
    ensure_profile_identity(alias, expected, &current)?;
    Ok(profile_tokens_changed(&current, expected).then_some(current))
}

fn validate_refreshed_identity(
    alias: &str,
    current_auth: &Value,
    expected: &ProfileTokens,
    refreshed: &RefreshedTokens,
) -> Result<Value> {
    let mut candidate = current_auth.clone();
    auth::apply_tokens(
        &mut candidate,
        &refreshed.id_token,
        &refreshed.access_token,
        &refreshed.refresh_token,
    )
    .map_err(|error| {
        crate::usage::RefreshSafetyError::new(format!(
            "{alias}: token refresh succeeded but the rotated credentials could not be validated: {error:#}"
        ))
    })?;
    let existing = crate::profile::extract_identity(current_auth);
    let incoming = crate::profile::extract_identity(&candidate);
    // Refuse only a genuine account change. The server has already rotated the
    // refresh token, so rejecting a response that merely gained (or lost) an
    // email/account claim would discard the only live credential.
    let compatible = |identity: &crate::profile::AccountIdentity| {
        crate::profile::identities_compatible(
            expected.account_id.as_deref(),
            expected.email.as_deref(),
            identity.account_id.as_deref(),
            identity.email.as_deref(),
        )
    };
    if !compatible(&existing) || !compatible(&incoming) {
        return Err(crate::usage::RefreshSafetyError::new(format!(
            "{alias}: authenticated account changed during token refresh; refusing to save or use rotated credentials"
        ))
        .into());
    }
    Ok(candidate)
}

/// Refresh the credentials used for a rejected API request, or adopt a
/// concurrent winner already written to the profile. The rotated token is
/// persisted with the profile's refresh-token compare-and-swap before it is
/// returned to the caller.
pub(crate) async fn refresh_profile_tokens(
    alias: &str,
    profile_path: &Path,
    expected: &ProfileTokens,
) -> Result<ProfileTokens> {
    let (current_auth, current) = read_profile_tokens(profile_path)?;
    ensure_profile_identity(alias, expected, &current)?;
    if profile_tokens_changed(&current, expected) {
        return Ok(current);
    }
    let refresh_token = current
        .refresh_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("{alias}: no refresh_token in profile"))?;
    if let Some(known) = crate::cache::get_auth_failure_async(alias, refresh_token).await {
        anyhow::bail!("{}", known.detail);
    }

    let client = auth::build_http_client_async().await?;
    let refreshed = match do_refresh_token(
        alias,
        &client,
        current.id_token.as_deref(),
        Some(&current.access_token),
        refresh_token,
    )
    .await
    {
        Ok(refreshed) => refreshed,
        Err(error) => {
            // A second process may have rotated the same single-use token
            // while this request was in flight. Adopt its persisted winner and
            // never replay either credential from this call.
            if let Ok((_, latest)) = read_profile_tokens(profile_path) {
                ensure_profile_identity(alias, expected, &latest)?;
                if profile_tokens_changed(&latest, &current) {
                    return Ok(latest);
                }
            }
            if let Some(terminal) = error.downcast_ref::<TerminalAuthError>() {
                let failure = UsageError {
                    summary: terminal.summary(),
                    detail: format!("{error:#}"),
                };
                remember_terminal_verdict(alias, &terminal.code, Some(refresh_token), &failure)
                    .await;
            }
            return Err(error);
        }
    };

    let updated_auth = validate_refreshed_identity(alias, &current_auth, expected, &refreshed)?;
    let persisted = crate::profile::update_profile_tokens_if_refresh_matches(
        alias,
        refresh_token,
        &refreshed.id_token,
        &refreshed.access_token,
        &refreshed.refresh_token,
    )
    .map_err(|error| {
        crate::usage::RefreshSafetyError::new(
            UsageError::token_persist_failed(alias, &error).detail,
        )
    })?;
    if persisted {
        return profile_tokens_from_auth(&updated_auth).map_err(|error| {
            crate::usage::RefreshSafetyError::new(format!(
                "{alias}: token refresh succeeded and was persisted, but the stored credentials could not be used: {error:#}"
            ))
            .into()
        });
    }

    // A different process won the CAS after our refresh response arrived. Its
    // profile is authoritative; never use or persist this now-stale response.
    let latest = read_profile_tokens(profile_path).map_err(|error| {
        crate::usage::RefreshSafetyError::new(format!(
            "{alias}: token refresh succeeded but the rotated credentials could not be reconciled with the profile: {error:#}"
        ))
    })?;
    ensure_profile_identity(alias, expected, &latest.1)?;
    if profile_tokens_changed(&latest.1, &current) {
        return Ok(latest.1);
    }
    Err(crate::usage::RefreshSafetyError::new(format!(
        "{alias}: token refresh succeeded but the profile credentials changed without a replacement; sign in again once the profile write problem is fixed"
    ))
    .into())
}

async fn fetch_usage_retried_inner(
    alias: &str,
    profile_path: &Path,
    _current_alias: &str,
    refresh: Refresh,
) -> std::result::Result<UsageInfo, UsageError> {
    let mut timing = UsageFetchTiming::new(alias);
    if !refresh.skips_usage_cache() {
        let cache_started = Instant::now();
        if let Some(cached) = crate::cache::get_async(alias).await {
            log_local_phase(alias, "cache_get_initial", cache_started, "hit");
            timing.cache = "hit";
            timing.outcome = "success";
            debug!("{alias}: cache hit");
            return Ok(cached);
        }
        log_local_phase(alias, "cache_get_initial", cache_started, "miss");
        timing.cache = "miss";
        debug!("{alias}: cache miss, fetching from API");
    } else {
        timing.cache = "bypass";
        debug!("{alias}: {refresh:?} refresh, bypassing the usage cache");
    }

    let policy_started = Instant::now();
    let policy_result = auth::ensure_chatgpt_backend_supported(&format!(
        "fetch ChatGPT usage for profile '{alias}'"
    ));
    log_local_phase(
        alias,
        "policy_check",
        policy_started,
        if policy_result.is_ok() {
            "allowed"
        } else {
            "denied"
        },
    );
    policy_result.map_err(|error| UsageError {
        summary: "managed authentication policy".into(),
        detail: format!("[{alias}] {error:#}"),
    })?;

    let profile_read_started = Instant::now();
    let profile_result = auth::read_auth(profile_path);
    log_local_phase(
        alias,
        "profile_read",
        profile_read_started,
        if profile_result.is_ok() {
            "read"
        } else {
            "error"
        },
    );
    let val = profile_result.map_err(|e| {
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
        && let Some(known) = {
            let started = Instant::now();
            let result = crate::cache::get_auth_failure_async(alias, rt).await;
            log_local_phase(
                alias,
                "cache_get_auth_verdict",
                started,
                if result.is_some() { "hit" } else { "miss" },
            );
            result
        }
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
        timing.attempts = attempt + 1;
        if attempt > 0 {
            let retry_started = Instant::now();
            tokio::time::sleep(RETRY_DELAY).await;
            info!(
                profile_alias = diagnostic_alias(alias),
                phase = "usage_retry_wait",
                attempt = attempt + 1,
                elapsed_ms = elapsed_ms(retry_started),
                "usage retry delay"
            );
            debug!("[{alias}] retry attempt {}/{max_attempts}", attempt + 1);
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
            persist_refreshed_tokens_blocking(
                alias,
                presented,
                new_tokens,
                "token_persist_reconcile",
            )
            .await?;
            at = new_tokens.access_token.clone();
            id_token = Some(new_tokens.id_token.clone());
            refresh_token = Some(new_tokens.refresh_token.clone());
        }

        match outcome.result {
            Ok(mut usage) => {
                let cache_get_started = Instant::now();
                let cached = crate::cache::get_async(alias).await;
                log_local_phase(
                    alias,
                    "cache_get_reset_merge",
                    cache_get_started,
                    if cached.is_some() { "hit" } else { "miss" },
                );
                merge_cached_reset_credits(&mut usage, cached.as_ref(), chrono::Utc::now());
                let cache_put_started = Instant::now();
                crate::cache::put_async(alias, &usage).await;
                log_local_phase(alias, "cache_put_usage", cache_put_started, "complete");
                timing.outcome = "success";
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
    let client_started = Instant::now();
    let client = match auth::build_http_client_async().await {
        Ok(client) => {
            info!(
                profile_alias = diagnostic_alias(alias),
                phase = "client_build",
                elapsed_ms = elapsed_ms(client_started),
                outcome = "success",
                "usage HTTP client build finished"
            );
            client
        }
        Err(error) => {
            info!(
                profile_alias = diagnostic_alias(alias),
                phase = "client_build",
                elapsed_ms = elapsed_ms(client_started),
                outcome = "error",
                "usage HTTP client build finished"
            );
            return Err(error);
        }
    };
    let usage_url = usage_url();
    let mut rejected_refresh: Option<anyhow::Error> = None;

    // Match Codex's OAuth refresh gate: a parsed access-token expiry controls
    // proactive rotation. An expired ID token alone is not a reason to spend a
    // single-use refresh token before the usage request.
    if let Some(rt) = refresh_token
        && token_needs_refresh(access_token, OPPORTUNISTIC_REFRESH_MARGIN)
    {
        info!("[{alias}] token expiring soon, proactively refreshing");

        match do_refresh_token(alias, &client, id_token, Some(access_token), rt).await {
            Ok(new_tokens) => {
                let bearer = new_tokens.access_token.clone();
                *refreshed = Some(new_tokens);
                if persist_rotated_tokens {
                    persist_refreshed_tokens_blocking(
                        alias,
                        rt,
                        refreshed.as_ref().unwrap(),
                        "token_persist_before_replay",
                    )
                    .await
                    .map_err(|error| anyhow::anyhow!(error.detail))?;
                }

                let resp = apply_account_routing_headers(
                    client
                        .get(&usage_url)
                        .header("Authorization", format!("Bearer {bearer}")),
                    account_id,
                    is_fedramp,
                );
                let resp = send_usage_request(alias, "usage_get_after_proactive_refresh", resp)
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
    let resp = send_usage_request(alias, "usage_get", resp)
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
                    persist_refreshed_tokens_blocking(
                        alias,
                        rt,
                        refreshed.as_ref().unwrap(),
                        "token_persist_before_replay",
                    )
                    .await
                    .map_err(|error| anyhow::anyhow!(error.detail))?;
                }

                let resp2 = apply_account_routing_headers(
                    client
                        .get(&usage_url)
                        .header("Authorization", format!("Bearer {bearer}")),
                    account_id,
                    is_fedramp,
                );
                let resp2 = send_usage_request(alias, "usage_get_after_401_refresh", resp2)
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
            let client = auth::build_http_client_async().await?;
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
    // Check before presenting a single-use refresh token to the auth server.
    // Once a response rotates it, persistence must be allowed to rescue the
    // replacement instead of rejecting it on a later policy check.
    auth::ensure_chatgpt_backend_supported(&format!(
        "refresh ChatGPT credentials for profile '{alias}'"
    ))?;
    let token_url = auth::token_url();
    debug!("[{alias}] sending token refresh request");

    let refresh_started = Instant::now();
    info!(
        profile_alias = diagnostic_alias(alias),
        phase = "refresh_post_started",
        outcome = "started",
        "credential refresh HTTP phase started"
    );
    let resp = match build_refresh_request(client, &token_url, refresh_token)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            info!(
                profile_alias = diagnostic_alias(alias),
                phase = "refresh_post",
                elapsed_ms = elapsed_ms(refresh_started),
                outcome = "transport_error",
                "credential refresh HTTP phase finished"
            );
            return Err(auth::format_auth_reqwest_error(
                "token refresh request failed",
                error,
            ));
        }
    };

    let status = resp.status();
    debug!("[{alias}] token refresh response: HTTP {status}");

    // Read the body once for parsing, but never log its contents: unknown
    // server error bodies can carry credentials outside our known schema.
    let body_text = match resp.text().await {
        Ok(body) => body,
        Err(error) => {
            info!(
                profile_alias = diagnostic_alias(alias),
                phase = "refresh_post",
                elapsed_ms = elapsed_ms(refresh_started),
                status = %status,
                outcome = "body_error",
                "credential refresh HTTP phase finished"
            );
            return Err(auth::format_auth_reqwest_error(
                &format!("failed to read token refresh response body (HTTP {status})"),
                error,
            ));
        }
    };
    info!(
        profile_alias = diagnostic_alias(alias),
        phase = "refresh_post",
        elapsed_ms = elapsed_ms(refresh_started),
        status = %status,
        response_bytes = body_text.len(),
        outcome = "response",
        "credential refresh HTTP phase finished"
    );

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
/// Refresh access tokens expiring within this many seconds, matching Codex.
const OPPORTUNISTIC_REFRESH_MARGIN: i64 = 5 * 60;
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
        // Match Codex's proactive refresh selection: ID-token expiry does not
        // spend a refresh token while the access token is still valid.
        let expiry = crate::jwt::token_expires_at(&at);
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
    let client = match auth::build_http_client_async().await {
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
                    Ok(new_tokens) => match persist_refreshed_tokens_blocking(&alias, &rt, &new_tokens, "token_persist_opportunistic").await {
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

    #[derive(Clone, Default)]
    struct TimingLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for TimingLog {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TimingLog {
        type Writer = TimingLog;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl TimingLog {
        fn contents(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
        }
    }

    fn timing_log_field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
        line.split_whitespace()
            .find_map(|part| part.strip_prefix(&format!("{name}=")))
            .map(|value| value.trim_matches('"'))
    }

    /// Persisting a rotated credential waits on the cross-process auth lock
    /// (up to 15 s). On the single-thread test runtime, a synchronous wait
    /// would freeze the ticker below; the blocking-pool wrapper must not.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn persisting_rotated_tokens_does_not_block_the_async_worker() {
        let _env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let previous = (
            std::env::var_os("CODEX_SWITCH_HOME"),
            std::env::var_os("CODEX_HOME"),
        );
        unsafe {
            std::env::set_var("CODEX_SWITCH_HOME", home.path());
            std::env::set_var("CODEX_HOME", home.path().join("codex"));
        }
        let lease = crate::profile::lock_launch_session().unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            drop(lease);
        });

        let ticks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ticker_ticks = ticks.clone();
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(25)).await;
                ticker_ticks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let tokens = RefreshedTokens {
            id_token: "id".into(),
            access_token: "access".into(),
            refresh_token: "new".into(),
        };
        let result = persist_refreshed_tokens_blocking("missing", "old", &tokens, "test").await;
        let observed = ticks.load(std::sync::atomic::Ordering::SeqCst);
        ticker.abort();
        releaser.join().unwrap();
        unsafe {
            match previous.0 {
                Some(value) => std::env::set_var("CODEX_SWITCH_HOME", value),
                None => std::env::remove_var("CODEX_SWITCH_HOME"),
            }
            match previous.1 {
                Some(value) => std::env::set_var("CODEX_HOME", value),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }

        // The profile does not exist, so persistence reports a failure, but
        // only after the lock wait the runtime stayed responsive through.
        assert!(result.is_err());
        assert!(
            observed >= 5,
            "runtime was blocked while waiting for the auth lock ({observed} ticks)"
        );
    }

    #[tokio::test]
    async fn usage_http_timing_includes_delayed_mock_response_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(120)).await;
            stream.write_all(b"{}").await.unwrap();
            stream.flush().await.unwrap();
        });

        let logs = TimingLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let response = send_usage_request(
            "private@example.invalid",
            "usage_get",
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .get(format!("http://{address}/usage")),
        )
        .await
        .unwrap();
        server.await.unwrap();

        assert_eq!(response.status, reqwest::StatusCode::OK);
        let output = logs.contents();
        let lines = output.lines().collect::<Vec<_>>();
        let started_line = lines
            .iter()
            .copied()
            .find(|line| {
                timing_log_field(line, "phase") == Some("usage_http_started")
                    && timing_log_field(line, "request_phase") == Some("usage_get")
            })
            .expect("usage GET start event should be captured");
        let usage_line = output
            .lines()
            .find(|line| timing_log_field(line, "phase") == Some("usage_get"))
            .expect("usage GET timing event should be captured");
        let start_index = lines.iter().position(|line| *line == started_line).unwrap();
        let finish_index = lines.iter().position(|line| *line == usage_line).unwrap();
        assert!(start_index < finish_index, "{output}");
        assert_eq!(
            timing_log_field(started_line, "profile_alias"),
            Some("<redacted-email-alias>")
        );
        assert_eq!(
            timing_log_field(usage_line, "phase"),
            Some("usage_get"),
            "{usage_line}"
        );
        assert!(output.contains("<redacted-email-alias>"), "{output}");
        assert!(!output.contains("private@example.invalid"), "{output}");
        assert_eq!(
            timing_log_field(usage_line, "status").and_then(|value| value.split(' ').next()),
            Some("200")
        );
        assert_eq!(timing_log_field(usage_line, "response_bytes"), Some("2"));
        let elapsed = timing_log_field(usage_line, "elapsed_ms")
            .and_then(|value| value.parse::<u64>().ok())
            .expect("timing log should contain integer elapsed_ms");
        assert!(
            elapsed >= 100,
            "delayed response was not included: {output}"
        );
    }

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
    fn only_access_token_expiry_triggers_proactive_refresh() {
        let now = crate::auth::now_unix_secs();
        let access = jwt_with_exp(now + 86_400);

        assert!(!token_needs_refresh(&access, 60));
        let expiring_access = jwt_with_exp(now + 30);
        assert!(token_needs_refresh(&expiring_access, 60));
    }

    struct EnvVarGuard {
        previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvVarGuard {
        fn set(values: &[(&'static str, Option<&std::ffi::OsStr>)]) -> Self {
            let previous = values
                .iter()
                .map(|(name, value)| {
                    let old = std::env::var_os(name);
                    match value {
                        Some(value) => unsafe { std::env::set_var(name, value) },
                        None => unsafe { std::env::remove_var(name) },
                    }
                    (*name, old)
                })
                .collect();
            Self { previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            for (name, value) in self.previous.drain(..) {
                match value {
                    Some(value) => unsafe { std::env::set_var(name, value) },
                    None => unsafe { std::env::remove_var(name) },
                }
            }
        }
    }

    async fn read_mock_request(stream: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let count = stream.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0, "mock client closed before sending headers");
            request.extend_from_slice(&chunk[..count]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(request).unwrap()
    }

    async fn write_mock_response(stream: &mut tokio::net::TcpStream, body: &str) {
        use tokio::io::AsyncWriteExt;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        stream.flush().await.unwrap();
    }

    #[tokio::test]
    // The process-wide environment lock must cover all awaited mock requests.
    #[allow(clippy::await_holding_lock)]
    async fn expired_id_token_with_valid_access_token_uses_get_without_refresh_post() {
        use tokio::net::TcpListener;

        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _env_lock = crate::profile::TEST_ENV_LOCK.lock().unwrap();
        let codex_home = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let token_url = format!("http://{address}/oauth/token");
        let usage_url = format!("http://{address}/usage");
        let _vars = EnvVarGuard::set(&[
            ("CODEX_HOME", Some(codex_home.path().as_os_str())),
            ("CS_TOKEN_URL", Some(std::ffi::OsStr::new(&token_url))),
            ("CS_USAGE_URL", Some(std::ffi::OsStr::new(&usage_url))),
            ("OPENAI_FEDERATION_RULE_ID", None),
            ("OPENAI_IDENTITY_TOKEN_FILE", None),
        ]);
        let now = crate::auth::now_unix_secs();
        let access = jwt_with_exp(now + 86_400);
        let expected_access = access.clone();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_mock_request(&mut stream).await;
                if request.starts_with("POST /oauth/token ") {
                    requests.push("POST /oauth/token".to_string());
                    // Keep any regression hermetic and terminal so this fake
                    // single-use credential is never sent to a live endpoint.
                    write_mock_response(&mut stream, r#"{"error":"invalid_grant"}"#).await;
                    continue;
                }
                assert!(
                    request.starts_with("GET /usage "),
                    "unexpected mock request"
                );
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer ")
                );
                assert!(
                    request.contains(&expected_access),
                    "mock GET used another access token"
                );
                requests.push("GET /usage".to_string());
                write_mock_response(
                    &mut stream,
                    r#"{"credits":{"has_credits":true,"balance":0}}"#,
                )
                .await;
                break;
            }
            requests
        });

        let outcome = fetch_usage_with_refresh(
            "test",
            &access,
            Some(&jwt_with_exp(now - 60)),
            Some("refresh-single-use"),
            None,
            false,
        )
        .await;
        assert!(outcome.result.is_ok());
        assert!(outcome.refreshed.is_none());
        assert_eq!(server.await.unwrap(), ["GET /usage"]);
    }

    #[tokio::test]
    // The process-wide environment lock must cover all awaited mock requests.
    #[allow(clippy::await_holding_lock)]
    async fn expiring_access_token_posts_refresh_then_replays_usage_get() {
        use tokio::net::TcpListener;

        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _env_lock = crate::profile::TEST_ENV_LOCK.lock().unwrap();
        let codex_home = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let token_url = format!("http://{address}/oauth/token");
        let usage_url = format!("http://{address}/usage");
        let _vars = EnvVarGuard::set(&[
            ("CODEX_HOME", Some(codex_home.path().as_os_str())),
            ("CS_TOKEN_URL", Some(std::ffi::OsStr::new(&token_url))),
            ("CS_USAGE_URL", Some(std::ffi::OsStr::new(&usage_url))),
            ("OPENAI_FEDERATION_RULE_ID", None),
            ("OPENAI_IDENTITY_TOKEN_FILE", None),
        ]);
        let server = tokio::spawn(async move {
            let (mut token_stream, _) = listener.accept().await.unwrap();
            let token_request = read_mock_request(&mut token_stream).await;
            assert!(token_request.starts_with("POST /oauth/token "));
            write_mock_response(
                &mut token_stream,
                r#"{"id_token":"id-new","access_token":"access-new","refresh_token":"refresh-new"}"#,
            )
            .await;
            let (mut usage_stream, _) = listener.accept().await.unwrap();
            let usage_request = read_mock_request(&mut usage_stream).await;
            assert!(usage_request.starts_with("GET /usage "));
            assert!(
                usage_request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer access-new")
            );
            write_mock_response(
                &mut usage_stream,
                r#"{"credits":{"has_credits":true,"balance":0}}"#,
            )
            .await;
            (token_request, usage_request)
        });

        let now = crate::auth::now_unix_secs();
        let outcome = fetch_usage_with_refresh(
            "test",
            &jwt_with_exp(now + 30),
            Some(&jwt_with_exp(now - 60)),
            Some("refresh-single-use"),
            None,
            false,
        )
        .await;
        assert!(outcome.result.is_ok());
        assert_eq!(outcome.refreshed.unwrap().refresh_token, "refresh-new");
        let (token_request, usage_request) = server.await.unwrap();
        assert!(token_request.starts_with("POST /oauth/token "));
        assert!(usage_request.starts_with("GET /usage "));
    }

    #[tokio::test]
    // The process-wide environment lock must cover all awaited mock requests.
    #[allow(clippy::await_holding_lock)]
    async fn opportunistic_refresh_ignores_expired_id_with_valid_access_token() {
        use tokio::{net::TcpListener, sync::oneshot};

        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let _env_lock = crate::profile::TEST_ENV_LOCK.lock().unwrap();
        let switch_home = tempfile::tempdir().unwrap();
        let codex_home = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let token_url = format!("http://{address}/oauth/token");
        let _vars = EnvVarGuard::set(&[
            ("CODEX_SWITCH_HOME", Some(switch_home.path().as_os_str())),
            ("CODEX_HOME", Some(codex_home.path().as_os_str())),
            ("CS_TOKEN_URL", Some(std::ffi::OsStr::new(&token_url))),
            ("OPENAI_FEDERATION_RULE_ID", None),
            ("OPENAI_IDENTITY_TOKEN_FILE", None),
        ]);

        let now = crate::auth::now_unix_secs();
        let alias = "expired-id-valid-access";
        let profile_path = crate::auth::profiles_dir()
            .unwrap()
            .join(alias)
            .join("auth.json");
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        let access = jwt_with_exp(now + 86_400);
        let id = jwt_with_exp(now - 60);
        let auth_value = json!({
            "tokens": {
                "id_token": id,
                "access_token": access,
                "refresh_token": "refresh-single-use"
            }
        });
        auth::write_auth(&profile_path, &auth_value).unwrap();

        let (stop_tx, mut stop_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.unwrap();
                        let request = read_mock_request(&mut stream).await;
                        requests.push(request.lines().next().unwrap_or_default().to_string());
                        write_mock_response(&mut stream, r#"{"error":"invalid_grant"}"#).await;
                    }
                }
            }
            requests
        });

        let failures = refresh_expiring_tokens_within(Duration::from_secs(1)).await;
        assert!(failures.is_empty());
        let _ = stop_tx.send(());
        assert!(
            server.await.unwrap().is_empty(),
            "valid access token must not trigger an opportunistic refresh POST"
        );
        let after = auth::read_auth(&profile_path).unwrap();
        assert_eq!(after, auth_value, "the saved profile must remain unchanged");
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

    #[test]
    fn refresh_errors_do_not_surface_server_descriptions_or_credentials() {
        let refresh_secret = "refresh-token-secret";
        let error = resolve_refreshed_tokens(
            RefreshResponse {
                id_token: None,
                access_token: None,
                refresh_token: None,
                error: Some(RefreshError::Detail {
                    code: Some("refresh_token_reused".to_string()),
                    message: Some(format!("credential rejected: {refresh_secret}")),
                    kind: None,
                }),
                error_description: Some(format!("request body echoed {refresh_secret}")),
            },
            reqwest::StatusCode::BAD_REQUEST,
            Some("id-token-secret"),
            Some("access-token-secret"),
            refresh_secret,
        )
        .err()
        .expect("the auth server rejected the refresh token");

        let detail = format!("{error:#}");
        assert!(detail.contains("refresh_token_reused"));
        for secret in [
            refresh_secret,
            "id-token-secret",
            "access-token-secret",
            "credential rejected",
            "request body echoed",
        ] {
            assert!(
                !detail.contains(secret),
                "refresh diagnostics exposed untrusted response data {secret:?}: {detail}"
            );
        }
    }
}
