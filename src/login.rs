/// Codex OAuth login flows
///
/// Two supported flows:
/// - PKCE Authorization Code Flow — browser-based, local HTTP callback on port 1455 or 1457
/// - Device Code Flow (`--device`) — for headless servers without a browser
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::Rng;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Write;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{debug, info};

use crate::auth::{CLIENT_ID, ISSUER};
use crate::http_retry::{self, ReplaySafety};
use crate::output::user_println;

const ORIGINATOR: &str = "codex_cli_rs";
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";
const CALLBACK_TIMEOUT_SECS: u64 = 600;
const CALLBACK_CONNECTION_TIMEOUT_SECS: u64 = 5;
const MAX_CONCURRENT_CALLBACK_CONNECTIONS: usize = 16;
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_FALLBACK_PORT: u16 = 1457;
const CALLBACK_HOST: &str = "127.0.0.1";
/// OAuth redirect_uri must use "localhost" to match OpenAI's registered URI.
const REDIRECT_HOST: &str = "localhost";

// ── Types ─────────────────────────────────────────────────

#[derive(Debug)]
pub struct LoginTokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    /// API key from the post-login token exchange (browser flow only,
    /// best-effort — Codex persists it as OPENAI_API_KEY).
    pub api_key: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("Cancelled by user.")]
pub(crate) struct LoginCancelled;

pub(crate) fn is_login_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<LoginCancelled>().is_some()
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    error: Option<TokenErrorField>,
    error_description: Option<String>,
}

/// `error` on OpenAI's `/oauth/token` responses is either the OAuth-standard string
/// (`{"error":"invalid_grant"}`, paired with a top-level `error_description`) or, on some
/// 401s, a nested object (`{"error":{"code":...,"message":...}}`). Both must deserialize,
/// or an object-shaped error breaks parsing of the whole response.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TokenErrorField {
    Code(String),
    Detail {
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        message: Option<String>,
    },
}

impl TokenErrorField {
    /// Returns `(code, detail_message)` regardless of which shape the server used.
    fn describe(&self, top_level_description: Option<&str>) -> (String, String) {
        match self {
            TokenErrorField::Code(code) => (
                code.clone(),
                top_level_description.unwrap_or_default().to_string(),
            ),
            TokenErrorField::Detail { code, message } => (
                code.clone().unwrap_or_default(),
                message.clone().unwrap_or_default(),
            ),
        }
    }
}

// ── PKCE helpers ──────────────────────────────────────────

struct Pkce {
    code_verifier: String,
    code_challenge: String,
}

fn generate_pkce() -> Pkce {
    let mut bytes = [0u8; 64];
    rand::rng().fill_bytes(&mut bytes);
    let code_verifier = URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = URL_SAFE_NO_PAD.encode(digest);
    Pkce {
        code_verifier,
        code_challenge,
    }
}

fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn redirect_uri(port: u16) -> String {
    format!("http://{REDIRECT_HOST}:{port}/auth/callback")
}

fn redacted_device_poll_log_body(body: &serde_json::Value) -> String {
    crate::auth::redact_sensitive_log_body(body)
}

// ── Main flow ─────────────────────────────────────────────

/// Run PKCE OAuth flow: open browser → wait for callback → exchange tokens
pub async fn run_device_auth() -> Result<LoginTokens> {
    crate::auth::ensure_file_credentials_store()?;
    let pkce = generate_pkce();
    let state = generate_state();

    // Try port 1455 first (OpenAI's registered redirect URI), with fallbacks for
    // Windows environments where that port may be blocked.
    let (listener, actual_port) = bind_callback_listener().await?;
    let actual_redirect = redirect_uri(actual_port);
    let forced_workspace_ids = crate::auth::configured_forced_workspace_ids();
    let authorize_url = build_authorize_url(
        &pkce.code_challenge,
        &state,
        &actual_redirect,
        &forced_workspace_ids,
    );

    user_println("");
    user_println("Opening browser for Codex login...");
    user_println("If the browser does not open, visit:");
    user_println(&authorize_url);
    user_println("");
    user_println(&format!(
        "Waiting for authorization callback ({CALLBACK_TIMEOUT_SECS}s timeout)..."
    ));

    open_browser(&authorize_url);

    let callback_result: CallbackResult = tokio::select! {
        result = tokio::time::timeout(
            Duration::from_secs(CALLBACK_TIMEOUT_SECS),
            wait_for_callback(listener, &state),
        ) => {
            result.map_err(|_| anyhow::anyhow!("Login timed out ({CALLBACK_TIMEOUT_SECS}s). Please try again."))??
        }
        _ = tokio::signal::ctrl_c() => {
            user_println("");
            return Err(LoginCancelled.into());
        }
    };

    info!(
        "OAuth callback received, code length={}",
        callback_result.code.len()
    );

    let mut tokens =
        exchange_code(&callback_result.code, &pkce.code_verifier, &actual_redirect).await?;
    crate::auth::validate_managed_chatgpt_account(&tokens.id_token)?;
    // Best-effort API key exchange, same as Codex's browser login. Failure
    // leaves OPENAI_API_KEY null, which Codex accepts.
    if let Ok(client) = crate::auth::build_http_client() {
        tokens.api_key = obtain_api_key(&client, &tokens.id_token).await;
    }
    Ok(tokens)
}

/// Exchange the id_token for an API key (`OPENAI_API_KEY`), mirroring
/// Codex's post-login token exchange.
async fn obtain_api_key(client: &reqwest::Client, id_token: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct ExchangeResp {
        access_token: String,
    }

    let token_url = format!("{ISSUER}/oauth/token");
    let resp = match http_retry::send(
        build_api_key_exchange_request(client, &token_url, id_token),
        ReplaySafety::UnsafePost,
    )
    .await
    {
        Ok(resp) => resp,
        Err(e) => {
            debug!("API key exchange request failed (continuing without): {e}");
            return None;
        }
    };
    if !resp.status.is_success() {
        debug!(
            "API key exchange returned HTTP {} (continuing without)",
            resp.status
        );
        return None;
    }
    match serde_json::from_slice::<ExchangeResp>(&resp.body) {
        Ok(body) => Some(body.access_token),
        Err(e) => {
            debug!("API key exchange parse failed (continuing without): {e}");
            None
        }
    }
}

