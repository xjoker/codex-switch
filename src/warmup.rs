use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Result, bail};
use tracing::{debug, warn};

const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
const FALLBACK_MODEL: &str = "gpt-5.3-codex";

static CODEX_VERSION: OnceLock<String> = OnceLock::new();
static MODEL_CACHE: Mutex<Option<String>> = Mutex::new(None);

fn detect_codex_version() -> &'static str {
    CODEX_VERSION.get_or_init(|| {
        std::process::Command::new("codex")
            .arg("--version")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.split_whitespace().last().map(|v| v.trim().to_string()))
            .unwrap_or_else(|| "0.1.0".to_string())
    })
}

async fn fetch_warmup_model(client: &reqwest::Client, access_token: &str) -> Result<String> {
    let version = detect_codex_version();
    let resp = client
        .get(MODELS_URL)
        .query(&[("client_version", version)])
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| crate::auth::format_reqwest_error("models fetch failed", &e))?;

    if !resp.status().is_success() {
        bail!("models endpoint returned {}", resp.status());
    }

    let body: serde_json::Value = resp.json().await?;
    let models = body["models"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("no models array in response"))?;

    let visible: Vec<&serde_json::Value> = models
        .iter()
        .filter(|m| m["visibility"].as_str() != Some("hide"))
        .collect();

    if visible.is_empty() {
        bail!("no visible models available");
    }

    // Prefer mini (lightest), fall back to highest priority (lowest number)
    let selected = visible
        .iter()
        .find(|m| m["slug"].as_str().is_some_and(|s| s.contains("mini")))
        .or_else(|| {
            visible
                .iter()
                .min_by_key(|m| m["priority"].as_i64().unwrap_or(i64::MAX))
        })
        .and_then(|m| m["slug"].as_str())
        .unwrap_or(FALLBACK_MODEL);

    debug!("warmup: model selected from API: {selected}");
    Ok(selected.to_string())
}

async fn resolve_model(client: &reqwest::Client, access_token: &str) -> String {
    if let Some(model) = MODEL_CACHE.lock().unwrap().clone() {
        return model;
    }
    match fetch_warmup_model(client, access_token).await {
        Ok(model) => {
            *MODEL_CACHE.lock().unwrap() = Some(model.clone());
            model
        }
        Err(e) => {
            warn!("failed to fetch warmup model list, using fallback: {e}");
            FALLBACK_MODEL.to_string()
        }
    }
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
    account_id: &Option<String>,
    body: &serde_json::Value,
) -> reqwest::RequestBuilder {
    let mut builder = client
        .post(RESPONSES_URL)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json");
    if let Some(acct_id) = account_id {
        builder = builder.header("ChatGPT-Account-Id", acct_id);
    }
    builder.json(body)
}

