use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::auth::app_home;

static CONFIG: OnceLock<AppConfig> = OnceLock::new();
static STARTUP_WARNINGS: OnceLock<Vec<String>> = OnceLock::new();
static CLI_PROXY: OnceLock<Option<String>> = OnceLock::new();

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    pub proxy: ProxyConfig,
    pub cache: CacheConfig,
    pub network: NetworkConfig,
    #[serde(default)]
    pub tui: TuiConfig,
    #[serde(rename = "use")]
    pub use_cfg: UseConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub launch: LaunchConfig,
}

impl AppConfig {
    fn normalize(mut self, warnings: &mut Vec<String>) -> Self {
        if self.network.max_concurrent == 0 {
            warnings.push("config.network.max_concurrent=0 is invalid; using 1 instead".into());
            self.network.max_concurrent = 1;
        }
        if self.tui.auto_refresh_interval_secs < 30 {
            warnings.push(format!(
                "config.tui.auto_refresh_interval_secs={} is invalid; using 30 instead",
                self.tui.auto_refresh_interval_secs
            ));
            self.tui.auto_refresh_interval_secs = 30;
        }
        if self.daemon.cache_refresh_interval_secs == 0 {
            warnings.push(
                "config.daemon.cache_refresh_interval_secs=0 is invalid; using 300 instead".into(),
            );
            self.daemon.cache_refresh_interval_secs = 300;
        }
        if self.daemon.poll_interval_secs == 0 {
            warnings.push("config.daemon.poll_interval_secs=0 is invalid; using 60 instead".into());
            self.daemon.poll_interval_secs = 60;
        }
        if self.daemon.token_check_interval_secs == 0 {
            warnings.push(
                "config.daemon.token_check_interval_secs=0 is invalid; using 300 instead".into(),
            );
            self.daemon.token_check_interval_secs = 300;
        }
        // Not merely a tidy default: at zero, `launch` restores the original
        // auth.json before Codex has read the staged one, so the session runs
        // on the wrong account with nothing reporting it.
        if self.launch.restore_delay_secs == 0 {
            warnings.push("config.launch.restore_delay_secs=0 is invalid; using 3 instead".into());
            self.launch.restore_delay_secs = 3;
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
pub struct TuiConfig {
    /// TUI auto-refresh interval in seconds (default: 300, minimum: 30)
    pub auto_refresh_interval_secs: u64,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            auto_refresh_interval_secs: 300,
        }
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
    /// Background cache refresh interval in seconds (default: 300)
    pub cache_refresh_interval_secs: u64,
    /// Warm up accounts whose quota window is not active during cache refresh (default: false)
    pub auto_warmup: bool,
    /// Token expiry check interval in seconds (default: 300)
    pub token_check_interval_secs: u64,
    /// Send desktop notification on switch (default: false)
    pub notify: bool,
    /// Log level for daemon (default: "error")
    pub log_level: String,
    /// Hold a pending switch while a Codex session is running (default: true)
    pub defer_switch_while_codex_running: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 60,
            switch_threshold: 80.0,
            cache_refresh_interval_secs: 300,
            auto_warmup: false,
            token_check_interval_secs: 300,
            notify: false,
            log_level: "error".to_string(),
            defer_switch_while_codex_running: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LaunchConfig {
    /// Seconds to wait after starting codex before restoring auth.json (default: 3).
    /// Codex CLI reads auth.json only at startup; this delay ensures it finishes reading
    /// before the original auth is restored.
    pub restore_delay_secs: u64,
}

impl Default for LaunchConfig {
    fn default() -> Self {
        Self {
            restore_delay_secs: 3,
        }
    }
}

pub fn config_path() -> anyhow::Result<PathBuf> {
    Ok(app_home()?.join("config.toml"))
}

/// Probe struct to detect deprecated `[use]` keys that are silently ignored.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DeprecatedConfigProbe {
    #[serde(rename = "use")]
    use_cfg: Option<DeprecatedUseProbe>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DeprecatedUseProbe {
    mode: Option<toml::Value>,
    min_remaining: Option<toml::Value>,
}

fn deprecated_key_warnings(raw: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    let Ok(probe) = toml::from_str::<DeprecatedConfigProbe>(raw) else {
        return warnings;
    };
    let Some(use_cfg) = probe.use_cfg else {
        return warnings;
    };
    if use_cfg.mode.is_some() {
        warnings.push(
            "config: [use] 'mode' is deprecated and ignored in v0.0.13+, \
             the adaptive algorithm replaces all selection modes"
                .into(),
        );
    }
    if use_cfg.min_remaining.is_some() {
        warnings.push(
            "config: [use] 'min_remaining' is deprecated and ignored in v0.0.13+, \
             the adaptive algorithm replaces all selection modes"
                .into(),
        );
    }
    warnings
}

fn load_from_str_with_warnings(
    raw: &str,
) -> std::result::Result<(AppConfig, Vec<String>), toml::de::Error> {
    let config = toml::from_str::<AppConfig>(raw)?;
    let mut warnings = deprecated_key_warnings(raw);
    Ok((config.normalize(&mut warnings), warnings))
}

#[cfg(test)]
fn load_from_str(raw: &str) -> std::result::Result<AppConfig, toml::de::Error> {
    load_from_str_with_warnings(raw).map(|(config, _)| config)
}

fn load_from_file() -> Result<(AppConfig, Vec<String>)> {
    let path = config_path().context("failed to determine config path")?;
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(&path) {
                Err(meta_err) if meta_err.kind() == std::io::ErrorKind::NotFound => {
                    return Ok((AppConfig::default(), vec![]));
                }
                Ok(_) => {
                    return Err(err)
                        .with_context(|| format!("failed to read config file {}", path.display()));
                }
                Err(meta_err) => {
                    return Err(meta_err).with_context(|| {
                        format!("failed to inspect config path {}", path.display())
                    });
                }
            }
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read config file {}", path.display()));
        }
    };
    load_from_str_with_warnings(&content).map_err(|err| {
        anyhow::anyhow!(
            "failed to parse config file {}: {}",
            path.display(),
            err.message()
        )
    })
}

pub fn init() -> Result<()> {
    let (config, warnings) = load_from_file()?;
    CONFIG
        .set(config)
        .map_err(|_| anyhow::anyhow!("configuration was initialized before config::init"))?;
    STARTUP_WARNINGS
        .set(warnings)
        .map_err(|_| anyhow::anyhow!("configuration warnings were already initialized"))
}

pub fn startup_warnings() -> &'static [String] {
    STARTUP_WARNINGS.get().map(Vec::as_slice).unwrap_or(&[])
}

