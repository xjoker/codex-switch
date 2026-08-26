use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fs4::{FileExt, TryLockError};
use serde::{Deserialize, Serialize};

use crate::auth;
use crate::usage::{ResetCredit, UsageError, UsageInfo};

/// How long a confirmed "this account has no workspace name" is trusted.
///
/// Bounded rather than permanent because, unlike a spent credential, this can
/// change without us: an account added to an organisation gains a name and
/// nothing announces it. A day removes the per-invocation request while keeping
/// the new name at most a day away — `--force` shows it immediately.
const WORKSPACE_ABSENCE_TTL: u64 = 24 * 60 * 60;

static CACHE_LOCK: Mutex<()> = Mutex::new(());
const CACHE_LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const CACHE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    ts: u64,
    primary_used: Option<f64>,
    primary_reset: Option<i64>,
    #[serde(default)]
    primary_window_minutes: Option<i64>,
    secondary_used: Option<f64>,
    secondary_reset: Option<i64>,
    #[serde(default)]
    secondary_window_minutes: Option<i64>,
    #[serde(default)]
    credits_balance: Option<f64>,
    #[serde(default)]
    unlimited_credits: Option<bool>,
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    reset_credits_available_count: Option<u64>,
    #[serde(default)]
    reset_credits: Vec<ResetCredit>,
    #[serde(default)]
    reset_credits_error: Option<String>,
    #[serde(default)]
    account_limited: bool,
    #[serde(default)]
    rate_limit_reached_type: Option<String>,
    #[serde(default)]
    individual_limit: Option<Box<crate::usage::SpendControlLimit>>,
    #[serde(default)]
    additional_limits: Vec<crate::usage::AdditionalRateLimit>,
}

/// A refusal the auth server will repeat for as long as the profile keeps the
/// credential it refused.
///
/// Unlike [`CacheEntry`] this carries no TTL. It is not a stale-data trade-off:
/// the server named a specific credential as spent, and that verdict can only
/// be undone by replacing the credential — which `credential` detects on its
/// own. Expiring the record on a timer would buy nothing but a periodic round
/// trip whose answer is already known.
#[derive(Serialize, Deserialize)]
struct AuthFailureEntry {
    ts: u64,
    /// Hex SHA-256 of the rejected `refresh_token`. The token itself is never
    /// stored: this file is not the credential store, and a fingerprint answers
    /// the only question asked of it — is this still the same credential?
    credential: String,
    summary: String,
    detail: String,
}

#[derive(Serialize, Deserialize, Default)]
struct CacheFile {
    entries: HashMap<String, CacheEntry>,
    /// Tracks the last time each profile was selected by `use` (unix seconds).
    #[serde(default)]
    last_used: HashMap<String, i64>,
    /// Workspace display names keyed by the stable ChatGPT account id.
    #[serde(default)]
    workspace_names: HashMap<String, String>,
    /// Accounts the server confirmed have no workspace name, and when it said
    /// so. Absence is an answer — without recording it, every personal plan is
    /// looked up again on every invocation, forever.
    #[serde(default)]
    workspace_names_absent: HashMap<String, u64>,
    /// Profiles whose credential the auth server has permanently refused.
    #[serde(default)]
    auth_failures: HashMap<String, AuthFailureEntry>,
}

fn cache_path() -> Result<PathBuf> {
    Ok(auth::app_home()?.join("cache.json"))
}

fn cache_lock_path() -> Result<PathBuf> {
    Ok(auth::app_home()?.join("cache.lock"))
}

fn open_cache_lock_file(path: &std::path::Path) -> Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating cache directory {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("setting permissions on {}", parent.display()))?;
        }
    }
    std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening cache lock {}", path.display()))
}

fn with_cache_file_lock_at<T>(
    path: &std::path::Path,
    timeout: Duration,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let file = open_cache_lock_file(path)?;
    let deadline = Instant::now() + timeout;
    loop {
        match FileExt::try_lock(&file) {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(CACHE_LOCK_POLL_INTERVAL);
            }
            Err(TryLockError::WouldBlock) => {
                anyhow::bail!(
                    "cache lock {} remained held for {:.3}s; refusing to replace the live lock file",
                    path.display(),
                    timeout.as_secs_f64()
                );
            }
            Err(TryLockError::Error(err)) => {
                return Err(anyhow::Error::from(err))
                    .with_context(|| format!("locking cache file {}", path.display()));
            }
        }
    }
    operation()
}

