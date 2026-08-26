//! Custom API provider profiles.
//!
//! A provider profile lets `codex-switch launch` run Codex against a third-party
//! OpenAI-compatible endpoint (OpenRouter, an LLM proxy, …) instead of a ChatGPT
//! OAuth account. Unlike an OAuth profile it carries no `auth.json`; it holds the
//! Codex model-provider definition plus a bearer API key.
//!
//! One provider is one endpoint + key. It may list several models, each with
//! its own reasoning effort and `web_search` setting. The alias is the only
//! user-facing name (Codex's required `model_providers.<id>.name` is the alias).
//!
//! Storage lives entirely under codex-switch's own home
//! (`$CODEX_SWITCH_HOME/providers/<alias>/provider.toml`, mode `0600`) so nothing
//! is written into `~/.codex`. At launch the profile is translated into
//! `codex -c …` overrides while the key is injected into the child process
//! environment under `env_key` — never onto the command line — so it stays out of
//! the process table. Because `-c` layers on top of the base config, the user's
//! `~/.codex/config.toml` (MCP servers, skills, …) is left untouched.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::auth;

/// Provider ids Codex reserves for its built-ins; a custom provider may not
/// reuse them.
const RESERVED_PROVIDER_IDS: [&str; 3] = ["openai", "ollama", "lmstudio"];

/// The only wire protocol current Codex supports (Chat Completions was removed
/// in early 2026). Kept configurable for forward-compatibility but defaulted.
const DEFAULT_WIRE_API: &str = "responses";

fn default_wire_api() -> String {
    DEFAULT_WIRE_API.to_string()
}

/// One model on a provider: the gateway slug plus per-model Codex request
/// settings. `reasoning` empty means no `model_reasoning_effort` override;
/// `no_web_search` saves `web_search=disabled`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_web_search: bool,
}

impl ProviderModel {
    pub fn from_id(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            reasoning: None,
            no_web_search: false,
        }
    }
}

/// A saved custom-provider profile. `alias` is the codex-switch-facing name and
/// the on-disk directory; it is derived from the path on load, never stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    #[serde(skip)]
    pub alias: String,
    /// The `[model_providers.<id>]` key Codex sees.
    pub provider_id: String,
    /// Codex requires `model_providers.<id>.name`. Always equal to `alias`.
    pub name: String,
    /// API base URL, e.g. `https://openrouter.ai/api/v1`.
    pub base_url: String,
    /// Environment variable Codex reads the key from. Derived from the alias and
    /// owned by codex-switch, so it never collides with a provider's own var.
    pub env_key: String,
    /// Model id handed to Codex when launch does not pick one.
    #[serde(default)]
    pub default_model: String,
    /// Models this endpoint can run. At least one; `default_model` must be in it.
    #[serde(default)]
    pub models: Vec<ProviderModel>,
    /// Legacy single-model field from pre-multi-model files. Read on load, never
    /// written back.
    #[serde(default, skip_serializing)]
    pub model: String,
    #[serde(default = "default_wire_api")]
    pub wire_api: String,
    /// Extra `codex -c key=value` overrides applied at launch after the selected
    /// model's own reasoning / web_search settings. Stored verbatim as
    /// `"key=value"` strings. Values pass through untouched; Codex — not
    /// codex-switch — is the source of truth for which keys and values are valid.
    #[serde(default)]
    pub codex_config: Vec<String>,
    /// Bearer API key. Secret: stored `0600`, injected as an env var at launch,
    /// and never printed or placed on the command line.
    pub api_key: String,
}

/// How reasoning is applied for one launch. Does not write the provider file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningLaunch {
    /// Use the selected model's saved `reasoning`.
    Saved,
    /// Omit `model_reasoning_effort` even if the model saved one.
    Skip,
    /// Force this effort for this launch only.
    Effort(String),
}

fn providers_dir() -> Result<PathBuf> {
    Ok(auth::app_home()?.join("providers"))
}