pub fn get() -> &'static AppConfig {
    // The binary entry point calls init() and fails fast for an unreadable or
    // invalid existing file. Library-only callers have no startup phase, so
    // they receive the in-memory defaults instead of panicking.
    CONFIG.get_or_init(AppConfig::default)
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

pub fn daemon_log_level() -> String {
    let trimmed = get().daemon.log_level.trim().to_string();
    if trimmed.is_empty() {
        "error".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::load_from_str;

    #[test]
    fn tui_auto_refresh_defaults_to_five_minutes() {
        let config = load_from_str("").unwrap();

        assert_eq!(config.tui.auto_refresh_interval_secs, 300);
    }

    #[test]
    fn daemon_zero_intervals_use_defaults() {
        let config = load_from_str(
            r#"
[daemon]
poll_interval_secs = 0
token_check_interval_secs = 0
cache_refresh_interval_secs = 0
"#,
        )
        .unwrap();

        assert_eq!(config.daemon.poll_interval_secs, 60);
        assert_eq!(config.daemon.token_check_interval_secs, 300);
        assert_eq!(config.daemon.cache_refresh_interval_secs, 300);
    }

    /// A zero restore delay makes `launch` put the original auth.json back
    /// before Codex has read the staged one, so the session silently runs on
    /// the wrong account. Every sibling interval already gets this treatment.
    #[test]
    fn launch_zero_restore_delay_uses_default_and_warns() {
        let (config, warnings) =
            super::load_from_str_with_warnings("[launch]\nrestore_delay_secs = 0\n").unwrap();

        assert_eq!(config.launch.restore_delay_secs, 3);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("restore_delay_secs")),
            "a silently-corrected launch delay is what hands Codex the wrong account: {warnings:?}"
        );
    }
}
