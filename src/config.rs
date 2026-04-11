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
    #[serde(default)]
    pub daemon: DaemonConfig,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UseConfig {
    /// 7d safety margin: when 7d remaining% falls below this, a scoring penalty kicks in (default: 20)
    pub safety_margin_7d: f64,
    /// Prioritize Team plan accounts (default: true)
    pub team_priority: bool,
}

impl Default for UseConfig {
    fn default() -> Self {
        Self {
            safety_margin_7d: 20.0,
            team_priority: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Usage poll interval in seconds (default: 60)
    pub poll_interval_secs: u64,
    /// 5h usage % threshold that triggers a switch (default: 80.0)
    pub switch_threshold: f64,
    /// Token expiry check interval in seconds (default: 300)
    pub token_check_interval_secs: u64,
    /// Send desktop notification on switch (default: false)
    pub notify: bool,
    /// Log level for daemon (default: "info")
    pub log_level: String,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 60,
            switch_threshold: 80.0,
            token_check_interval_secs: 300,
            notify: false,
            log_level: "info".to_string(),
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


pub fn resolve_no_proxy() -> Option<String> {
    if let Some(np) = &get().proxy.no_proxy
        && !np.is_empty()
    {
        return Some(np.clone());
    }
    None
}