fn with_cache_lock<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let _process_lock = CACHE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("cache process lock poisoned"))?;
    with_cache_file_lock_at(&cache_lock_path()?, CACHE_LOCK_WAIT_TIMEOUT, operation)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ttl() -> u64 {
    crate::config::get().cache.ttl
}

fn load_cache() -> CacheFile {
    let path = match cache_path() {
        Ok(p) => p,
        Err(_) => return CacheFile::default(),
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cache(cache: &CacheFile) -> Result<()> {
    let path = cache_path()?;
    save_cache_at(&path, cache)
}

fn save_cache_at(path: &std::path::Path, cache: &CacheFile) -> Result<()> {
    let json = serde_json::to_string(cache).context("serializing cache")?;
    auth::atomic_write_private(path, json.as_bytes())
        .with_context(|| format!("writing cache file {}", path.display()))
}

fn to_entry(u: &UsageInfo) -> CacheEntry {
    CacheEntry {
        ts: u
            .fetched_at
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or_else(now_secs),
        primary_used: u.primary.as_ref().and_then(|w| w.used_percent),
        primary_reset: u.primary.as_ref().and_then(|w| w.resets_at),
        primary_window_minutes: u.primary.as_ref().and_then(|w| w.window_minutes),
        secondary_used: u.secondary.as_ref().and_then(|w| w.used_percent),
        secondary_reset: u.secondary.as_ref().and_then(|w| w.resets_at),
        secondary_window_minutes: u.secondary.as_ref().and_then(|w| w.window_minutes),
        credits_balance: u.credits_balance,
        unlimited_credits: u.unlimited_credits,
        plan_type: u.plan_type.clone(),
        reset_credits_available_count: u.reset_credits_available_count,
        reset_credits: u.reset_credits.clone(),
        reset_credits_error: u.reset_credits_error.clone(),
        account_limited: u.account_limited,
        rate_limit_reached_type: u.rate_limit_reached_type.clone(),
        individual_limit: u.individual_limit.clone(),
        additional_limits: u.additional_limits.clone(),
    }
}

fn from_entry(e: &CacheEntry) -> UsageInfo {
    use crate::usage::WindowUsage;
    let primary = if e.primary_used.is_some() || e.primary_reset.is_some() {
        Some(WindowUsage {
            used_percent: e.primary_used,
            resets_at: e.primary_reset,
            window_minutes: e.primary_window_minutes,
        })
    } else {
        None
    };
    let secondary = if e.secondary_used.is_some() || e.secondary_reset.is_some() {
        Some(WindowUsage {
            used_percent: e.secondary_used,
            resets_at: e.secondary_reset,
            window_minutes: e.secondary_window_minutes,
        })
    } else {
        None
    };
    UsageInfo {
        fetched_at: Some(e.ts as i64),
        primary,
        secondary,
        credits_balance: e.credits_balance,
        unlimited_credits: e.unlimited_credits,
        plan_type: e.plan_type.clone(),
        reset_credits_available_count: e.reset_credits_available_count,
        reset_credits: e.reset_credits.clone(),
        reset_credits_error: e.reset_credits_error.clone(),
        account_limited: e.account_limited,
        rate_limit_reached_type: e.rate_limit_reached_type.clone(),
        individual_limit: e.individual_limit.clone(),
        additional_limits: e.additional_limits.clone(),
    }
}

/// Get cached usage for an alias if within TTL.
pub fn get(alias: &str) -> Option<UsageInfo> {
    match with_cache_lock(|| {
        let cache = load_cache();
        let Some(entry) = cache.entries.get(alias) else {
            return Ok(None);
        };
        if now_secs().saturating_sub(entry.ts) > ttl() {
            return Ok(None);
        }
        Ok(Some(from_entry(entry)))
    }) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!("Failed to read cache for {alias}: {err}");
            None
        }
    }
}