/// Body shape must match Codex 0.144.1's `obtain_api_key` token exchange.
fn build_api_key_exchange_request(
    client: &reqwest::Client,
    token_url: &str,
    id_token: &str,
) -> reqwest::RequestBuilder {
    client
        .post(token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type={}&client_id={}&requested_token={}&subject_token={}&subject_token_type={}",
            urlencoding::encode("urn:ietf:params:oauth:grant-type:token-exchange"),
            urlencoding::encode(CLIENT_ID),
            urlencoding::encode("openai-api-key"),
            urlencoding::encode(id_token),
            urlencoding::encode("urn:ietf:params:oauth:token-type:id_token")
        ))
}

// ── Authorization URL ─────────────────────────────────────

fn build_authorize_url(
    code_challenge: &str,
    state: &str,
    redirect_uri: &str,
    forced_workspace_ids: &[String],
) -> String {
    let mut params = vec![
        ("response_type", "code".to_string()),
        ("client_id", CLIENT_ID.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("scope", SCOPE.to_string()),
        ("code_challenge", code_challenge.to_string()),
        ("code_challenge_method", "S256".to_string()),
        ("id_token_add_organizations", "true".to_string()),
        ("codex_cli_simplified_flow", "true".to_string()),
        ("state", state.to_string()),
        ("originator", ORIGINATOR.to_string()),
    ];
    // Codex pre-restricts the workspace picker on the consent page when the
    // managed config forces workspaces.
    if !forced_workspace_ids.is_empty() {
        params.push(("allowed_workspace_id", forced_workspace_ids.join(",")));
    }

    let qs = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    format!("{ISSUER}/oauth/authorize?{qs}")
}

// ── Local callback server ─────────────────────────────────

struct CallbackResult {
    code: String,
}

async fn write_callback_response(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

async fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<CallbackResult> {
    let expected_state: std::sync::Arc<str> = expected_state.into();
    let mut connections = tokio::task::JoinSet::new();
    loop {
        if connections.len() >= MAX_CONCURRENT_CALLBACK_CONNECTIONS {
            let completed = connections
                .join_next()
                .await
                .expect("a full callback task set cannot be empty");
            if let Some(callback) = completed.context("OAuth callback connection task failed")?? {
                return Ok(callback);
            }
            continue;
        }

        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accepting OAuth callback connection")?;
                let expected_state = expected_state.clone();
                connections.spawn(async move {
                    handle_callback_connection(stream, expected_state.as_ref()).await
                });
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                let completed = completed.expect("a non-empty callback task set must yield a task");
                if let Some(callback) =
                    completed.context("OAuth callback connection task failed")??
                {
                    return Ok(callback);
                }
            }
        }
    }
}

async fn handle_callback_connection(
    mut stream: tokio::net::TcpStream,
    expected_state: &str,
) -> Result<Option<CallbackResult>> {
    // Read until we have the full first line (may arrive in multiple reads on Windows).
    let mut buf = vec![0u8; 8192];
    let mut total = 0;
    let request_complete = tokio::time::timeout(
        Duration::from_secs(CALLBACK_CONNECTION_TIMEOUT_SECS),
        async {
            loop {
                let n = match stream.read(&mut buf[total..]).await {
                    Ok(n) => n,
                    Err(_) => break false,
                };
                if n == 0 {
                    break false;
                }
                total += n;
                if buf[..total].windows(4).any(|w| w == b"\r\n\r\n")
                    || buf[..total].windows(2).any(|w| w == b"\n\n")
                {
                    break true;
                }
                if total >= buf.len() {
                    break false;
                }
            }
        },
    )
    .await;
    let (request_complete, status, body) = match request_complete {
        Ok(true) => (true, "", ""),
        Ok(false) => (false, "400 Bad Request", "Invalid callback request"),
        Err(_) => (false, "408 Request Timeout", "Callback request timed out"),
    };
    if !request_complete {
        write_callback_response(&mut stream, status, body).await;
        return Ok(None);
    }

    let request = String::from_utf8_lossy(&buf[..total]);
    let first_line = request.lines().next().unwrap_or("");
    let mut request_parts = first_line.split_whitespace();
    let method = request_parts.next().unwrap_or("");
    let target = request_parts.next().unwrap_or("");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    if method != "GET" || path != "/auth/callback" {
        write_callback_response(&mut stream, "404 Not Found", "Not found").await;
        return Ok(None);
    }

    let mut code = None;
    let mut returned_state = None;
    let mut error = None;

    for part in query.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            let decoded = urlencoding::decode(v).unwrap_or_default().into_owned();
            match k {
                "code" => code = Some(decoded),
                "state" => returned_state = Some(decoded),
                "error" => error = Some(decoded),
                _ => {}
            }
        }
    }

    if returned_state.as_deref() != Some(expected_state) {
        write_callback_response(&mut stream, "403 Forbidden", "Invalid callback state").await;
        return Ok(None);
    }

    if let Some(e) = error {
        write_callback_response(&mut stream, "400 Bad Request", "Authorization failed").await;
        bail!("Authorization failed: {e}");
    }

    let Some(code) = code.filter(|code| !code.is_empty()) else {
        write_callback_response(
            &mut stream,
            "400 Bad Request",
            "Callback did not include an authorization code",
        )
        .await;
        return Ok(None);
    };

    let html = r#"<!DOCTYPE html><html><body style="font-family:sans-serif;text-align:center;padding:60px">
<h2>✓ Login successful</h2><p>You may close this tab and return to the terminal.</p>
</body></html>"#;
    write_callback_response(&mut stream, "200 OK", html).await;
    Ok(Some(CallbackResult { code }))
}

// ── Token exchange ────────────────────────────────────────

async fn exchange_code(code: &str, code_verifier: &str, redirect_uri: &str) -> Result<LoginTokens> {
    exchange_code_with_redirect(code, code_verifier, redirect_uri).await
}

/// Transport-level failures (connect/timeout) mean the request never reached the server, so
/// nothing about the one-shot authorization code has been evaluated yet and a retry is safe.
/// HTTP 4xx (bad/reused code, PKCE mismatch, etc.) is the server's deterministic verdict on
/// that code — retrying it can't succeed and only burns the timeout the user has to redo the
/// browser login. 5xx is treated like a transport hiccup since the server didn't reach a verdict.
const TOKEN_EXCHANGE_MAX_ATTEMPTS: u32 = 3;

