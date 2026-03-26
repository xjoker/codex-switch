/// Codex OAuth PKCE login flow
///
/// Matches the Codex CLI / Codex-Manager flow:
/// - PKCE Authorization Code Flow (not Device Flow)
/// - Local HTTP callback server on port 1455, with localhost fallback port selection
/// - Browser completes authorization and redirects back
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{debug, info};

use crate::auth::{CLIENT_ID, ISSUER};
use crate::output::user_println;

const ORIGINATOR: &str = "codex_cli_rs";
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";
const CALLBACK_TIMEOUT_SECS: u64 = 300;
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_HOST: &str = "127.0.0.1";

// ── Types ─────────────────────────────────────────────────

pub struct LoginTokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
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
    format!("http://{CALLBACK_HOST}:{port}/auth/callback")
}

// ── Main flow ─────────────────────────────────────────────

/// Run PKCE OAuth flow: open browser → wait for callback → exchange tokens
pub async fn run_device_auth() -> Result<LoginTokens> {
    let pkce = generate_pkce();
    let state = generate_state();

    let listener = match TcpListener::bind(format!("{CALLBACK_HOST}:{CALLBACK_PORT}")).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            info!("Port {CALLBACK_PORT} in use, falling back to random port");
            TcpListener::bind(format!("{CALLBACK_HOST}:0"))
                .await
                .map_err(|e| anyhow::anyhow!("Cannot bind callback server: {e}"))?
        }
        Err(e) => return Err(anyhow::anyhow!("Cannot bind port {CALLBACK_PORT}: {e}")),
    };
    let actual_port = listener.local_addr()?.port();
    let actual_redirect = redirect_uri(actual_port);
    let authorize_url = build_authorize_url(&pkce.code_challenge, &state, &actual_redirect);

    user_println("");
    user_println("Opening browser for Codex login…");
    user_println("If the browser does not open, visit:");
    user_println(&authorize_url);
    user_println("");
    if actual_port != CALLBACK_PORT {
        user_println(&format!(
            "Port {CALLBACK_PORT} is busy, using callback port {actual_port}."
        ));
    }
    user_println(&format!(
        "Waiting for authorization callback ({CALLBACK_TIMEOUT_SECS}s timeout)…"
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
            bail!("Cancelled by user.");
        }
    };

    info!(
        "OAuth callback received, code length={}",
        callback_result.code.len()
    );

    let tokens =
        exchange_code(&callback_result.code, &pkce.code_verifier, &actual_redirect).await?;
    Ok(tokens)
}

// ── Authorization URL ─────────────────────────────────────

fn build_authorize_url(code_challenge: &str, state: &str, redirect_uri: &str) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPE),
        ("code_challenge", code_challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", ORIGINATOR),
    ];

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

async fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<CallbackResult> {
    let (mut stream, _) = listener
        .accept()
        .await
        .context("accepting OAuth callback connection")?;

    // Read until we have the full first line (may arrive in multiple reads on Windows)
    let mut buf = vec![0u8; 8192];
    let mut total = 0;
    loop {
        let n = stream
            .read(&mut buf[total..])
            .await
            .context("reading OAuth callback request")?;
        if n == 0 {
            break;
        }
        total += n;
        // Stop once we see the end of the HTTP request line
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n")
            || buf[..total].windows(2).any(|w| w == b"\n\n")
        {
            break;
        }
        if total >= buf.len() {
            break;
        }
    }
    let request = String::from_utf8_lossy(&buf[..total]);

    let html = r#"HTTP/1.1 200 OK
Content-Type: text/html; charset=utf-8
Connection: close

<!DOCTYPE html><html><body style="font-family:sans-serif;text-align:center;padding:60px">
<h2>✓ Login successful</h2><p>You may close this tab and return to the terminal.</p>
</body></html>"#;
    let _ = stream.write_all(html.as_bytes()).await;

    let first_line = request.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("");
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");

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

    if let Some(e) = error {
        bail!("Authorization failed: {e}");
    }

    if returned_state.as_deref() != Some(expected_state) {
        bail!("State mismatch — possible CSRF attack, login aborted");
    }

    match code {
        Some(c) if !c.is_empty() => Ok(CallbackResult { code: c }),
        _ => bail!("Callback did not include an authorization code"),
    }
}

// ── Token exchange ────────────────────────────────────────

async fn exchange_code(code: &str, code_verifier: &str, redirect_uri: &str) -> Result<LoginTokens> {
    exchange_code_with_redirect(code, code_verifier, redirect_uri).await
}

