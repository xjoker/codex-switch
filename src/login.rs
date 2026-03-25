/// Codex OAuth PKCE login flow
///
/// Matches the Codex CLI / Codex-Manager flow:
/// - PKCE Authorization Code Flow (not Device Flow)
/// - Local HTTP callback server on port 1455
/// - Browser completes authorization and redirects back
use std::time::Duration;

use anyhow::{bail, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::info;

use crate::auth::{CLIENT_ID, ISSUER};

const ORIGINATOR: &str = "codex_cli_rs";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";
const CALLBACK_TIMEOUT_SECS: u64 = 300;

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
    rand::thread_rng().fill_bytes(&mut bytes);
    let code_verifier = URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = URL_SAFE_NO_PAD.encode(digest);
    Pkce { code_verifier, code_challenge }
}

fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

// ── Main flow ─────────────────────────────────────────────

/// Run PKCE OAuth flow: open browser → wait for callback → exchange tokens
pub async fn run_device_auth() -> Result<LoginTokens> {
    let pkce = generate_pkce();
    let state = generate_state();

    let authorize_url = build_authorize_url(&pkce.code_challenge, &state);

    let listener = TcpListener::bind("127.0.0.1:1455")
        .await
        .map_err(|e| anyhow::anyhow!("Cannot bind port 1455 (already in use?): {e}"))?;

    println!();
    println!("Opening browser for Codex login…");
    println!("If the browser does not open, visit:");
    println!("{authorize_url}");
    println!();
    println!("Waiting for authorization callback ({CALLBACK_TIMEOUT_SECS}s timeout)…");

    open_browser(&authorize_url);

    let callback_result = tokio::time::timeout(
        Duration::from_secs(CALLBACK_TIMEOUT_SECS),
        wait_for_callback(listener, &state),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Login timed out ({CALLBACK_TIMEOUT_SECS}s). Please try again."))??;

    info!("OAuth callback received, code length={}", callback_result.code.len());

    let tokens = exchange_code(&callback_result.code, &pkce.code_verifier).await?;
    Ok(tokens)
}

// ── Authorization URL ─────────────────────────────────────

fn build_authorize_url(code_challenge: &str, state: &str) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
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
    let (mut stream, _) = listener.accept().await?;

    // Read until we have the full first line (may arrive in multiple reads on Windows)
    let mut buf = vec![0u8; 8192];
    let mut total = 0;
    loop {
        let n = stream.read(&mut buf[total..]).await?;
        if n == 0 { break; }
        total += n;
        // Stop once we see the end of the HTTP request line
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n")
            || buf[..total].windows(2).any(|w| w == b"\n\n")
        {
            break;
        }
        if total >= buf.len() { break; }
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

async fn exchange_code(code: &str, code_verifier: &str) -> Result<LoginTokens> {
    let client = crate::auth::build_http_client()?;

    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        urlencoding::encode(code),
        urlencoding::encode(REDIRECT_URI),
        urlencoding::encode(CLIENT_ID),
        urlencoding::encode(code_verifier),
    );

    let resp = client
        .post(format!("{ISSUER}/oauth/token"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;

    let status = resp.status();
    let token_resp: TokenResponse = resp.json().await?;

    if let Some(e) = token_resp.error {
        let desc = token_resp.error_description.as_deref().unwrap_or("");
        bail!("Token exchange failed (HTTP {status}): {e} — {desc}");
    }

    match (token_resp.id_token, token_resp.access_token, token_resp.refresh_token) {
        (Some(id), Some(access), Some(refresh)) => {
            info!("Token exchange succeeded");
            Ok(LoginTokens { id_token: id, access_token: access, refresh_token: refresh })
        }
        _ => bail!("Token response missing required fields (HTTP {status})"),
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