/// True when no HTTP response came back at all. reqwest reports both a refused
/// connection and a TLS handshake that died mid-negotiation as `is_request()`
/// rather than `is_connect()`, so keying on the latter alone never retries the
/// very failures this exists for. Without a response the server never ruled on
/// the code, and the worst case of retrying is a definitive `invalid_grant`.
fn is_retryable_transport_error(err: &reqwest::Error) -> bool {
    err.is_connect() || err.is_timeout() || err.is_request()
}

fn token_exchange_retry_backoff(attempt: u32) -> Duration {
    Duration::from_millis(150 * u64::from(attempt))
}

async fn exchange_code_with_redirect(
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<LoginTokens> {
    let client = crate::auth::build_http_client()?;
    let token_url = crate::auth::token_url();

    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        urlencoding::encode(code),
        urlencoding::encode(redirect_uri),
        urlencoding::encode(CLIENT_ID),
        urlencoding::encode(code_verifier),
    );

    debug!("Token exchange: POST {token_url} redirect_uri={redirect_uri}");

    let mut attempt: u32 = 0;
    let resp = loop {
        attempt += 1;
        match client
            .post(&token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body.clone())
            .send()
            .await
        {
            Ok(resp) => {
                http_retry::wait_after_429_headers(resp.status(), resp.headers(), attempt - 1)
                    .await;
                if resp.status().is_server_error() && attempt < TOKEN_EXCHANGE_MAX_ATTEMPTS {
                    debug!(
                        "Token exchange got HTTP {} (attempt {attempt}/{TOKEN_EXCHANGE_MAX_ATTEMPTS}), retrying",
                        resp.status()
                    );
                    tokio::time::sleep(token_exchange_retry_backoff(attempt)).await;
                    continue;
                }
                break resp;
            }
            Err(e) if is_retryable_transport_error(&e) && attempt < TOKEN_EXCHANGE_MAX_ATTEMPTS => {
                debug!(
                    "Token exchange transport error (attempt {attempt}/{TOKEN_EXCHANGE_MAX_ATTEMPTS}): {e}"
                );
                tokio::time::sleep(token_exchange_retry_backoff(attempt)).await;
            }
            Err(e) => {
                return Err(crate::auth::format_reqwest_error(
                    "Token exchange request failed",
                    &e,
                ));
            }
        }
    };

    let status = resp.status();
    debug!("Token exchange: HTTP {status}");
    let token_resp: TokenResponse = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to parse token response (HTTP {status}): {e}"))?;

    if let Some(err) = token_resp.error {
        let (code, detail) = err.describe(token_resp.error_description.as_deref());
        bail!("Token exchange failed (HTTP {status}): {code} -- {detail}");
    }

    match (
        token_resp.id_token,
        token_resp.access_token,
        token_resp.refresh_token,
    ) {
        (Some(id), Some(access), Some(refresh)) => {
            info!("Token exchange succeeded");
            Ok(LoginTokens {
                id_token: id,
                access_token: access,
                refresh_token: refresh,
                api_key: None,
            })
        }
        _ => bail!("Token response missing required fields (HTTP {status})"),
    }
}

// ── Device Code Flow (RFC 8628) ──────────────────────────

const DEVICE_USERCODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const DEVICE_POLL_INTERVAL_SECS: u64 = 5;

#[derive(Default)]
struct DevicePollFailureTracker {
    consecutive: u32,
}

impl DevicePollFailureTracker {
    fn record(&mut self) -> u32 {
        self.consecutive = self.consecutive.saturating_add(1);
        self.consecutive
    }

    fn reset(&mut self) {
        self.consecutive = 0;
    }
}
const DEVICE_TIMEOUT_SECS: u64 = 900; // 15 minutes

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: Option<String>,
    user_code: Option<String>,
    #[serde(default)]
    interval: Option<String>,
    error: Option<serde_json::Value>,
}

/// Device token poll response — returns an authorization_code, NOT tokens directly.
/// We then exchange the code for actual tokens via /oauth/token.
#[derive(Debug, Deserialize)]
struct DeviceTokenResponse {
    authorization_code: String,
    code_challenge: String,
    code_verifier: String,
}

fn parse_device_poll_success(body: serde_json::Value) -> Result<(String, String)> {
    let response: DeviceTokenResponse = serde_json::from_value(body)
        .map_err(|e| anyhow::anyhow!("Invalid device authorization response: {e}"))?;

    if response.authorization_code.trim().is_empty() {
        bail!("Invalid device authorization response: authorization_code is empty");
    }
    if response.code_challenge.trim().is_empty() {
        bail!("Invalid device authorization response: code_challenge is empty");
    }
    if response.code_verifier.trim().is_empty() {
        bail!("Invalid device authorization response: code_verifier is empty");
    }

    Ok((response.authorization_code, response.code_verifier))
}

#[derive(Debug, PartialEq, Eq)]
enum DevicePollErrorAction {
    Continue,
    SlowDown,
    Expired,
    AccessDenied,
    RetryUnknown { code: String, message: String },
}

fn device_poll_error_action(body: &serde_json::Value) -> Option<DevicePollErrorAction> {
    let err = body.get("error")?;

    let (code, message) = if let Some(code) = err.as_str() {
        let message = body
            .get("error_description")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        (code, message)
    } else {
        let code = err.get("code").and_then(|c| c.as_str()).unwrap_or("");
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .or_else(|| err.get("error_description").and_then(|d| d.as_str()))
            .unwrap_or("");
        (code, message)
    };

    Some(match code {
        "deviceauth_authorization_unknown" | "authorization_pending" => {
            DevicePollErrorAction::Continue
        }
        "slow_down" => DevicePollErrorAction::SlowDown,
        "expired_token" | "deviceauth_expired" => DevicePollErrorAction::Expired,
        "access_denied" => DevicePollErrorAction::AccessDenied,
        _ => DevicePollErrorAction::RetryUnknown {
            code: code.to_string(),
            message: message.to_string(),
        },
    })
}

fn is_device_poll_pending_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::NOT_FOUND
}

fn is_device_poll_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn device_poll_next_wake(
    now: tokio::time::Instant,
    deadline: tokio::time::Instant,
    interval_secs: u64,
) -> tokio::time::Instant {
    (now + Duration::from_secs(interval_secs)).min(deadline)
}

