use std::path::Path;

use anyhow::{Result, bail};
use tracing::{debug, warn};

/// Codex responses endpoint (ChatGPT auth mode).
const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

/// Model used for the warmup request.
/// Update if OpenAI renames the model (check `openai/codex` source for current slug).
pub const WARMUP_MODEL: &str = "gpt-5.2-codex";

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
        && crate::jwt::is_token_expiring(&access_token, 60) == Some(true) {
        debug!("[{alias}] access_token expiring soon, refreshing before warmup");
        match crate::usage::do_refresh_token(alias, &client, rt).await {
            Ok(refreshed) => {
                let _ = crate::auth::update_tokens(
                    profile_path,
                    &refreshed.id_token,
                    &refreshed.access_token,
                    &refreshed.refresh_token,
                );
                // Sync live auth.json if this is the current profile
                if crate::profile::read_current() == alias
                    && let Ok(live) = crate::auth::codex_auth_path() {
                    let _ = crate::auth::update_tokens(
                        &live,
                        &refreshed.id_token,
                        &refreshed.access_token,
                        &refreshed.refresh_token,
                    );
                }
                access_token = refreshed.access_token;
                refresh_token = Some(refreshed.refresh_token);
            }
            Err(e) => warn!("[{alias}] pre-warmup token refresh failed: {e}"),
        }
    }

    // Minimal valid Responses API body — 1-token input, streaming enabled.
    let body = serde_json::json!({
        "model": WARMUP_MODEL,
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
    });

    let mut builder = client
        .post(RESPONSES_URL)
        .bearer_auth(&access_token)
        .header("Content-Type", "application/json");

    if let Some(ref acct_id) = account_id {
        builder = builder.header("ChatGPT-Account-Id", acct_id);
    }

    debug!("[{alias}] warmup POST → {RESPONSES_URL}");

    let mut resp = builder
        .json(&body)
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
        401 | 403 => {
            // Retry once with refreshed token
            if let Some(ref rt) = refresh_token {
                debug!("[{alias}] got {status}, attempting token refresh and retry");
                match crate::usage::do_refresh_token(alias, &client, rt).await {
                    Ok(refreshed) => {
                        let _ = crate::auth::update_tokens(
                            profile_path,
                            &refreshed.id_token,
                            &refreshed.access_token,
                            &refreshed.refresh_token,
                        );
                        if crate::profile::read_current() == alias
                            && let Ok(live) = crate::auth::codex_auth_path() {
                            let _ = crate::auth::update_tokens(
                                &live,
                                &refreshed.id_token,
                                &refreshed.access_token,
                                &refreshed.refresh_token,
                            );
                        }
                        let mut retry_builder = client
                            .post(RESPONSES_URL)
                            .bearer_auth(&refreshed.access_token)
                            .header("Content-Type", "application/json");
                        if let Some(ref acct_id) = account_id {
                            retry_builder = retry_builder.header("ChatGPT-Account-Id", acct_id);
                        }
                        let mut retry_resp = retry_builder
                            .json(&body)
                            .send()
                            .await
                            .map_err(|e| crate::auth::format_reqwest_error("warmup retry failed", &e))?;
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
