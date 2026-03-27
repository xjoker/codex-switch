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
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let val: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(val)
}

pub fn write_auth(path: &Path, val: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            {
                tracing::warn!(
                    "Failed to set directory permissions on {}: {e}",
                    parent.display()
                );
            }
        }
    }
    let raw = serde_json::to_string_pretty(val)?;
    std::fs::write(path, raw).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
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
    std::fs::copy(path, &bak)
        .with_context(|| format!("backing up {} -> {}", path.display(), bak.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&bak, std::fs::Permissions::from_mode(0o600));
    }
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

pub fn apply_tokens(
    val: &mut serde_json::Value,
    id_token: &str,
    access_token: &str,
    refresh_token: &str,
) -> Result<()> {
    let tokens = val
        .get_mut("tokens")
        .and_then(|t| t.as_object_mut())
        .ok_or_else(|| anyhow::anyhow!("auth.json missing tokens object"))?;

    tokens.insert("id_token".into(), serde_json::json!(id_token));
    tokens.insert("access_token".into(), serde_json::json!(access_token));
    tokens.insert("refresh_token".into(), serde_json::json!(refresh_token));
    Ok(())
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

pub fn validate_auth_value(val: &serde_json::Value) -> Result<crate::jwt::AccountInfo> {
    let tokens = val
        .get("tokens")
        .and_then(|t| t.as_object())
        .ok_or_else(|| anyhow::anyhow!("auth.json missing tokens object"))?;

    let id_token = tokens
        .get("id_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("tokens.id_token is required"))?;

    let has_access = tokens
        .get("access_token")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());
    let has_refresh = tokens
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());

    if !has_access && !has_refresh {
        return Err(anyhow::anyhow!(
            "tokens.access_token or tokens.refresh_token is required"
        ));
    }

    let payload = id_token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("tokens.id_token is not a valid JWT"))?;
    let decoded = {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| anyhow::anyhow!("tokens.id_token payload is not valid base64url"))?
    };
    let _: serde_json::Value = serde_json::from_slice(&decoded)
        .map_err(|_| anyhow::anyhow!("tokens.id_token payload is not valid JSON"))?;

    let info = crate::jwt::parse_account_info(val);
    if info.email.is_none() && info.account_id.is_none() {
        return Err(anyhow::anyhow!(
            "id_token does not contain a usable email or account_id"
        ));
    }

    Ok(info)
}

/// Build a shared reqwest client with standard user-agent and proxy support.
pub fn build_http_client() -> Result<reqwest::Client> {
    let proxy_url = crate::config::resolve_proxy();
    build_http_client_with_proxy(proxy_url.as_deref())
}

pub fn build_http_client_with_proxy(proxy_url: Option<&str>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(60));

    if let Some(url) = proxy_url {
        let sanitized_url = sanitize_proxy_url(url);
        tracing::debug!("Using proxy: {sanitized_url}");
        let mut proxy = reqwest::Proxy::all(url)
            .map_err(|e| anyhow::anyhow!("invalid proxy URL '{sanitized_url}': {e}"))?;
        if let Some(no_proxy) = crate::config::resolve_no_proxy() {
            tracing::debug!("No-proxy list: {no_proxy}");
            proxy = proxy.no_proxy(reqwest::NoProxy::from_string(&no_proxy));
        }
        builder = builder.proxy(proxy);
    }

    Ok(builder.build()?)
}

fn sanitize_proxy_url(url: &str) -> String {
    let Some(scheme_sep) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_sep + 3;
    let authority_end = url[authority_start..]
        .find(['/', '?', '#'])
        .map(|idx| authority_start + idx)
        .unwrap_or(url.len());
    let authority = &url[authority_start..authority_end];
    let Some(userinfo_end) = authority.rfind('@') else {
        return url.to_string();
    };
    let at_pos = authority_start + userinfo_end;

    let mut sanitized = String::with_capacity(url.len());
    sanitized.push_str(&url[..authority_start]);
    sanitized.push_str("***:***");
    sanitized.push_str(&url[at_pos..]);
    sanitized
}

/// Format a reqwest error with the full source chain for diagnostics.
pub fn format_reqwest_error(context: &str, err: &reqwest::Error) -> anyhow::Error {
    let mut msg = format!("{context}: {err}");
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        msg.push_str(&format!("\n  caused by: {cause}"));
        source = std::error::Error::source(cause);
    }
    anyhow::anyhow!("{msg}")
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_sanitize_proxy_url_masks_userinfo() {
        let url = "http://user:pass@example.com:8080/path?q=1";

        assert_eq!(
            sanitize_proxy_url(url),
            "http://***:***@example.com:8080/path?q=1"
        );
    }

    #[test]
    fn test_sanitize_proxy_url_keeps_url_without_userinfo() {
        let url = "socks5://example.com:1080";

        assert_eq!(sanitize_proxy_url(url), url);
    }

    #[cfg(unix)]
    #[test]
    fn test_write_auth_sets_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        write_auth(&path, &json!({ "tokens": {} })).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn test_backup_auth_sets_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        write_auth(&path, &json!({ "tokens": {} })).unwrap();
        backup_auth(&path).unwrap();

        let backup = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .find(|candidate| candidate != &path)
            .expect("backup file should exist");

        let mode = std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