/// Send a minimal completion request to trigger the quota window countdown for a profile.
///
/// The 5-hour and 7-day windows only start after the first real API call.
/// This sends the lightest valid request ("ping") and discards the response body,
/// which is enough for the server to stamp the window start time.
pub async fn warmup_account(alias: &str, profile_path: &Path) -> Result<()> {
    let val = crate::auth::read_auth(profile_path)
        .map_err(|e| anyhow::anyhow!("{alias}: cannot read auth: {e}"))?;

    let (at, rt) = crate::auth::extract_tokens(&val);
    let mut access_token = at
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{alias}: no access_token in profile"))?;
    let mut refresh_token = rt.filter(|s| !s.is_empty());

    let info = crate::auth::read_account_info(profile_path);
    let account_id = info.account_id;

    let client = crate::auth::build_http_client()?;

    // Pre-refresh: if token is about to expire, refresh proactively
    if let Some(ref rt) = refresh_token
        && crate::jwt::is_token_expiring(&access_token, 60) == Some(true)
    {
        debug!("[{alias}] access_token expiring soon, refreshing before warmup");
        match crate::usage::do_refresh_token(alias, &client, rt).await {
            Ok(refreshed) => {
                if let Err(e) = crate::auth::update_tokens(
                    profile_path,
                    &refreshed.id_token,
                    &refreshed.access_token,
                    &refreshed.refresh_token,
                ) {
                    warn!("[{alias}] failed to persist refreshed tokens: {e}");
                }
                if crate::profile::read_current() == alias
                    && let Ok(live) = crate::auth::codex_auth_path()
                    && let Err(e) = crate::auth::update_tokens(
                        &live,
                        &refreshed.id_token,
                        &refreshed.access_token,
                        &refreshed.refresh_token,
                    )
                {
                    warn!("[{alias}] failed to persist refreshed tokens to live auth: {e}");
                }
                access_token = refreshed.access_token;
                refresh_token = Some(refreshed.refresh_token);
            }
            Err(e) => warn!("[{alias}] pre-warmup token refresh failed: {e}"),
        }
    }

    let model = resolve_model(&client, &access_token).await;
    let body = build_body(&model);

    debug!("[{alias}] warmup POST → {RESPONSES_URL} (model={model})");

    let mut resp = make_request(&client, &access_token, &account_id, &body)
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
            Ok(())
        }
        400 => {
            let text = resp.text().await.unwrap_or_default();
            if text.contains("not supported") {
                // Model deprecated — clear cache, fetch fresh model list, retry once
                debug!(
                    "[{alias}] model {model:?} not supported, refreshing model cache and retrying"
                );
                *MODEL_CACHE.lock().unwrap() = None;
                let new_model = resolve_model(&client, &access_token).await;
                let retry_body = build_body(&new_model);
                let mut retry_resp =
                    make_request(&client, &access_token, &account_id, &retry_body)
                        .send()
                        .await
                        .map_err(|e| crate::auth::format_reqwest_error("warmup retry failed", &e))?;
                let retry_status = retry_resp.status();
                if retry_status.is_success() {
                    let _ = retry_resp.chunk().await;
                    return Ok(());
                }
                let retry_text = retry_resp.text().await.unwrap_or_default();
                let snippet: String = retry_text.chars().take(160).collect();
                bail!("{alias}: HTTP {retry_status} after model refresh — {snippet}")
            }
            let snippet: String = text.chars().take(160).collect();
            bail!("{alias}: HTTP 400 — {snippet}")
        }
        401 | 403 => {
            // Retry once with refreshed token
            if let Some(ref rt) = refresh_token {
                debug!("[{alias}] got {status}, attempting token refresh and retry");
                match crate::usage::do_refresh_token(alias, &client, rt).await {
                    Ok(refreshed) => {
                        if let Err(e) = crate::auth::update_tokens(
                            profile_path,
                            &refreshed.id_token,
                            &refreshed.access_token,
                            &refreshed.refresh_token,
                        ) {
                            warn!("[{alias}] failed to persist refreshed tokens: {e}");
                        }
                        if crate::profile::read_current() == alias
                            && let Ok(live) = crate::auth::codex_auth_path()
                            && let Err(e) = crate::auth::update_tokens(
                                &live,
                                &refreshed.id_token,
                                &refreshed.access_token,
                                &refreshed.refresh_token,
                            )
                        {
                            warn!("[{alias}] failed to persist refreshed tokens to live auth: {e}");
                        }
                        let mut retry_resp =
                            make_request(&client, &refreshed.access_token, &account_id, &body)
                                .send()
                                .await
                                .map_err(|e| {
                                    crate::auth::format_reqwest_error("warmup retry failed", &e)
                                })?;
                        let retry_status = retry_resp.status();
                        if retry_status.is_success() {
                            let _ = retry_resp.chunk().await;
                            return Ok(());
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
        code => {
            let text = resp.text().await.unwrap_or_default();
            let snippet: String = text.chars().take(160).collect();
            bail!("{alias}: HTTP {code} — {snippet}")
        }
    }
}
