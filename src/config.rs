use std::path::PathBuf;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::auth::app_home;

static CONFIG: OnceLock<AppConfig> = OnceLock::new();
static CLI_PROXY: OnceLock<Option<String>> = OnceLock::new();

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    pub proxy: ProxyConfig,
    pub cache: CacheConfig,
    pub network: NetworkConfig,
    #[serde(rename = "use")]
    pub use_cfg: UseConfig,
}

impl AppConfig {
    fn normalize(mut self) -> Self {
        if self.network.max_concurrent == 0 {
            tracing::warn!("config.network.max_concurrent=0 is invalid; using 1 instead");
            self.network.max_concurrent = 1;
        }
        self
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct ProxyConfig {
    pub url: Option<String>,
    pub no_proxy: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// Cache TTL in seconds (default: 300)
    pub ttl: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { ttl: 300 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// Max concurrent usage requests (default: 20)
    pub max_concurrent: usize,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self { max_concurrent: 20 }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigSelectMode {
    #[default]
    MaxRemaining,
    DrainFirst,
    RoundRobin,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UseConfig {
    /// Selection mode (default: max-remaining)
    pub mode: ConfigSelectMode,
    /// drain-first: accounts with remaining% below this threshold are deprioritized (default: 5)
    pub min_remaining: f64,
    /// 7d safety margin: when 7d remaining% falls below this, a scoring penalty kicks in (default: 20)
    pub safety_margin_7d: f64,
}

impl Default for UseConfig {
    fn default() -> Self {
        Self {
            mode: ConfigSelectMode::default(),
            min_remaining: 5.0,
            safety_margin_7d: 20.0,
        }
    }
}

pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(app_home()?.join("config.toml"))
}

fn load_from_file() -> AppConfig {
    let path = match config_path() {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!("Failed to determine config path: {err}");
            return AppConfig::default();
        }
    };
    if !path.exists() {
        return AppConfig::default();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => match toml::from_str::<AppConfig>(&content) {
            Ok(config) => config.normalize(),
            Err(err) => {
                tracing::warn!("Failed to load config: {err}");
                AppConfig::default()
            }
        },
        Err(err) => {
            tracing::warn!("Failed to load config: {err}");
            AppConfig::default()
        }
    }
}

pub fn init() {
    let _ = CONFIG.set(load_from_file());
}

pub fn get() -> &'static AppConfig {
    CONFIG.get_or_init(load_from_file)
}

pub fn set_cli_proxy(proxy: Option<String>) {
    let _ = CLI_PROXY.set(proxy);
}

pub fn resolve_proxy() -> Option<String> {
    if let Some(Some(p)) = CLI_PROXY.get()
        && !p.is_empty()
    {
        return Some(p.clone());
    }
    if let Some(p) = &get().proxy.url
        && !p.is_empty()
    {
        return Some(p.clone());
    }
    None
}

/// Resolve the effective select mode: CLI flag takes precedence over config.
pub fn resolve_select_mode(cli: Option<crate::cli::SelectMode>) -> ConfigSelectMode {
    match cli {
        Some(crate::cli::SelectMode::MaxRemaining) => ConfigSelectMode::MaxRemaining,
        Some(crate::cli::SelectMode::DrainFirst) => ConfigSelectMode::DrainFirst,
        Some(crate::cli::SelectMode::RoundRobin) => ConfigSelectMode::RoundRobin,
        None => get().use_cfg.mode,
    }
}

pub fn resolve_no_proxy() -> Option<String> {
    if let Some(np) = &get().proxy.no_proxy
        && !np.is_empty()
    {
        return Some(np.clone());
    }
    None
}