/// Store usage result in cache.
pub fn put(alias: &str, usage: &UsageInfo) {
    if let Err(err) = with_cache_lock(|| {
        let mut cache = load_cache();
        cache.entries.insert(alias.to_string(), to_entry(usage));
        save_cache(&cache)
    }) {
        tracing::warn!("Failed to write cache: {err}");
    }
}

/// Update only the Reset Card snapshot without replacing newer main Usage data.
pub fn put_reset_credits(
    alias: &str,
    expected_fetched_at: Option<i64>,
    available_count: Option<u64>,
    credits: &[ResetCredit],
    error: Option<&str>,
) {
    if let Err(err) = with_cache_lock(|| {
        let mut cache = load_cache();
        let Some(entry) = cache.entries.get_mut(alias) else {
            return Ok(());
        };
        if expected_fetched_at != Some(entry.ts as i64) {
            return Ok(());
        }
        entry.reset_credits_available_count = available_count;
        entry.reset_credits = credits.to_vec();
        entry.reset_credits_error = error.map(sanitize_for_storage);
        save_cache(&cache)
    }) {
        tracing::warn!("Failed to update reset-card cache for {alias}: {err}");
    }
}

fn credential_fingerprint(refresh_token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(refresh_token.as_bytes()))
}

/// Upper bound on server-supplied text kept in the cache file.
const STORED_TEXT_MAX: usize = 512;

/// Make server-supplied text safe to keep and to re-render.
///
/// The wording in a verdict comes from the auth server. It used to be shown
/// once and dropped; recording it means it is written to disk and printed
/// again on every later listing, so escape sequences would be replayed into
/// the terminal each time and an oversized `error_description` would sit in
/// the cache file forever. Control characters go, length is bounded, and
/// everything readable — including non-ASCII — is preserved.
fn sanitize_for_storage(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(STORED_TEXT_MAX)
        .collect()
}

fn record_auth_failure(
    cache: &mut CacheFile,
    alias: &str,
    refresh_token: &str,
    summary: &str,
    detail: &str,
) {
    cache.auth_failures.insert(
        alias.to_string(),
        AuthFailureEntry {
            ts: now_secs(),
            credential: credential_fingerprint(refresh_token),
            summary: sanitize_for_storage(summary),
            detail: sanitize_for_storage(detail),
        },
    );
}

/// Forget everything keyed by `alias`. Returns whether anything was removed.
fn drop_alias(cache: &mut CacheFile, alias: &str) -> bool {
    let dropped_usage = cache.entries.remove(alias).is_some();
    let dropped_failure = cache.auth_failures.remove(alias).is_some();
    dropped_usage || dropped_failure
}

fn auth_failure_for<'a>(
    cache: &'a CacheFile,
    alias: &str,
    refresh_token: &str,
) -> Option<&'a AuthFailureEntry> {
    let entry = cache.auth_failures.get(alias)?;
    (entry.credential == credential_fingerprint(refresh_token)).then_some(entry)
}

/// Move every record keyed by `old` over to `new`. Returns whether anything moved.
fn migrate_alias(cache: &mut CacheFile, old: &str, new: &str) -> bool {
    // Each map may hold `old` without the others — a profile can have been used
    // but never fetched, or refused before it was ever selected.
    let mut changed = false;
    if let Some(entry) = cache.entries.remove(old) {
        cache.entries.insert(new.to_string(), entry);
        changed = true;
    }
    if let Some(ts) = cache.last_used.remove(old) {
        cache.last_used.insert(new.to_string(), ts);
        changed = true;
    }
    if let Some(failure) = cache.auth_failures.remove(old) {
        cache.auth_failures.insert(new.to_string(), failure);
        changed = true;
    }
    changed
}

/// The auth server's standing refusal for `alias`, if it still concerns the
/// credential the profile currently holds.
pub fn get_auth_failure(alias: &str, refresh_token: &str) -> Option<UsageError> {
    match with_cache_lock(|| {
        Ok(
            auth_failure_for(&load_cache(), alias, refresh_token).map(|entry| UsageError {
                summary: entry.summary.clone(),
                detail: entry.detail.clone(),
            }),
        )
    }) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!("Failed to read auth failure for {alias}: {err}");
            None
        }
    }
}

