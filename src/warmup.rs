use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock};

use anyhow::{Context, Result, bail};
use tokio::sync::{Mutex, OnceCell};
use tracing::{debug, warn};

use crate::http_retry::{self, ReplaySafety};

const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";

fn responses_url() -> String {
    std::env::var("CS_RESPONSES_URL").unwrap_or_else(|_| RESPONSES_URL.to_string())
}

fn models_url() -> String {
    std::env::var("CS_MODELS_URL").unwrap_or_else(|_| MODELS_URL.to_string())
}

static CODEX_VERSION: OnceCell<String> = OnceCell::const_new();
/// The models one warmup should touch, keyed by account *and* by the quota
/// pools that produced the selection (see [`warmup_cache_key`]).
///
/// The whole selected set is cached, not just the main-pool model: the main
/// request and the additional-pool requests are answered by a single `/models`
/// response, so caching only the first one made every warmup fetch that
/// response twice — and with no additional pools, threw the second away.
static MODEL_CACHE: LazyLock<Mutex<HashMap<String, Vec<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
// Serialize duplicate fetches for the same account without blocking unrelated accounts.
static MODEL_FETCH_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn model_cache_get(cache: &HashMap<String, Vec<String>>, key: &str) -> Option<Vec<String>> {
    cache.get(key).cloned()
}

fn model_cache_set(cache: &mut HashMap<String, Vec<String>>, key: &str, models: Vec<String>) {
    cache.insert(key.to_string(), models);
}

fn model_cache_invalidate(cache: &mut HashMap<String, Vec<String>>, key: &str) {
    cache.remove(key);
}

/// Detects the local `codex` CLI version. Runs the subprocess probe on a
/// blocking thread pool so it never stalls a tokio worker thread.
async fn detect_codex_version() -> &'static str {
    CODEX_VERSION
        .get_or_init(|| async {
            tokio::task::spawn_blocking(|| {
                std::process::Command::new("codex")
                    .arg("--version")
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .and_then(|s| parse_codex_version(&s))
                    .unwrap_or_else(|| crate::auth::ALIGNED_CODEX_VERSION.to_string())
            })
            .await
            .unwrap_or_else(|_| crate::auth::ALIGNED_CODEX_VERSION.to_string())
        })
        .await
}

/// Pick the version token out of `codex --version` output. Output shapes vary
/// (`codex-cli 0.144.1`, `codex-cli 0.1.0 (build abc)`), so take the first
/// dotted token that starts with a digit rather than the last token.
fn parse_codex_version(stdout: &str) -> Option<String> {
    stdout
        .split_whitespace()
        .find(|t| t.starts_with(|c: char| c.is_ascii_digit()) && t.contains('.'))
        .map(|v| v.to_string())
}

fn build_models_request(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    version: &str,
) -> reqwest::RequestBuilder {
    crate::usage::apply_account_routing_headers(
        client
            .get(models_url())
            .query(&[("client_version", version)])
            .bearer_auth(access_token),
        account_id,
        is_fedramp,
    )
}

/// One entry from the `/models` endpoint's `models[]` array.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ModelEntry {
    pub slug: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub visibility: Option<String>,
    pub priority: Option<i64>,
    pub supported_in_api: Option<bool>,
    pub context_window: Option<u64>,
    pub default_reasoning_effort: Option<String>,
    pub supported_reasoning_efforts: Vec<String>,
    pub input_modalities: Vec<String>,
    pub additional_speed_tiers: Vec<String>,
    pub service_tiers: Vec<String>,
    pub default_service_tier: Option<String>,
    pub max_context_window: Option<u64>,
    pub auto_compact_token_limit: Option<u64>,
    pub effective_context_window_percent: Option<i64>,
    pub supports_parallel_tool_calls: Option<bool>,
    pub supports_image_detail_original: Option<bool>,
    pub experimental_supported_tools: Vec<String>,
    pub supports_search_tool: Option<bool>,
    pub use_responses_lite: Option<bool>,
}

