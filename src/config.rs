use std::path::PathBuf;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::auth::app_home;

static CONFIG: OnceLock<AppConfig> = OnceLock::new();
static CLI_PROXY: OnceLock<Option<String>> = OnceLock::new();

/// Application config — loaded once, accessible globally.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub proxy: ProxyConfig,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ProxyConfig {
    /// Proxy URL: http://, https://, socks5://  (with optional user:pass@)
    pub url: Option<String>,
    /// Comma-separated list of hosts to bypass proxy
    pub no_proxy: Option<String>,
}

/// Config file path: ~/.codex-switch/config.toml
pub fn config_path() -> PathBuf {
    app_home().join("config.toml")
}

/// Load config from file (silently returns default if file missing or invalid).
fn load_from_file() -> AppConfig {
    let path = config_path();
    if !path.exists() {
        return AppConfig::default();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => toml::from_str(&content).unwrap_or_default(),
        Err(_) => AppConfig::default(),
    }
}

/// Initialize global config. Call once at startup.
pub fn init() {
    let _ = CONFIG.set(load_from_file());
}

/// Get the global config (panics if init() not called — but default is harmless).
pub fn get() -> &'static AppConfig {
    CONFIG.get_or_init(load_from_file)
}

/// Store CLI --proxy value (call once at startup).
pub fn set_cli_proxy(proxy: Option<String>) {
    let _ = CLI_PROXY.set(proxy);
}

/// Resolve the effective proxy URL with priority:
/// 1. CLI --proxy / CS_PROXY env var
/// 2. Config file proxy.url
/// 3. Standard env vars (HTTP_PROXY / HTTPS_PROXY / ALL_PROXY) — handled by reqwest automatically
pub fn resolve_proxy() -> Option<String> {
    // CLI / CS_PROXY (clap merges these via `env = "CS_PROXY"`)
    if let Some(Some(p)) = CLI_PROXY.get() {
        if !p.is_empty() {
            return Some(p.clone());
        }
    }
    // Config file
    if let Some(p) = &get().proxy.url {
        if !p.is_empty() {
            return Some(p.clone());
        }
    }
    // Fallback: let reqwest read HTTP_PROXY/HTTPS_PROXY/ALL_PROXY on its own
    None
}

/// Resolve NO_PROXY: config file → env var (reqwest reads env automatically)
pub fn resolve_no_proxy() -> Option<String> {
    if let Some(np) = &get().proxy.no_proxy {
        if !np.is_empty() {
            return Some(np.clone());
        }
    }
    None
}