/// Remember that the auth server refused `refresh_token` for good.
pub fn put_auth_failure(alias: &str, refresh_token: &str, error: &UsageError) {
    if let Err(err) = with_cache_lock(|| {
        let mut cache = load_cache();
        record_auth_failure(
            &mut cache,
            alias,
            refresh_token,
            &error.summary,
            &error.detail,
        );
        save_cache(&cache)
    }) {
        tracing::warn!("Failed to record auth failure for {alias}: {err}");
    }
}

pub fn get_workspace_name(account_id: &str) -> Option<String> {
    match with_cache_lock(|| Ok(load_cache().workspace_names.get(account_id).cloned())) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!("Failed to read cached workspace name: {err}");
            None
        }
    }
}

pub fn set_workspace_name(account_id: &str, name: Option<&str>) -> Result<()> {
    let account_id = account_id.trim();
    if account_id.is_empty() {
        return Ok(());
    }
    let name = name.map(str::trim).filter(|name| !name.is_empty());
    with_cache_lock(|| {
        let mut cache = load_cache();
        let changed = update_workspace_name(&mut cache, account_id, name);
        if changed {
            save_cache(&cache)?;
        }
        Ok(())
    })
}

fn update_workspace_name(cache: &mut CacheFile, account_id: &str, name: Option<&str>) -> bool {
    match name {
        Some(name) => {
            // A name that arrived retires any record saying there was none.
            let cleared = cache.workspace_names_absent.remove(account_id).is_some();
            if cache.workspace_names.get(account_id).map(String::as_str) == Some(name) {
                return cleared;
            }
            cache
                .workspace_names
                .insert(account_id.to_string(), name.to_string());
            true
        }
        None => {
            cache.workspace_names.remove(account_id);
            cache
                .workspace_names_absent
                .insert(account_id.to_string(), now_secs());
            true
        }
    }
}

/// Whether the workspace name for `account_id` has been resolved — to a name,
/// or to an absence still inside [`WORKSPACE_ABSENCE_TTL`].
///
/// This is the question callers must ask before looking one up. Asking
/// `get_workspace_name(..).is_none()` instead cannot tell "never looked up"
/// apart from "looked up, and there is none", so every personal plan answered
/// the second as if it were the first.
fn workspace_name_resolved(cache: &CacheFile, account_id: &str) -> bool {
    if cache.workspace_names.contains_key(account_id) {
        return true;
    }
    cache
        .workspace_names_absent
        .get(account_id)
        .is_some_and(|recorded| now_secs().saturating_sub(*recorded) <= WORKSPACE_ABSENCE_TTL)
}

/// Public form of [`workspace_name_resolved`].
pub fn workspace_name_is_known(account_id: &str) -> bool {
    match with_cache_lock(|| Ok(workspace_name_resolved(&load_cache(), account_id))) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!("Failed to read cached workspace state: {err}");
            false
        }
    }
}

/// Both answers from one read: the outer `None` means "not resolved, look it
/// up"; `Some(inner)` means resolved, where `inner` is the name if there is
/// one.
fn resolved_workspace_name(cache: &CacheFile, account_id: &str) -> Option<Option<String>> {
    workspace_name_resolved(cache, account_id)
        .then(|| cache.workspace_names.get(account_id).cloned())
}

/// Async form of [`resolved_workspace_name`], taking the lock once.
///
/// The lookup path runs inside up to `network.max_concurrent` tasks. Asking
/// "is it resolved" and "what is it" separately would take a cross-process
/// lock twice per task on a tokio worker — and leave a window between them in
/// which another process can change the answer.
pub async fn resolved_workspace_name_async(account_id: &str) -> Option<Option<String>> {
    let account_id = account_id.to_string();
    tokio::task::spawn_blocking(move || {
        match with_cache_lock(|| Ok(resolved_workspace_name(&load_cache(), &account_id))) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!("Failed to read cached workspace state: {err}");
                None
            }
        }
    })
    .await
    .ok()
    .flatten()
}

pub fn apply_workspace_name(info: &mut crate::jwt::AccountInfo) {
    let Some(account_id) = info.account_id.as_deref() else {
        return;
    };
    if let Some(name) = get_workspace_name(account_id) {
        info.workspace_name = Some(name);
    }
}