/// Run Device Code Flow: request code → display to user → poll for token
pub async fn run_device_code_auth() -> Result<LoginTokens> {
    crate::auth::ensure_file_credentials_store()?;
    let client = crate::auth::build_http_client()?;

    // Step 1: Request device code
    user_println("  Requesting device code...");
    let _ = std::io::stdout().flush();
    info!("Requesting device code from {DEVICE_USERCODE_URL}");
    let resp = client
        .post(DEVICE_USERCODE_URL)
        .json(&serde_json::json!({
            "client_id": CLIENT_ID,
            "scope": SCOPE,
            "originator": ORIGINATOR,
        }))
        .send()
        .await
        .map_err(|e| crate::auth::format_reqwest_error("Failed to request device code", &e))?;

    let status = resp.status();
    http_retry::wait_after_429_headers(status, resp.headers(), 0).await;
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("Device code request failed (HTTP {status}): {body}");
    }

    let dc: DeviceCodeResponse = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to parse device code response: {e}"))?;

    if let Some(e) = dc.error {
        bail!("Device code error: {e}");
    }

    let device_auth_id = dc
        .device_auth_id
        .ok_or_else(|| anyhow::anyhow!("No device_auth_id in response"))?;
    let user_code = dc
        .user_code
        .ok_or_else(|| anyhow::anyhow!("No user_code in response"))?;
    let mut interval_secs: u64 = dc
        .interval
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEVICE_POLL_INTERVAL_SECS);
    let timeout = DEVICE_TIMEOUT_SECS;

    // Step 2: Display instructions
    user_println("");
    user_println(&format!("  To sign in, visit:  {DEVICE_VERIFICATION_URI}"));
    user_println(&format!("  Enter code:         {user_code}"));
    user_println("");
    user_println(&format!(
        "  Waiting for authorization (polling every {interval_secs}s)..."
    ));
    let _ = std::io::stdout().flush();

    // Step 3: Poll for token (Ctrl+C safe)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
    let mut poll_count = 0u32;
    let mut consecutive_429 = 0u32;
    let mut poll_failures = DevicePollFailureTracker::default();

    loop {
        let next_wake = device_poll_next_wake(tokio::time::Instant::now(), deadline, interval_secs);
        tokio::select! {
            _ = tokio::time::sleep_until(next_wake) => {}
            _ = tokio::signal::ctrl_c() => {
                user_println("");
                return Err(LoginCancelled.into());
            }
        }

        if tokio::time::Instant::now() >= deadline {
            bail!("Device authorization timed out. Please try again.");
        }

        poll_count += 1;
        eprint!("\r  Polling... ({poll_count})    ");

        let poll_request = client
            .post(DEVICE_TOKEN_URL)
            .json(&serde_json::json!({
                "device_auth_id": device_auth_id,
                "user_code": user_code,
                "client_id": CLIENT_ID,
            }))
            .send();
        let poll_result = tokio::select! {
            result = tokio::time::timeout_at(deadline, poll_request) => result,
            _ = tokio::signal::ctrl_c() => {
                user_println("");
                return Err(LoginCancelled.into());
            }
        };
        let poll_resp = match poll_result {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => {
                info!("Device poll network error (retrying): {e}");
                let detail = format!("network error: {e}");
                let failure_count = poll_failures.record();
                eprintln!(
                    "\n  Device polling failed ({failure_count}); retrying until the 15-minute timeout: {detail}"
                );
                continue;
            }
            Err(_) => bail!("Device authorization timed out. Please try again."),
        };

        let poll_status = poll_resp.status();
        let poll_headers = poll_resp.headers().clone();
        let body_result = tokio::select! {
            result = tokio::time::timeout_at(deadline, poll_resp.json()) => result,
            _ = tokio::signal::ctrl_c() => {
                user_println("");
                return Err(LoginCancelled.into());
            }
        };
        let body: serde_json::Value = match body_result {
            Ok(Ok(body)) => body,
            Ok(Err(_)) if is_device_poll_pending_status(poll_status) => {
                poll_failures.reset();
                continue;
            }
            Ok(Err(e))
                if poll_status.is_success() || is_device_poll_retryable_status(poll_status) =>
            {
                info!("Device poll parse error (retrying): {e}");
                let detail = format!("invalid response: {e}");
                let failure_count = poll_failures.record();
                eprintln!(
                    "\n  Device polling failed ({failure_count}); retrying until the 15-minute timeout: {detail}"
                );
                continue;
            }
            Ok(Err(e)) => {
                bail!("Device authorization failed (HTTP {poll_status}): invalid response: {e}")
            }
            Err(_) => bail!("Device authorization timed out. Please try again."),
        };
        let log_body = redacted_device_poll_log_body(&body);
        debug!("Device poll response: {log_body}");

        if poll_status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let encoded = serde_json::to_vec(&body).unwrap_or_default();
            let decision =
                http_retry::rate_limit_decision(&poll_headers, &encoded, consecutive_429);
            consecutive_429 = consecutive_429.saturating_add(1);
            let wake = (tokio::time::Instant::now() + decision.delay).min(deadline);
            debug!(
                "Device poll HTTP 429; waiting {:.3}s ({:?})",
                decision.delay.as_secs_f64(),
                decision.source
            );
            tokio::time::sleep_until(wake).await;
            continue;
        }
        consecutive_429 = 0;

        if let Some(action) = device_poll_error_action(&body) {
            match action {
                DevicePollErrorAction::Continue => {
                    poll_failures.reset();
                    continue;
                }
                DevicePollErrorAction::SlowDown => {
                    poll_failures.reset();
                    interval_secs = interval_secs.saturating_add(5);
                    continue;
                }
                DevicePollErrorAction::Expired => {
                    user_println("");
                    bail!("Device code expired. Please try again.");
                }
                DevicePollErrorAction::AccessDenied => {
                    user_println("");
                    bail!("Authorization was denied by the user.");
                }
                DevicePollErrorAction::RetryUnknown { code, message } => {
                    let detail = format!("unrecognized server error '{code}': {message}");
                    if !poll_status.is_success()
                        && !is_device_poll_pending_status(poll_status)
                        && !is_device_poll_retryable_status(poll_status)
                    {
                        bail!("Device authorization failed (HTTP {poll_status}): {detail}");
                    }
                    let failure_count = poll_failures.record();
                    eprintln!(
                        "\n  Device polling failed ({failure_count}); retrying until the 15-minute timeout: {detail}"
                    );
                    continue;
                }
            }
        }

        if is_device_poll_pending_status(poll_status) {
            poll_failures.reset();
            continue;
        }

        if !poll_status.is_success() {
            if is_device_poll_retryable_status(poll_status) {
                let failure_count = poll_failures.record();
                eprintln!(
                    "\n  Device polling failed ({failure_count}); retrying until the 15-minute timeout: HTTP {poll_status}"
                );
                continue;
            }
            bail!("Device authorization failed (HTTP {poll_status})");
        }

        // Success — got authorization_code, need to exchange for tokens.
        let (auth_code, verifier) = match parse_device_poll_success(body) {
            Ok(success) => success,
            Err(error) => {
                let detail = error.to_string();
                let failure_count = poll_failures.record();
                eprintln!(
                    "\n  Device polling failed ({failure_count}); retrying until the 15-minute timeout: {detail}"
                );
                continue;
            }
        };
        poll_failures.reset();

        eprint!("\r                          \r");
        info!("Device authorization successful, exchanging code for tokens");
        user_println("  Authorization successful, exchanging tokens...");

        // Use the standard /oauth/token endpoint with the returned code + verifier
        // The redirect_uri for device flow is the OpenAI deviceauth callback
        let device_redirect = format!("{ISSUER}/deviceauth/callback");
        let tokens = exchange_code_with_redirect(&auth_code, &verifier, &device_redirect).await?;
        crate::auth::validate_managed_chatgpt_account(&tokens.id_token)?;
        return Ok(tokens);
    }
}