fn provider_dir(alias: &str) -> Result<PathBuf> {
    Ok(providers_dir()?.join(alias))
}

pub fn provider_path(alias: &str) -> Result<PathBuf> {
    Ok(provider_dir(alias)?.join("provider.toml"))
}

/// Whether a provider profile with this alias exists.
pub fn exists(alias: &str) -> bool {
    provider_path(alias).map(|p| p.exists()).unwrap_or(false)
}

/// Derive the codex-switch-owned environment variable name for an alias, e.g.
/// `my-router` → `CODEX_SWITCH_MY_ROUTER_KEY`. Using our own name (rather than a
/// provider's conventional var) keeps the injected key isolated from whatever
/// the user may already have exported.
pub fn derive_env_key(alias: &str) -> String {
    let body: String = alias
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("CODEX_SWITCH_{body}_KEY")
}

/// Derive a Codex `model_providers.<id>` id from an alias: lowercased, with any
/// character outside `[a-z0-9_]` replaced by `_`.
pub fn sanitize_provider_id(alias: &str) -> String {
    alias
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn is_valid_env_key(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Pull legacy provider-level `model_reasoning_effort` / `web_search=disabled`
/// out of `codex_config` when migrating a single-model file. Other overrides
/// stay on the provider.
fn extract_legacy_model_settings(codex_config: &[String]) -> (Option<String>, bool, Vec<String>) {
    let mut reasoning = None;
    let mut no_web_search = false;
    let mut rest = Vec::new();
    for entry in codex_config {
        if let Some(value) = entry.strip_prefix("model_reasoning_effort=")
            && reasoning.is_none()
        {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                reasoning = Some(trimmed.to_string());
                continue;
            }
        }
        if entry == "web_search=disabled" && !no_web_search {
            no_web_search = true;
            continue;
        }
        rest.push(entry.clone());
    }
    (reasoning, no_web_search, rest)
}

impl ProviderProfile {
    /// Build a profile whose Codex display name is the alias.
    pub fn build(
        alias: impl Into<String>,
        base_url: impl Into<String>,
        models: Vec<ProviderModel>,
        api_key: impl Into<String>,
    ) -> Self {
        let alias = alias.into();
        let default_model = models
            .first()
            .map(|model| model.id.clone())
            .unwrap_or_default();
        Self {
            provider_id: sanitize_provider_id(&alias),
            name: alias.clone(),
            base_url: base_url.into(),
            env_key: derive_env_key(&alias),
            default_model,
            models,
            model: String::new(),
            wire_api: default_wire_api(),
            codex_config: Vec::new(),
            api_key: api_key.into(),
            alias,
        }
    }

    /// Fold a pre-multi-model file into `models` / `default_model`, and keep
    /// the Codex display name equal to the alias.
    pub fn normalize(&mut self) {
        self.name = self.alias.clone();
        if self.models.is_empty() && !self.model.trim().is_empty() {
            let (reasoning, no_web_search, rest) =
                extract_legacy_model_settings(&self.codex_config);
            self.models.push(ProviderModel {
                id: self.model.trim().to_string(),
                reasoning,
                no_web_search,
            });
            self.codex_config = rest;
        }
        if self.default_model.trim().is_empty()
            && let Some(first) = self.models.first()
        {
            self.default_model = first.id.clone();
        }
        self.model.clear();
    }

    /// Reject anything Codex (or our launch translation) would choke on before
    /// it is written to disk.
    pub fn validate(&self) -> Result<()> {
        crate::profile::validate_alias(&self.alias)?;
        if self.provider_id.is_empty()
            || !self
                .provider_id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            anyhow::bail!(
                "provider id '{}' must contain only lowercase letters, digits, and '_'",
                self.provider_id
            );
        }
        if RESERVED_PROVIDER_IDS.contains(&self.provider_id.as_str()) {
            anyhow::bail!(
                "provider id '{}' is reserved by Codex; choose a different alias",
                self.provider_id
            );
        }
        if self.name != self.alias {
            anyhow::bail!("provider name must equal alias");
        }
        if !(self.base_url.starts_with("http://") || self.base_url.starts_with("https://")) {
            anyhow::bail!("base_url must start with http:// or https://");
        }
        if !is_valid_env_key(&self.env_key) {
            anyhow::bail!(
                "env_key '{}' is not a valid environment variable name",
                self.env_key
            );
        }
        if self.models.is_empty() {
            anyhow::bail!("provider must have at least one model");
        }
        let mut seen = HashSet::new();
        for model in &self.models {
            let id = model.id.trim();
            if id.is_empty() {
                anyhow::bail!("model id cannot be empty");
            }
            if !seen.insert(id.to_string()) {
                anyhow::bail!("duplicate model '{id}'");
            }
            if let Some(effort) = &model.reasoning
                && effort.trim().is_empty()
            {
                anyhow::bail!("model '{id}' reasoning cannot be empty when set");
            }
        }
        if self
            .models
            .iter()
            .all(|model| model.id.trim() != self.default_model.trim())
        {
            anyhow::bail!(
                "default_model '{}' is not in the provider's model list",
                self.default_model
            );
        }
        if self.wire_api.trim().is_empty() {
            anyhow::bail!("wire_api cannot be empty");
        }
        for entry in &self.codex_config {
            match entry.split_once('=') {
                Some((key, _)) if !key.trim().is_empty() => {}
                _ => anyhow::bail!(
                    "codex config override '{entry}' must be in KEY=VALUE form with a non-empty key"
                ),
            }
        }
        if self.api_key.is_empty() {
            anyhow::bail!("api_key cannot be empty");
        }
        Ok(())
    }

    pub fn resolve_model(&self, model_id: Option<&str>) -> Result<&ProviderModel> {
        let wanted = model_id
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .unwrap_or(self.default_model.trim());
        self.models
            .iter()
            .find(|model| model.id.trim() == wanted)
            .with_context(|| {
                format!(
                    "model '{wanted}' is not on provider '{}'; saved models: {}",
                    self.alias,
                    self.models
                        .iter()
                        .map(|model| model.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
    }

    /// Compact list label: the default model, plus how many others exist.
    pub fn models_label(&self) -> String {
        let extra = self.models.len().saturating_sub(1);
        if extra == 0 {
            self.default_model.clone()
        } else {
            format!("{}  +{extra}", self.default_model)
        }
    }

    /// A display-safe rendering of the key: never the raw value.
    pub fn redacted_key(&self) -> String {
        redact_key(&self.api_key)
    }

    /// The `codex -c …` override arguments that define and select this provider
    /// for a single launch. These layer on top of the user's base
    /// `~/.codex/config.toml` (so MCP servers and other settings are preserved)
    /// and nothing is written to disk.
    ///
    /// The API key is intentionally **not** here — it is handed to Codex through
    /// the environment (see [`launch_env`](Self::launch_env)) so it never appears
    /// in argv or the process table.
    pub fn codex_config_args(&self, model_id: Option<&str>) -> Result<Vec<String>> {
        self.codex_config_args_with(model_id, ReasoningLaunch::Saved)
    }

    pub fn codex_config_args_with(
        &self,
        model_id: Option<&str>,
        reasoning: ReasoningLaunch,
    ) -> Result<Vec<String>> {
        let model = self.resolve_model(model_id)?;
        let id = &self.provider_id;
        let mut pairs = vec![
            format!("model_providers.{id}.name={}", toml_string(&self.alias)),
            format!(
                "model_providers.{id}.base_url={}",
                toml_string(&self.base_url)
            ),
            format!(
                "model_providers.{id}.env_key={}",
                toml_string(&self.env_key)
            ),
            format!(
                "model_providers.{id}.wire_api={}",
                toml_string(&self.wire_api)
            ),
            format!("model_provider={}", toml_string(id)),
            format!("model={}", toml_string(&model.id)),
        ];
        let effort = match &reasoning {
            ReasoningLaunch::Saved => model.reasoning.as_deref(),
            ReasoningLaunch::Skip => None,
            ReasoningLaunch::Effort(value) => Some(value.as_str()),
        };
        if let Some(effort) = effort {
            pairs.push(format!("model_reasoning_effort={effort}"));
        }
        if model.no_web_search {
            pairs.push("web_search=disabled".to_string());
        }
        // Provider-saved extras layer on top, after the selected model, and
        // pass through verbatim (the user is responsible for their TOML form).
        pairs.extend(self.codex_config.iter().cloned());
        Ok(pairs
            .into_iter()
            .flat_map(|kv| ["-c".to_string(), kv])
            .collect())
    }

    /// The single environment override that hands Codex the API key under the
    /// profile's `env_key`. Injected into the child process only.
    pub fn launch_env(&self) -> (String, String) {
        (self.env_key.clone(), self.api_key.clone())
    }
}

/// Render a string as a TOML basic (quoted) string for a `codex -c key=value`
/// override, escaping the characters TOML requires. Codex parses the value part
/// as TOML, so a plain unquoted string would be misread (or rejected).
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Mask a secret for display: keep the last 4 characters when long enough,
/// otherwise fully mask. Never returns the raw key.
pub fn redact_key(key: &str) -> String {
    let len = key.chars().count();
    if len <= 4 {
        "****".to_string()
    } else {
        let tail: String = key.chars().skip(len - 4).collect();
        format!("…{tail}")
    }
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("creating directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    Ok(())
}

/// List saved provider aliases (directories holding a `provider.toml`), sorted.
pub fn list_providers() -> Result<Vec<String>> {
    let dir = providers_dir()?;
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading providers directory {}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|alias| exists(alias))
        .collect();
    names.sort();
    Ok(names)
}

/// Load a provider profile by alias.
pub fn load(alias: &str) -> Result<ProviderProfile> {
    let path = provider_path(alias)?;
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading provider profile {}", path.display()))?;
    let mut profile: ProviderProfile = toml::from_str(&raw)
        .with_context(|| format!("parsing provider profile {}", path.display()))?;
    profile.alias = alias.to_string();
    profile.normalize();
    Ok(profile)
}

/// Persist a provider profile (directory `0700`, file `0600`).
pub fn save(profile: &ProviderProfile) -> Result<()> {
    let mut stored = profile.clone();
    stored.normalize();
    stored.validate()?;
    let dir = provider_dir(&stored.alias)?;
    ensure_private_dir(&dir)?;
    let path = dir.join("provider.toml");
    let toml = toml::to_string_pretty(&stored).context("serializing provider profile")?;
    auth::atomic_write_private(&path, toml.as_bytes())
        .with_context(|| format!("writing provider profile {}", path.display()))
}

/// Remove a provider profile and its stored key.
pub fn remove(alias: &str) -> Result<()> {
    let dir = provider_dir(alias)?;
    if !dir.exists() {
        anyhow::bail!("provider '{alias}' not found");
    }
    std::fs::remove_dir_all(&dir)
        .with_context(|| format!("removing provider profile {}", dir.display()))
}

/// Rename a provider directory and re-derive `provider_id` from the new alias.
/// `env_key` is re-derived only when it still matches the old default; a
/// custom key name is kept so launch still injects into the variable the
/// user configured. Display name follows the alias.
pub fn rename(old: &str, new: &str) -> Result<()> {
    crate::profile::validate_alias(new)?;
    if old == new {
        return Ok(());
    }
    if !exists(old) {
        anyhow::bail!("provider '{old}' not found");
    }
    if exists(new) {
        anyhow::bail!("provider '{new}' already exists");
    }
    if crate::profile::list_profiles()?.iter().any(|p| p == new) {
        anyhow::bail!("'{new}' already names a ChatGPT profile; choose a different alias");
    }
    let mut profile = load(old)?;
    let old_dir = provider_dir(old)?;
    let new_dir = provider_dir(new)?;
    std::fs::rename(&old_dir, &new_dir).with_context(|| {
        format!(
            "renaming provider {} -> {}",
            old_dir.display(),
            new_dir.display()
        )
    })?;
    profile.alias = new.to_string();
    profile.provider_id = sanitize_provider_id(new);
    if profile.env_key == derive_env_key(old) {
        profile.env_key = derive_env_key(new);
    }
    profile.name = new.to_string();
    if let Err(err) = save(&profile) {
        let _ = std::fs::rename(&new_dir, &old_dir);
        return Err(err);
    }
    Ok(())
}

/// Walk CLI tokens and attach `--reasoning` / `--no-web-search` to the most
/// recently declared `--model`. Used by `provider add` so a mixed list can
/// carry per-model settings without a second syntax.
pub fn models_from_cli_args<S: AsRef<str>>(args: &[S]) -> Result<Vec<ProviderModel>> {
    let mut models = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let token = args[i].as_ref();
        if let Some(id) = flag_value(args, &mut i, "--model") {
            if id.is_empty() {
                anyhow::bail!("--model requires a non-empty id");
            }
            models.push(ProviderModel::from_id(id));
            continue;
        }
        if let Some(effort) = flag_value(args, &mut i, "--reasoning") {
            let last = models.last_mut().ok_or_else(|| {
                anyhow::anyhow!("--reasoning must follow a --model so it can attach to that model")
            })?;
            if effort.trim().is_empty() {
                anyhow::bail!("--reasoning requires a non-empty effort");
            }
            last.reasoning = Some(effort);
            continue;
        }
        if token == "--no-web-search" {
            let last = models.last_mut().ok_or_else(|| {
                anyhow::anyhow!(
                    "--no-web-search must follow a --model so it can attach to that model"
                )
            })?;
            last.no_web_search = true;
            i += 1;
            continue;
        }
        i += 1;
    }
    Ok(models)
}

fn flag_value<S: AsRef<str>>(args: &[S], i: &mut usize, flag: &str) -> Option<String> {
    let token = args[*i].as_ref();
    if token == flag {
        let value = args.get(*i + 1)?.as_ref().to_string();
        if value.starts_with("--") {
            return None;
        }
        *i += 2;
        return Some(value);
    }
    let prefix = format!("{flag}=");
    if let Some(value) = token.strip_prefix(&prefix) {
        *i += 1;
        return Some(value.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::MutexGuard;

    struct TestHome {
        _lock: MutexGuard<'static, ()>,
        _home: tempfile::TempDir,
        previous: Option<OsString>,
    }

    impl TestHome {
        fn new() -> Self {
            let lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = tempfile::tempdir().unwrap();
            let previous = std::env::var_os("CODEX_SWITCH_HOME");
            unsafe {
                std::env::set_var("CODEX_SWITCH_HOME", home.path());
            }
            Self {
                _lock: lock,
                _home: home,
                previous,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var("CODEX_SWITCH_HOME", value),
                    None => std::env::remove_var("CODEX_SWITCH_HOME"),
                }
            }
        }
    }

    fn sample(alias: &str) -> ProviderProfile {
        ProviderProfile::build(
            alias,
            "https://openrouter.ai/api/v1",
            vec![ProviderModel::from_id("openai/gpt-5.3-codex")],
            "sk-secret-1234",
        )
    }

    #[test]
    fn env_key_is_derived_from_the_alias_and_owned_by_codex_switch() {
        assert_eq!(derive_env_key("openrouter"), "CODEX_SWITCH_OPENROUTER_KEY");
        assert_eq!(
            derive_env_key("my-router.2"),
            "CODEX_SWITCH_MY_ROUTER_2_KEY"
        );
    }

    #[test]
    fn provider_id_is_sanitized_lowercase() {
        assert_eq!(sanitize_provider_id("My-Router.2"), "my_router_2");
    }

    #[test]
    fn validate_accepts_a_well_formed_profile() {
        assert!(sample("openrouter").validate().is_ok());
    }

    #[test]
    fn validate_rejects_reserved_ids_empty_name_and_bad_url() {
        let mut reserved = sample("openai");
        reserved.provider_id = "openai".to_string();
        assert!(reserved.validate().is_err(), "reserved id must be rejected");

        let mut no_name = sample("p");
        no_name.name = "  ".to_string();
        assert!(
            no_name.validate().is_err(),
            "name that is not the alias must be rejected"
        );

        let mut bad_url = sample("p");
        bad_url.base_url = "openrouter.ai/api/v1".to_string();
        assert!(
            bad_url.validate().is_err(),
            "base_url without a scheme must be rejected"
        );

        let mut no_key = sample("p");
        no_key.api_key = String::new();
        assert!(no_key.validate().is_err(), "empty api_key must be rejected");
    }

    #[test]
    fn validate_rejects_empty_or_duplicate_models_and_unknown_default() {
        let mut empty = sample("p");
        empty.models.clear();
        empty.default_model.clear();
        assert!(empty.validate().is_err(), "no models must be rejected");

        let mut dup = sample("p");
        dup.models = vec![ProviderModel::from_id("a"), ProviderModel::from_id("a")];
        dup.default_model = "a".into();
        assert!(
            dup.validate().is_err(),
            "duplicate model ids must be rejected"
        );

        let mut missing_default = sample("p");
        missing_default.default_model = "other".into();
        assert!(
            missing_default.validate().is_err(),
            "default_model outside the list must be rejected"
        );
    }

    #[test]
    fn redact_never_leaks_the_raw_key() {
        assert_eq!(redact_key("sk-secret-1234"), "…1234");
        assert_eq!(redact_key("tiny"), "****");
        assert!(!redact_key("sk-secret-1234").contains("secret"));
    }

    #[test]
    fn save_load_list_remove_round_trip() {
        let _home = TestHome::new();
        assert!(list_providers().unwrap().is_empty());

        let profile = sample("openrouter");
        save(&profile).unwrap();

        assert!(exists("openrouter"));
        assert_eq!(list_providers().unwrap(), vec!["openrouter".to_string()]);

        let loaded = load("openrouter").unwrap();
        assert_eq!(loaded.alias, "openrouter");
        assert_eq!(loaded.name, "openrouter");
        assert_eq!(loaded.base_url, profile.base_url);
        assert_eq!(loaded.env_key, "CODEX_SWITCH_OPENROUTER_KEY");
        assert_eq!(loaded.api_key, "sk-secret-1234");
        assert_eq!(loaded.wire_api, "responses");
        assert_eq!(loaded.default_model, "openai/gpt-5.3-codex");
        assert_eq!(loaded.models.len(), 1);

        remove("openrouter").unwrap();
        assert!(!exists("openrouter"));
        assert!(list_providers().unwrap().is_empty());
        assert!(remove("openrouter").is_err(), "removing twice must error");
    }

    #[test]
    fn load_migrates_legacy_single_model_and_provider_level_settings() {
        let _home = TestHome::new();
        let dir = provider_dir("legacy").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("provider.toml"),
            r#"
provider_id = "legacy"
name = "Old Display"
base_url = "https://openrouter.ai/api/v1"
env_key = "CODEX_SWITCH_LEGACY_KEY"
model = "deepseek/deepseek-r1-0528"
wire_api = "responses"
codex_config = ["model_reasoning_effort=medium", "web_search=disabled", "foo=bar"]
api_key = "sk-legacy-key"
"#,
        )
        .unwrap();

        let loaded = load("legacy").unwrap();
        assert_eq!(loaded.name, "legacy");
        assert_eq!(loaded.default_model, "deepseek/deepseek-r1-0528");
        assert_eq!(loaded.models.len(), 1);
        assert_eq!(loaded.models[0].id, "deepseek/deepseek-r1-0528");
        assert_eq!(loaded.models[0].reasoning.as_deref(), Some("medium"));
        assert!(loaded.models[0].no_web_search);
        assert_eq!(loaded.codex_config, vec!["foo=bar".to_string()]);

        save(&loaded).unwrap();
        let raw = std::fs::read_to_string(provider_path("legacy").unwrap()).unwrap();
        assert!(
            !raw.contains("\nmodel = "),
            "legacy model field must not be written back: {raw}"
        );
        assert!(raw.contains("[[models]]"), "migrated file must list models");
    }

    #[test]
    fn rename_moves_the_directory_and_rederives_ids() {
        let _home = TestHome::new();
        save(&sample("old")).unwrap();
        rename("old", "new-router").unwrap();
        assert!(!exists("old"));
        let loaded = load("new-router").unwrap();
        assert_eq!(loaded.alias, "new-router");
        assert_eq!(loaded.name, "new-router");
        assert_eq!(loaded.provider_id, "new_router");
        assert_eq!(loaded.env_key, "CODEX_SWITCH_NEW_ROUTER_KEY");
        assert_eq!(loaded.api_key, "sk-secret-1234");
    }

    #[test]
    fn rename_keeps_a_custom_env_key() {
        let _home = TestHome::new();
        let mut profile = sample("old");
        profile.env_key = "OPENROUTER_API_KEY".into();
        save(&profile).unwrap();
        rename("old", "new-router").unwrap();
        let loaded = load("new-router").unwrap();
        assert_eq!(loaded.env_key, "OPENROUTER_API_KEY");
        assert_eq!(loaded.provider_id, "new_router");
    }

    #[test]
    fn codex_config_args_define_and_select_the_default_model_without_the_key() {
        let p = sample("openrouter");
        let args = p.codex_config_args(None).unwrap();
        let joined = args.join(" ");

        assert_eq!(args.iter().filter(|a| a.as_str() == "-c").count(), 6);
        assert!(joined.contains(r#"model_providers.openrouter.name="openrouter""#));
        assert!(
            joined
                .contains(r#"model_providers.openrouter.base_url="https://openrouter.ai/api/v1""#)
        );
        assert!(
            joined.contains(r#"model_providers.openrouter.env_key="CODEX_SWITCH_OPENROUTER_KEY""#)
        );
        assert!(joined.contains(r#"model_providers.openrouter.wire_api="responses""#));
        assert!(joined.contains(r#"model_provider="openrouter""#));
        assert!(joined.contains(r#"model="openai/gpt-5.3-codex""#));
        assert!(
            !args.iter().any(|a| a.contains("sk-secret-1234")),
            "the API key must never appear in argv"
        );
    }

    #[test]
    fn selected_model_settings_layer_before_provider_extras() {
        let mut p = sample("openrouter");
        p.models = vec![
            ProviderModel::from_id("openai/gpt-5.3-codex"),
            ProviderModel {
                id: "deepseek/deepseek-r1-0528".into(),
                reasoning: Some("high".into()),
                no_web_search: true,
            },
        ];
        p.default_model = "openai/gpt-5.3-codex".into();
        p.codex_config = vec!["foo=bar".to_string()];

        let args = p
            .codex_config_args(Some("deepseek/deepseek-r1-0528"))
            .unwrap();
        assert!(
            args.iter()
                .any(|a| a == r#"model="deepseek/deepseek-r1-0528""#)
        );
        let model_pos = args
            .iter()
            .position(|a| a == r#"model="deepseek/deepseek-r1-0528""#)
            .unwrap();
        let reasoning_pos = args
            .iter()
            .position(|a| a == "model_reasoning_effort=high")
            .unwrap();
        let web_pos = args
            .iter()
            .position(|a| a == "web_search=disabled")
            .unwrap();
        let extra_pos = args.iter().position(|a| a == "foo=bar").unwrap();
        assert!(model_pos < reasoning_pos && reasoning_pos < web_pos && web_pos < extra_pos);
    }

    #[test]
    fn launch_reasoning_override_replaces_or_skips_saved_effort() {
        let mut p = sample("openrouter");
        p.models = vec![ProviderModel {
            id: "deepseek/deepseek-r1-0528".into(),
            reasoning: Some("high".into()),
            no_web_search: false,
        }];
        p.default_model = "deepseek/deepseek-r1-0528".into();

        let forced = p
            .codex_config_args_with(
                Some("deepseek/deepseek-r1-0528"),
                ReasoningLaunch::Effort("low".into()),
            )
            .unwrap();
        assert!(forced.iter().any(|a| a == "model_reasoning_effort=low"));
        assert!(!forced.iter().any(|a| a == "model_reasoning_effort=high"));

        let skipped = p
            .codex_config_args_with(Some("deepseek/deepseek-r1-0528"), ReasoningLaunch::Skip)
            .unwrap();
        assert!(
            !skipped
                .iter()
                .any(|a| a.starts_with("model_reasoning_effort="))
        );
    }

    #[test]
    fn unknown_launch_model_is_rejected() {
        let p = sample("openrouter");
        assert!(p.codex_config_args(Some("missing")).is_err());
    }

    #[test]
    fn validate_rejects_a_codex_override_without_a_key() {
        let mut missing_eq = sample("p");
        missing_eq.codex_config = vec!["web_search".to_string()];
        assert!(
            missing_eq.validate().is_err(),
            "an override without '=' must be rejected"
        );

        let mut empty_key = sample("p");
        empty_key.codex_config = vec!["=disabled".to_string()];
        assert!(
            empty_key.validate().is_err(),
            "an override with an empty key must be rejected"
        );

        let mut ok = sample("p");
        ok.codex_config = vec!["temperature=0".to_string()];
        assert!(
            ok.validate().is_ok(),
            "a KEY=VALUE override must be accepted"
        );
    }

    #[test]
    fn models_from_cli_args_attach_flags_to_the_preceding_model() {
        let models = models_from_cli_args(&[
            "codex-switch",
            "provider",
            "add",
            "openrouter",
            "--base-url",
            "https://openrouter.ai/api/v1",
            "--model",
            "openai/gpt-5.3-codex",
            "--model",
            "deepseek/deepseek-r1-0528",
            "--reasoning",
            "high",
            "--no-web-search",
            "--model=openai/gpt-oss-20b",
            "--no-web-search",
        ])
        .unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "openai/gpt-5.3-codex");
        assert!(models[0].reasoning.is_none());
        assert!(!models[0].no_web_search);
        assert_eq!(models[1].id, "deepseek/deepseek-r1-0528");
        assert_eq!(models[1].reasoning.as_deref(), Some("high"));
        assert!(models[1].no_web_search);
        assert_eq!(models[2].id, "openai/gpt-oss-20b");
        assert!(models[2].no_web_search);
    }

    #[test]
    fn models_from_cli_args_reject_flags_before_a_model() {
        assert!(models_from_cli_args(&["--reasoning", "high"]).is_err());
        assert!(models_from_cli_args(&["--no-web-search"]).is_err());
    }

    #[test]
    fn launch_env_carries_the_key_under_the_derived_var() {
        let p = sample("openrouter");
        assert_eq!(
            p.launch_env(),
            (
                "CODEX_SWITCH_OPENROUTER_KEY".to_string(),
                "sk-secret-1234".to_string()
            )
        );
    }

    #[test]
    fn toml_string_quotes_and_escapes() {
        assert_eq!(toml_string("OpenRouter"), r#""OpenRouter""#);
        assert_eq!(toml_string(r#"a"b\c"#), r#""a\"b\\c""#);
    }

    #[cfg(unix)]
    #[test]
    fn saved_key_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let _home = TestHome::new();
        save(&sample("openrouter")).unwrap();
        let mode = std::fs::metadata(provider_path("openrouter").unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "the stored API key must not be world/group readable"
        );
    }
}