/// Remove cached usage for an alias while preserving last-used metadata.
///
/// Also drops any standing auth refusal: callers use this to force the next
/// read back onto the network, and leaving the refusal behind would answer
/// that read from cache anyway.
pub fn invalidate(alias: &str) -> Result<()> {
    with_cache_lock(|| {
        let mut cache = load_cache();
        if drop_alias(&mut cache, alias) {
            save_cache(&cache).context("writing usage cache invalidation")?;
        }
        Ok(())
    })
}

/// Async wrapper around [`get`]: runs the blocking lock + file read on a
/// dedicated blocking thread so it never stalls a tokio worker. Use this on
/// the high-concurrency usage-fetch path (up to `network.max_concurrent`
/// tasks) instead of calling [`get`] directly inside an async task.
pub async fn get_async(alias: &str) -> Option<UsageInfo> {
    let alias = alias.to_string();
    tokio::task::spawn_blocking(move || get(&alias))
        .await
        .ok()
        .flatten()
}

/// Async wrapper around [`put`]; see [`get_async`] for rationale.
pub async fn put_async(alias: &str, usage: &UsageInfo) {
    let alias = alias.to_string();
    let usage = usage.clone();
    let _ = tokio::task::spawn_blocking(move || put(&alias, &usage)).await;
}

/// Async wrapper around [`get_auth_failure`]; see [`get_async`] for rationale.
pub async fn get_auth_failure_async(alias: &str, refresh_token: &str) -> Option<UsageError> {
    let alias = alias.to_string();
    let refresh_token = refresh_token.to_string();
    tokio::task::spawn_blocking(move || get_auth_failure(&alias, &refresh_token))
        .await
        .ok()
        .flatten()
}

/// Async wrapper around [`put_auth_failure`]; see [`get_async`] for rationale.
pub async fn put_auth_failure_async(alias: &str, refresh_token: &str, error: &UsageError) {
    let alias = alias.to_string();
    let refresh_token = refresh_token.to_string();
    let error = error.clone();
    let _ =
        tokio::task::spawn_blocking(move || put_auth_failure(&alias, &refresh_token, &error)).await;
}

/// Get the last-used timestamp for an alias (0 if never used).
pub fn get_last_used(alias: &str) -> i64 {
    match with_cache_lock(|| Ok(load_cache().last_used.get(alias).copied().unwrap_or(0))) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!("Failed to read last-used cache for {alias}: {err}");
            0
        }
    }
}

/// Record that an alias was just selected by `use`.
pub fn set_last_used(alias: &str) -> Result<()> {
    with_cache_lock(|| {
        let mut cache = load_cache();
        cache
            .last_used
            .insert(alias.to_string(), crate::auth::now_unix_secs());
        save_cache(&cache).context("writing last_used cache")
    })
}