// ── Build auth.json ───────────────────────────────────────

/// Build (auth.json value, parsed AccountInfo) from a fresh token set.
/// Used by both the CLI `login` flow and the TUI re-login / add flows.
pub fn build_auth_from_tokens(
    tokens: &LoginTokens,
) -> (serde_json::Value, crate::jwt::AccountInfo) {
    let temp = serde_json::json!({
        "tokens": {
            "id_token": tokens.id_token,
            "access_token": tokens.access_token,
            "refresh_token": tokens.refresh_token,
            "account_id": ""
        }
    });
    let info = crate::jwt::parse_account_info(&temp);
    let account_id = info.account_id.as_deref().unwrap_or("").to_string();
    (build_auth_json(tokens, &account_id), info)
}

pub fn build_auth_json(tokens: &LoginTokens, account_id: &str) -> serde_json::Value {
    use crate::output::format_iso8601;
    let ts = crate::auth::now_unix_secs();

    // Same shape Codex 0.144.1 writes on a ChatGPT login: auth_mode is
    // persisted and an unknown account_id is null rather than "".
    let account_id_value = if account_id.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(account_id.to_string())
    };
    serde_json::json!({
        "OPENAI_API_KEY": tokens.api_key,
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": tokens.id_token,
            "access_token": tokens.access_token,
            "refresh_token": tokens.refresh_token,
            "account_id": account_id_value
        },
        "last_refresh": format_iso8601(ts)
    })
}

// ── Browser open ──────────────────────────────────────────

// ── Callback listener ─────────────────────────────────────

/// Bind the PKCE callback listener and return `(listener, actual_port)`.
///
/// Preference order:
/// 1. `127.0.0.1:1455` — the port OpenAI registers as the primary redirect URI.
/// 2. `127.0.0.1:1457` — Codex's registered fallback redirect port.
/// 3. Error with remediation hint.
async fn bind_callback_listener() -> Result<(TcpListener, u16)> {
    let [primary_port, _] = callback_ports();
    match TcpListener::bind(format!("{CALLBACK_HOST}:{primary_port}")).await {
        Ok(l) => Ok((l, primary_port)),
        Err(e) => {
            debug!("IPv4 bind on {primary_port} failed: {e}");
            bind_callback_listener_fallback(e).await
        }
    }
}

fn callback_ports() -> [u16; 2] {
    [CALLBACK_PORT, CALLBACK_FALLBACK_PORT]
}

#[cfg(target_os = "windows")]
async fn bind_callback_listener_fallback(ipv4_err: std::io::Error) -> Result<(TcpListener, u16)> {
    match TcpListener::bind(format!("{CALLBACK_HOST}:{CALLBACK_FALLBACK_PORT}")).await {
        Ok(listener) => Ok((listener, CALLBACK_FALLBACK_PORT)),
        Err(fallback_err) if ipv4_err.raw_os_error() == Some(10013) => Err(anyhow::anyhow!(
            "Cannot bind OAuth callback ports {CALLBACK_PORT} or {CALLBACK_FALLBACK_PORT}: \
             {ipv4_err}; fallback error: {fallback_err}.\n\
            \nRun the following as Administrator to release reserved ports, then retry:\n\
            \n  net stop winnat\n  net stop hns\n  net start winnat\n  net start hns\
            \n\nOr use device code flow (no port needed):\n  codex-switch login --device"
        )),
        Err(fallback_err) => Err(anyhow::anyhow!(
            "Cannot bind OAuth callback ports {CALLBACK_PORT} or {CALLBACK_FALLBACK_PORT}: \
             {ipv4_err}; fallback error: {fallback_err}"
        )),
    }
}

#[cfg(not(target_os = "windows"))]
async fn bind_callback_listener_fallback(ipv4_err: std::io::Error) -> Result<(TcpListener, u16)> {
    match TcpListener::bind(format!("{CALLBACK_HOST}:{CALLBACK_FALLBACK_PORT}")).await {
        Ok(listener) => Ok((listener, CALLBACK_FALLBACK_PORT)),
        Err(fallback_err) => Err(anyhow::anyhow!(
            "Cannot bind OAuth callback ports {CALLBACK_PORT} or {CALLBACK_FALLBACK_PORT}: \
             {ipv4_err}; fallback error: {fallback_err}"
        )),
    }
}

// ── Browser open ──────────────────────────────────────────