async fn exchange_code_with_redirect(
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<LoginTokens> {
    let client = crate::auth::build_http_client()?;

    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        urlencoding::encode(code),
        urlencoding::encode(redirect_uri),
        urlencoding::encode(CLIENT_ID),
        urlencoding::encode(code_verifier),
    );

    debug!("Token exchange: POST {ISSUER}/oauth/token redirect_uri={redirect_uri}");

    let resp = client
        .post(format!("{ISSUER}/oauth/token"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| crate::auth::format_reqwest_error("Token exchange request failed", &e))?;

    let status = resp.status();
    debug!("Token exchange: HTTP {status}");
    let token_resp: TokenResponse = resp
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to parse token response (HTTP {status}): {e}"))?;

    if let Some(e) = token_resp.error {
        let desc = token_resp.error_description.as_deref().unwrap_or("");
        bail!("Token exchange failed (HTTP {status}): {e} — {desc}");
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
    authorization_code: Option<String>,
    code_verifier: Option<String>,
    status: Option<String>,
}

/// Run Device Code Flow: request code → display to user → poll for token
pub async fn run_device_code_auth() -> Result<LoginTokens> {
    let client = crate::auth::build_http_client()?;

    // Step 1: Request device code
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
        "  Waiting for authorization (polling every {interval_secs}s)…"
    ));

    // Step 3: Poll for token (Ctrl+C safe)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
    let mut poll_count = 0u32;

    loop {
        // Sleep with Ctrl+C support
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(interval_secs)) => {}
            _ = tokio::signal::ctrl_c() => {
                user_println("");
                bail!("Cancelled by user.");
            }
        }

        if tokio::time::Instant::now() >= deadline {
            bail!("Device authorization timed out. Please try again.");
        }

        poll_count += 1;
        eprint!("\r  Polling… ({poll_count})    ");

        let poll_resp = match client
            .post(DEVICE_TOKEN_URL)
            .json(&serde_json::json!({
                "device_auth_id": device_auth_id,
                "user_code": user_code,
                "client_id": CLIENT_ID,
            }))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                info!("Device poll network error (retrying): {e}");
                continue;
            }
        };

        let body: serde_json::Value = match poll_resp.json().await {
            Ok(v) => v,
            Err(e) => {
                info!("Device poll parse error (retrying): {e}");
                continue;
            }
        };

        debug!(
            "Device poll response: {}",
            serde_json::to_string(&body).unwrap_or_default()
        );

        // Check for error response
        if let Some(err) = body.get("error") {
            let code = err.get("code").and_then(|c| c.as_str()).unwrap_or("");
            let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("");

            match code {
                "deviceauth_authorization_unknown" | "authorization_pending" => continue,
                "slow_down" => {
                    interval_secs = interval_secs.saturating_add(5);
                    continue;
                }
                "expired_token" | "deviceauth_expired" => {
                    user_println("");
                    bail!("Device code expired. Please try again.");
                }
                "access_denied" => {
                    user_println("");
                    bail!("Authorization was denied by the user.");
                }
                _ => {
                    user_println("");
                    bail!("Device token error: {msg}");
                }
            }
        }

        // Success — got authorization_code, need to exchange for tokens
        let dt: DeviceTokenResponse = match serde_json::from_value(body) {
            Ok(r) => r,
            Err(e) => {
                debug!("Device poll parse into DeviceTokenResponse failed: {e}");
                continue;
            }
        };

        if dt.status.as_deref() != Some("success") {
            debug!("Device poll status: {:?}", dt.status);
            continue;
        }

        let auth_code = match dt.authorization_code {
            Some(c) => c,
            None => {
                debug!("No authorization_code in success response");
                continue;
            }
        };
        let verifier = match dt.code_verifier {
            Some(v) => v,
            None => {
                debug!("No code_verifier in success response");
                continue;
            }
        };

        eprint!("\r                          \r");
        info!("Device authorization successful, exchanging code for tokens");
        user_println("  Authorization successful, exchanging tokens…");

        // Use the standard /oauth/token endpoint with the returned code + verifier
        // The redirect_uri for device flow is the OpenAI deviceauth callback
        let device_redirect = format!("{ISSUER}/deviceauth/callback");
        let tokens = exchange_code_with_redirect(&auth_code, &verifier, &device_redirect).await?;
        return Ok(tokens);
    }
}

// ── Build auth.json ───────────────────────────────────────

pub fn build_auth_json(tokens: &LoginTokens, account_id: &str) -> serde_json::Value {
    use crate::output::format_iso8601;
    let ts = crate::auth::now_unix_secs();

    serde_json::json!({
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": tokens.id_token,
            "access_token": tokens.access_token,
            "refresh_token": tokens.refresh_token,
            "account_id": account_id
        },
        "last_refresh": format_iso8601(ts)
    })
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
