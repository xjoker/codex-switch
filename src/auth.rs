use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::error::CsError;

const MAX_BACKUPS: usize = 3;

pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(crate) const USER_AGENT: &str = "codex/0.2.0";
pub(crate) const ISSUER: &str = "https://auth.openai.com";
pub(crate) const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// ~/.codex/auth.json (or $CODEX_HOME/auth.json)
pub fn codex_auth_path() -> PathBuf {
    if let Ok(home) = std::env::var("CODEX_HOME") {
        return PathBuf::from(home).join("auth.json");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("auth.json")
}

/// ~/.codex-switch/
pub fn app_home() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex-switch")
}

/// ~/.codex-switch/profiles/
pub fn profiles_dir() -> PathBuf {
    app_home().join("profiles")
}

/// ~/.codex-switch/current
pub fn current_file() -> PathBuf {
    app_home().join("current")
}

pub fn read_auth(path: &Path) -> Result<serde_json::Value> {
    if !path.exists() {
        return Err(CsError::NoAuthFile(path.display().to_string()).into());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let val: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(val)
}

pub fn write_auth(path: &Path, val: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(val)?;
    std::fs::write(path, raw)?;
    Ok(())
}

pub fn sha256_file(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let digest = Sha256::digest(&data);
    Some(hex::encode(digest))
}

pub fn backup_auth(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let ts = now_unix_secs();
    let bak = path.with_extension(format!("json.bak.{ts}"));
    std::fs::copy(path, &bak)?;
    cleanup_old_backups(path);
    Ok(())
}

pub fn update_tokens(
    path: &Path,
    id_token: &str,
    access_token: &str,
    refresh_token: &str,
) -> Result<()> {
    let mut val = read_auth(path)?;
    if let Some(tokens) = val.get_mut("tokens").and_then(|t| t.as_object_mut()) {
        tokens.insert("id_token".into(), serde_json::json!(id_token));
        tokens.insert("access_token".into(), serde_json::json!(access_token));
        tokens.insert("refresh_token".into(), serde_json::json!(refresh_token));
    }
    write_auth(path, &val)
}

/// Extract (access_token, refresh_token) from an auth.json Value.
pub fn extract_tokens(val: &serde_json::Value) -> (Option<String>, Option<String>) {
    let at = val
        .pointer("/tokens/access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let rt = val
        .pointer("/tokens/refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    (at, rt)
}

/// Current unix timestamp in seconds.
pub fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read auth.json and parse AccountInfo in one step (returns default on error).
pub fn read_account_info(path: &Path) -> crate::jwt::AccountInfo {
    read_auth(path)
        .map(|v| crate::jwt::parse_account_info(&v))
        .unwrap_or_default()
}

/// Build a shared reqwest client with standard user-agent and proxy support.
pub fn build_http_client() -> Result<reqwest::Client> {
    let proxy_url = crate::config::resolve_proxy();
    build_http_client_with_proxy(proxy_url.as_deref())
}

pub fn build_http_client_with_proxy(proxy_url: Option<&str>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().user_agent(USER_AGENT);

    if let Some(url) = proxy_url {
        let mut proxy = reqwest::Proxy::all(url)
            .map_err(|e| anyhow::anyhow!("invalid proxy URL '{url}': {e}"))?;
        if let Some(no_proxy) = crate::config::resolve_no_proxy() {
            proxy = proxy.no_proxy(reqwest::NoProxy::from_string(&no_proxy));
        }
        builder = builder.proxy(proxy);
    }

    Ok(builder.build()?)
}

fn cleanup_old_backups(path: &Path) {
    let parent = match path.parent() {
        Some(p) => p,
        None => return,
    };
    let stem = match path.file_name().and_then(|f| f.to_str()) {
        Some(s) => s,
        None => return,
    };
    let prefix = format!("{stem}.bak.");

    let mut backups: Vec<PathBuf> = std::fs::read_dir(parent)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|name| name.starts_with(&prefix))
                .unwrap_or(false)
        })
        .map(|e| e.path())
        .collect();

    if backups.len() <= MAX_BACKUPS {
        return;
    }

    backups.sort();
    let to_remove = backups.len() - MAX_BACKUPS;
    for old in &backups[..to_remove] {
        let _ = std::fs::remove_file(old);
    }
}