/// Parse the `/models` endpoint's JSON body into a `Vec<ModelEntry>`. Entries
/// missing a `slug` are skipped; other fields are treated as optional
/// (defensively ignoring unknown fields per the upstream contract).
fn parse_models_body(body: &serde_json::Value) -> Result<Vec<ModelEntry>> {
    let models = body["models"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("no models array in response"))?;

    Ok(models
        .iter()
        .filter_map(|m| {
            let slug = m["slug"].as_str()?.to_string();
            let string_list = |key: &str| {
                m.get(key)
                    .and_then(serde_json::Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            Some(ModelEntry {
                slug,
                display_name: m["display_name"].as_str().map(String::from),
                description: m["description"].as_str().map(String::from),
                visibility: m["visibility"].as_str().map(String::from),
                priority: m["priority"].as_i64(),
                supported_in_api: m["supported_in_api"].as_bool(),
                context_window: m["context_window"].as_u64(),
                default_reasoning_effort: m["default_reasoning_level"]
                    .as_str()
                    .or_else(|| m["default_reasoning_effort"].as_str())
                    .map(String::from),
                supported_reasoning_efforts: m
                    .get("supported_reasoning_levels")
                    .or_else(|| m.get("supported_reasoning_efforts"))
                    .and_then(serde_json::Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| {
                                item.as_str()
                                    .or_else(|| item.get("effort").and_then(|v| v.as_str()))
                                    .or_else(|| {
                                        item.get("reasoning_effort").and_then(|v| v.as_str())
                                    })
                                    .map(String::from)
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                input_modalities: string_list("input_modalities"),
                additional_speed_tiers: string_list("additional_speed_tiers"),
                service_tiers: m
                    .get("service_tiers")
                    .and_then(serde_json::Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| {
                                item.as_str()
                                    .or_else(|| item.get("id").and_then(|v| v.as_str()))
                                    .map(String::from)
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                default_service_tier: m["default_service_tier"].as_str().map(String::from),
                max_context_window: m["max_context_window"].as_u64(),
                auto_compact_token_limit: m["auto_compact_token_limit"].as_u64(),
                effective_context_window_percent: m["effective_context_window_percent"].as_i64(),
                supports_parallel_tool_calls: m["supports_parallel_tool_calls"].as_bool(),
                supports_image_detail_original: m["supports_image_detail_original"].as_bool(),
                experimental_supported_tools: string_list("experimental_supported_tools"),
                supports_search_tool: m["supports_search_tool"].as_bool(),
                use_responses_lite: m["use_responses_lite"].as_bool(),
            })
        })
        .collect())
}

/// Sort models for display: ascending priority (lowest number first), unknown
/// priority sorts last. Does not filter hidden models — callers decide how to
/// present `visibility == "hide"` entries (e.g. dim them rather than drop them).
pub(crate) fn sorted_models_for_display(models: &[ModelEntry]) -> Vec<&ModelEntry> {
    let mut sorted: Vec<&ModelEntry> = models.iter().collect();
    sorted.sort_by_key(|m| m.priority.unwrap_or(i64::MAX));
    sorted
}

/// Fetch and parse the full model list from the `/models` endpoint.
pub(crate) async fn fetch_models(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
) -> Result<Vec<ModelEntry>> {
    let version = detect_codex_version().await;
    for attempt in 1..=3 {
        let response = http_retry::send(
            build_models_request(client, access_token, account_id, is_fedramp, version),
            ReplaySafety::Idempotent,
        )
        .await;
        match response {
            Ok(resp) if resp.status.is_success() => {
                let body: serde_json::Value = serde_json::from_slice(&resp.body)?;
                return parse_models_body(&body);
            }
            Ok(resp) => {
                let status = resp.status;
                let retryable = status.is_server_error();
                if !retryable || attempt == 3 {
                    bail!("models endpoint returned {status}");
                }
                debug!("models fetch attempt {attempt}/3 returned {status}; retrying");
            }
            Err(error) => {
                if attempt == 3 {
                    return Err(error.context("models fetch failed after 3 attempts"));
                }
                debug!("models fetch attempt {attempt}/3 failed: {error}; retrying");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(250 * attempt)).await;
    }
    unreachable!("models fetch loop always returns")
}

/// Resolve every model this warmup should touch, from one `/models` response:
/// the main-pool model first, then one per additional quota pool.
async fn fetch_warmup_models(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    additional_limits: &[crate::usage::AdditionalRateLimit],
) -> Result<Vec<String>> {
    let models = fetch_models(client, access_token, account_id, is_fedramp).await?;
    let selected = select_warmup_models(&models, additional_limits)?;
    if selected.is_empty() {
        return require_official_model(Err(anyhow::anyhow!(
            "official models endpoint returned no main-pool model"
        )));
    }
    Ok(selected)
}

fn require_official_model<T>(result: Result<T>) -> Result<T> {
    result.map_err(|error| anyhow::anyhow!("could not resolve an official warmup model: {error:#}"))
}

fn normalized_pool_name(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The cache key for one account's resolved warmup model set.
///
/// The alias alone is not enough to name the entry: the resolved set bakes in
/// the additional pools that existed when it was built. A process that outlives
/// a pool change — the daemon with `auto_warmup`, which runs for days — would
/// otherwise keep warming the old set, and a pool the account just gained would
/// never get its quota window opened until someone restarted the daemon. That
/// failure is silent: nothing errors, so nothing invalidates the entry either.
///
/// Only the pools `select_warmup_models` acts on take part, and they are sorted,
/// so an upstream reordering does not needlessly discard a good entry.
fn warmup_cache_key(
    alias: &str,
    additional_limits: &[crate::usage::AdditionalRateLimit],
) -> String {
    let mut pools: Vec<String> = additional_limits
        .iter()
        .filter(|limit| is_model_quota_limit(limit))
        .map(|limit| normalized_pool_name(limit.limit_name.as_deref().unwrap_or_default()))
        .collect();
    pools.sort_unstable();
    // Unit separator: cannot appear in an alias or a normalized pool name, so
    // no pool list can be confused with a different account's key.
    format!("{alias}\u{1f}{}", pools.join("\u{1e}"))
}

fn is_model_quota_limit(limit: &crate::usage::AdditionalRateLimit) -> bool {
    limit
        .metered_feature
        .as_deref()
        .is_some_and(|feature| feature.starts_with("codex_"))
        && limit.allowed != Some(false)
        && limit.limit_reached != Some(true)
}

fn select_warmup_models(
    models: &[ModelEntry],
    additional_limits: &[crate::usage::AdditionalRateLimit],
) -> Result<Vec<String>> {
    let visible: Vec<&ModelEntry> = models
        .iter()
        .filter(|m| m.visibility.as_deref() != Some("hide"))
        .collect();

    if visible.is_empty() {
        bail!("no visible models available");
    }

    let model_limits: Vec<&crate::usage::AdditionalRateLimit> = additional_limits
        .iter()
        .filter(|limit| is_model_quota_limit(limit))
        .collect();
    let additional_models: Vec<&ModelEntry> = model_limits
        .iter()
        .filter_map(|limit| {
            let pool_name = normalized_pool_name(limit.limit_name.as_deref()?);
            visible.iter().copied().find(|model| {
                let slug = normalized_pool_name(&model.slug);
                let display = model
                    .display_name
                    .as_deref()
                    .map(normalized_pool_name)
                    .unwrap_or_default();
                !pool_name.is_empty()
                    && (pool_name == slug
                        || pool_name == display
                        || slug.contains(&pool_name)
                        || display.contains(&pool_name))
            })
        })
        .collect();
    if additional_models.len() != model_limits.len() {
        let unmatched = model_limits
            .iter()
            .filter(|limit| {
                let Some(name) = limit.limit_name.as_deref() else {
                    return true;
                };
                let pool_name = normalized_pool_name(name);
                !visible.iter().any(|model| {
                    let slug = normalized_pool_name(&model.slug);
                    let display = model
                        .display_name
                        .as_deref()
                        .map(normalized_pool_name)
                        .unwrap_or_default();
                    !pool_name.is_empty()
                        && (pool_name == slug
                            || pool_name == display
                            || slug.contains(&pool_name)
                            || display.contains(&pool_name))
                })
            })
            .map(|limit| limit.limit_name.as_deref().unwrap_or("unnamed"))
            .collect::<Vec<_>>()
            .join(", ");
        bail!("no model matched quota pool(s): {unmatched}");
    }
    let additional_slugs: HashSet<&str> = additional_models
        .iter()
        .map(|model| model.slug.as_str())
        .collect();
    let main_candidates: Vec<&ModelEntry> = visible
        .iter()
        .copied()
        .filter(|model| {
            model.supported_in_api != Some(false) && !additional_slugs.contains(model.slug.as_str())
        })
        .collect();

    // Prefer mini (lightest), fall back to highest priority (lowest number).
    // Models mapped to additional pools must not replace the main-pool request.
    let main = main_candidates
        .iter()
        .find(|m| m.slug.contains("mini"))
        .or_else(|| {
            main_candidates
                .iter()
                .min_by_key(|m| m.priority.unwrap_or(i64::MAX))
        })
        .map(|m| m.slug.clone());

    let mut selected: Vec<String> = main.into_iter().collect();
    for model in additional_models {
        if !selected.contains(&model.slug) {
            selected.push(model.slug.clone());
        }
    }

    debug!("warmup: models selected from API: {selected:?}");
    Ok(selected)
}

async fn resolve_warmup_models(
    cache_key: &str,
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    additional_limits: &[crate::usage::AdditionalRateLimit],
) -> Result<Vec<String>> {
    if let Some(models) = model_cache_get(&*MODEL_CACHE.lock().await, cache_key) {
        return Ok(models);
    }

    let fetch_lock = {
        let mut locks = MODEL_FETCH_LOCKS.lock().await;
        locks
            .entry(cache_key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let _fetch_guard = fetch_lock.lock().await;
    if let Some(models) = model_cache_get(&*MODEL_CACHE.lock().await, cache_key) {
        return Ok(models);
    }

    let models = fetch_warmup_models(
        client,
        access_token,
        account_id,
        is_fedramp,
        additional_limits,
    )
    .await?;
    model_cache_set(&mut *MODEL_CACHE.lock().await, cache_key, models.clone());
    Ok(models)
}

/// Split a resolved set into the main-pool model and the additional-pool ones.
///
/// `fetch_warmup_models` rejects an empty set, so the `None` arm is only
/// reachable through a cache entry that was never produced that way.
fn split_main_model(models: &[String]) -> Result<(&str, &[String])> {
    models
        .split_first()
        .map(|(main, additional)| (main.as_str(), additional))
        .ok_or_else(|| anyhow::anyhow!("no warmup model was resolved"))
}

fn build_body(model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "instructions": "You are a helpful assistant.",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "ping"}]
        }],
        "tools": [],
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "stream": true,
        "store": false,
        "include": []
    })
}

fn make_request(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    body: &serde_json::Value,
) -> reqwest::RequestBuilder {
    crate::usage::apply_account_routing_headers(
        client
            .post(responses_url())
            .bearer_auth(access_token)
            .header("Content-Type", "application/json"),
        account_id,
        is_fedramp,
    )
    .json(body)
}

/// Warm one request per additional quota pool.
///
/// Takes the models already resolved for this warmup rather than fetching the
/// list again: both halves come from the same `/models` answer, and
/// `select_warmup_models` already excludes the main-pool model from this slice.
async fn warmup_additional_models(
    client: &reqwest::Client,
    access_token: &str,
    account_id: Option<&str>,
    is_fedramp: bool,
    additional_models: &[String],
) -> Result<()> {
    for model in additional_models {
        let body = build_body(model);
        debug!("warmup additional pool POST → {RESPONSES_URL} (model={model})");
        let mut resp = make_request(client, access_token, account_id, is_fedramp, &body)
            .send()
            .await
            .map_err(|e| crate::auth::format_reqwest_error("additional warmup failed", &e))?;
        if !resp.status().is_success() {
            let status = resp.status();
            bail!("additional model {model}: HTTP {status}");
        }
        let _ = resp.chunk().await;
    }
    Ok(())
}

/// Write credentials the auth server just rotated back to the profile.
///
/// OpenAI's `refresh_token` is single-use: the previous one is already dead
/// server-side the moment these arrive, so a failed write leaves the only
/// credential the server still accepts in this process's memory. Finishing the
/// warmup (or the `/models` fetch) with it would exit successfully and hand the
/// user a profile that silently stops working at the next start, which makes
/// this a reportable failure rather than something to warn about and walk past.
///
/// The wording is shared with the usage path's [`crate::usage::UsageError::token_persist_failed`]
/// so the report stays distinguishable from a *rejected* refresh: here the
/// tokens are valid and the local write needs fixing, there the profile needs a
/// new sign-in.
///
/// Each caller owns a single account, so propagating this aborts that account
/// only — batch drivers keep processing the rest.
fn persist_refreshed_tokens(
    alias: &str,
    presented_refresh_token: &str,
    refreshed: &crate::usage::RefreshedTokens,
) -> Result<()> {
    let persisted = crate::profile::update_profile_tokens_if_refresh_matches(
        alias,
        presented_refresh_token,
        &refreshed.id_token,
        &refreshed.access_token,
        &refreshed.refresh_token,
    )
    .map_err(|err| {
        anyhow::anyhow!(
            "{}",
            crate::usage::UsageError::token_persist_failed(alias, &err).detail
        )
    })?;
    if !persisted {
        // Deliberately weaker than the usage path, which answers the same race
        // by re-reading the profile and retrying (`reload_rotated_credentials`).
        // Losing the CAS means a peer already wrote a newer credential, so the
        // profile is healthy and warmup has nothing left to do — warmup only
        // opens a quota window, and the next one will use the stored token.
        // Adding a recovery round here would buy nothing and duplicate the
        // hardest logic in the codebase.
        debug!(
            "[{alias}] skipped stale refreshed tokens because another process replaced the \
             presented refresh token"
        );
    }
    Ok(())
}

/// Send a minimal completion request to trigger the quota window countdown for a profile.
///
/// The 5-hour and 7-day windows only start after the first real API call.
/// This sends the lightest valid request ("ping") and discards the response body,
/// which is enough for the server to stamp the window start time.
pub async fn warmup_account(alias: &str, profile_path: &Path) -> Result<()> {
    let usage = match crate::cache::get(alias) {
        Some(usage) => Some(usage),
        None => {
            let current = crate::profile::read_current();
            match crate::usage::fetch_usage_retried_unattended(alias, profile_path, &current).await
            {
                Ok(usage) => Some(usage),
                Err(error) => {
                    warn!(
                        "[{alias}] could not discover additional quota pools: {}",
                        error.summary
                    );
                    None
                }
            }
        }
    };
    let additional_limits = usage
        .map(|usage| usage.additional_limits)
        .unwrap_or_default();
    let val = crate::auth::read_auth(profile_path)
        .map_err(|e| anyhow::anyhow!("{alias}: cannot read auth: {e}"))?;

    let (at, rt) = crate::auth::extract_tokens(&val);
    let mut id_token = crate::auth::extract_id_token(&val);
    let mut access_token = at
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{alias}: no access_token in profile"))?;
    let mut refresh_token = rt.filter(|s| !s.is_empty());

    let info = crate::auth::read_account_info(profile_path);
    let account_id = info.account_id;
    let is_fedramp = info.is_fedramp;

    let client = crate::auth::build_http_client()?;

    // Set when the pre-warmup proactive refresh below is rejected by the auth
    // server outright (e.g. `refresh_token_reused`): that refresh_token is now
    // permanently dead, so a later 401/403 must not spend a second round trip
    // replaying it — it can only re-trigger reuse detection.
    let mut rejected_refresh: Option<anyhow::Error> = None;

    // Pre-refresh: if token is about to expire, refresh proactively
    if let Some(ref rt) = refresh_token
        && crate::jwt::is_token_expiring(&access_token, 60) == Some(true)
    {
        tracing::info!(
            action = "token_refresh",
            alias,
            trigger = "warmup_expiry",
            "token refresh started"
        );
        match crate::usage::do_refresh_token(
            alias,
            &client,
            id_token.as_deref(),
            Some(&access_token),
            rt,
        )
        .await
        {
            Ok(refreshed) => {
                persist_refreshed_tokens(alias, rt, &refreshed)?;
                access_token = refreshed.access_token;
                id_token = Some(refreshed.id_token);
                refresh_token = Some(refreshed.refresh_token);
            }
            Err(e) => {
                if let Some(terminal) = e.downcast_ref::<crate::usage::TerminalAuthError>() {
                    warn!(
                        alias,
                        code = terminal.code,
                        "pre-warmup token refresh rejected permanently"
                    );
                    rejected_refresh = Some(e);
                } else {
                    warn!("[{alias}] pre-warmup token refresh failed");
                }
            }
        }
    }

    // One `/models` answer covers both the main-pool request below and every
    // additional-pool request after it.
    let cache_key = warmup_cache_key(alias, &additional_limits);
    let selected_models = resolve_warmup_models(
        &cache_key,
        &client,
        &access_token,
        account_id.as_deref(),
        is_fedramp,
        &additional_limits,
    )
    .await
    .with_context(|| format!("{alias}: failed to select a supported warmup model"))?;
    let (model, additional_models) = split_main_model(&selected_models)
        .with_context(|| format!("{alias}: failed to select a supported warmup model"))?;
    let body = build_body(model);

    debug!("[{alias}] warmup POST → {RESPONSES_URL} (model={model})");

    let mut resp = make_request(
        &client,
        &access_token,
        account_id.as_deref(),
        is_fedramp,
        &body,
    )
    .send()
    .await
    .map_err(|e| crate::auth::format_reqwest_error("warmup request failed", &e))?;

    let status = resp.status();
    debug!("[{alias}] warmup status: {status}");

    match status.as_u16() {
        200 => {
            // Quota window is triggered server-side on request receipt.
            // Read one chunk to confirm streaming started, then drop.
            let _ = resp.chunk().await;
            warmup_additional_models(
                &client,
                &access_token,
                account_id.as_deref(),
                is_fedramp,
                additional_models,
            )
            .await
        }
        400 => {
            let text = resp.text().await.unwrap_or_default();
            if text.contains("not supported") {
                // Model deprecated — clear cache, fetch fresh model list, retry once
                debug!(
                    "[{alias}] model {model:?} not supported, refreshing model cache and retrying"
                );
                model_cache_invalidate(&mut *MODEL_CACHE.lock().await, &cache_key);
                let refreshed_models = resolve_warmup_models(
                    &cache_key,
                    &client,
                    &access_token,
                    account_id.as_deref(),
                    is_fedramp,
                    &additional_limits,
                )
                .await
                .with_context(|| {
                    format!("{alias}: failed to refresh the supported warmup model")
                })?;
                let (new_model, new_additional_models) = split_main_model(&refreshed_models)
                    .with_context(|| {
                        format!("{alias}: failed to refresh the supported warmup model")
                    })?;
                let retry_body = build_body(new_model);
                let mut retry_resp = make_request(
                    &client,
                    &access_token,
                    account_id.as_deref(),
                    is_fedramp,
                    &retry_body,
                )
                .send()
                .await
                .map_err(|e| crate::auth::format_reqwest_error("warmup retry failed", &e))?;
                let retry_status = retry_resp.status();
                if retry_status.is_success() {
                    let _ = retry_resp.chunk().await;
                    return warmup_additional_models(
                        &client,
                        &access_token,
                        account_id.as_deref(),
                        is_fedramp,
                        new_additional_models,
                    )
                    .await;
                }
                bail!("{alias}: HTTP {retry_status} after model refresh")
            }
            bail!("{alias}: HTTP 400")
        }
        401 | 403 => {
            // The pre-warmup proactive refresh already got a terminal rejection
            // from the auth server for this same refresh_token — retrying here
            // would just replay a dead credential and burn another round trip.
            if let Some(e) = rejected_refresh {
                return Err(e.context(format!(
                    "{alias}: authentication failed (HTTP {status}) after proactive token refresh was already rejected"
                )));
            }
            // Retry once with refreshed token
            if let Some(ref rt) = refresh_token {
                debug!("[{alias}] got {status}, attempting token refresh and retry");
                match crate::usage::do_refresh_token(
                    alias,
                    &client,
                    id_token.as_deref(),
                    Some(&access_token),
                    rt,
                )
                .await
                {
                    Ok(refreshed) => {
                        persist_refreshed_tokens(alias, rt, &refreshed)?;
                        let mut retry_resp = make_request(
                            &client,
                            &refreshed.access_token,
                            account_id.as_deref(),
                            is_fedramp,
                            &body,
                        )
                        .send()
                        .await
                        .map_err(|e| {
                            crate::auth::format_reqwest_error("warmup retry failed", &e)
                        })?;
                        let retry_status = retry_resp.status();
                        if retry_status.is_success() {
                            let _ = retry_resp.chunk().await;
                            return warmup_additional_models(
                                &client,
                                &refreshed.access_token,
                                account_id.as_deref(),
                                is_fedramp,
                                additional_models,
                            )
                            .await;
                        }
                        bail!("{alias}: HTTP {retry_status} after token refresh retry")
                    }
                    Err(e) => bail!("{alias}: authentication failed and token refresh failed: {e}"),
                }
            }
            bail!(
                "{alias}: authentication failed — token may be expired (run `codex-switch list` to refresh)"
            )
        }
        429 => bail!("{alias}: rate limited"),
        code => bail!("{alias}: HTTP {code}"),
    }
}

/// Fetch the full model list for a profile (for display, e.g. the TUI detail
/// panel). Unlike `warmup_account`, this never sends a warmup ping — it only
/// refreshes an expiring access token before calling the `/models` endpoint.
pub(crate) async fn fetch_models_for_profile(
    alias: &str,
    profile_path: &Path,
) -> Result<Vec<ModelEntry>> {
    let val = crate::auth::read_auth(profile_path)
        .map_err(|e| anyhow::anyhow!("{alias}: cannot read auth: {e}"))?;

    let (at, rt) = crate::auth::extract_tokens(&val);
    let id_token = crate::auth::extract_id_token(&val);
    let mut access_token = at
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{alias}: no access_token in profile"))?;
    let refresh_token = rt.filter(|s| !s.is_empty());

    let info = crate::auth::read_account_info(profile_path);
    let account_id = info.account_id;
    let is_fedramp = info.is_fedramp;

    let client = crate::auth::build_http_client()?;

    if let Some(ref rt) = refresh_token
        && crate::jwt::is_token_expiring(&access_token, 60) == Some(true)
    {
        match crate::usage::do_refresh_token(
            alias,
            &client,
            id_token.as_deref(),
            Some(&access_token),
            rt,
        )
        .await
        {
            Ok(refreshed) => {
                // No degrade here: the refresh *worked*, so the old token this
                // would fall back to has already been invalidated server-side.
                persist_refreshed_tokens(alias, rt, &refreshed)?;
                access_token = refreshed.access_token;
            }
            // Deliberate degrade: fall through and try /models with the
            // existing (possibly expiring) token rather than failing here.
            // Still worth a diagnosable trace — silently swallowing this
            // sent people chasing an unrelated /models error instead of the
            // real cause (a rejected/expired refresh_token).
            Err(e) => {
                if let Some(terminal) = e.downcast_ref::<crate::usage::TerminalAuthError>() {
                    warn!(
                        alias,
                        code = terminal.code,
                        "proactive token refresh rejected, continuing with existing token"
                    );
                } else {
                    warn!(
                        "[{alias}] proactive token refresh failed, continuing with existing token"
                    );
                }
            }
        }
    }

    fetch_models(&client, &access_token, account_id.as_deref(), is_fedramp).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_cache_keys_are_isolated_per_account() {
        let mut cache: HashMap<String, Vec<String>> = HashMap::new();
        model_cache_set(&mut cache, "account-a", vec!["model-a".to_string()]);

        assert_eq!(
            model_cache_get(&cache, "account-a"),
            Some(vec!["model-a".to_string()])
        );
        assert_eq!(model_cache_get(&cache, "account-b"), None);
    }

    #[test]
    fn test_model_cache_invalidation_only_affects_target_key() {
        let mut cache: HashMap<String, Vec<String>> = HashMap::new();
        model_cache_set(&mut cache, "account-a", vec!["model-a".to_string()]);
        model_cache_set(&mut cache, "account-b", vec!["model-b".to_string()]);

        model_cache_invalidate(&mut cache, "account-a");

        assert_eq!(model_cache_get(&cache, "account-a"), None);
        assert_eq!(
            model_cache_get(&cache, "account-b"),
            Some(vec!["model-b".to_string()])
        );
    }

    /// The cache holds the whole resolved set, so an additional-pool model
    /// survives alongside the main one and the second `/models` fetch that used
    /// to retrieve it is unnecessary.
    #[test]
    fn test_model_cache_round_trips_the_whole_selected_set() {
        let mut cache: HashMap<String, Vec<String>> = HashMap::new();
        let selected = vec!["gpt-5-mini".to_string(), "gpt-5-spark".to_string()];
        model_cache_set(&mut cache, "account-a", selected.clone());

        let cached = model_cache_get(&cache, "account-a").expect("entry must round-trip");
        let (main, additional) = split_main_model(&cached).unwrap();

        assert_eq!(main, "gpt-5-mini");
        assert_eq!(additional, ["gpt-5-spark".to_string()]);
    }

    #[test]
    fn test_split_main_model_rejects_an_empty_selection() {
        assert!(split_main_model(&[]).is_err());
    }

    fn model_pool(limit_name: &str) -> crate::usage::AdditionalRateLimit {
        crate::usage::AdditionalRateLimit {
            limit_name: Some(limit_name.to_string()),
            metered_feature: Some("codex_mini".to_string()),
            allowed: Some(true),
            limit_reached: Some(false),
            primary: None,
            secondary: None,
        }
    }

    #[test]
    fn cache_key_separates_accounts_that_share_a_pool_set() {
        assert_ne!(
            warmup_cache_key("alice", &[model_pool("gpt-5-mini")]),
            warmup_cache_key("bob", &[model_pool("gpt-5-mini")])
        );
    }

    /// A changed pool set must produce a different key — that miss is the only
    /// thing that re-resolves the model list for a long-running daemon.
    #[test]
    fn cache_key_changes_when_a_pool_is_added() {
        let before = warmup_cache_key("alice", &[]);
        let after = warmup_cache_key("alice", &[model_pool("gpt-5-mini")]);
        assert_ne!(before, after);
    }

    /// The mirror image: upstream reordering the same pools must not throw away
    /// a perfectly good entry and buy a `/models` round trip per warmup.
    #[test]
    fn cache_key_ignores_pool_order() {
        let one = warmup_cache_key(
            "alice",
            &[model_pool("gpt-5-mini"), model_pool("gpt-5-spark")],
        );
        let other = warmup_cache_key(
            "alice",
            &[model_pool("gpt-5-spark"), model_pool("gpt-5-mini")],
        );
        assert_eq!(one, other);
    }

    /// Pools that `select_warmup_models` never acts on must not perturb the key
    /// either, or an unrelated non-model quota would invalidate a good entry.
    #[test]
    fn cache_key_ignores_pools_that_are_not_warmed() {
        let non_model = crate::usage::AdditionalRateLimit {
            metered_feature: Some("code_review".to_string()),
            ..model_pool("Code review")
        };
        let exhausted = crate::usage::AdditionalRateLimit {
            limit_reached: Some(true),
            ..model_pool("gpt-5-spark")
        };
        assert_eq!(
            warmup_cache_key("alice", &[model_pool("gpt-5-mini")]),
            warmup_cache_key("alice", &[model_pool("gpt-5-mini"), non_model, exhausted])
        );
    }

    #[test]
    fn test_parse_models_body_full_entry() {
        let body = serde_json::json!({
            "models": [{
                "slug": "gpt-5.3-codex",
                "display_name": "GPT-5.3 Codex",
                "description": "Best for coding",
                "visibility": "List",
                "priority": 1,
                "supported_in_api": true,
                "context_window": 128000,
                "default_reasoning_level": "medium",
                "supported_reasoning_levels": [
                    {"effort": "low"},
                    {"reasoning_effort": "high"}
                ],
                "input_modalities": ["text", "image"],
                "additional_speed_tiers": ["fast"],
                "service_tiers": [{"id": "fast"}],
                "default_service_tier": "fast",
                "max_context_window": 256000,
                "auto_compact_token_limit": 110000,
                "effective_context_window_percent": 95,
                "supports_parallel_tool_calls": true,
                "supports_image_detail_original": true,
                "experimental_supported_tools": ["computer"],
                "supports_search_tool": true,
                "use_responses_lite": false
            }]
        });

        let models = parse_models_body(&body).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(
            models[0],
            ModelEntry {
                slug: "gpt-5.3-codex".to_string(),
                display_name: Some("GPT-5.3 Codex".to_string()),
                description: Some("Best for coding".to_string()),
                visibility: Some("List".to_string()),
                priority: Some(1),
                supported_in_api: Some(true),
                context_window: Some(128000),
                default_reasoning_effort: Some("medium".to_string()),
                supported_reasoning_efforts: vec!["low".to_string(), "high".to_string()],
                input_modalities: vec!["text".to_string(), "image".to_string()],
                additional_speed_tiers: vec!["fast".to_string()],
                service_tiers: vec!["fast".to_string()],
                default_service_tier: Some("fast".to_string()),
                max_context_window: Some(256000),
                auto_compact_token_limit: Some(110000),
                effective_context_window_percent: Some(95),
                supports_parallel_tool_calls: Some(true),
                supports_image_detail_original: Some(true),
                experimental_supported_tools: vec!["computer".to_string()],
                supports_search_tool: Some(true),
                use_responses_lite: Some(false),
            }
        );
    }

    #[test]
    fn test_parse_models_body_missing_optional_fields() {
        let body = serde_json::json!({
            "models": [{"slug": "gpt-5-mini"}]
        });

        let models = parse_models_body(&body).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "gpt-5-mini");
        assert_eq!(models[0].display_name, None);
        assert_eq!(models[0].visibility, None);
        assert_eq!(models[0].priority, None);
        assert_eq!(models[0].supported_in_api, None);
        assert_eq!(models[0].context_window, None);
    }

    #[test]
    fn test_parse_models_body_empty_list() {
        let body = serde_json::json!({"models": []});
        let models = parse_models_body(&body).unwrap();
        assert!(models.is_empty());
    }

    #[test]
    fn test_parse_models_body_missing_array_errors() {
        let body = serde_json::json!({});
        assert!(parse_models_body(&body).is_err());
    }

    #[test]
    fn test_sorted_models_for_display_orders_by_priority_ascending() {
        let models = vec![
            ModelEntry {
                slug: "b".to_string(),
                display_name: None,
                visibility: None,
                priority: Some(3),
                ..Default::default()
            },
            ModelEntry {
                slug: "a".to_string(),
                display_name: None,
                visibility: None,
                priority: Some(1),
                ..Default::default()
            },
            ModelEntry {
                slug: "c-no-priority".to_string(),
                display_name: None,
                visibility: None,
                priority: None,
                ..Default::default()
            },
        ];

        let sorted = sorted_models_for_display(&models);
        let slugs: Vec<&str> = sorted.iter().map(|m| m.slug.as_str()).collect();
        assert_eq!(slugs, vec!["a", "b", "c-no-priority"]);
    }

    #[test]
    fn test_sorted_models_for_display_empty_list() {
        assert!(sorted_models_for_display(&[]).is_empty());
    }

    #[test]
    fn test_warmup_models_include_main_pool_and_spark_pool() {
        let models = vec![
            ModelEntry {
                slug: "gpt-5.4-mini".to_string(),
                display_name: None,
                visibility: Some("List".to_string()),
                priority: Some(10),
                supported_in_api: Some(true),
                ..Default::default()
            },
            ModelEntry {
                slug: "gpt-5.3-codex-spark".to_string(),
                display_name: None,
                visibility: Some("List".to_string()),
                priority: Some(26),
                supported_in_api: Some(false),
                ..Default::default()
            },
        ];
        let limits = vec![crate::usage::AdditionalRateLimit {
            limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
            metered_feature: Some("codex_bengalfox".to_string()),
            ..Default::default()
        }];

        assert_eq!(
            select_warmup_models(&models, &limits).unwrap(),
            vec!["gpt-5.4-mini", "gpt-5.3-codex-spark"]
        );
    }

    #[test]
    fn test_warmup_models_exclude_disallowed_additional_pool() {
        let models = vec![
            ModelEntry {
                slug: "gpt-5.4-mini".to_string(),
                visibility: Some("List".to_string()),
                supported_in_api: Some(true),
                ..Default::default()
            },
            ModelEntry {
                slug: "gpt-5.3-codex-spark".to_string(),
                visibility: Some("List".to_string()),
                supported_in_api: Some(true),
                ..Default::default()
            },
        ];
        let limits = vec![crate::usage::AdditionalRateLimit {
            limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
            metered_feature: Some("codex_bengalfox".to_string()),
            allowed: Some(false),
            limit_reached: Some(false),
            ..Default::default()
        }];

        assert_eq!(
            select_warmup_models(&models, &limits).unwrap(),
            vec!["gpt-5.4-mini"]
        );
    }

    #[test]
    fn test_warmup_models_exclude_exhausted_additional_pool() {
        let models = vec![
            ModelEntry {
                slug: "gpt-5.4-mini".to_string(),
                visibility: Some("List".to_string()),
                supported_in_api: Some(true),
                ..Default::default()
            },
            ModelEntry {
                slug: "gpt-5.3-codex-spark".to_string(),
                visibility: Some("List".to_string()),
                supported_in_api: Some(true),
                ..Default::default()
            },
        ];
        let limits = vec![crate::usage::AdditionalRateLimit {
            limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
            metered_feature: Some("codex_bengalfox".to_string()),
            allowed: Some(true),
            limit_reached: Some(true),
            ..Default::default()
        }];

        assert_eq!(
            select_warmup_models(&models, &limits).unwrap(),
            vec!["gpt-5.4-mini"]
        );
    }

    #[test]
    fn test_warmup_models_do_not_use_spark_as_the_main_pool_fallback() {
        let models = vec![
            ModelEntry {
                slug: "gpt-5.6-codex".to_string(),
                visibility: Some("List".to_string()),
                priority: Some(10),
                supported_in_api: Some(true),
                ..Default::default()
            },
            ModelEntry {
                slug: "gpt-5.3-codex-spark".to_string(),
                visibility: Some("List".to_string()),
                priority: Some(1),
                supported_in_api: Some(true),
                ..Default::default()
            },
        ];
        let limits = vec![crate::usage::AdditionalRateLimit {
            limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
            metered_feature: Some("codex_bengalfox".to_string()),
            ..Default::default()
        }];

        assert_eq!(
            select_warmup_models(&models, &limits).unwrap(),
            vec!["gpt-5.6-codex", "gpt-5.3-codex-spark"]
        );
    }

    #[test]
    fn test_warmup_models_cover_all_matching_model_quota_pools() {
        let models = vec![
            ModelEntry {
                slug: "gpt-5.4-mini".to_string(),
                display_name: Some("GPT-5.4 Mini".to_string()),
                visibility: Some("List".to_string()),
                priority: Some(10),
                supported_in_api: Some(true),
                ..Default::default()
            },
            ModelEntry {
                slug: "gpt-5.3-codex-spark".to_string(),
                display_name: Some("GPT-5.3-Codex-Spark".to_string()),
                visibility: Some("List".to_string()),
                priority: Some(2),
                supported_in_api: Some(true),
                ..Default::default()
            },
            ModelEntry {
                slug: "gpt-6-codex-burst".to_string(),
                display_name: Some("GPT-6 Codex Burst".to_string()),
                visibility: Some("List".to_string()),
                priority: Some(1),
                supported_in_api: Some(true),
                ..Default::default()
            },
        ];
        let limits = vec![
            crate::usage::AdditionalRateLimit {
                limit_name: Some("GPT-5.3-Codex-Spark".to_string()),
                metered_feature: Some("codex_bengalfox".to_string()),
                ..Default::default()
            },
            crate::usage::AdditionalRateLimit {
                limit_name: Some("GPT-6-Codex-Burst".to_string()),
                metered_feature: Some("codex_futureburst".to_string()),
                ..Default::default()
            },
        ];

        assert_eq!(
            select_warmup_models(&models, &limits).unwrap(),
            vec!["gpt-5.4-mini", "gpt-5.3-codex-spark", "gpt-6-codex-burst"]
        );
    }

    #[test]
    fn test_unmatched_model_quota_pool_is_reported() {
        let models = vec![ModelEntry {
            slug: "gpt-5.4-mini".to_string(),
            display_name: Some("GPT-5.4 Mini".to_string()),
            visibility: Some("List".to_string()),
            supported_in_api: Some(true),
            ..Default::default()
        }];
        let limits = vec![crate::usage::AdditionalRateLimit {
            limit_name: Some("GPT-6-Codex-Burst".to_string()),
            metered_feature: Some("codex_futureburst".to_string()),
            ..Default::default()
        }];

        let error = select_warmup_models(&models, &limits).unwrap_err();
        assert!(error.to_string().contains("GPT-6-Codex-Burst"));
    }

    #[test]
    fn test_model_fetch_failure_is_not_replaced_with_a_hardcoded_model() {
        let error = require_official_model::<Vec<String>>(Err(anyhow::anyhow!(
            "models endpoint unavailable"
        )))
        .unwrap_err();

        assert!(error.to_string().contains("models endpoint unavailable"));
        assert!(!error.to_string().contains("gpt-5.3-codex"));
    }

    #[test]
    fn test_parse_codex_version_picks_semver_token() {
        assert_eq!(
            parse_codex_version("codex-cli 0.144.1\n"),
            Some("0.144.1".to_string())
        );
        assert_eq!(
            parse_codex_version("codex-cli 0.1.0 (build abc)\n"),
            Some("0.1.0".to_string())
        );
        assert_eq!(parse_codex_version("0.5.0\n"), Some("0.5.0".to_string()));
        assert_eq!(parse_codex_version("command not found\n"), None);
    }

    #[test]
    fn test_models_request_includes_workspace_and_fedramp_headers() {
        let request = build_models_request(
            &reqwest::Client::new(),
            "access-token",
            Some("workspace-123"),
            true,
            "0.144.1",
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
    fn test_responses_request_includes_workspace_and_fedramp_headers() {
        let request = make_request(
            &reqwest::Client::new(),
            "access-token",
            Some("workspace-123"),
            true,
            &build_body("gpt-test"),
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

    // ── Terminal refresh failure must not be replayed ──────────────────
    //
    // `CS_TOKEN_URL`, `CS_MODELS_URL` and `CS_RESPONSES_URL` are process-global
    // env vars (see `responses_url()` / `models_url()` and
    // `auth::token_url()`), and login's tests retarget `CS_TOKEN_URL` as well,
    // so every test in this group takes the crate-wide `auth::URL_ENV_LOCK`
    // rather than a lock private to this module.
    mod refresh_short_circuit {
        use super::*;
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        use axum::{Json, Router};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::auth::URL_ENV_LOCK as ENV_LOCK;

        struct EnvVarGuard {
            key: &'static str,
            previous: Option<String>,
        }

        impl EnvVarGuard {
            fn set(key: &'static str, value: &str) -> Self {
                let previous = std::env::var(key).ok();
                unsafe {
                    std::env::set_var(key, value);
                }
                Self { key, previous }
            }
        }

        impl Drop for EnvVarGuard {
            fn drop(&mut self) {
                unsafe {
                    match &self.previous {
                        Some(value) => std::env::set_var(self.key, value),
                        None => std::env::remove_var(self.key),
                    }
                }
            }
        }

        fn make_jwt(claims: &serde_json::Value) -> String {
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
            format!("header.{payload}.signature")
        }

        /// An access_token JWT that `is_token_expiring` already treats as expired,
        /// so `warmup_account`'s pre-warmup proactive refresh always fires.
        fn expired_access_token() -> String {
            make_jwt(&serde_json::json!({ "exp": crate::auth::now_unix_secs() - 10 }))
        }

        fn write_test_auth(path: &std::path::Path, access_token: &str, refresh_token: &str) {
            let val = serde_json::json!({
                "tokens": {
                    "id_token": make_jwt(&serde_json::json!({})),
                    "access_token": access_token,
                    "refresh_token": refresh_token,
                }
            });
            crate::auth::write_auth(path, &val).unwrap();
        }

        /// Starts a mock server answering all three warmup-relevant endpoints and
        /// points `CS_TOKEN_URL` / `CS_MODELS_URL` / `CS_RESPONSES_URL` at it.
        /// Returns the request counters and the env guards (drop order keeps the
        /// guards alive for the caller's whole test).
        async fn start_mock_server(
            token_status: StatusCode,
            token_body: serde_json::Value,
            responses_status: StatusCode,
        ) -> (Arc<AtomicUsize>, Vec<EnvVarGuard>) {
            let token_calls = Arc::new(AtomicUsize::new(0));
            let counter = token_calls.clone();

            let app = Router::new()
                .route(
                    "/oauth/token",
                    post(move || {
                        let counter = counter.clone();
                        let body = token_body.clone();
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            (token_status, Json(body))
                        }
                    }),
                )
                .route(
                    "/codex/models",
                    get(|| async {
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "models": [{"slug": "gpt-5-mini", "supported_in_api": true}]
                            })),
                        )
                    }),
                )
                .route(
                    "/codex/responses",
                    post(move || async move { (responses_status, "") }),
                );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            let guards = vec![
                EnvVarGuard::set("CS_TOKEN_URL", &format!("http://{addr}/oauth/token")),
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models")),
                EnvVarGuard::set(
                    "CS_RESPONSES_URL",
                    &format!("http://{addr}/codex/responses"),
                ),
            ];
            (token_calls, guards)
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn model_resolution_for_different_accounts_fetches_concurrently() {
            let _lock = ENV_LOCK.lock().await;
            let (arrival_tx, mut arrival_rx) = tokio::sync::mpsc::unbounded_channel();
            let release_first = Arc::new(tokio::sync::Semaphore::new(0));

            let app = Router::new().route(
                "/codex/models",
                get({
                    let release_first = release_first.clone();
                    move |headers: axum::http::HeaderMap| {
                        let arrival_tx = arrival_tx.clone();
                        let release_first = release_first.clone();
                        async move {
                            let account_id = headers
                                .get("chatgpt-account-id")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_string();
                            arrival_tx.send(account_id.clone()).unwrap();
                            if account_id == "workspace-one" {
                                let _permit = release_first.acquire().await.unwrap();
                            }
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "models": [{
                                        "slug": "gpt-5-mini",
                                        "supported_in_api": true
                                    }]
                                })),
                            )
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let _models_url =
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models"));

            let client = reqwest::Client::new();
            let first_client = client.clone();
            let first = tokio::spawn(async move {
                resolve_warmup_models(
                    "concurrent-model-account-one",
                    &first_client,
                    "token-one",
                    Some("workspace-one"),
                    false,
                    &[],
                )
                .await
            });
            assert_eq!(arrival_rx.recv().await.as_deref(), Some("workspace-one"));

            let second = tokio::spawn(async move {
                resolve_warmup_models(
                    "concurrent-model-account-two",
                    &client,
                    "token-two",
                    Some("workspace-two"),
                    false,
                    &[],
                )
                .await
            });
            let second_arrived_in_parallel =
                tokio::time::timeout(std::time::Duration::from_millis(300), arrival_rx.recv())
                    .await
                    .is_ok();

            release_first.add_permits(1);
            first.await.unwrap().unwrap();
            second.await.unwrap().unwrap();

            assert!(
                second_arrived_in_parallel,
                "a slow /models fetch for one account must not block another account"
            );
        }

        // `CODEX_SWITCH_HOME` is also mutated by `profile::tests` under its own
        // `TEST_ENV_LOCK` — take that lock too for the whole test body, or the two
        // test modules race on the same process-global env var when the harness
        // runs them in parallel. Holding it across `.await` is safe here: these
        // are `#[tokio::test]` current-thread runtimes, so no other task on this
        // thread ever needs the lock back before the test finishes.
        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn terminal_pre_refresh_failure_is_not_replayed_on_the_401_retry() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = tempfile::tempdir().unwrap();
            let _codex_switch_home =
                EnvVarGuard::set("CODEX_SWITCH_HOME", &home.path().display().to_string());

            let alias = "terminal-refresh-test";
            // Pre-populate the usage cache so `warmup_account` never calls the
            // (unrelated) usage-fetch path, which has its own independent
            // proactive-refresh call — that would inflate the auth-endpoint
            // call count for a reason this test isn't about.
            crate::cache::put(alias, &crate::usage::UsageInfo::default());

            let profile_path = home.path().join("auth.json");
            write_test_auth(&profile_path, &expired_access_token(), "refresh-token-1");

            // Every refresh attempt is rejected as reused; the auth server never
            // issues new tokens. The warmup POST is unreachable with a live token,
            // so it must also come back unauthorized.
            let (token_calls, _guards) = start_mock_server(
                StatusCode::UNAUTHORIZED,
                serde_json::json!({
                    "error": {
                        "code": "refresh_token_reused",
                        "message": "This refresh token has already been used.",
                    }
                }),
                StatusCode::UNAUTHORIZED,
            )
            .await;

            let result = warmup_account(alias, &profile_path).await;

            assert!(
                result.is_err(),
                "a permanently rejected refresh_token must not be reported as a successful warmup"
            );
            assert_eq!(
                token_calls.load(Ordering::SeqCst),
                1,
                "a terminal refresh rejection must not be replayed a second time from the \
                 401 handler — it can only ever fail again and costs a full round trip"
            );
        }

        // ── `fetch_models_for_profile` must not swallow a refresh failure ──

        #[derive(Clone, Default)]
        struct LogBuf(Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for LogBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
            type Writer = LogBuf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        impl LogBuf {
            fn contents(&self) -> String {
                String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
            }
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn fetch_models_for_profile_logs_the_reason_when_proactive_refresh_fails() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = tempfile::tempdir().unwrap();
            let _codex_switch_home =
                EnvVarGuard::set("CODEX_SWITCH_HOME", &home.path().display().to_string());

            let alias = "fetch-models-refresh-log-test";
            let profile_path = home.path().join("auth.json");
            write_test_auth(&profile_path, &expired_access_token(), "refresh-token-2");

            // `fetch_models_for_profile` never sends a warmup ping, so the
            // responses-endpoint status is irrelevant here.
            let (_token_calls, _guards) = start_mock_server(
                StatusCode::UNAUTHORIZED,
                serde_json::json!({
                    "error": {
                        "code": "refresh_token_reused",
                        "message": "This refresh token has already been used.",
                    }
                }),
                StatusCode::UNAUTHORIZED,
            )
            .await;

            let log_buf = LogBuf::default();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(log_buf.clone())
                .with_max_level(tracing::Level::WARN)
                .with_ansi(false)
                .without_time()
                .finish();

            let result = {
                let _tracing_guard = tracing::subscriber::set_default(subscriber);
                fetch_models_for_profile(alias, &profile_path).await
            };

            // Existing degrade-gracefully behavior must be preserved: a failed
            // proactive refresh still falls through to /models with the old token.
            assert!(
                result.is_ok(),
                "a refresh failure must not abort the /models fetch: {result:?}"
            );

            let logs = log_buf.contents();
            assert!(
                logs.contains("refresh_token_reused"),
                "the real rejection reason must be traceable in the logs, not silently \
                 dropped — captured log output was: {logs:?}"
            );
        }

        // ── rotated tokens that never reach disk must abort the account ──
        //
        // OpenAI's refresh_token is single-use: the moment the auth server hands
        // back a rotated one, the previous token is dead. If the write back to
        // the profile then fails, the only credential the server still accepts
        // lives in this process's memory. Finishing the request with it and
        // exiting zero leaves a bricked profile and no signal, so every one of
        // these paths has to report instead.

        /// Substring every persist-failure report must carry, so the message
        /// can never be mistaken for the auth server rejecting the refresh.
        const PERSIST_FAILURE_MARKER: &str =
            "token refresh succeeded but the rotated credentials could not be saved";

        /// An access_token JWT far from expiry, so the pre-warmup proactive
        /// refresh stays out of the way and the 401 retry path can be exercised
        /// on its own.
        fn live_access_token() -> String {
            make_jwt(&serde_json::json!({ "exp": crate::auth::now_unix_secs() + 3600 }))
        }

        /// Stage a profile that reads fine but can never be written back: the
        /// alias-derived `profiles/<alias>/auth.json` is occupied by a
        /// *directory*, which fails the write on unix and Windows alike (no
        /// permission-bit semantics involved). The tokens the run starts from
        /// live in a separate readable file, so the refresh itself succeeds and
        /// only the persist step fails — exactly the production window.
        fn stage_unwritable_profile(
            home: &std::path::Path,
            alias: &str,
            access_token: &str,
        ) -> std::path::PathBuf {
            let readable = home.join("staged").join(alias).join("auth.json");
            write_test_auth(&readable, access_token, "refresh-token-live");
            std::fs::create_dir_all(home.join("profiles").join(alias).join("auth.json")).unwrap();
            readable
        }

        /// Stage a normal profile whose rotated tokens can be written back.
        fn stage_writable_profile(
            home: &std::path::Path,
            alias: &str,
            access_token: &str,
        ) -> std::path::PathBuf {
            let path = home.join("profiles").join(alias).join("auth.json");
            write_test_auth(&path, access_token, "refresh-token-live");
            path
        }

        /// Mock server whose `/oauth/token` always rotates successfully, so the
        /// only thing that can go wrong is the write back. `/codex/responses`
        /// walks `responses_statuses` one entry per request and repeats the last
        /// entry once exhausted — a request counter, never a timer, decides what
        /// comes back.
        async fn start_rotating_mock_server(
            responses_statuses: Vec<StatusCode>,
        ) -> (Arc<AtomicUsize>, Vec<EnvVarGuard>) {
            let token_calls = Arc::new(AtomicUsize::new(0));
            let counter = token_calls.clone();
            let responses_calls = Arc::new(AtomicUsize::new(0));

            let app = Router::new()
                .route(
                    "/oauth/token",
                    post(move || {
                        let counter = counter.clone();
                        async move {
                            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "id_token": make_jwt(&serde_json::json!({})),
                                    "access_token": live_access_token(),
                                    "refresh_token": format!("rotated-refresh-{n}"),
                                })),
                            )
                        }
                    }),
                )
                .route(
                    "/codex/models",
                    get(|| async {
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "models": [{"slug": "gpt-5-mini", "supported_in_api": true}]
                            })),
                        )
                    }),
                )
                .route(
                    "/codex/responses",
                    post(move || {
                        let calls = responses_calls.clone();
                        let statuses = responses_statuses.clone();
                        async move {
                            let index = calls.fetch_add(1, Ordering::SeqCst);
                            let status = statuses
                                .get(index)
                                .copied()
                                .or_else(|| statuses.last().copied())
                                .unwrap_or(StatusCode::OK);
                            (status, "")
                        }
                    }),
                );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            let guards = vec![
                EnvVarGuard::set("CS_TOKEN_URL", &format!("http://{addr}/oauth/token")),
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models")),
                EnvVarGuard::set(
                    "CS_RESPONSES_URL",
                    &format!("http://{addr}/codex/responses"),
                ),
            ];
            (token_calls, guards)
        }

        fn assert_reports_persist_failure(detail: &str) {
            assert!(
                detail.contains(PERSIST_FAILURE_MARKER),
                "a rotated credential that never reached disk must be reported as a local \
                 write failure, got: {detail}"
            );
            assert!(
                !detail.contains("token refresh failed"),
                "the report must stay distinguishable from the auth server rejecting the \
                 refresh — that one needs a re-login, this one needs the write fixed. \
                 Got: {detail}"
            );
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn warmup_aborts_when_pre_warmup_rotated_tokens_cannot_be_saved() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = tempfile::tempdir().unwrap();
            let _codex_switch_home =
                EnvVarGuard::set("CODEX_SWITCH_HOME", &home.path().display().to_string());

            let alias = "persist-fail-pre-warmup";
            // Keep the (independent) usage-fetch refresh path out of this test.
            crate::cache::put(alias, &crate::usage::UsageInfo::default());
            let profile_path =
                stage_unwritable_profile(home.path(), alias, &expired_access_token());

            let (_token_calls, _guards) = start_rotating_mock_server(vec![StatusCode::OK]).await;

            let result = warmup_account(alias, &profile_path).await;

            let error = result.expect_err(
                "the pre-warmup refresh rotated the credential and the write back failed, so \
                 the warmup must not report success with a token that only exists in memory",
            );
            assert_reports_persist_failure(&format!("{error:#}"));
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn warmup_aborts_when_the_401_retry_rotated_tokens_cannot_be_saved() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = tempfile::tempdir().unwrap();
            let _codex_switch_home =
                EnvVarGuard::set("CODEX_SWITCH_HOME", &home.path().display().to_string());

            let alias = "persist-fail-401-retry";
            crate::cache::put(alias, &crate::usage::UsageInfo::default());
            // Not expiring, so only the 401 handler triggers a refresh.
            let profile_path = stage_unwritable_profile(home.path(), alias, &live_access_token());

            // First warmup POST is unauthorized (drives the refresh), the retry
            // would have succeeded — which is precisely how the failure used to
            // exit zero.
            let (_token_calls, _guards) =
                start_rotating_mock_server(vec![StatusCode::UNAUTHORIZED, StatusCode::OK]).await;

            let result = warmup_account(alias, &profile_path).await;

            let error = result.expect_err(
                "the 401 retry refreshed and rotated the credential; a failed write back must \
                 abort rather than let the retry succeed on an unsaved token",
            );
            assert_reports_persist_failure(&format!("{error:#}"));
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn fetch_models_for_profile_aborts_when_rotated_tokens_cannot_be_saved() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = tempfile::tempdir().unwrap();
            let _codex_switch_home =
                EnvVarGuard::set("CODEX_SWITCH_HOME", &home.path().display().to_string());

            let alias = "persist-fail-fetch-models";
            let profile_path =
                stage_unwritable_profile(home.path(), alias, &expired_access_token());

            let (_token_calls, _guards) = start_rotating_mock_server(vec![StatusCode::OK]).await;

            let result = fetch_models_for_profile(alias, &profile_path).await;

            let error = result.map(|models| format!("{models:?}")).expect_err(
                "degrading to the old token is only correct when the refresh was refused; a \
                 refresh that succeeded and then failed to save must abort instead",
            );
            assert_reports_persist_failure(&format!("{error:#}"));
        }

        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn one_unsaveable_profile_does_not_abort_the_rest_of_the_warmup_batch() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = tempfile::tempdir().unwrap();
            let _codex_switch_home =
                EnvVarGuard::set("CODEX_SWITCH_HOME", &home.path().display().to_string());

            let broken = "batch-persist-broken";
            let healthy = "batch-persist-healthy";
            crate::cache::put(broken, &crate::usage::UsageInfo::default());
            crate::cache::put(healthy, &crate::usage::UsageInfo::default());
            let broken_path =
                stage_unwritable_profile(home.path(), broken, &expired_access_token());
            let healthy_path =
                stage_writable_profile(home.path(), healthy, &expired_access_token());

            let (_token_calls, _guards) = start_rotating_mock_server(vec![StatusCode::OK]).await;

            // Mirrors the batch driver in `commands::misc`: one task per alias,
            // outcomes collected independently.
            let mut tasks = tokio::task::JoinSet::new();
            for (alias, path) in [(broken, broken_path), (healthy, healthy_path)] {
                let alias = alias.to_string();
                tasks.spawn(async move {
                    let result = warmup_account(&alias, &path).await;
                    (alias, result)
                });
            }
            let mut outcomes: HashMap<String, Result<()>> = HashMap::new();
            while let Some(joined) = tasks.join_next().await {
                let (alias, result) = joined.unwrap();
                outcomes.insert(alias, result);
            }

            let broken_error = outcomes
                .remove(broken)
                .expect("the broken profile must produce an outcome")
                .expect_err("the profile whose rotated tokens could not be saved must report");
            assert_reports_persist_failure(&format!("{broken_error:#}"));

            let healthy_result = outcomes
                .remove(healthy)
                .expect("the healthy profile must produce an outcome");
            assert!(
                healthy_result.is_ok(),
                "one account's persist failure must not take down the others in the batch: \
                 {healthy_result:?}"
            );
        }

        // ── /models is resolved once per warmup ──────────────────
        //
        // The model list decides both the main-pool request and the additional
        // -pool ones, so it is one question with one answer. Asking twice costs
        // an upstream round trip per warmup, and the daemon runs warmup on a
        // timer across every profile when `auto_warmup` is on.

        /// Mock server that counts `/codex/models` requests. `/codex/responses`
        /// always succeeds, so nothing but the fetch count is under test.
        async fn start_models_counting_mock_server() -> (Arc<AtomicUsize>, Vec<EnvVarGuard>) {
            let models_calls = Arc::new(AtomicUsize::new(0));
            let counter = models_calls.clone();

            let app = Router::new()
                .route(
                    "/codex/models",
                    get(move || {
                        let counter = counter.clone();
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "models": [{"slug": "gpt-5-mini", "supported_in_api": true}]
                                })),
                            )
                        }
                    }),
                )
                .route("/codex/responses", post(|| async { (StatusCode::OK, "") }));

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            let guards = vec![
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models")),
                EnvVarGuard::set(
                    "CS_RESPONSES_URL",
                    &format!("http://{addr}/codex/responses"),
                ),
            ];
            (models_calls, guards)
        }

        /// The common case: an account with no additional quota pools. The
        /// second fetch's answer was filtered down to nothing and discarded, so
        /// the request bought precisely nothing.
        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn a_warmup_without_additional_pools_fetches_the_model_list_once() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = tempfile::tempdir().unwrap();
            let _codex_switch_home =
                EnvVarGuard::set("CODEX_SWITCH_HOME", &home.path().display().to_string());

            // Unique alias: MODEL_CACHE is process-global and outlives one test.
            let alias = "models-fetch-count-no-pools";
            // Cached usage with no `additional_limits`, so the usage-fetch path
            // stays out of this and there is no additional pool to warm.
            crate::cache::put(alias, &crate::usage::UsageInfo::default());
            let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());

            let (models_calls, _guards) = start_models_counting_mock_server().await;

            warmup_account(alias, &profile_path)
                .await
                .expect("a warmup against a healthy mock server must succeed");

            assert_eq!(
                models_calls.load(Ordering::SeqCst),
                1,
                "the model list answers one question and must be fetched once; a second \
                 /models request with no additional pool to warm is a round trip whose \
                 answer is thrown away"
            );
        }

        /// The same guarantee where the second fetch actually had a consumer:
        /// an additional pool still gets warmed, from the list already in hand.
        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn a_warmup_with_an_additional_pool_still_fetches_the_model_list_once() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = tempfile::tempdir().unwrap();
            let _codex_switch_home =
                EnvVarGuard::set("CODEX_SWITCH_HOME", &home.path().display().to_string());

            let alias = "models-fetch-count-with-pool";
            crate::cache::put(
                alias,
                &crate::usage::UsageInfo {
                    additional_limits: vec![crate::usage::AdditionalRateLimit {
                        limit_name: Some("gpt-5-mini".to_string()),
                        metered_feature: Some("codex_mini".to_string()),
                        allowed: Some(true),
                        limit_reached: Some(false),
                        primary: None,
                        secondary: None,
                    }],
                    ..Default::default()
                },
            );
            let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());

            let (models_calls, _guards) = start_models_counting_mock_server().await;

            warmup_account(alias, &profile_path)
                .await
                .expect("a warmup against a healthy mock server must succeed");

            assert_eq!(
                models_calls.load(Ordering::SeqCst),
                1,
                "warming an additional pool must reuse the list already resolved for the \
                 main pool rather than asking again"
            );
        }

        /// Same mock as above, but it serves two models and also counts the
        /// warmup requests, so a test can tell *which* pools were warmed rather
        /// than only how often the model list was fetched. Two models are the
        /// minimum that distinguishes them: a pool claiming the only model would
        /// leave the main pool no candidate (see `select_warmup_models`), and
        /// both requests would collapse into one.
        async fn start_counting_mock_server()
        -> (Arc<AtomicUsize>, Arc<AtomicUsize>, Vec<EnvVarGuard>) {
            let models_calls = Arc::new(AtomicUsize::new(0));
            let responses_calls = Arc::new(AtomicUsize::new(0));
            let models_counter = models_calls.clone();
            let responses_counter = responses_calls.clone();

            let app = Router::new()
                .route(
                    "/codex/models",
                    get(move || {
                        let counter = models_counter.clone();
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "models": [
                                        {"slug": "gpt-5-mini", "supported_in_api": true},
                                        {"slug": "gpt-5-spark", "supported_in_api": true}
                                    ]
                                })),
                            )
                        }
                    }),
                )
                .route(
                    "/codex/responses",
                    post(move || {
                        let counter = responses_counter.clone();
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            (StatusCode::OK, "")
                        }
                    }),
                );

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            let guards = vec![
                EnvVarGuard::set("CS_MODELS_URL", &format!("http://{addr}/codex/models")),
                EnvVarGuard::set(
                    "CS_RESPONSES_URL",
                    &format!("http://{addr}/codex/responses"),
                ),
            ];
            (models_calls, responses_calls, guards)
        }

        /// The resolved set bakes in the additional pools that existed when it
        /// was cached, so keying the cache on the alias alone freezes it for the
        /// life of the process. The CLI exits between warmups and never notices;
        /// the daemon with `auto_warmup` runs for days, so an account that gains
        /// a model quota pool would keep warming the old set — the new pool's
        /// quota window silently never opens until someone restarts the daemon.
        #[allow(clippy::await_holding_lock)]
        #[tokio::test]
        async fn a_pool_added_after_the_first_warmup_is_still_warmed() {
            let _lock = ENV_LOCK.lock().await;
            let _profile_env_lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = tempfile::tempdir().unwrap();
            let _codex_switch_home =
                EnvVarGuard::set("CODEX_SWITCH_HOME", &home.path().display().to_string());

            let alias = "models-cache-pool-set-changed";
            let profile_path = stage_writable_profile(home.path(), alias, &live_access_token());
            let (models_calls, responses_calls, _guards) = start_counting_mock_server().await;

            // First warmup: the account has no additional quota pool.
            crate::cache::put(alias, &crate::usage::UsageInfo::default());
            warmup_account(alias, &profile_path)
                .await
                .expect("the first warmup against a healthy mock server must succeed");
            assert_eq!(
                responses_calls.load(Ordering::SeqCst),
                1,
                "with no additional pool only the main-pool request is expected"
            );

            // The account gains a model quota pool while the process keeps running.
            crate::cache::put(
                alias,
                &crate::usage::UsageInfo {
                    additional_limits: vec![crate::usage::AdditionalRateLimit {
                        limit_name: Some("gpt-5-spark".to_string()),
                        metered_feature: Some("codex_spark".to_string()),
                        allowed: Some(true),
                        limit_reached: Some(false),
                        primary: None,
                        secondary: None,
                    }],
                    ..Default::default()
                },
            );
            warmup_account(alias, &profile_path)
                .await
                .expect("the second warmup against a healthy mock server must succeed");

            assert_eq!(
                models_calls.load(Ordering::SeqCst),
                2,
                "a changed pool set must miss the cache; reusing the entry resolved for the \
                 old set is what leaves the new pool cold"
            );
            assert_eq!(
                responses_calls.load(Ordering::SeqCst),
                3,
                "the second warmup must open a quota window for the main pool AND the pool \
                 the account just gained"
            );
        }
    }
}