fn open_browser(url: &str) {
    // Windows: rundll32 is more reliable than `cmd /c start` for URLs with special chars
    #[cfg(target_os = "windows")]
    {
        let result = std::process::Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn();
        if result.is_ok() {
            return;
        }
    }
    // All platforms: webbrowser crate handles macOS/Linux/Windows fallback
    if let Err(e) = webbrowser::open(url) {
        tracing::warn!("Failed to open browser: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    async fn send_callback_request(addr: std::net::SocketAddr, path: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();

        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[test]
    fn pkce_challenge_is_s256_of_rfc7636_verifier() {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        use sha2::{Digest, Sha256};

        let pkce = generate_pkce();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.code_verifier.as_bytes()));

        assert_eq!(pkce.code_challenge, expected);
        assert!((43..=128).contains(&pkce.code_verifier.len()));
        assert!(pkce.code_verifier.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '.' | '_' | '~')
        }));
        assert!(!pkce.code_verifier.contains('='));
        assert!(!pkce.code_challenge.contains('='));
    }

    #[test]
    fn generated_states_are_nonempty_unique_and_url_safe() {
        let first = generate_state();
        let second = generate_state();

        assert!(!first.is_empty());
        assert_ne!(first, second);
        assert!(
            first.chars().all(
                |character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            )
        );
        assert!(!first.contains('='));
    }

    #[test]
    fn test_build_auth_json_structure() {
        let tokens = LoginTokens {
            id_token: "id-token".to_string(),
            access_token: "access-token".to_string(),
            refresh_token: "refresh-token".to_string(),
            api_key: None,
        };

        let before = crate::auth::now_unix_secs();
        let auth = build_auth_json(&tokens, "acct-123");
        let after = crate::auth::now_unix_secs();

        assert_eq!(
            auth.pointer("/tokens/id_token").and_then(|v| v.as_str()),
            Some("id-token")
        );
        assert_eq!(
            auth.pointer("/tokens/access_token")
                .and_then(|v| v.as_str()),
            Some("access-token")
        );
        assert_eq!(
            auth.pointer("/tokens/refresh_token")
                .and_then(|v| v.as_str()),
            Some("refresh-token")
        );
        assert_eq!(
            auth.pointer("/tokens/account_id").and_then(|v| v.as_str()),
            Some("acct-123")
        );

        let last_refresh = auth
            .get("last_refresh")
            .and_then(|v| v.as_str())
            .expect("last_refresh should be present");
        let parsed = DateTime::parse_from_rfc3339(last_refresh)
            .unwrap()
            .timestamp();
        assert!(parsed >= before && parsed <= after);

        // Codex 0.144.1 persists auth_mode on ChatGPT logins.
        assert_eq!(
            auth.get("auth_mode").and_then(|v| v.as_str()),
            Some("chatgpt")
        );
    }

    #[test]
    fn test_build_auth_json_persists_api_key_when_present() {
        let tokens = LoginTokens {
            id_token: "id-token".to_string(),
            access_token: "access-token".to_string(),
            refresh_token: "refresh-token".to_string(),
            api_key: Some("sk-test-key".to_string()),
        };

        let auth = build_auth_json(&tokens, "acct-123");

        assert_eq!(
            auth.get("OPENAI_API_KEY").and_then(|v| v.as_str()),
            Some("sk-test-key")
        );
    }

    #[test]
    fn test_authorize_url_includes_forced_workspace_ids() {
        let ids = vec!["ws-1".to_string(), "ws-2".to_string()];
        let url = build_authorize_url(
            "challenge",
            "state",
            "http://localhost:1455/auth/callback",
            &ids,
        );
        assert!(url.contains("allowed_workspace_id=ws-1%2Cws-2"), "{url}");

        let url = build_authorize_url(
            "challenge",
            "state",
            "http://localhost:1455/auth/callback",
            &[],
        );
        assert!(!url.contains("allowed_workspace_id"), "{url}");
    }

    #[test]
    fn test_api_key_exchange_request_matches_codex_contract() {
        let request = build_api_key_exchange_request(
            &reqwest::Client::new(),
            "https://auth.openai.com/oauth/token",
            "the-id-token",
        )
        .build()
        .unwrap();

        assert_eq!(
            request
                .headers()
                .get("Content-Type")
                .and_then(|v| v.to_str().ok()),
            Some("application/x-www-form-urlencoded")
        );
        let body = std::str::from_utf8(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(
            body,
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange\
             &client_id=app_EMoamEEZ73f0CkXaXp7hrann\
             &requested_token=openai-api-key\
             &subject_token=the-id-token\
             &subject_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Aid_token"
        );
    }

    #[test]
    fn test_build_auth_json_writes_null_account_id_when_unknown() {
        let tokens = LoginTokens {
            id_token: "id-token".to_string(),
            access_token: "access-token".to_string(),
            refresh_token: "refresh-token".to_string(),
            api_key: None,
        };

        let auth = build_auth_json(&tokens, "");

        // Upstream serializes a missing account_id as null, not "".
        assert!(
            auth.pointer("/tokens/account_id")
                .expect("account_id key should exist")
                .is_null()
        );
    }

    #[test]
    fn test_redacted_device_poll_log_body_masks_sensitive_fields() {
        let body = serde_json::json!({
            "authorization_code": "auth-code",
            "code_verifier": "verifier",
            "access_token": "access",
            "refresh_token": "refresh",
            "id_token": "id",
            "status": "ok",
        });

        let redacted: serde_json::Value =
            serde_json::from_str(&redacted_device_poll_log_body(&body)).unwrap();

        assert_eq!(redacted["authorization_code"], "***");
        assert_eq!(redacted["code_verifier"], "***");
        assert_eq!(redacted["access_token"], "***");
        assert_eq!(redacted["refresh_token"], "***");
        assert_eq!(redacted["id_token"], "***");
        assert_eq!(redacted["status"], "ok");
    }

    #[test]
    fn test_device_poll_error_action_accepts_oauth_standard_pending() {
        let body = serde_json::json!({
            "error": "authorization_pending",
            "error_description": "authorization is still pending",
        });

        assert_eq!(
            device_poll_error_action(&body),
            Some(DevicePollErrorAction::Continue)
        );
    }

    #[test]
    fn test_device_poll_error_action_keeps_nested_pending() {
        let body = serde_json::json!({
            "error": {
                "code": "deviceauth_authorization_unknown",
                "message": "authorization is still pending",
            },
        });

        assert_eq!(
            device_poll_error_action(&body),
            Some(DevicePollErrorAction::Continue)
        );
    }

    #[test]
    fn test_device_poll_error_action_handles_standard_slow_down() {
        let body = serde_json::json!({
            "error": "slow_down",
            "error_description": "poll less frequently",
        });

        assert_eq!(
            device_poll_error_action(&body),
            Some(DevicePollErrorAction::SlowDown)
        );
    }

    #[test]
    fn device_poll_forbidden_and_not_found_statuses_mean_authorization_is_pending() {
        assert!(super::is_device_poll_pending_status(
            reqwest::StatusCode::FORBIDDEN
        ));
        assert!(super::is_device_poll_pending_status(
            reqwest::StatusCode::NOT_FOUND
        ));
        assert!(!super::is_device_poll_pending_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
    }

    #[test]
    fn device_poll_retries_transient_http_statuses_but_not_deterministic_client_errors() {
        assert!(super::is_device_poll_retryable_status(
            reqwest::StatusCode::REQUEST_TIMEOUT
        ));
        assert!(super::is_device_poll_retryable_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS
        ));
        assert!(super::is_device_poll_retryable_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(!super::is_device_poll_retryable_status(
            reqwest::StatusCode::BAD_REQUEST
        ));
    }

    #[test]
    fn device_poll_sleep_is_capped_at_the_authorization_deadline() {
        let now = tokio::time::Instant::now();
        let deadline = now + Duration::from_secs(3);

        assert_eq!(super::device_poll_next_wake(now, deadline, 30), deadline);
    }

    #[test]
    fn test_device_poll_error_action_retries_unknown_errors() {
        let body = serde_json::json!({
            "error": "temporarily_unavailable",
            "error_description": "try again later",
        });

        assert_eq!(
            device_poll_error_action(&body),
            Some(DevicePollErrorAction::RetryUnknown {
                code: "temporarily_unavailable".to_string(),
                message: "try again later".to_string(),
            })
        );
    }

    #[test]
    fn test_device_poll_success_accepts_current_codex_shape_without_status() {
        let body = serde_json::json!({
            "authorization_code": "auth-code",
            "code_challenge": "challenge",
            "code_verifier": "verifier",
        });

        let (authorization_code, code_verifier) = parse_device_poll_success(body).unwrap();

        assert_eq!(authorization_code, "auth-code");
        assert_eq!(code_verifier, "verifier");
    }

    #[test]
    fn test_device_poll_success_rejects_incomplete_shape() {
        let body = serde_json::json!({
            "authorization_code": "auth-code",
            "code_verifier": "verifier",
        });

        let err = parse_device_poll_success(body).unwrap_err();

        assert!(err.to_string().contains("code_challenge"));
    }

    #[test]
    fn test_callback_ports_match_current_codex_fallback_order() {
        assert_eq!(callback_ports(), [1455, 1457]);
    }

    #[tokio::test]
    async fn test_callback_ignores_stray_and_wrong_state_before_valid_callback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let callback = tokio::spawn(async move { wait_for_callback(listener, "expected").await });

        let stray = send_callback_request(addr, "/favicon.ico").await;
        assert!(stray.starts_with("HTTP/1.1 404 Not Found"));

        let wrong_state =
            send_callback_request(addr, "/auth/callback?code=attacker&state=wrong").await;
        assert!(wrong_state.starts_with("HTTP/1.1 403 Forbidden"));

        let wrong_state_error =
            send_callback_request(addr, "/auth/callback?error=access_denied&state=wrong").await;
        assert!(wrong_state_error.starts_with("HTTP/1.1 403 Forbidden"));

        let valid =
            send_callback_request(addr, "/auth/callback?code=valid-code&state=expected").await;
        assert!(valid.starts_with("HTTP/1.1 200 OK"));

        assert_eq!(callback.await.unwrap().unwrap().code, "valid-code");
    }

    #[tokio::test]
    async fn callback_slow_connection_does_not_block_a_valid_callback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let callback = tokio::spawn(async move { wait_for_callback(listener, "expected").await });

        let mut slow_connections = Vec::new();
        for _ in 0..8 {
            slow_connections.push(tokio::net::TcpStream::connect(addr).await.unwrap());
        }
        let valid = tokio::time::timeout(
            Duration::from_secs(1),
            send_callback_request(addr, "/auth/callback?code=valid-code&state=expected"),
        )
        .await
        .expect("half-open connections must be handled concurrently");

        assert!(valid.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(callback.await.unwrap().unwrap().code, "valid-code");
        drop(slow_connections);
    }

    #[tokio::test]
    async fn callback_accepts_a_request_split_across_reads() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let callback = tokio::spawn(async move { wait_for_callback(listener, "expected").await });
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /auth/callback?code=split&state=expected HTTP/1.1\r\n")
            .await
            .unwrap();
        stream.write_all(b"Host: localhost\r\n\r\n").await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();

        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert_eq!(callback.await.unwrap().unwrap().code, "split");
    }

    #[test]
    fn device_poll_transient_failures_do_not_end_before_timeout() {
        let mut failures = super::DevicePollFailureTracker::default();
        for attempt in 1..=4 {
            assert_eq!(failures.record(), attempt);
        }
    }

    #[test]
    fn interactive_login_waits_ten_minutes_for_browser_and_fifteen_for_device_code() {
        assert_eq!(super::CALLBACK_TIMEOUT_SECS, 10 * 60);
        assert_eq!(super::DEVICE_TIMEOUT_SECS, 15 * 60);
    }

    #[test]
    fn user_cancellation_has_a_typed_identity_for_batch_login() {
        let error: anyhow::Error = super::LoginCancelled.into();

        assert!(super::is_login_cancelled(&error));
        assert!(!super::is_login_cancelled(&anyhow::anyhow!(
            "network error"
        )));
    }

    #[test]
    fn valid_device_poll_response_resets_failure_tracker() {
        let mut failures = super::DevicePollFailureTracker::default();
        assert_eq!(failures.record(), 1);
        assert_eq!(failures.record(), 2);
        failures.reset();
        assert_eq!(failures.record(), 1);
        assert_eq!(failures.record(), 2);
    }

    // ── Token exchange retry / error-shape tests ──────────────
    //
    // `exchange_code_with_redirect` is private (the `login` module is not part of the
    // public library API), so these live as unit tests here rather than as an external
    // `tests/` integration file. CS_TOKEN_URL is process-global and warmup's tests
    // retarget it too, so every test here takes the crate-wide `auth::URL_ENV_LOCK`
    // rather than a lock private to this module.

    mod token_exchange {
        use super::*;
        use axum::{Json, Router, http::StatusCode, routing::post};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::auth::URL_ENV_LOCK as TOKEN_URL_ENV_LOCK;

        struct EnvVarGuard {
            previous: Option<String>,
        }

        impl EnvVarGuard {
            fn set(value: &str) -> Self {
                let previous = std::env::var("CS_TOKEN_URL").ok();
                unsafe {
                    std::env::set_var("CS_TOKEN_URL", value);
                }
                Self { previous }
            }
        }

        impl Drop for EnvVarGuard {
            fn drop(&mut self) {
                unsafe {
                    match &self.previous {
                        Some(value) => std::env::set_var("CS_TOKEN_URL", value),
                        None => std::env::remove_var("CS_TOKEN_URL"),
                    }
                }
            }
        }

        /// Bind then immediately drop: the OS gives back a port nothing is listening on
        /// yet, so a connection attempt to it fails fast with "connection refused" — a
        /// genuine transport error, not a timeout.
        fn reserve_closed_port() -> u16 {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        }

        #[tokio::test]
        async fn token_exchange_retries_until_the_transient_failure_clears() {
            let _lock = TOKEN_URL_ENV_LOCK.lock().await;
            let request_count = Arc::new(AtomicUsize::new(0));
            let counter = request_count.clone();

            // Driven by the request counter, not by wall-clock: the server is up the
            // whole time and decides per request whether this is still the outage.
            // A version keyed on "mock binds at t=300ms" raced the retry schedule and
            // could connect mid-bind, failing with SendRequest instead of Connect.
            let app = Router::new().route(
                "/oauth/token",
                post(move || {
                    let counter = counter.clone();
                    async move {
                        let seen = counter.fetch_add(1, Ordering::SeqCst);
                        if seen < 2 {
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                Json(serde_json::json!({"error": "temporarily unavailable"})),
                            );
                        }
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "id_token": "id-1",
                                "access_token": "access-1",
                                "refresh_token": "refresh-1",
                            })),
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            let _env = EnvVarGuard::set(&format!("http://{addr}/oauth/token"));

            let tokens = exchange_code_with_redirect(
                "code",
                "verifier",
                "http://localhost:1455/auth/callback",
            )
            .await
            .expect("token exchange should succeed once the transient outage clears");

            assert_eq!(request_count.load(Ordering::SeqCst), 3);
            assert_eq!(tokens.id_token, "id-1");
            assert_eq!(tokens.access_token, "access-1");
            assert_eq!(tokens.refresh_token, "refresh-1");
        }

        #[tokio::test]
        async fn a_refused_connection_is_classified_as_retryable() {
            let port = reserve_closed_port();
            let err = crate::auth::build_http_client()
                .unwrap()
                .post(format!("http://127.0.0.1:{port}/oauth/token"))
                .send()
                .await
                .expect_err("nothing is listening on a reserved-then-dropped port");

            assert!(
                is_retryable_transport_error(&err),
                "refused connection must be retryable, but was classified \
                 connect={} timeout={} request={} body={}: {err}",
                err.is_connect(),
                err.is_timeout(),
                err.is_request(),
                err.is_body(),
            );
        }

        #[tokio::test]
        async fn token_exchange_retries_connection_failures_before_giving_up() {
            let _lock = TOKEN_URL_ENV_LOCK.lock().await;
            let port = reserve_closed_port();
            let _env = EnvVarGuard::set(&format!("http://127.0.0.1:{port}/oauth/token"));

            let started = std::time::Instant::now();
            let err = exchange_code_with_redirect(
                "code",
                "verifier",
                "http://localhost:1455/auth/callback",
            )
            .await
            .expect_err("an endpoint that never answers must eventually fail");
            let elapsed = started.elapsed();

            // Lower bound only: the two backoffs (150ms + 300ms) must have elapsed, so
            // connection failures really did go through the retry path rather than
            // failing on the first attempt. A slow machine only makes this more true.
            assert!(
                elapsed >= Duration::from_millis(400),
                "connect failures should have been retried with backoff, gave up after {elapsed:?}"
            );
            assert!(
                err.to_string().contains("Token exchange request failed"),
                "{err}"
            );
        }

        #[tokio::test]
        async fn token_exchange_fails_immediately_on_deterministic_bad_request() {
            let _lock = TOKEN_URL_ENV_LOCK.lock().await;
            let request_count = Arc::new(AtomicUsize::new(0));
            let counter = request_count.clone();

            let app = Router::new().route(
                "/oauth/token",
                post(move || {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "error": "invalid_grant",
                                "error_description": "authorization code already used",
                            })),
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            let _env = EnvVarGuard::set(&format!("http://{addr}/oauth/token"));

            let err = exchange_code_with_redirect(
                "code",
                "verifier",
                "http://localhost:1455/auth/callback",
            )
            .await
            .expect_err("a deterministic 400 must not be swallowed into success");

            assert_eq!(
                request_count.load(Ordering::SeqCst),
                1,
                "a deterministic 4xx must not be retried — it would burn the one-shot code for nothing"
            );
            assert!(err.to_string().contains("invalid_grant"), "{err}");
        }

        #[tokio::test]
        async fn token_exchange_surfaces_object_shaped_error_code_and_message() {
            let _lock = TOKEN_URL_ENV_LOCK.lock().await;

            let app = Router::new().route(
                "/oauth/token",
                post(|| async {
                    (
                        StatusCode::UNAUTHORIZED,
                        Json(serde_json::json!({
                            "error": {
                                "code": "refresh_token_reused",
                                "message": "This refresh token has already been used.",
                                "param": null,
                                "type": "invalid_request_error"
                            }
                        })),
                    )
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            let _env = EnvVarGuard::set(&format!("http://{addr}/oauth/token"));

            let err = exchange_code_with_redirect(
                "code",
                "verifier",
                "http://localhost:1455/auth/callback",
            )
            .await
            .expect_err("object-shaped error body must still fail with a helpful message");

            let msg = err.to_string();
            assert!(msg.contains("refresh_token_reused"), "{msg}");
            assert!(
                msg.contains("This refresh token has already been used."),
                "{msg}"
            );
        }
    }
}