pub fn rename(old: &str, new: &str) -> Result<()> {
    with_cache_lock(|| {
        let mut cache = load_cache();
        if migrate_alias(&mut cache, old, new) {
            save_cache(&cache)?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs4::FileExt;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn test_cache_entry_deserialize_without_credits() {
        let entry: CacheEntry = serde_json::from_value(json!({
            "ts": 123,
            "primary_used": 25.0,
            "primary_reset": 456,
            "secondary_used": 75.0,
            "secondary_reset": 789
        }))
        .unwrap();

        assert_eq!(entry.credits_balance, None);
        assert_eq!(entry.unlimited_credits, None);
        assert_eq!(entry.reset_credits_available_count, None);
        assert!(entry.reset_credits.is_empty());
        assert_eq!(entry.reset_credits_error, None);
        assert!(!entry.account_limited);

        let usage = from_entry(&entry);
        assert_eq!(usage.credits_balance, None);
        assert_eq!(usage.unlimited_credits, None);
        assert_eq!(usage.reset_credits_available_count, None);
        assert!(usage.reset_credits.is_empty());
        assert!(!usage.account_limited);
    }

    #[test]
    fn test_cache_round_trip_preserves_limit_details() {
        let usage = UsageInfo {
            account_limited: true,
            rate_limit_reached_type: Some("rate_limit_reached".to_string()),
            additional_limits: vec![crate::usage::AdditionalRateLimit {
                limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
                metered_feature: Some("codex_bengalfox".to_string()),
                allowed: Some(true),
                limit_reached: Some(false),
                primary: None,
                secondary: None,
            }],
            ..Default::default()
        };

        let entry = to_entry(&usage);
        assert!(entry.account_limited);
        let restored = from_entry(&entry);
        assert!(restored.account_limited);
        assert_eq!(
            restored.rate_limit_reached_type.as_deref(),
            Some("rate_limit_reached")
        );
        assert_eq!(restored.additional_limits.len(), 1);
        assert_eq!(
            restored.additional_limits[0].metered_feature.as_deref(),
            Some("codex_bengalfox")
        );
    }

    #[test]
    fn a_recorded_verdict_is_readable_while_the_credential_is_unchanged() {
        let mut cache = CacheFile::default();
        record_auth_failure(
            &mut cache,
            "dead",
            "refresh_old",
            "re-login required (refresh_token_reused)",
            "detail",
        );

        let found = auth_failure_for(&cache, "dead", "refresh_old")
            .expect("the same credential must still be considered rejected");
        assert_eq!(found.summary, "re-login required (refresh_token_reused)");
    }

    #[test]
    fn a_recorded_verdict_does_not_apply_to_a_replacement_credential() {
        // Signing in again is the only cure for a terminal verdict, and it is
        // visible here as a different refresh token. Keying the record on the
        // alias instead would survive the re-login and keep a working account
        // marked dead.
        let mut cache = CacheFile::default();
        record_auth_failure(&mut cache, "dead", "refresh_old", "summary", "detail");

        assert!(
            auth_failure_for(&cache, "dead", "refresh_new").is_none(),
            "a verdict about a spent credential says nothing about its replacement"
        );
    }

    #[test]
    fn renaming_a_profile_carries_its_recorded_verdict() {
        let mut cache = CacheFile::default();
        record_auth_failure(&mut cache, "old", "refresh_old", "summary", "detail");

        migrate_alias(&mut cache, "old", "new");

        assert!(auth_failure_for(&cache, "old", "refresh_old").is_none());
        assert!(
            auth_failure_for(&cache, "new", "refresh_old").is_some(),
            "a rename must not resurrect network calls for a credential already known dead"
        );
    }

    #[test]
    fn one_read_answers_both_questions_about_a_workspace_name() {
        // The lookup path runs inside up to `network.max_concurrent` tasks, so
        // "is it resolved" and "what is it" must not cost two separate
        // acquisitions of a cross-process lock — nor leave a window between
        // them in which another process changes the answer.
        let mut cache = CacheFile::default();
        assert_eq!(resolved_workspace_name(&cache, "acct"), None);

        update_workspace_name(&mut cache, "acct", None);
        assert_eq!(resolved_workspace_name(&cache, "acct"), Some(None));

        update_workspace_name(&mut cache, "acct", Some("Platform"));
        assert_eq!(
            resolved_workspace_name(&cache, "acct"),
            Some(Some("Platform".to_string()))
        );
    }

    #[test]
    fn a_stored_verdict_keeps_no_control_characters_and_stays_bounded() {
        // `summary`/`detail` are server-controlled. They used to be shown once
        // and discarded; now they are persisted and re-rendered to the terminal
        // on every later listing, which makes escape sequences and unbounded
        // length worth stripping at the point they become durable.
        let mut cache = CacheFile::default();
        let hostile = format!("\u{1b}[31mred\u{1b}[0m\r\n{}", "x".repeat(4096));
        record_auth_failure(&mut cache, "dead", "refresh_old", &hostile, &hostile);

        let stored = auth_failure_for(&cache, "dead", "refresh_old").unwrap();
        for field in [&stored.summary, &stored.detail] {
            assert!(
                !field.chars().any(char::is_control),
                "control characters must not survive into the cache: {field:?}"
            );
            assert!(
                field.chars().count() <= STORED_TEXT_MAX,
                "stored text must be bounded, got {} chars",
                field.chars().count()
            );
        }
        assert!(
            stored.summary.contains("red"),
            "the readable part must survive: {:?}",
            stored.summary
        );
    }

    #[test]
    fn invalidating_an_alias_also_drops_its_recorded_verdict() {
        // `invalidate` is what makes the forced re-fetch after a reset-card
        // consume actually reach the network.
        let mut cache = CacheFile::default();
        record_auth_failure(&mut cache, "dead", "refresh_old", "summary", "detail");

        assert!(drop_alias(&mut cache, "dead"));

        assert!(auth_failure_for(&cache, "dead", "refresh_old").is_none());
    }

    #[test]
    fn a_confirmed_absence_is_remembered_so_the_account_is_not_looked_up_again() {
        // Personal plans have no workspace name. The server saying so is an
        // answer; not storing it made every invocation ask the same question.
        let mut cache = CacheFile::default();
        assert!(!workspace_name_resolved(&cache, "acct-personal"));

        assert!(update_workspace_name(&mut cache, "acct-personal", None));

        assert!(workspace_name_resolved(&cache, "acct-personal"));
        assert!(
            !cache.workspace_names.contains_key("acct-personal"),
            "a confirmed absence must not masquerade as a name"
        );
    }

    #[test]
    fn a_workspace_name_that_appears_later_supersedes_a_recorded_absence() {
        let mut cache = CacheFile::default();
        update_workspace_name(&mut cache, "acct", None);

        assert!(update_workspace_name(
            &mut cache,
            "acct",
            Some("Night City")
        ));

        assert_eq!(
            cache.workspace_names.get("acct").map(String::as_str),
            Some("Night City")
        );
        assert!(
            !cache.workspace_names_absent.contains_key("acct"),
            "a name that arrived must clear the record saying there was none"
        );
    }

    #[test]
    fn a_recorded_absence_expires_so_joining_an_organisation_is_noticed() {
        // Unlike a spent credential, this verdict can change on its own: an
        // account with no workspace today can be added to one tomorrow, and
        // nothing tells us. Bounding the record is what lets the new name
        // appear without the user knowing to reach for `--force`.
        let mut cache = CacheFile::default();
        update_workspace_name(&mut cache, "acct", None);
        cache
            .workspace_names_absent
            .insert("acct".to_string(), now_secs() - WORKSPACE_ABSENCE_TTL - 1);

        assert!(!workspace_name_resolved(&cache, "acct"));
    }

    #[test]
    fn authoritative_empty_workspace_name_clears_stale_cache() {
        let mut cache = CacheFile::default();
        assert!(update_workspace_name(
            &mut cache,
            "acct-team",
            Some("Old Team")
        ));
        assert!(update_workspace_name(&mut cache, "acct-team", None));
        assert!(!cache.workspace_names.contains_key("acct-team"));
    }

    #[test]
    fn cache_mutation_waits_for_cross_process_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("cache.lock");
        let holder = open_cache_lock_file(&lock_path).unwrap();
        FileExt::lock(&holder).unwrap();

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker_path = lock_path.clone();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx
                .send(with_cache_file_lock_at(
                    &worker_path,
                    Duration::from_secs(1),
                    || Ok(()),
                ))
                .unwrap();
        });
        started_rx.recv().unwrap();
        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "cache mutation must wait for an independently-held OS lock"
        );

        drop(holder);
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn cache_atomic_write_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let mut first = CacheFile::default();
        first.last_used.insert("alice".into(), 1);
        save_cache_at(&path, &first).unwrap();

        let mut second = CacheFile::default();
        second.last_used.insert("bob".into(), 2);
        save_cache_at(&path, &second).unwrap();

        let saved: CacheFile = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.last_used.get("bob"), Some(&2));
        assert!(!saved.last_used.contains_key("alice"));
    }

    #[test]
    fn cache_lock_timeout_preserves_live_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("cache.lock");
        let holder = open_cache_lock_file(&lock_path).unwrap();
        std::fs::write(&lock_path, "holder-marker").unwrap();
        FileExt::lock(&holder).unwrap();

        let err =
            with_cache_file_lock_at(&lock_path, Duration::from_millis(25), || Ok(())).unwrap_err();
        assert!(err.to_string().contains("cache lock"));
        let reopened = open_cache_lock_file(&lock_path).unwrap();
        assert!(matches!(
            FileExt::try_lock(&reopened),
            Err(fs4::TryLockError::WouldBlock)
        ));
        FileExt::unlock(&holder).unwrap();
        assert_eq!(
            std::fs::read_to_string(&lock_path).unwrap(),
            "holder-marker"
        );
    }
}
