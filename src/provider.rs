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
//! Provider definitions and keys live under codex-switch's own home.
//! New launches retain the user's CODEX_HOME and select a separate native
//! config profile. Model routing is bound through process arguments; API keys
//! are injected only into the child environment. Shared Codex resources remain
//! available without copying or exit-time merging. Existing isolated histories
//! are retained for recovery through their original homes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fs4::{FileExt, TryLockError};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::debug;

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
    /// Stable identity for the provider's history. It deliberately survives
    /// alias changes and is regenerated when an alias is recreated.
    #[serde(default)]
    pub identity_id: String,
    /// The `[model_providers.<id>]` key Codex sees.
    pub provider_id: String,
    /// Codex requires `model_providers.<id>.name`. Always equal to `alias`.
    pub name: String,
    /// API base URL, e.g. `https://openrouter.ai/api/v1`.
    pub base_url: String,
    /// Explicit opt-in for sending the provider bearer key over plain HTTP.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_insecure_http: bool,
    /// Environment variable Codex reads the key from. Derived from the alias and
    /// owned by codex-switch, so it never collides with a provider's own var.
    pub env_key: String,
    /// Model id handed to Codex when launch does not pick one.
    #[serde(default)]
    pub default_model: String,
    /// Models this endpoint can run. At least one; `default_model` must be in it.
    #[serde(default)]
    pub models: Vec<ProviderModel>,
    /// Recent conclusive result from `provider probe`. Entries are tied to the
    /// effective endpoint and credentials, and expire so a transient outage
    /// can never permanently deny a model at launch.
    #[serde(
        default,
        deserialize_with = "deserialize_responses_support",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    responses_support: BTreeMap<String, ResponsesSupportRecord>,
    /// Legacy single-model field from pre-multi-model files. Read on load, never
    /// written back.
    #[serde(default, skip_serializing)]
    pub model: String,
    /// Catalog metadata fallback: HTTP(S) URL, local JSON path, or `none` to
    /// skip. Empty means env (`CODEX_SWITCH_METADATA_FALLBACK`, then
    /// `CODEX_SWITCH_OPENROUTER_MODELS_URL`) or the public OpenRouter list.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub metadata_fallback: String,
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
    /// Do not send `model_reasoning_effort`, even if the model or extras saved
    /// one. Codex 0.150 still applies a leftover value from the isolated home.
    Skip,
    /// Force this effort for this launch only.
    Effort(String),
}

fn providers_dir() -> Result<PathBuf> {
    Ok(auth::app_home()?.join("providers"))
}

fn provider_dir(alias: &str) -> Result<PathBuf> {
    crate::profile::validate_alias(alias)?;
    Ok(providers_dir()?.join(alias))
}

fn existing_provider_dir(alias: &str) -> Result<PathBuf> {
    let root = providers_dir()?;
    let dir = provider_dir(alias)?;
    if !dir.exists() {
        anyhow::bail!("provider '{alias}' not found");
    }
    let canonical_root = std::fs::canonicalize(&root)
        .with_context(|| format!("resolving providers directory {}", root.display()))?;
    let canonical_dir = std::fs::canonicalize(&dir)
        .with_context(|| format!("resolving provider directory {}", dir.display()))?;
    if canonical_dir.parent() != Some(canonical_root.as_path()) {
        anyhow::bail!(
            "provider '{alias}' resolves outside {}",
            canonical_root.display()
        );
    }
    Ok(dir)
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

fn new_identity_id() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let value = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &value[0..8],
        &value[8..12],
        &value[12..16],
        &value[16..20],
        &value[20..32]
    )
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
            identity_id: new_identity_id(),
            provider_id: sanitize_provider_id(&alias),
            name: alias.clone(),
            base_url: base_url.into(),
            allow_insecure_http: false,
            env_key: derive_env_key(&alias),
            default_model,
            models,
            responses_support: BTreeMap::new(),
            model: String::new(),
            metadata_fallback: String::new(),
            wire_api: default_wire_api(),
            codex_config: Vec::new(),
            api_key: api_key.into(),
            alias,
        }
    }

    /// Fold a pre-multi-model file into `models` / `default_model`, and keep
    /// the Codex display name equal to the alias.
    pub fn normalize(&mut self) {
        if self.identity_id.trim().is_empty() {
            self.identity_id = new_identity_id();
        }
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
        self.responses_support
            .retain(|slug, _| self.models.iter().any(|model| model.id == *slug));
        self.trim_responses_support();
        self.model.clear();
    }

    fn trim_responses_support(&mut self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default();
        self.responses_support.retain(|_, record| {
            record.checked_at <= now
                && now.saturating_sub(record.checked_at) <= RESPONSES_SUPPORT_TTL_SECS
        });
        if self.responses_support.len() <= MAX_RESPONSES_SUPPORT_ENTRIES {
            return;
        }
        let mut oldest: Vec<(u64, String)> = self
            .responses_support
            .iter()
            .map(|(model, record)| (record.checked_at, model.clone()))
            .collect();
        oldest.sort_unstable();
        let remove_count = oldest.len() - MAX_RESPONSES_SUPPORT_ENTRIES;
        for (_, model) in oldest.into_iter().take(remove_count) {
            self.responses_support.remove(&model);
        }
    }

    /// Reject anything Codex (or our launch translation) would choke on before
    /// it is written to disk.
    pub fn validate(&self) -> Result<()> {
        crate::profile::validate_alias(&self.alias)?;
        if self.identity_id.trim().is_empty()
            || !self
                .identity_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            anyhow::bail!("provider identity id is invalid");
        }
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
        validate_base_url(&self.base_url, self.allow_insecure_http)?;
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
        let wire_api = self.wire_api.trim();
        if wire_api.is_empty() {
            anyhow::bail!("wire_api cannot be empty");
        }
        if wire_api == "chat" {
            anyhow::bail!("wire_api = \"chat\" was removed upstream; use wire_api = \"responses\"");
        }
        for entry in &self.codex_config {
            match entry.split_once('=') {
                Some((key, _)) if !key.trim().is_empty() => {
                    validate_provider_override_shape(&self.provider_id, key)?;
                }
                _ => anyhow::bail!(
                    "codex config override '{entry}' must be in KEY=VALUE form with a non-empty key"
                ),
            }
        }
        if self.api_key.is_empty() {
            anyhow::bail!("api_key cannot be empty");
        }
        if !self.metadata_fallback.trim().is_empty() {
            validate_metadata_fallback(&self.metadata_fallback)?;
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

    pub(crate) fn responses_support_for(&self, model: &str) -> Option<bool> {
        let record = self.responses_support.get(model)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        if record.fingerprint.is_empty()
            || record.checked_at > now
            || now.saturating_sub(record.checked_at) > RESPONSES_SUPPORT_TTL_SECS
            || self.responses_support_fingerprint().ok()?.as_str() != record.fingerprint
        {
            return None;
        }
        match record.support {
            ResponsesSupport::Supported => Some(true),
            ResponsesSupport::Unsupported => Some(false),
            ResponsesSupport::Unknown => None,
        }
    }

    pub(crate) fn record_responses_probes(&mut self, probes: &[ResponsesProbe]) {
        let Ok(fingerprint) = self.responses_support_fingerprint() else {
            self.responses_support.clear();
            return;
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default();
        self.responses_support.retain(|_, record| {
            record.fingerprint == fingerprint
                && record.checked_at <= now
                && now.saturating_sub(record.checked_at) <= RESPONSES_SUPPORT_TTL_SECS
        });
        for probe in probes {
            match probe.support {
                ResponsesSupport::Supported => {
                    self.responses_support.insert(
                        probe.model.clone(),
                        ResponsesSupportRecord {
                            support: ResponsesSupport::Supported,
                            fingerprint: fingerprint.clone(),
                            checked_at: now,
                        },
                    );
                }
                ResponsesSupport::Unsupported => {
                    self.responses_support.insert(
                        probe.model.clone(),
                        ResponsesSupportRecord {
                            support: ResponsesSupport::Unsupported,
                            fingerprint: fingerprint.clone(),
                            checked_at: now,
                        },
                    );
                }
                ResponsesSupport::Unknown => {
                    self.responses_support.remove(&probe.model);
                }
            }
        }
        if self.responses_support.len() > MAX_RESPONSES_SUPPORT_ENTRIES {
            let mut oldest: Vec<(u64, String)> = self
                .responses_support
                .iter()
                .map(|(model, record)| (record.checked_at, model.clone()))
                .collect();
            oldest.sort_unstable();
            let remove_count = oldest.len() - MAX_RESPONSES_SUPPORT_ENTRIES;
            for (_, model) in oldest.into_iter().take(remove_count) {
                self.responses_support.remove(&model);
            }
        }
    }

    fn responses_support_fingerprint(&self) -> Result<String> {
        Ok(resolve_provider_connection(self)?.fingerprint())
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

    /// Test helper: launch `-c` overrides without writing a catalog.
    #[cfg(test)]
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
        if let Some(effort) = thinking_effort(effort) {
            pairs.push(format!("model_reasoning_effort={effort}"));
        }
        if model.no_web_search {
            pairs.push("web_search=disabled".to_string());
        }
        // Provider-saved extras layer on top, after the selected model, and
        // pass through verbatim (the user is responsible for their TOML form).
        // Skip must also drop a leftover `--set model_reasoning_effort=…`, or
        // that extra re-injects the thinking level this launch opted out of.
        pairs.extend(
            self.codex_config
                .iter()
                .filter(|entry| {
                    !matches!(reasoning, ReasoningLaunch::Skip)
                        || !is_reasoning_effort_override(entry)
                })
                .cloned(),
        );
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

    /// Clone this profile for a native Codex run, moving saved provider leaf
    /// overrides along with the runtime provider ID. The native session uses
    /// a per-run provider ID, so leaving these `-c model_providers.<alias>.*`
    /// entries untouched creates an incomplete second provider in Codex.
    pub(crate) fn for_runtime_provider_id(&self, runtime_provider_id: &str) -> Self {
        let mut runtime = self.clone();
        let original_prefix = format!("model_providers.{}.", self.provider_id);
        let runtime_prefix = format!("model_providers.{runtime_provider_id}.");
        for entry in &mut runtime.codex_config {
            if let Some((key, value)) = entry.split_once('=') {
                if key.trim() == "model_provider" {
                    *entry = format!("model_provider={}", toml_string(runtime_provider_id));
                } else if let Some(suffix) = key.trim().strip_prefix(&original_prefix) {
                    *entry = format!("{runtime_prefix}{suffix}={value}");
                }
            }
        }
        runtime.provider_id = runtime_provider_id.to_string();
        runtime
    }

    pub(crate) fn has_explicit_model_catalog(&self) -> bool {
        override_value(&self.codex_config, "model_catalog_json").is_some()
    }

    /// Launch-time overrides backed only by the catalog already on disk.
    ///
    /// Provider discovery is an explicit operation. Launch must stay usable
    /// offline and must not replace previously fetched metadata with a weaker
    /// fallback. A local base is generated only when no matching saved catalog
    /// exists; launches that change its model-specific view receive a private
    /// tailored copy.
    pub(crate) fn codex_config_args_from_saved_catalog_at(
        &self,
        model_id: Option<&str>,
        reasoning: ReasoningLaunch,
        launch_dir: &Path,
    ) -> Result<Vec<String>> {
        let mut args = self.codex_config_args_with(model_id, reasoning.clone())?;
        if self.has_explicit_model_catalog() {
            return Ok(args);
        }
        let selected = self.resolve_model(model_id)?;
        let default = self.resolve_model(None)?;
        let dir = provider_dir(&self.alias)?;
        let saved_path = dir.join("models.json");
        let saved_slugs = self.saved_model_slugs(&default.id);
        if !saved_catalog_matches(&saved_path, &saved_slugs) {
            self.write_model_catalog(&default.id, default.reasoning.as_deref(), &[], &[])?;
        }
        let body = std::fs::read(&saved_path)
            .with_context(|| format!("reading provider model catalog {}", saved_path.display()))?;
        let mut catalog: serde_json::Value = serde_json::from_slice(&body)
            .with_context(|| format!("parsing provider model catalog {}", saved_path.display()))?;
        let saved_catalog = catalog.clone();
        tailor_saved_catalog(
            &mut catalog,
            &self.saved_model_slugs(&selected.id),
            &self.models,
            &selected.id,
            &reasoning,
            override_context_window(&self.codex_config),
        )?;
        let path = if catalog == saved_catalog {
            saved_path
        } else {
            ensure_private_dir(launch_dir)?;
            let path = launch_dir.join("models.json");
            let body = serde_json::to_vec_pretty(&catalog)
                .context("serializing provider launch model catalog")?;
            auth::atomic_write_private(&path, &body)
                .with_context(|| format!("writing provider launch catalog {}", path.display()))?;
            path
        };
        let path_utf8 = path
            .to_str()
            .map(str::to_string)
            .with_context(|| format!("model catalog path {} is not valid UTF-8", path.display()))?;
        args.extend([
            "-c".to_string(),
            format!("model_catalog_json={}", toml_string(&path_utf8)),
        ]);
        Ok(args)
    }

    /// Persist the metadata gathered by an explicit model sync for later
    /// offline launches. Metadata fallback is also resolved here, while the
    /// user is explicitly asking for network discovery.
    pub(crate) async fn save_synced_model_catalog(&self, primary: &[RemoteModel]) -> Result<()> {
        if self.has_explicit_model_catalog() {
            return Ok(());
        }
        let fallback = load_metadata_fallback(self, primary).await;
        let model = self.resolve_model(None)?;
        self.write_model_catalog(&model.id, model.reasoning.as_deref(), primary, &fallback)?;
        Ok(())
    }

    pub(crate) fn save_synced_model_catalog_blocking(&self, primary: &[RemoteModel]) -> Result<()> {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| {
                handle.block_on(self.save_synced_model_catalog(primary))
            }),
            Err(_) => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("starting runtime for provider catalog save")?;
                runtime.block_on(self.save_synced_model_catalog(primary))
            }
        }
    }

    fn write_model_catalog(
        &self,
        default_slug: &str,
        default_reasoning: Option<&str>,
        remote: &[RemoteModel],
        fallback: &[RemoteModel],
    ) -> Result<PathBuf> {
        let dir = provider_dir(&self.alias)?;
        ensure_private_dir(&dir)?;
        let path = dir.join("models.json");
        let json = build_model_catalog(
            &self.saved_model_slugs(default_slug),
            &self.models,
            remote,
            fallback,
            default_slug,
            override_context_window(&self.codex_config),
            default_reasoning,
        );
        let body =
            serde_json::to_vec_pretty(&json).context("serializing provider model catalog")?;
        auth::atomic_write_private(&path, &body)
            .with_context(|| format!("writing provider model catalog {}", path.display()))?;
        Ok(path)
    }

    /// Selected slug first, then the rest of the saved model ids, de-duplicated.
    fn saved_model_slugs(&self, default_slug: &str) -> Vec<String> {
        let mut out = Vec::new();
        for slug in std::iter::once(default_slug).chain(self.models.iter().map(|m| m.id.as_str())) {
            let slug = slug.trim();
            if slug.is_empty() || out.iter().any(|existing| existing == slug) {
                continue;
            }
            out.push(slug.to_string());
        }
        out
    }
}

fn validate_base_url(base_url: &str, allow_insecure_http: bool) -> Result<()> {
    let url = reqwest::Url::parse(base_url).context("base_url must be a valid URL")?;
    match url.scheme() {
        "https" => Ok(()),
        "http" => {
            if allow_insecure_http {
                Ok(())
            } else {
                anyhow::bail!(
                    "base_url must use https; to send the key over plain HTTP, pass --allow-insecure-http (CLI) or untick \"HTTPS only\" (TUI)"
                )
            }
        }
        _ => anyhow::bail!("base_url must use http or https"),
    }
}

fn saved_catalog_matches(path: &Path, saved_slugs: &[String]) -> bool {
    let Ok(body) = std::fs::read(path) else {
        return false;
    };
    let Ok(catalog) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return false;
    };
    let Some(models) = catalog.get("models").and_then(serde_json::Value::as_array) else {
        return false;
    };
    models.len() == saved_slugs.len()
        && saved_slugs.iter().all(|slug| {
            models.iter().any(|model| {
                model.get("slug").and_then(serde_json::Value::as_str) == Some(slug.as_str())
            })
        })
}

fn tailor_saved_catalog(
    catalog: &mut serde_json::Value,
    ordered_slugs: &[String],
    models: &[ProviderModel],
    selected_slug: &str,
    reasoning: &ReasoningLaunch,
    selected_context_window: Option<i64>,
) -> Result<()> {
    let entries = catalog
        .get_mut("models")
        .and_then(serde_json::Value::as_array_mut)
        .context("saved provider catalog has no models array")?;
    let mut remaining = std::mem::take(entries);
    let mut ordered = Vec::with_capacity(ordered_slugs.len());
    for (priority, slug) in ordered_slugs.iter().enumerate() {
        let index = remaining
            .iter()
            .position(|entry| {
                entry.get("slug").and_then(serde_json::Value::as_str) == Some(slug.as_str())
            })
            .with_context(|| format!("saved provider catalog is missing model '{slug}'"))?;
        let mut entry = remaining.remove(index);
        repair_legacy_generated_instructions(&mut entry)?;
        let effort = if slug == selected_slug {
            match reasoning {
                ReasoningLaunch::Saved => models
                    .iter()
                    .find(|model| model.id == *slug)
                    .and_then(|model| model.reasoning.as_deref()),
                ReasoningLaunch::Skip => None,
                ReasoningLaunch::Effort(value) => Some(value.as_str()),
            }
        } else {
            models
                .iter()
                .find(|model| model.id == *slug)
                .and_then(|model| model.reasoning.as_deref())
        };
        apply_catalog_reasoning_with_clear(
            &mut entry,
            effort,
            slug == selected_slug
                && (matches!(reasoning, ReasoningLaunch::Skip)
                    || (effort.is_some() && thinking_effort(effort).is_none())),
        )?;
        let object = entry
            .as_object_mut()
            .context("saved provider catalog model is not an object")?;
        object
            .entry("supports_parallel_tool_calls")
            .or_insert(false.into());
        object.insert(
            "priority".into(),
            serde_json::Value::from(i64::try_from(priority).unwrap_or(i64::MAX)),
        );
        if slug == selected_slug
            && let Some(context_window) = selected_context_window
        {
            let max_context_window = object.get("max_context_window").and_then(json_positive_i64);
            object.insert(
                "context_window".into(),
                max_context_window
                    .map_or(context_window, |max| context_window.min(max))
                    .into(),
            );
        }
        ordered.push(entry);
    }
    *entries = ordered;
    Ok(())
}

/// Codex 0.159.2's fallback metadata uses a 272k context for unknown slugs.
const DEFAULT_PROVIDER_CONTEXT_WINDOW: i64 = 272_000;

/// Gateways at or under this size can be imported wholesale with
/// `--fetch-models` / TUI `f`. Larger catalogs (OpenRouter is hundreds) must
/// be picked with `--model` or the TUI picker.
pub(crate) const SMALL_REMOTE_CATALOG_LIMIT: usize = 48;

const GATEWAY_MODELS_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_GATEWAY_MODELS_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROVIDER_CATALOG_BODY_BYTES: usize = 1024 * 1024;
const MAX_RESPONSES_PROBE_BODY_BYTES: usize = 1024 * 1024;
/// How long a saved `provider probe` verdict is kept. Launch never trusts a
/// saved denial on its own: it re-checks it live first (see
/// [`recheck_cached_responses_denial`]), so a long lifetime only avoids probing
/// on the normal path. A connection change invalidates a record immediately
/// through its fingerprint, regardless of age.
const RESPONSES_SUPPORT_TTL_SECS: u64 = 7 * 24 * 60 * 60;
const MAX_RESPONSES_SUPPORT_ENTRIES: usize = 256;

const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

/// Fields from a gateway `GET {base_url}/models` row that Codex's catalog uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteModel {
    pub slug: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub context_window: Option<i64>,
    pub input_modalities: Vec<String>,
    /// Full Codex-native model metadata, retained so catalog extensions and
    /// instruction templates survive a read/write round trip.
    pub catalog_entry: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResponsesSupportRecord {
    support: ResponsesSupport,
    fingerprint: String,
    checked_at: u64,
}

/// Old files stored `model -> bool`. Retain their shape as stale records so
/// they load cleanly but cannot preserve an old launch denial.
fn deserialize_responses_support<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, ResponsesSupportRecord>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let stored = BTreeMap::<String, serde_json::Value>::deserialize(deserializer)?;
    stored
        .into_iter()
        .map(|(model, value)| {
            let record = match value {
                serde_json::Value::Bool(supported) => ResponsesSupportRecord {
                    support: if supported {
                        ResponsesSupport::Supported
                    } else {
                        ResponsesSupport::Unsupported
                    },
                    fingerprint: String::new(),
                    checked_at: 0,
                },
                value => serde_json::from_value(value).map_err(serde::de::Error::custom)?,
            };
            Ok((model, record))
        })
        .collect()
}

fn override_value<'a>(config: &'a [String], key: &str) -> Option<&'a str> {
    config.iter().rev().find_map(|entry| {
        let (k, v) = entry.split_once('=')?;
        (k.trim() == key).then_some(v.trim())
    })
}

fn is_reasoning_effort_override(entry: &str) -> bool {
    entry
        .split_once('=')
        .is_some_and(|(key, _)| key.trim() == "model_reasoning_effort")
}

fn override_context_window(config: &[String]) -> Option<i64> {
    override_value(config, "model_context_window")
        .and_then(|value| value.trim_matches('"').parse().ok())
        .filter(|value| *value > 0)
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn validate_metadata_fallback(value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("none") {
        return Ok(());
    }
    if value.starts_with("http://") || value.starts_with("https://") {
        return Ok(());
    }
    if value.contains("://") {
        anyhow::bail!("metadata fallback must be an http(s) URL, a JSON file path, or none");
    }
    Ok(())
}

/// Per-provider `--metadata-fallback`, then env, then public OpenRouter.
fn metadata_fallback_source(profile: &ProviderProfile) -> String {
    let from_profile = profile.metadata_fallback.trim();
    if !from_profile.is_empty() {
        return from_profile.to_string();
    }
    env_nonempty("CODEX_SWITCH_METADATA_FALLBACK")
        .or_else(|| env_nonempty("CODEX_SWITCH_OPENROUTER_MODELS_URL"))
        .unwrap_or_else(|| OPENROUTER_MODELS_URL.to_string())
}

fn is_none_fallback(source: &str) -> bool {
    source.trim().eq_ignore_ascii_case("none")
}

/// Skip a second GET when the fallback is already the gateway `/models` URL.
fn same_models_endpoint(base_url: &str, fallback_source: &str) -> bool {
    let gateway = format!("{}/models", base_url.trim_end_matches('/'));
    let norm = |s: &str| s.trim().trim_end_matches('/').to_ascii_lowercase();
    let left = norm(&gateway);
    let right = norm(fallback_source);
    !left.is_empty() && left == right
}

/// Effective HTTP settings after applying the provider's saved `-c` values to
/// the generated model-provider block. Values stay in memory only; the
/// fingerprint hashes them before persistence.
#[derive(Debug, Clone, Serialize)]
struct ProviderHttpConfig {
    base_url: String,
    model_catalog_url: Option<String>,
    wire_api: String,
    allow_insecure_http: bool,
    api_key: Option<String>,
    stored_api_key: String,
    headers: BTreeMap<String, String>,
    query_params: BTreeMap<String, String>,
}

impl ProviderHttpConfig {
    fn fingerprint(&self) -> String {
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        hex::encode(Sha256::digest(bytes))
    }

    fn secrets(&self) -> Vec<String> {
        let mut out = Vec::new();
        for candidate in std::iter::once(self.stored_api_key.as_str())
            .chain(self.api_key.as_deref())
            .chain(self.headers.values().map(String::as_str))
            .chain(self.query_params.values().map(String::as_str))
        {
            if !candidate.is_empty() && !out.iter().any(|secret| secret == candidate) {
                out.push(candidate.to_string());
            }
        }
        for endpoint in
            std::iter::once(self.base_url.as_str()).chain(self.model_catalog_url.as_deref())
        {
            if let Ok(url) = reqwest::Url::parse(endpoint) {
                for (_, value) in url.query_pairs() {
                    if !value.is_empty() && !out.contains(&value.to_string()) {
                        out.push(value.to_string());
                    }
                }
            }
        }
        out
    }

    fn url_for_path(&self, path: &str) -> Result<reqwest::Url> {
        let mut url =
            reqwest::Url::parse(&self.base_url).context("provider base URL is invalid")?;
        let mut full_path = url.path().trim_end_matches('/').to_string();
        full_path.push('/');
        full_path.push_str(path.trim_start_matches('/'));
        url.set_path(&full_path);
        append_query_params(&mut url, &self.query_params);
        Ok(url)
    }

    fn models_url(&self) -> Result<(reqwest::Url, bool)> {
        if let Some(catalog_url) = &self.model_catalog_url {
            let mut url = reqwest::Url::parse(catalog_url)
                .context("provider model catalog URL is invalid")?;
            append_query_params(&mut url, &self.query_params);
            url.query_pairs_mut()
                .append_pair("client_version", auth::codex_cli_version());
            // Codex's explicit model catalog URL is a complete URL and rejects
            // redirects, even when a redirect would stay on the same origin.
            return Ok((url, false));
        }
        let mut url = self.url_for_path("models")?;
        url.query_pairs_mut()
            .append_pair("client_version", auth::codex_cli_version());
        Ok((url, true))
    }
}

fn append_query_params(url: &mut reqwest::Url, params: &BTreeMap<String, String>) {
    if params.is_empty() {
        return;
    }
    let mut query = url.query_pairs_mut();
    for (name, value) in params {
        query.append_pair(name, value);
    }
}

fn string_table(value: Option<&toml::Value>, field: &str) -> Result<BTreeMap<String, String>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let table = value
        .as_table()
        .with_context(|| format!("provider {field} must be a table of strings"))?;
    table
        .iter()
        .map(|(name, value)| {
            let value = value
                .as_str()
                .with_context(|| format!("provider {field} values must be strings"))?;
            Ok((name.clone(), value.to_string()))
        })
        .collect()
}

fn provider_override_value(raw: &str) -> toml::Value {
    toml::from_str::<toml::Value>(&format!("value = {raw}"))
        .ok()
        .and_then(|value| value.get("value").cloned())
        .unwrap_or_else(|| toml::Value::String(raw.to_string()))
}

fn validate_provider_override_shape(provider_id: &str, raw_key: &str) -> Result<()> {
    // A whole model_providers table or a whole table for this provider cannot
    // be remapped safely into the per-run cs_* provider. Require dotted leaf
    // overrides so fetch/probe and launch can resolve the same fields.
    let key = raw_key.trim().replace('"', "");
    if key == "model_providers" || key == format!("model_providers.{provider_id}") {
        anyhow::bail!(
            "full-table provider overrides are not supported; use dotted leaf keys such as model_providers.{provider_id}.base_url"
        );
    }
    Ok(())
}

fn resolve_provider_connection(profile: &ProviderProfile) -> Result<ProviderHttpConfig> {
    resolve_provider_connection_from_parts(
        &profile.base_url,
        &profile.api_key,
        &profile.env_key,
        profile.allow_insecure_http,
        &profile.wire_api,
        &profile.provider_id,
        &profile.codex_config,
    )
}

fn resolve_provider_connection_from_parts(
    base_url: &str,
    stored_api_key: &str,
    default_env_key: &str,
    allow_insecure_http: bool,
    wire_api: &str,
    provider_id: &str,
    codex_config: &[String],
) -> Result<ProviderHttpConfig> {
    let provider_id = if provider_id.is_empty() {
        "provider"
    } else {
        provider_id
    };
    for entry in codex_config {
        if let Some((key, _)) = entry.split_once('=') {
            validate_provider_override_shape(provider_id, key)?;
        }
    }
    let prefix = format!("model_providers.{provider_id}.");
    let mut config = toml::map::Map::new();
    for (key, value) in [
        ("base_url", toml::Value::String(base_url.to_string())),
        ("env_key", toml::Value::String(default_env_key.to_string())),
        ("wire_api", toml::Value::String(wire_api.to_string())),
        ("http_headers", toml::Value::Table(toml::map::Map::new())),
        (
            "env_http_headers",
            toml::Value::Table(toml::map::Map::new()),
        ),
        ("query_params", toml::Value::Table(toml::map::Map::new())),
    ] {
        insert_toml_override(
            &mut config,
            &format!("model_providers.{provider_id}.{key}"),
            value,
        )?;
    }
    for entry in codex_config {
        let Some((key, raw_value)) = entry.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if let Some(suffix) = key.strip_prefix(&prefix)
            && !suffix.is_empty()
        {
            insert_toml_override(
                &mut config,
                &format!("model_providers.{provider_id}.{suffix}"),
                provider_override_value(raw_value.trim()),
            )?;
        }
    }
    let provider = config
        .get("model_providers")
        .and_then(toml::Value::as_table)
        .and_then(|providers| providers.get(provider_id))
        .and_then(toml::Value::as_table)
        .context("provider connection settings could not be resolved")?;
    let string_field = |field: &str, default: &str| -> Result<String> {
        provider
            .get(field)
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .with_context(|| format!("provider {field} must be a string"))
            })
            .unwrap_or_else(|| Ok(default.to_string()))
    };
    let base_url = string_field("base_url", base_url)?;
    let wire_api = string_field("wire_api", wire_api)?;
    if wire_api != "responses" {
        anyhow::bail!("provider wire_api must be \"responses\" for Codex model requests");
    }
    validate_base_url(&base_url, allow_insecure_http)?;
    let model_catalog_url = provider
        .get("model_catalog_url")
        .map(|value| {
            value
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .context("provider model_catalog_url must be a URL string")
        })
        .transpose()?;
    if let Some(catalog_url) = &model_catalog_url {
        validate_base_url(catalog_url, allow_insecure_http)?;
    }

    let static_headers = string_table(provider.get("http_headers"), "http_headers")?;
    let env_headers = string_table(provider.get("env_http_headers"), "env_http_headers")?;
    let query_params = string_table(provider.get("query_params"), "query_params")?;
    let env_key = string_field("env_key", default_env_key)?;
    if provider.get("auth").is_some() || provider.get("aws").is_some() {
        anyhow::bail!(
            "provider command or AWS authentication cannot be resolved for model sync or probe"
        );
    }
    if env_key.is_empty() && !default_env_key.is_empty() {
        anyhow::bail!("provider env_key is empty");
    }
    let api_key = if default_env_key.is_empty() && env_key.is_empty() {
        (!stored_api_key.is_empty()).then(|| stored_api_key.to_string())
    } else if env_key.is_empty() {
        None
    } else if env_key == default_env_key {
        (!stored_api_key.is_empty()).then(|| stored_api_key.to_string())
    } else {
        Some(
            std::env::var(&env_key)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .with_context(|| {
                    format!("provider API key environment variable '{env_key}' is not set")
                })?,
        )
    };
    let experimental_bearer = provider
        .get("experimental_bearer_token")
        .and_then(toml::Value::as_str)
        .filter(|value| !value.is_empty());
    if api_key.is_some() && experimental_bearer.is_some() {
        anyhow::bail!("provider env_key and experimental_bearer_token cannot both be configured");
    }
    let api_key = api_key.or_else(|| experimental_bearer.map(str::to_string));

    // Keep the configured string once `HeaderValue::try_from` has accepted it.
    // Reading it back with `HeaderValue::to_str` would reject the non-ASCII
    // (obs-text) values that `try_from` allows, and `provider_http_headers`
    // rebuilds the value from this same string.
    let mut headers = BTreeMap::new();
    for (name, value) in static_headers {
        if let (Ok(name), Ok(_)) = (
            reqwest::header::HeaderName::try_from(name.as_str()),
            reqwest::header::HeaderValue::try_from(value.as_str()),
        ) {
            headers.insert(name.as_str().to_string(), value);
        }
    }
    for (name, env_name) in env_headers {
        let value = if env_name == default_env_key && !stored_api_key.is_empty() {
            Some(stored_api_key.to_string())
        } else {
            std::env::var(&env_name).ok()
        };
        if let Some(value) = value
            && !value.trim().is_empty()
            && let (Ok(name), Ok(_)) = (
                reqwest::header::HeaderName::try_from(name.as_str()),
                reqwest::header::HeaderValue::try_from(value.as_str()),
            )
        {
            headers.insert(name.as_str().to_string(), value);
        }
    }
    // EndpointSession applies its AuthProvider after provider headers, so the
    // configured env key is authoritative when both define Authorization.
    if let Some(key) = &api_key {
        let value = format!("Bearer {key}");
        reqwest::header::HeaderValue::try_from(value.as_str())
            .context("provider API key cannot be used as an HTTP header")?;
        headers.insert(reqwest::header::AUTHORIZATION.as_str().to_string(), value);
    }

    Ok(ProviderHttpConfig {
        base_url,
        model_catalog_url,
        wire_api,
        allow_insecure_http,
        api_key,
        stored_api_key: stored_api_key.to_string(),
        headers,
        query_params,
    })
}

fn display_safe_url(url: &reqwest::Url) -> String {
    let mut safe = url.clone();
    let _ = safe.set_username("");
    let _ = safe.set_password(None);
    // Provider URLs can contain credentials in a path segment as well as in
    // userinfo or the query (for example, a gateway token in the path). Keep
    // diagnostics to the origin so none of those parts reach errors or logs.
    safe.set_path("/");
    safe.set_query(None);
    safe.set_fragment(None);
    safe.to_string().trim_end_matches('/').to_string()
}

fn safe_fallback_source(source: &str) -> String {
    match reqwest::Url::parse(source) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => display_safe_url(&url),
        _ => source.to_string(),
    }
}

fn redact_connection_secrets(value: &str, connection: &ProviderHttpConfig) -> String {
    connection
        .secrets()
        .into_iter()
        .fold(value.to_string(), |message, secret| {
            message.replace(&secret, "[redacted]")
        })
}

fn provider_http_headers(
    connection: &ProviderHttpConfig,
    with_json_content_type: bool,
) -> Result<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in &connection.headers {
        let name = reqwest::header::HeaderName::try_from(name.as_str())
            .context("provider contains an invalid HTTP header name")?;
        let value = reqwest::header::HeaderValue::try_from(value.as_str())
            .context("provider contains an invalid HTTP header value")?;
        headers.insert(name, value);
    }
    if with_json_content_type && !headers.contains_key(reqwest::header::CONTENT_TYPE) {
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
    }
    Ok(headers)
}

async fn read_limited_response_body(
    mut response: reqwest::Response,
    max_bytes: usize,
    description: &str,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        anyhow::bail!("{description} exceeds the {max_bytes}-byte response limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| anyhow::anyhow!("reading {description}: {}", error.without_url()))?
    {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            anyhow::bail!("{description} exceeds the {max_bytes}-byte response limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn same_origin_redirect_policy(url: &reqwest::Url) -> reqwest::redirect::Policy {
    let origin = url.origin();
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 10 || attempt.url().origin() != origin {
            attempt.stop()
        } else {
            attempt.follow()
        }
    })
}

/// `GET {base_url}/models` with the provider key for an explicit sync action.
pub(crate) async fn fetch_gateway_models(profile: &ProviderProfile) -> Result<Vec<RemoteModel>> {
    let models = fetch_gateway_models_for_profile(profile).await?;
    debug!(
        "provider '{}' gateway model catalog returned {} entries",
        profile.alias,
        models.len()
    );
    Ok(models)
}

pub(crate) async fn fetch_gateway_models_for_profile(
    profile: &ProviderProfile,
) -> Result<Vec<RemoteModel>> {
    let connection = resolve_provider_connection(profile)?;
    fetch_gateway_models_with_connection(&connection).await
}

pub(crate) async fn fetch_gateway_models_with_overrides(
    base_url: &str,
    api_key: &str,
    default_env_key: &str,
    allow_insecure_http: bool,
    wire_api: &str,
    provider_id: &str,
    codex_config: &[String],
) -> Result<Vec<RemoteModel>> {
    let connection = resolve_provider_connection_from_parts(
        base_url,
        api_key,
        default_env_key,
        allow_insecure_http,
        wire_api,
        provider_id,
        codex_config,
    )?;
    fetch_gateway_models_with_connection(&connection).await
}

async fn fetch_gateway_models_with_connection(
    connection: &ProviderHttpConfig,
) -> Result<Vec<RemoteModel>> {
    let (url, allow_same_origin_redirects) = connection.models_url()?;
    let display_url = redact_connection_secrets(&display_safe_url(&url), connection);
    let client = auth::build_http_client_with_redirect_policy(if allow_same_origin_redirects {
        same_origin_redirect_policy(&url)
    } else {
        reqwest::redirect::Policy::none()
    })?;
    let headers = provider_http_headers(connection, false)?;
    let response = client
        .get(url.clone())
        .headers(headers)
        .timeout(GATEWAY_MODELS_TIMEOUT)
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("GET {display_url}: {}", error.without_url()))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("GET {display_url} returned {status}");
    }
    let max_bytes = if connection.model_catalog_url.is_some() {
        MAX_PROVIDER_CATALOG_BODY_BYTES
    } else {
        MAX_GATEWAY_MODELS_BODY_BYTES
    };
    let body = read_limited_response_body(response, max_bytes, "provider model catalog").await?;
    let value: serde_json::Value =
        serde_json::from_slice(&body).context("parsing provider model catalog JSON")?;
    Ok(parse_gateway_models(&value))
}

pub(crate) async fn fetch_gateway_models_at(
    base_url: &str,
    api_key: &str,
    allow_insecure_http: bool,
) -> Result<Vec<RemoteModel>> {
    fetch_gateway_models_with_overrides(
        base_url,
        api_key,
        "",
        allow_insecure_http,
        DEFAULT_WIRE_API,
        "provider",
        &[],
    )
    .await
}

/// Same as [`fetch_gateway_models_with_overrides`] from a sync caller
/// (CLI add, TUI `f`).
pub(crate) fn fetch_gateway_models_overrides_blocking(
    base_url: &str,
    api_key: &str,
    default_env_key: &str,
    allow_insecure_http: bool,
    wire_api: &str,
    provider_id: &str,
    codex_config: &[String],
) -> Result<Vec<RemoteModel>> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| {
            handle.block_on(fetch_gateway_models_with_overrides(
                base_url,
                api_key,
                default_env_key,
                allow_insecure_http,
                wire_api,
                provider_id,
                codex_config,
            ))
        }),
        Err(_) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("starting runtime for gateway model catalog")?;
            runtime.block_on(fetch_gateway_models_with_overrides(
                base_url,
                api_key,
                default_env_key,
                allow_insecure_http,
                wire_api,
                provider_id,
                codex_config,
            ))
        }
    }
}

/// Same as [`fetch_gateway_models_at`] from a sync caller (legacy callers).
pub(crate) fn fetch_gateway_models_blocking(
    base_url: &str,
    api_key: &str,
    allow_insecure_http: bool,
) -> Result<Vec<RemoteModel>> {
    fetch_gateway_models_overrides_blocking(
        base_url,
        api_key,
        "",
        allow_insecure_http,
        DEFAULT_WIRE_API,
        "provider",
        &[],
    )
}

pub(crate) async fn fetch_fallback_models(source: &str) -> Result<Vec<RemoteModel>> {
    let source = source.trim();
    if is_none_fallback(source) {
        return Ok(Vec::new());
    }
    if source.starts_with("http://") || source.starts_with("https://") {
        let models = fetch_models_url(source, None).await?;
        debug!(
            "metadata fallback GET {} returned {} entries",
            safe_fallback_source(source),
            models.len()
        );
        return Ok(models);
    }
    let body = std::fs::read_to_string(source)
        .with_context(|| format!("reading metadata fallback {}", source))?;
    let value: serde_json::Value =
        serde_json::from_str(&body).context("parsing metadata fallback JSON")?;
    Ok(parse_gateway_models(&value))
}

async fn fetch_models_url(url: &str, bearer: Option<&str>) -> Result<Vec<RemoteModel>> {
    let client = auth::build_http_client()?;
    let parsed_url = reqwest::Url::parse(url).context("model catalog URL is invalid")?;
    let display_url = display_safe_url(&parsed_url);
    let mut request = client
        .get(parsed_url.clone())
        .timeout(GATEWAY_MODELS_TIMEOUT);
    if let Some(key) = bearer {
        request = request.header("Authorization", format!("Bearer {key}"));
    }
    let response = request
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("GET {display_url}: {}", error.without_url()))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("GET {display_url} returned {status}");
    }
    let body = read_limited_response_body(response, MAX_GATEWAY_MODELS_BODY_BYTES, "model catalog")
        .await?;
    let value: serde_json::Value =
        serde_json::from_slice(&body).context("parsing model catalog JSON")?;
    Ok(parse_gateway_models(&value))
}

/// Whether `{base_url}/responses` will accept this slug for Codex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResponsesSupport {
    /// The Responses handler ran (typically HTTP 400 missing `input`).
    Supported,
    /// Gateway listed the slug, but POSTing `/responses` 404s (Chat Completions only).
    Unsupported,
    /// Auth, rate limit, transport, or an unclassified status. Do not block launch.
    Unknown,
}

impl ResponsesSupport {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }
}

/// Result of a zero-token Responses probe: `POST {base}/responses` with only `model`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResponsesProbe {
    pub model: String,
    pub url: String,
    pub support: ResponsesSupport,
    pub status: u16,
    pub code: Option<String>,
    pub message: String,
}

impl ResponsesProbe {
    pub(crate) fn summary(&self) -> String {
        match (&self.code, self.message.is_empty()) {
            (Some(code), false) => format!("{} {}: {}", self.status, code, self.message),
            (Some(code), true) => format!("{} {code}", self.status),
            (None, false) => format!("{} {}", self.status, self.message),
            (None, true) => self.status.to_string(),
        }
    }

    pub(crate) fn refusal_message(&self, alias: &str) -> String {
        format!(
            "Model '{}' on provider '{alias}' has no Codex Responses channel. \
             POST {} returned {}. Chat Completions may still work, but current \
             Codex only speaks /responses. Probe saved models with \
             `codex-switch provider probe {alias}`.",
            self.model,
            self.url,
            self.summary()
        )
    }

    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "model": self.model,
            "url": self.url,
            "support": self.support.as_str(),
            "status": self.status,
            "code": self.code,
            "message": self.message,
        })
    }
}

/// `POST {base_url}/responses` with `{"model": slug}` and no `input`.
///
/// A supporting Responses handler rejects that at validation (HTTP 400) without
/// generating tokens. New API returns 404 `bad_response_status_code` when the
/// slug exists only as Chat Completions. Never send `input`: a 200 would bill.
pub(crate) async fn probe_responses_support(
    base_url: &str,
    api_key: &str,
    model: &str,
    allow_insecure_http: bool,
) -> Result<ResponsesProbe> {
    let connection = resolve_provider_connection_from_parts(
        base_url,
        api_key,
        "",
        allow_insecure_http,
        DEFAULT_WIRE_API,
        "provider",
        &[],
    )?;
    probe_responses_support_with_connection(&connection, model).await
}

async fn probe_responses_support_with_connection(
    connection: &ProviderHttpConfig,
    model: &str,
) -> Result<ResponsesProbe> {
    let url = connection.url_for_path("responses")?;
    let display_url = redact_connection_secrets(&display_safe_url(&url), connection);
    let client = auth::build_http_client_with_redirect_policy(same_origin_redirect_policy(&url))?;
    let headers = provider_http_headers(connection, true)?;
    let body = serde_json::to_vec(&serde_json::json!({ "model": model }))
        .context("serializing Responses probe")?;
    let response = client
        .post(url.clone())
        .headers(headers)
        .timeout(GATEWAY_MODELS_TIMEOUT)
        .body(body)
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("POST {display_url}: {}", error.without_url()))?;
    let status = response.status().as_u16();
    let body = read_limited_response_body(
        response,
        MAX_RESPONSES_PROBE_BODY_BYTES,
        "Responses probe body",
    )
    .await?;
    let body_text = String::from_utf8_lossy(&body);
    let (message, error_type, code) = openai_error_fields(&body_text);
    let message = message.map(|message| redact_connection_secrets(&message, connection));
    let code = code.map(|code| redact_connection_secrets(&code, connection));
    let support = classify_responses_probe(
        status,
        code.as_deref(),
        error_type.as_deref(),
        message.as_deref(),
    );
    debug!(
        model,
        status,
        support = support.as_str(),
        code = code.as_deref().unwrap_or(""),
        "responses probe"
    );
    Ok(ResponsesProbe {
        model: model.to_string(),
        url: display_url,
        support,
        status,
        code,
        message: message.unwrap_or_default(),
    })
}

pub(crate) async fn probe_provider_models(
    profile: &ProviderProfile,
    model: Option<&str>,
) -> Result<Vec<ResponsesProbe>> {
    let connection = resolve_provider_connection(profile)?;
    let slugs: Vec<String> = match model {
        Some(id) => {
            let selected = profile.resolve_model(Some(id))?;
            vec![selected.id.clone()]
        }
        None => profile.models.iter().map(|m| m.id.clone()).collect(),
    };
    let mut results = Vec::with_capacity(slugs.len());
    for slug in slugs {
        results.push(probe_responses_support_with_connection(&connection, &slug).await?);
    }
    Ok(results)
}

/// Re-check a saved "unsupported" verdict before a launch is refused because of
/// it. Returns `true` only when a live probe confirms the denial.
///
/// A live "supported" answer clears the denial. An inconclusive answer or a
/// failed request (network, timeout, DNS, TLS) fails open: the saved verdict is
/// dropped so it stops triggering, and a warning names the reason on stderr.
/// The refreshed verdict is persisted only while the provider's connection
/// still matches the one that was probed.
pub(crate) async fn recheck_cached_responses_denial(
    profile: &ProviderProfile,
    model: &str,
) -> Result<bool> {
    let connection = resolve_provider_connection(profile)?;
    let fingerprint = connection.fingerprint();
    let probe = match probe_responses_support_with_connection(&connection, model).await {
        Ok(probe) => probe,
        Err(error) => {
            let message = redact_connection_secrets(&format!("{error:#}"), &connection);
            eprintln!(
                "Warning: the saved probe marked model '{model}' on provider '{}' unsupported, but a fresh check failed ({message}); launching anyway.",
                profile.alias
            );
            forget_responses_verdict(&profile.alias, model, &fingerprint);
            return Ok(false);
        }
    };
    let confirmed = probe.support == ResponsesSupport::Unsupported;
    if probe.support == ResponsesSupport::Unknown {
        eprintln!(
            "Warning: the saved probe marked model '{model}' on provider '{}' unsupported, but a fresh check was inconclusive (HTTP {}); launching anyway.",
            profile.alias, probe.status
        );
    }
    if let Err(error) = store_responses_probe(&profile.alias, &fingerprint, &probe) {
        eprintln!(
            "Warning: could not save the refreshed probe result for provider '{}': {error:#}",
            profile.alias
        );
    }
    Ok(confirmed)
}

fn forget_responses_verdict(alias: &str, model: &str, fingerprint: &str) {
    let unknown = ResponsesProbe {
        model: model.to_string(),
        url: String::new(),
        support: ResponsesSupport::Unknown,
        status: 0,
        code: None,
        message: String::new(),
    };
    if let Err(error) = store_responses_probe(alias, fingerprint, &unknown) {
        eprintln!(
            "Warning: could not drop the stale probe result for provider '{alias}': {error:#}"
        );
    }
}

/// Apply one probe result to the freshest saved profile. This deliberately
/// bypasses [`save`]: that function restores saved probe records whenever the
/// incoming profile has none, which would resurrect a verdict just removed.
fn store_responses_probe(alias: &str, fingerprint: &str, probe: &ResponsesProbe) -> Result<()> {
    let mut latest = load(alias)?;
    if latest.responses_support_fingerprint()? != fingerprint {
        // The connection changed while probing; the result no longer applies.
        return Ok(());
    }
    latest.record_responses_probes(std::slice::from_ref(probe));
    latest.normalize();
    latest.validate()?;
    existing_provider_dir(alias)?;
    write_profile(&provider_dir(alias)?.join("provider.toml"), &latest)
}

fn openai_error_fields(body: &str) -> (Option<String>, Option<String>, Option<String>) {
    let trimmed = body.trim();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        if trimmed.is_empty() {
            return (None, None, None);
        }
        let preview: String = trimmed.chars().take(200).collect();
        return (Some(preview), None, None);
    };
    let err = match value.get("error") {
        Some(err) => err,
        None => return (None, None, None),
    };
    if let Some(message) = err.as_str() {
        let code = value
            .get("code")
            .and_then(|c| c.as_str().map(str::to_string));
        return (Some(message.to_string()), None, code);
    }
    let message = err
        .get("message")
        .and_then(|m| m.as_str())
        .map(str::to_string);
    let error_type = err.get("type").and_then(|t| t.as_str()).map(str::to_string);
    let code = err.get("code").and_then(|c| {
        c.as_str()
            .map(str::to_string)
            .or_else(|| c.as_i64().map(|n| n.to_string()))
    });
    (message, error_type, code)
}

fn classify_responses_probe(
    status: u16,
    code: Option<&str>,
    error_type: Option<&str>,
    message: Option<&str>,
) -> ResponsesSupport {
    let blob = format!(
        "{} {} {}",
        code.unwrap_or(""),
        error_type.unwrap_or(""),
        message.unwrap_or("")
    )
    .to_ascii_lowercase();
    if status == 200 {
        return ResponsesSupport::Supported;
    }
    if matches!(status, 400 | 422)
        && ((blob.contains("missing_required_parameter") && blob.contains("input"))
            || (blob.contains("missing required parameter") && blob.contains("input"))
            || blob.contains("input is required")
            || blob.contains("required field: input"))
    {
        return ResponsesSupport::Supported;
    }

    // A model lookup failure does not establish anything about the Responses
    // route. Many gateways report it as HTTP 404, so it must remain unknown.
    let model_unavailable = blob.contains("model_not_found")
        || blob.contains("model not found")
        || blob.contains("unknown model")
        || blob.contains("model does not exist");
    if model_unavailable {
        return ResponsesSupport::Unknown;
    }

    let endpoint_unavailable = blob.contains("unsupported_endpoint")
        || blob.contains("endpoint_not_found")
        || blob.contains("route_not_found")
        || blob.contains("method_not_allowed")
        || blob.contains("not_implemented")
        || ((blob.contains("cannot post")
            || blob.contains("no route")
            || blob.contains("unknown route"))
            && blob.contains("/responses"));
    if (status == 404 && endpoint_unavailable) || status == 405 || status == 501 {
        return ResponsesSupport::Unsupported;
    }
    ResponsesSupport::Unknown
}

async fn load_metadata_fallback(
    profile: &ProviderProfile,
    primary: &[RemoteModel],
) -> Vec<RemoteModel> {
    let source = metadata_fallback_source(profile);
    if !needs_metadata_fallback(
        &profile.saved_model_slugs(&profile.default_model),
        primary,
        &profile.base_url,
        &source,
    ) {
        return Vec::new();
    }
    match fetch_fallback_models(&source).await {
        Ok(models) => models,
        Err(err) => {
            debug!(
                "metadata fallback unavailable ({}): {err:#}",
                safe_fallback_source(&source)
            );
            Vec::new()
        }
    }
}

fn needs_metadata_fallback(
    saved: &[String],
    primary: &[RemoteModel],
    base_url: &str,
    fallback_source: &str,
) -> bool {
    if is_none_fallback(fallback_source) {
        return false;
    }
    if same_models_endpoint(base_url, fallback_source) {
        return false;
    }
    select_catalog_slugs(saved).iter().any(|slug| {
        find_exact_model(primary, slug)
            .and_then(|model| model.context_window)
            .is_none()
    })
}

/// OpenAI `{data:[{id,…}]}`, OpenRouter extras (`name`, `context_length`), or
/// Codex `{models:[{slug,…}]}`. Unrecognized bodies yield an empty list.
fn parse_gateway_models(body: &serde_json::Value) -> Vec<RemoteModel> {
    if let Some(data) = body.get("data").and_then(serde_json::Value::as_array) {
        return data.iter().filter_map(parse_openai_model).collect();
    }
    if let Some(models) = body.get("models").and_then(serde_json::Value::as_array) {
        return models.iter().filter_map(parse_named_model).collect();
    }
    Vec::new()
}

fn parse_openai_model(item: &serde_json::Value) -> Option<RemoteModel> {
    let slug = nonempty_slug(item.get("id").and_then(serde_json::Value::as_str))?;
    Some(remote_from_item(slug, item))
}

fn parse_named_model(item: &serde_json::Value) -> Option<RemoteModel> {
    let slug = nonempty_slug(item.get("slug").and_then(serde_json::Value::as_str))
        .or_else(|| nonempty_slug(item.get("id").and_then(serde_json::Value::as_str)))?;
    Some(remote_from_item(slug, item))
}

fn nonempty_slug(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|slug| !slug.is_empty())
}

fn json_positive_i64(value: &serde_json::Value) -> Option<i64> {
    let parsed = value.as_i64().or_else(|| {
        value
            .as_u64()
            .and_then(|n| i64::try_from(n).ok())
            .or_else(|| {
                value.as_f64().and_then(|n| {
                    (n.is_finite() && n > 0.0 && n <= i64::MAX as f64).then_some(n as i64)
                })
            })
    })?;
    (parsed > 0).then_some(parsed)
}

fn remote_from_item(slug: &str, item: &serde_json::Value) -> RemoteModel {
    let display_name = item
        .get("name")
        .or_else(|| item.get("display_name"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let description = item
        .get("description")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let context_window = item
        .get("context_length")
        .or_else(|| item.get("context_window"))
        .or_else(|| item.pointer("/top_provider/context_length"))
        .and_then(json_positive_i64);
    RemoteModel {
        slug: slug.to_string(),
        display_name,
        description,
        context_window,
        input_modalities: parse_input_modalities(item),
        catalog_entry: is_codex_catalog_entry(item)
            .then(|| item.as_object().cloned())
            .flatten(),
    }
}

fn is_codex_catalog_entry(item: &serde_json::Value) -> bool {
    item.get("slug")
        .and_then(serde_json::Value::as_str)
        .is_some()
        && (item.get("model_messages").is_some()
            || item.get("base_instructions").is_some()
            || item.get("supported_reasoning_levels").is_some())
}

fn parse_input_modalities(item: &serde_json::Value) -> Vec<String> {
    let Some(raw) = item
        .pointer("/architecture/input_modalities")
        .or_else(|| item.get("input_modalities"))
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for value in raw {
        let Some(name) = value.as_str() else {
            continue;
        };
        if matches!(name, "text" | "image" | "audio")
            && !out.iter().any(|existing| existing == name)
        {
            out.push(name.to_string());
        }
    }
    out
}

fn find_exact_model<'a>(models: &'a [RemoteModel], slug: &str) -> Option<&'a RemoteModel> {
    models.iter().find(|model| model.slug == slug)
}

/// Strip an OpenRouter `:variant` suffix (`z-ai/glm-5.3-flash:free` →
/// `z-ai/glm-5.3-flash`). Bare slugs without a vendor prefix are left alone.
fn openrouter_base_id(id: &str) -> &str {
    match id.rsplit_once(':') {
        Some((base, variant)) if !variant.contains('/') && base.contains('/') => base,
        _ => id,
    }
}

/// Match a provider slug against OpenRouter ids: exact, then unique
/// `vendor/{slug}`, preferring a row without a `:variant`. Ambiguous matches
/// (two vendors, same model name) return none rather than guessing.
fn lookup_fallback_model<'a>(models: &'a [RemoteModel], slug: &str) -> Option<&'a RemoteModel> {
    if let Some(model) = find_exact_model(models, slug) {
        return Some(model);
    }
    let suffix = format!("/{slug}");
    let matches: Vec<&RemoteModel> = models
        .iter()
        .filter(|model| {
            let base = openrouter_base_id(&model.slug);
            base == slug || base.ends_with(&suffix)
        })
        .collect();
    if matches.is_empty() {
        return None;
    }
    if matches.len() == 1 {
        return Some(matches[0]);
    }
    let no_variant: Vec<&RemoteModel> = matches
        .iter()
        .copied()
        .filter(|model| openrouter_base_id(&model.slug) == model.slug)
        .collect();
    if no_variant.len() == 1 {
        return Some(no_variant[0]);
    }
    let bases: HashSet<&str> = matches
        .iter()
        .map(|model| openrouter_base_id(&model.slug))
        .collect();
    if bases.len() == 1 {
        return no_variant.into_iter().next().or(Some(matches[0]));
    }
    None
}

fn overlay_remote_metadata(
    slug: &str,
    primary: &[RemoteModel],
    fallback: &[RemoteModel],
) -> Option<RemoteModel> {
    let primary = find_exact_model(primary, slug);
    let fallback = lookup_fallback_model(fallback, slug);
    if primary.is_none() && fallback.is_none() {
        return None;
    }
    let pick_text = |primary: Option<&String>, fallback: Option<&String>| {
        primary
            .map(String::as_str)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                fallback
                    .map(String::as_str)
                    .filter(|value| !value.is_empty())
            })
            .map(str::to_string)
    };
    let primary_modalities = primary
        .map(|model| model.input_modalities.as_slice())
        .unwrap_or(&[]);
    let fallback_modalities = fallback
        .map(|model| model.input_modalities.as_slice())
        .unwrap_or(&[]);
    let mut catalog_entry = fallback
        .and_then(|model| model.catalog_entry.as_ref())
        .cloned()
        .unwrap_or_default();
    if let Some(primary_entry) = primary.and_then(|model| model.catalog_entry.as_ref()) {
        catalog_entry.extend(primary_entry.clone());
    }
    let display_name = pick_text(
        primary.and_then(|model| model.display_name.as_ref()),
        fallback.and_then(|model| model.display_name.as_ref()),
    );
    let description = pick_text(
        primary.and_then(|model| model.description.as_ref()),
        fallback.and_then(|model| model.description.as_ref()),
    );
    let context_window = primary
        .and_then(|model| model.context_window)
        .or_else(|| fallback.and_then(|model| model.context_window));
    let input_modalities = if primary_modalities.is_empty() {
        fallback_modalities.to_vec()
    } else {
        primary_modalities.to_vec()
    };
    if !catalog_entry.is_empty() && primary.is_some_and(|model| model.catalog_entry.is_none()) {
        if let Some(display_name) = &display_name {
            catalog_entry.insert("display_name".into(), display_name.clone().into());
        }
        if let Some(description) = &description {
            catalog_entry.insert("description".into(), description.clone().into());
        }
        if !input_modalities.is_empty() {
            catalog_entry.insert(
                "input_modalities".into(),
                serde_json::Value::Array(
                    input_modalities
                        .iter()
                        .cloned()
                        .map(serde_json::Value::from)
                        .collect(),
                ),
            );
        }
        if let Some(context_window) = primary.and_then(|model| model.context_window) {
            catalog_entry.insert("max_context_window".into(), context_window.into());
        }
    }
    Some(RemoteModel {
        slug: slug.to_string(),
        display_name,
        description,
        context_window,
        input_modalities,
        catalog_entry: (!catalog_entry.is_empty()).then_some(catalog_entry),
    })
}

fn select_catalog_slugs(saved: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for slug in saved {
        if !slug.is_empty() && !out.iter().any(|existing| existing == slug) {
            out.push(slug.clone());
        }
    }
    out
}

/// Embedding / reranker slugs cannot run Codex's Responses loop.
pub(crate) fn is_vector_model_slug(slug: &str) -> bool {
    slug.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|token| {
            matches!(
                token.to_ascii_lowercase().as_str(),
                "embed" | "embedding" | "embeddings" | "rerank" | "reranker" | "reranking"
            )
        })
}

/// Chat slugs a user can import from a gateway `/models` body.
/// Embedding and reranker ids are dropped. Size is not a fetch error:
/// wholesale import vs pick is decided by [`apply_fetched_models`].
pub(crate) fn chat_slugs_from_gateway(remote: &[RemoteModel]) -> Result<Vec<String>> {
    if remote.is_empty() {
        anyhow::bail!("gateway /models returned no models");
    }
    let mut out = Vec::new();
    for model in remote {
        if model.slug.is_empty() || is_vector_model_slug(&model.slug) {
            continue;
        }
        if !out.iter().any(|existing| existing == &model.slug) {
            out.push(model.slug.clone());
        }
    }
    if out.is_empty() {
        anyhow::bail!(
            "gateway /models listed only embedding/reranker ids; pass --model for a chat slug"
        );
    }
    Ok(out)
}

fn large_catalog_pick_message(slugs: &[String]) -> String {
    let preview: Vec<&str> = slugs.iter().take(3).map(String::as_str).collect();
    format!(
        "gateway listed {} chat models; pass --model to pick slugs (e.g. {})",
        slugs.len(),
        preview.join(", ")
    )
}

fn overlay_pick(existing: &[ProviderModel], pick: &ProviderModel) -> ProviderModel {
    let mut row = settings_for(existing, &pick.id);
    if pick.reasoning.is_some() {
        row.reasoning = pick.reasoning.clone();
    }
    if pick.no_web_search {
        row.no_web_search = true;
    }
    row
}

/// Keep only `picks` that exist on the gateway chat list. Used when the
/// catalog is too large to import wholesale.
pub(crate) fn apply_picked_models(
    existing: &[ProviderModel],
    current_default: Option<&str>,
    allowed: &[String],
    picks: &[ProviderModel],
) -> Result<(Vec<ProviderModel>, String)> {
    let mut models: Vec<ProviderModel> = Vec::new();
    for pick in picks {
        if pick.id.is_empty() {
            continue;
        }
        if !allowed.iter().any(|slug| slug == &pick.id) {
            anyhow::bail!("'{}' is not in gateway /models", pick.id);
        }
        if !models.iter().any(|row| row.id == pick.id) {
            models.push(overlay_pick(existing, pick));
        }
    }
    finish_model_list(models, current_default)
}

fn finish_model_list(
    models: Vec<ProviderModel>,
    current_default: Option<&str>,
) -> Result<(Vec<ProviderModel>, String)> {
    let default = current_default
        .filter(|id| models.iter().any(|model| model.id == *id))
        .map(str::to_string)
        .or_else(|| models.first().map(|model| model.id.clone()))
        .ok_or_else(|| anyhow::anyhow!("pass --model ID or --fetch-models"))?;
    Ok((models, default))
}

fn settings_for(existing: &[ProviderModel], slug: &str) -> ProviderModel {
    existing
        .iter()
        .find(|model| model.id == slug)
        .cloned()
        .unwrap_or_else(|| ProviderModel::from_id(slug))
}

/// Build the saved model list from a gateway fetch.
///
/// `prepend` (CLI `--model`) stays first and keeps its settings. Other gateway
/// chat slugs are appended. Matching ids reuse existing reasoning /
/// `no_web_search`. Default is `current_default` when still present, else the
/// first model in the result.
pub(crate) fn apply_fetched_models(
    existing: &[ProviderModel],
    current_default: Option<&str>,
    remote: &[RemoteModel],
    prepend: &[ProviderModel],
) -> Result<(Vec<ProviderModel>, String)> {
    let fetched = chat_slugs_from_gateway(remote)?;
    if fetched.len() > SMALL_REMOTE_CATALOG_LIMIT {
        if prepend.is_empty() {
            anyhow::bail!("{}", large_catalog_pick_message(&fetched));
        }
        return apply_picked_models(existing, current_default, &fetched, prepend);
    }
    let mut models: Vec<ProviderModel> = Vec::new();
    for model in prepend {
        if model.id.is_empty() {
            continue;
        }
        if !models.iter().any(|row| row.id == model.id) {
            models.push(overlay_pick(existing, model));
        }
    }
    for slug in &fetched {
        if !models.iter().any(|row| row.id == *slug) {
            models.push(settings_for(existing, slug));
        }
    }
    finish_model_list(models, current_default)
}

/// Replace `profile.models` with chat slugs from the gateway. Matching ids keep
/// their reasoning / `no_web_search`. The default stays if it is still listed.
pub(crate) async fn fetch_and_apply_models(
    profile: &mut ProviderProfile,
    picks: &[ProviderModel],
) -> Result<(usize, Vec<RemoteModel>)> {
    let remote = fetch_gateway_models(profile).await?;
    let (models, default) = apply_fetched_models(
        &profile.models,
        Some(profile.default_model.as_str()),
        &remote,
        picks,
    )?;
    let n = models.len();
    profile.models = models;
    profile.default_model = default;
    Ok((n, remote))
}

fn entry_context_window(
    slug: &str,
    default_slug: &str,
    user_context: Option<i64>,
    remote: Option<&RemoteModel>,
) -> i64 {
    let catalog = remote.and_then(|model| model.catalog_entry.as_ref());
    let resolved_context = remote
        .and_then(|model| model.context_window)
        .or_else(|| {
            catalog
                .and_then(|entry| entry.get("context_window"))
                .and_then(json_positive_i64)
        })
        .or_else(|| {
            catalog
                .and_then(|entry| entry.get("max_context_window"))
                .and_then(json_positive_i64)
        })
        .unwrap_or(DEFAULT_PROVIDER_CONTEXT_WINDOW);
    if slug != default_slug {
        return resolved_context;
    }
    let Some(value) = user_context else {
        return resolved_context;
    };
    let max = catalog
        .and_then(|entry| entry.get("max_context_window"))
        .and_then(json_positive_i64)
        .or_else(|| {
            remote.and_then(|model| {
                model
                    .catalog_entry
                    .is_none()
                    .then_some(model.context_window)
                    .flatten()
            })
        });
    max.map_or(value, |max| value.min(max))
}

/// A Codex `model_catalog_json` body. Native entries keep all upstream fields;
/// generated entries use the conservative fallback metadata understood by
/// Codex 0.159.2.
fn build_model_catalog(
    saved: &[String],
    models: &[ProviderModel],
    remote: &[RemoteModel],
    fallback: &[RemoteModel],
    default_slug: &str,
    user_context: Option<i64>,
    launch_reasoning: Option<&str>,
) -> serde_json::Value {
    let models_json: Vec<serde_json::Value> = select_catalog_slugs(saved)
        .iter()
        .enumerate()
        .map(|(index, slug)| {
            let owned = overlay_remote_metadata(slug, remote, fallback);
            let meta = owned.as_ref();
            let reasoning = if slug == default_slug {
                launch_reasoning
            } else {
                models
                    .iter()
                    .find(|model| model.id == *slug)
                    .and_then(|model| model.reasoning.as_deref())
            };
            catalog_entry(
                slug,
                entry_context_window(slug, default_slug, user_context, meta),
                reasoning,
                meta,
                i64::try_from(index).unwrap_or(i64::MAX),
            )
        })
        .collect();
    serde_json::json!({ "models": models_json })
}

fn thinking_effort(value: Option<&str>) -> Option<&str> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("none"))
}

fn apply_catalog_reasoning(entry: &mut serde_json::Value, reasoning: Option<&str>) -> Result<()> {
    let clear_default = reasoning.is_some() && thinking_effort(reasoning).is_none();
    apply_catalog_reasoning_with_clear(entry, reasoning, clear_default)
}

fn apply_catalog_reasoning_with_clear(
    entry: &mut serde_json::Value,
    reasoning: Option<&str>,
    clear_default: bool,
) -> Result<()> {
    let thinking = thinking_effort(reasoning);
    let object = entry
        .as_object_mut()
        .context("provider catalog model is not an object")?;
    match thinking {
        Some(effort) => {
            object.insert("default_reasoning_level".into(), effort.into());
        }
        None if clear_default => {
            object.remove("default_reasoning_level");
        }
        None => {}
    }
    Ok(())
}

fn catalog_entry(
    slug: &str,
    context_window: i64,
    reasoning: Option<&str>,
    meta: Option<&RemoteModel>,
    priority: i64,
) -> serde_json::Value {
    let display_name = meta
        .and_then(|model| model.display_name.as_deref())
        .filter(|value| !value.is_empty())
        .unwrap_or(slug);
    let description = meta
        .and_then(|model| model.description.as_deref())
        .filter(|value| !value.is_empty())
        .unwrap_or(display_name);
    if let Some(native_entry) = meta.and_then(|model| model.catalog_entry.as_ref()) {
        let mut entry = serde_json::Value::Object(native_entry.clone());
        let object = entry
            .as_object_mut()
            .expect("cloned catalog entry is an object");
        object.insert("slug".into(), slug.into());
        object.insert("priority".into(), priority.into());
        object.insert("context_window".into(), context_window.into());
        if !object.contains_key("display_name") {
            object.insert("display_name".into(), display_name.into());
        }
        if !object.contains_key("description") {
            object.insert("description".into(), description.into());
        }
        if !object.contains_key("input_modalities") {
            let modalities = meta
                .map(|model| model.input_modalities.clone())
                .filter(|values| !values.is_empty())
                .unwrap_or_else(|| vec!["text".to_string()]);
            object.insert("input_modalities".into(), modalities.into());
        }
        ensure_required_catalog_fields(object);
        ensure_catalog_instructions(object);
        apply_catalog_reasoning(&mut entry, reasoning).expect("native catalog entry is an object");
        return entry;
    }
    let modalities: Vec<String> = meta
        .map(|model| model.input_modalities.clone())
        .filter(|values| !values.is_empty())
        .unwrap_or_else(|| vec!["text".to_string()]);
    // The fallback context window is a conservative default, not an
    // authoritative limit. Keep the maximum unknown so a later explicit
    // context override is not capped by this synthesized default.
    let max_context_window = meta.and_then(|model| model.context_window);
    let instructions = CODEX_MODEL_INSTRUCTIONS;
    let mut entry = serde_json::json!({
        "slug": slug,
        "display_name": display_name,
        "description": description,
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": priority,
        "base_instructions": instructions,
        "model_messages": {"instructions_template": instructions},
        "default_reasoning_summary": "none",
        "supported_reasoning_levels": [],
        "support_verbosity": false,
        "supports_parallel_tool_calls": false,
        "supports_reasoning_summary_parameter": false,
        "supports_image_detail_original": false,
        "apply_patch_tool_type": null,
        "web_search_tool_type": "text",
        "truncation_policy": {"mode": "bytes", "limit": 10000},
        "context_window": context_window,
        "max_context_window": max_context_window,
        "effective_context_window_percent": 95,
        "experimental_supported_tools": [],
        "input_modalities": modalities,
    });
    apply_catalog_reasoning(&mut entry, reasoning).expect("catalog entry is an object");
    entry
}

const CODEX_MODEL_INSTRUCTIONS: &str =
    include_str!("../assets/upstream-codex/model-instructions.md");

fn ensure_catalog_instructions(object: &mut serde_json::Map<String, serde_json::Value>) {
    if object
        .get("base_instructions")
        .is_some_and(|value| !value.is_string() && !value.is_null())
    {
        object.remove("base_instructions");
    }
    let has_legacy_instructions = object
        .get("base_instructions")
        .is_some_and(serde_json::Value::is_string);
    if object
        .get("model_messages")
        .is_some_and(|messages| !messages.is_object() && !messages.is_null())
    {
        object.insert("model_messages".into(), serde_json::json!({}));
    }
    let has_message_instructions = object
        .get("model_messages")
        .and_then(|messages| messages.get("instructions_template"))
        .is_some_and(serde_json::Value::is_string);
    if has_legacy_instructions || has_message_instructions {
        if let Some(messages) = object
            .get_mut("model_messages")
            .and_then(serde_json::Value::as_object_mut)
            && messages
                .get("instructions_template")
                .is_some_and(|value| !value.is_string() && !value.is_null())
        {
            messages.remove("instructions_template");
        }
        return;
    }
    object.insert("base_instructions".into(), CODEX_MODEL_INSTRUCTIONS.into());
    let messages = object
        .entry("model_messages")
        .or_insert_with(|| serde_json::json!({}));
    if !messages.is_object() {
        *messages = serde_json::json!({});
    }
    messages
        .as_object_mut()
        .expect("model_messages is an object")
        .insert(
            "instructions_template".into(),
            CODEX_MODEL_INSTRUCTIONS.into(),
        );
}

fn ensure_required_catalog_fields(object: &mut serde_json::Map<String, serde_json::Value>) {
    if !object
        .get("display_name")
        .is_some_and(serde_json::Value::is_string)
    {
        let slug = object
            .get("slug")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("custom-model");
        object.insert("display_name".into(), slug.into());
    }
    if !object
        .get("shell_type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| {
            matches!(
                value,
                "unified_exec" | "shell_command" | "default" | "local" | "disabled"
            )
        })
    {
        object.insert("shell_type".into(), "shell_command".into());
    }
    if !object
        .get("visibility")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| matches!(value, "list" | "hide" | "none"))
    {
        object.insert("visibility".into(), "list".into());
    }
    if !object
        .get("supported_in_api")
        .is_some_and(serde_json::Value::is_boolean)
    {
        object.insert("supported_in_api".into(), true.into());
    }
    if !object
        .get("support_verbosity")
        .is_some_and(serde_json::Value::is_boolean)
    {
        object.insert("support_verbosity".into(), false.into());
    }
    if !object
        .get("supported_reasoning_levels")
        .is_some_and(serde_json::Value::is_array)
    {
        object.insert("supported_reasoning_levels".into(), serde_json::json!([]));
    } else if let Some(levels) = object
        .get_mut("supported_reasoning_levels")
        .and_then(serde_json::Value::as_array_mut)
    {
        levels.retain(|level| {
            level
                .get("effort")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|effort| !effort.is_empty())
                && level
                    .get("description")
                    .is_some_and(serde_json::Value::is_string)
        });
    }
    if !object
        .get("truncation_policy")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|policy| {
            policy
                .get("mode")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|mode| matches!(mode, "bytes" | "tokens"))
                && policy.get("limit").and_then(json_positive_i64).is_some()
        })
    {
        object.insert(
            "truncation_policy".into(),
            serde_json::json!({"mode": "bytes", "limit": 10000}),
        );
    }
    if !object
        .get("experimental_supported_tools")
        .is_some_and(serde_json::Value::is_array)
    {
        object.insert("experimental_supported_tools".into(), serde_json::json!([]));
    } else if let Some(tools) = object
        .get_mut("experimental_supported_tools")
        .and_then(serde_json::Value::as_array_mut)
    {
        tools.retain(serde_json::Value::is_string);
    }
    if !object
        .get("input_modalities")
        .is_some_and(serde_json::Value::is_array)
    {
        object.insert("input_modalities".into(), serde_json::json!(["text"]));
    } else if let Some(modalities) = object
        .get_mut("input_modalities")
        .and_then(serde_json::Value::as_array_mut)
    {
        modalities.retain(|modality| {
            modality
                .as_str()
                .is_some_and(|value| matches!(value, "text" | "image" | "audio"))
        });
    }
}

fn repair_legacy_generated_instructions(entry: &mut serde_json::Value) -> Result<()> {
    let object = entry
        .as_object_mut()
        .context("saved provider catalog model is not an object")?;
    let is_legacy_generated = object
        .get("base_instructions")
        .and_then(serde_json::Value::as_str)
        == Some("")
        && !object.contains_key("model_messages")
        && object
            .get("supports_reasoning_summaries")
            .is_some_and(serde_json::Value::is_boolean);
    if !is_legacy_generated {
        return Ok(());
    }
    object.insert("base_instructions".into(), CODEX_MODEL_INSTRUCTIONS.into());
    object.insert(
        "model_messages".into(),
        serde_json::json!({"instructions_template": CODEX_MODEL_INSTRUCTIONS}),
    );
    object.remove("supports_reasoning_summaries");
    Ok(())
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

/// Keys that provider `launch` supplies via `codex -c`. On legacy isolated
/// runs they are stripped from the copied user config; on native-profile runs
/// they are the only keys codex-switch owns in the run's `cs-*.config.toml`.
const PROVIDER_SESSION_KEYS: [&str; 6] = [
    "model",
    "model_provider",
    "model_reasoning_effort",
    "model_catalog_json",
    "model_providers",
    "web_search",
];

const USER_PROMPT_LINKS: [&str; 3] = ["AGENTS.md", "prompts", "skills"];

/// Legacy per-launch Codex home for a custom provider, kept to resume
/// sessions recorded before the native-profile layout existed. New launches
/// use [`ProviderLaunchProfile`], which shares the user's Codex home.
///
/// Each run directory links `prompts/`, `skills/`, and `AGENTS.md` to the user
/// home, copies non-model config (MCP, …), and three-way-merges those keys
/// back on exit.
pub(crate) struct ProviderCodexHome {
    pub path: PathBuf,
    user_config_path: PathBuf,
    base_config: Option<toml::Value>,
    restored: bool,
}

pub(crate) trait ProviderHomeInput {
    fn provider_home_identity(&self) -> (String, String);
}

impl ProviderHomeInput for &ProviderProfile {
    fn provider_home_identity(&self) -> (String, String) {
        (self.identity_id.clone(), self.alias.clone())
    }
}

impl ProviderHomeInput for &str {
    fn provider_home_identity(&self) -> (String, String) {
        load(self)
            .map(|profile| (profile.identity_id, profile.alias))
            .unwrap_or_else(|_| ((*self).to_string(), (*self).to_string()))
    }
}

impl ProviderCodexHome {
    pub(crate) fn begin<P: ProviderHomeInput>(provider: P) -> Result<Self> {
        let (identity_id, alias) = provider.provider_home_identity();
        let user_home = auth::user_codex_home()?;
        let user_config_path = user_home.join("config.toml");
        let base_config = load_toml_if_present(&user_config_path)?;
        let path = unique_run_dir(&identity_id)?;
        ensure_private_dir(&path)?;
        for name in USER_PROMPT_LINKS {
            link_user_entry(&user_home.join(name), &path.join(name))?;
        }
        if let Some(base) = &base_config {
            let mut live = base.clone();
            strip_provider_session_keys(&mut live);
            write_codex_config(&path.join("config.toml"), &live)?;
        }
        write_run_meta(
            &path,
            &ProviderRunMeta {
                provider_identity_id: identity_id,
                alias,
                model: None,
                cwd: std::env::current_dir().ok(),
                created_at: now_rfc3339(),
                codex_home: None,
                profile_name: None,
                runtime_provider_id: None,
                child_pid: None,
            },
        )?;
        Ok(Self {
            path,
            user_config_path,
            base_config,
            restored: false,
        })
    }

    pub(crate) fn open_existing(profile: &ProviderProfile, path: &Path) -> Result<Self> {
        let identity_root = auth::app_home()?
            .join("provider-runs")
            .join(&profile.identity_id);
        if !path.starts_with(&identity_root) || !path.is_dir() {
            anyhow::bail!(
                "provider session run {} is not part of provider '{}'",
                path.display(),
                profile.alias
            );
        }
        let user_home = auth::user_codex_home()?;
        let user_config_path = user_home.join("config.toml");
        let _config_lock = crate::profile::lock_codex_config_merge()?;
        let base_config = load_toml_if_present(&user_config_path)?;
        refresh_existing_run_config(&path.join("config.toml"), base_config.as_ref())?;
        drop(_config_lock);
        for name in USER_PROMPT_LINKS {
            link_user_entry(&user_home.join(name), &path.join(name))?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            user_config_path,
            base_config,
            restored: false,
        })
    }

    pub(crate) fn write_model(&self, profile: &ProviderProfile, model: &str) -> Result<()> {
        write_run_meta(
            &self.path,
            &ProviderRunMeta {
                provider_identity_id: profile.identity_id.clone(),
                alias: profile.alias.clone(),
                model: Some(model.to_string()),
                cwd: std::env::current_dir().ok(),
                created_at: now_rfc3339(),
                codex_home: None,
                profile_name: None,
                runtime_provider_id: None,
                child_pid: None,
            },
        )
    }

    pub(crate) fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        merge_isolated_config_into_user(
            &self.user_config_path,
            self.base_config.as_ref(),
            &self.path.join("config.toml"),
        )?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for ProviderCodexHome {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        if let Err(err) = merge_isolated_config_into_user(
            &self.user_config_path,
            self.base_config.as_ref(),
            &self.path.join("config.toml"),
        ) {
            tracing::error!(
                error = %err,
                path = %self.user_config_path.display(),
                "failed to merge Codex config after provider launch"
            );
        } else {
            self.restored = true;
        }
    }
}

fn unique_run_dir(alias: &str) -> Result<PathBuf> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    crate::profile::validate_alias(alias)?;
    Ok(auth::app_home()?
        .join("provider-runs")
        .join(alias)
        .join(format!("{}-{nanos}-{seq}", std::process::id())))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProviderRunMeta {
    provider_identity_id: String,
    alias: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<PathBuf>,
    created_at: String,
    /// Native-profile runs keep Codex in the user's home.  These fields are
    /// optional so history made by the former isolated-home layout remains
    /// readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codex_home: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    runtime_provider_id: Option<String>,
    /// Pid of the Codex child launched for a native run.  The launcher writes
    /// this after a successful spawn so a later resume can tell a still-alive
    /// Codex (launcher crashed or was killed, child orphaned and healthy) from
    /// a finished run.  A second Codex must never append to a live rollout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    child_pid: Option<u32>,
}

/// A provider launch backed by Codex's native configuration profile.
///
/// `codex_home` deliberately remains the user's normal home: MCP, skills,
/// plugins, hooks, auth and other Codex-owned resources are therefore loaded
/// exactly as they are for an Accounts launch.  Only model routing is written
/// to this launch's profile file.
pub(crate) struct ProviderLaunchProfile {
    pub path: PathBuf,
    pub codex_home: PathBuf,
    pub profile_name: String,
    pub runtime_provider_id: String,
    child_pid: Option<u32>,
    /// Fresh runs clean up after themselves unless a Codex child took them
    /// over; reopened runs never self-destruct.
    abandon_on_drop: bool,
    /// Fresh runs hold their resume lock for the launcher's whole lifetime so
    /// a concurrent sweep can never see them as unclaimed dead state.
    _lease: Option<ProviderRunLease>,
}

/// Native provider profiles are single CODEX_HOME filenames. Keep this
/// validation shared by resume and cleanup so damaged run metadata cannot
/// redirect a profile-file operation outside the Codex home.
fn is_valid_native_profile_name(name: &str) -> bool {
    name.starts_with("cs-")
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

impl ProviderLaunchProfile {
    pub(crate) fn begin(profile: &ProviderProfile) -> Result<Self> {
        let path = unique_run_dir(&profile.identity_id)?;
        ensure_private_dir(&path)?;
        let codex_home = auth::user_codex_home()?;
        let run_id = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("provider run has no valid directory name"))?;
        let identity = sanitize_provider_id(&profile.identity_id);
        let run = sanitize_provider_id(run_id);
        let profile_name = format!("cs-{identity}-{run}");
        let runtime_provider_id = format!("cs_{identity}_{run}");
        let launch = Self {
            _lease: Some(ProviderRunLease::acquire(&path)?),
            path,
            codex_home,
            profile_name,
            runtime_provider_id,
            child_pid: None,
            abandon_on_drop: true,
        };
        launch.write_meta(profile, None)?;
        Ok(launch)
    }

    pub(crate) fn open_existing(profile: &ProviderProfile, path: &Path) -> Result<Self> {
        validate_run_path(profile, path)?;
        let meta = read_run_meta(path)?.ok_or_else(|| {
            anyhow::anyhow!("provider session run {} has no metadata", path.display())
        })?;
        if meta.provider_identity_id != profile.identity_id {
            anyhow::bail!(
                "provider session run {} does not belong to provider '{}'",
                path.display(),
                profile.alias
            );
        }
        let (Some(codex_home), Some(profile_name), Some(runtime_provider_id)) =
            (meta.codex_home, meta.profile_name, meta.runtime_provider_id)
        else {
            anyhow::bail!(
                "provider session run {} uses the legacy isolated home",
                path.display()
            );
        };
        if !is_valid_native_profile_name(&profile_name) {
            anyhow::bail!("invalid native provider profile name in {}", path.display());
        }
        if !runtime_provider_id.starts_with("cs_")
            || !runtime_provider_id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        {
            anyhow::bail!("invalid native provider id in {}", path.display());
        }
        if !codex_home.is_absolute() {
            anyhow::bail!(
                "provider session home must be absolute: {}",
                codex_home.display()
            );
        }
        if let Some(pid) = meta.child_pid
            && pid_alive(pid)
        {
            anyhow::bail!(
                "provider session run {} still has a live Codex process (pid {pid}); \
                 resume it only after that Codex exits",
                path.display()
            );
        }
        Ok(Self {
            path: path.to_path_buf(),
            codex_home,
            profile_name,
            runtime_provider_id,
            // A live child was rejected above; a dead one is finished business.
            child_pid: None,
            abandon_on_drop: false,
            // The caller holds the resume lease across this reopen.
            _lease: None,
        })
    }

    pub(crate) fn config_file_path(&self) -> PathBuf {
        self.codex_home
            .join(format!("{}.config.toml", self.profile_name))
    }

    pub(crate) fn saved_config(&self) -> Result<Option<toml::Value>> {
        load_toml_if_present(&self.config_file_path())
    }

    /// Record the spawned Codex pid so a later resume can refuse to double a
    /// rollout the orphaned child is still writing.
    pub(crate) fn set_child_pid(
        &mut self,
        profile: &ProviderProfile,
        model: &str,
        pid: u32,
    ) -> Result<()> {
        self.child_pid = Some(pid);
        self.write_meta(profile, Some(model))
    }

    /// The Codex child owns this run now: keep the profile file and run
    /// directory for resume even if the launcher later fails or dies.
    pub(crate) fn disarm(&mut self) {
        self.abandon_on_drop = false;
    }

    /// The Codex child finished normally; clear its pid so a later pid reuse
    /// cannot make this finished run look alive to a resume.
    pub(crate) fn clear_child_pid(&mut self, profile: &ProviderProfile, model: &str) -> Result<()> {
        self.child_pid = None;
        self.write_meta(profile, Some(model))
    }

    pub(crate) fn is_native_run(path: &Path) -> Result<bool> {
        Ok(read_run_meta(path)?.is_some_and(|meta| {
            meta.codex_home.is_some()
                && meta.profile_name.is_some()
                && meta.runtime_provider_id.is_some()
        }))
    }

    pub(crate) fn write_config(
        &self,
        profile: &ProviderProfile,
        args: &[String],
        model: &str,
    ) -> Result<()> {
        let config_path = self.config_file_path();
        let mut value = load_toml_if_present(&config_path)?
            .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
        let table = value.as_table_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "provider profile config {} is not a TOML table",
                config_path.display()
            )
        })?;

        // These are the only keys codex-switch owns in a native profile.  A
        // user can safely edit every other setting in this profile; resuming a
        // run will not erase it.
        for key in PROVIDER_SESSION_KEYS {
            table.remove(key);
        }

        let mut overrides = provider_profile_overrides(args, profile, &self.runtime_provider_id)?;
        overrides.insert("model".to_string(), toml::Value::String(model.to_string()));
        for (key, value) in overrides {
            insert_toml_override(table, &key, value)?;
        }
        write_codex_config(&config_path, &value)?;
        self.write_meta(profile, Some(model))
    }

    fn write_meta(&self, profile: &ProviderProfile, model: Option<&str>) -> Result<()> {
        write_run_meta(
            &self.path,
            &ProviderRunMeta {
                provider_identity_id: profile.identity_id.clone(),
                alias: profile.alias.clone(),
                model: model.map(str::to_string),
                cwd: std::env::current_dir().ok(),
                created_at: now_rfc3339(),
                codex_home: Some(self.codex_home.clone()),
                profile_name: Some(self.profile_name.clone()),
                runtime_provider_id: Some(self.runtime_provider_id.clone()),
                child_pid: self.child_pid,
            },
        )
    }
}

impl Drop for ProviderLaunchProfile {
    fn drop(&mut self) {
        if self.abandon_on_drop {
            let _ = std::fs::remove_file(self.config_file_path());
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn validate_run_path(profile: &ProviderProfile, path: &Path) -> Result<()> {
    let identity_root = auth::app_home()?
        .join("provider-runs")
        .join(&profile.identity_id);
    if !path.starts_with(&identity_root) || !path.is_dir() {
        anyhow::bail!(
            "provider session run {} is not part of provider '{}'",
            path.display(),
            profile.alias
        );
    }
    Ok(())
}

fn provider_profile_overrides(
    args: &[String],
    profile: &ProviderProfile,
    runtime_provider_id: &str,
) -> Result<BTreeMap<String, toml::Value>> {
    let mut result = BTreeMap::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] != "-c" {
            index += 1;
            continue;
        }
        let pair = args
            .get(index + 1)
            .ok_or_else(|| anyhow::anyhow!("-c is missing its key=value value"))?;
        index += 2;
        let Some((key, raw_value)) = pair.split_once('=') else {
            continue;
        };
        let key = if key == "model_provider" {
            "model_provider".to_string()
        } else if let Some(suffix) = key
            .strip_prefix(&format!("model_providers.{}.", profile.provider_id))
            .or_else(|| key.strip_prefix(&format!("model_providers.{runtime_provider_id}.")))
        {
            format!("model_providers.{runtime_provider_id}.{suffix}")
        } else if PROVIDER_SESSION_KEYS.contains(&key) {
            key.to_string()
        } else {
            continue;
        };
        let raw_value = if key == "model_provider" {
            toml_string(runtime_provider_id)
        } else {
            raw_value.to_string()
        };
        // Codex -c accepts bare strings as well as TOML literals.
        let parsed = toml::from_str::<toml::Value>(&format!("value = {raw_value}"))
            .ok()
            .and_then(|value| value.get("value").cloned())
            .unwrap_or(toml::Value::String(raw_value));
        result.insert(key, parsed);
    }
    Ok(result)
}

fn insert_toml_override(
    table: &mut toml::map::Map<String, toml::Value>,
    dotted_key: &str,
    value: toml::Value,
) -> Result<()> {
    let parts: Vec<_> = dotted_key.split('.').collect();
    if parts.is_empty() || parts.iter().any(|part| part.is_empty()) {
        anyhow::bail!("invalid empty provider config key '{dotted_key}'");
    }
    let mut current = table;
    for part in &parts[..parts.len() - 1] {
        let entry = current
            .entry((*part).to_string())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        current = entry.as_table_mut().ok_or_else(|| {
            anyhow::anyhow!("provider config key '{dotted_key}' conflicts with a non-table value")
        })?;
    }
    current.insert(parts[parts.len() - 1].to_string(), value);
    Ok(())
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn write_run_meta(path: &Path, meta: &ProviderRunMeta) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(meta).context("serializing provider run metadata")?;
    auth::atomic_write_private(&path.join("provider_run.json"), &bytes)
        .with_context(|| format!("writing provider run metadata in {}", path.display()))
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderSession {
    pub(crate) session_id: String,
    pub(crate) name: String,
    pub(crate) updated_at: String,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) interactive: bool,
    pub(crate) model: Option<String>,
    pub(crate) run_id: String,
    pub(crate) run_path: PathBuf,
    name_source: SessionMetadataSource,
    updated_at_source: SessionMetadataSource,
    cwd_source: SessionMetadataSource,
    interactive_source: SessionMetadataSource,
    model_source: SessionMetadataSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SessionMetadataSource {
    Rollout,
    Index,
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderResumeFilter {
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) all: bool,
    pub(crate) include_noninteractive: bool,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ProviderSessionIndex {
    sessions: Vec<ProviderSession>,
}

impl ProviderSessionIndex {
    pub(crate) fn rebuild(root: &Path, identity_id: &str) -> Result<Self> {
        let identity_root = root.join(identity_id);
        if !identity_root.exists() {
            return Ok(Self::default());
        }
        let mut sessions = Vec::new();
        for entry in std::fs::read_dir(&identity_root)
            .with_context(|| format!("reading provider history {}", identity_root.display()))?
        {
            let entry = entry.with_context(|| format!("reading {}", identity_root.display()))?;
            let path = entry.path();
            if !path.is_dir() || entry.file_name() == "tombstone.json" {
                continue;
            }
            // The direct layout is the current one. Accepting a nested
            // `runs/` directory also lets a future layout migrate without
            // widening the provider identity search boundary.
            if path.file_name().is_some_and(|name| name == "runs") {
                for nested in std::fs::read_dir(&path)
                    .with_context(|| format!("reading provider runs {}", path.display()))?
                {
                    let nested = nested
                        .with_context(|| format!("reading provider runs {}", path.display()))?;
                    if nested.path().is_dir() {
                        collect_run_sessions(&nested.path(), identity_id, &mut sessions)?;
                    }
                }
            } else {
                collect_run_sessions(&path, identity_id, &mut sessions)?;
            }
        }
        let mut native_by_home = HashMap::<PathBuf, Vec<(PathBuf, ProviderRunMeta)>>::new();
        for entry in std::fs::read_dir(&identity_root)
            .with_context(|| format!("reading provider history {}", identity_root.display()))?
        {
            let entry = entry.with_context(|| format!("reading {}", identity_root.display()))?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(meta) = read_run_meta(&path)? else {
                continue;
            };
            if meta.provider_identity_id != identity_id {
                continue;
            }
            if let Some(home) = meta.codex_home.clone()
                && meta.profile_name.is_some()
                && meta.runtime_provider_id.is_some()
            {
                native_by_home.entry(home).or_default().push((path, meta));
            }
        }
        for (home, runs) in native_by_home {
            collect_native_home_sessions(&home, &runs, identity_id, &mut sessions)?;
        }
        sessions.sort_by(|left, right| {
            left.updated_at
                .cmp(&right.updated_at)
                .then_with(|| left.session_id.cmp(&right.session_id))
        });
        Ok(Self { sessions })
    }

    pub(crate) fn find_by_session_id(&self, session_id: &str) -> Result<&ProviderSession> {
        self.sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .ok_or_else(|| anyhow::anyhow!("provider session '{session_id}' was not found"))
    }

    pub(crate) fn find_unique_name(&self, name: &str) -> Result<&ProviderSession> {
        let matches: Vec<_> = self
            .sessions
            .iter()
            .filter(|session| session.name == name)
            .collect();
        match matches.as_slice() {
            [session] => Ok(session),
            [] => anyhow::bail!("provider session named '{name}' was not found"),
            _ => anyhow::bail!("provider session name '{name}' is ambiguous"),
        }
    }

    pub(crate) fn last(&self, filter: &ProviderResumeFilter) -> Result<&ProviderSession> {
        self.filtered(filter)
            .into_iter()
            .max_by(|left, right| {
                left.updated_at
                    .cmp(&right.updated_at)
                    .then_with(|| left.session_id.cmp(&right.session_id))
            })
            .ok_or_else(|| anyhow::anyhow!("no provider session matches the requested scope"))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub(crate) fn filtered<'a>(
        &'a self,
        filter: &ProviderResumeFilter,
    ) -> Vec<&'a ProviderSession> {
        self.sessions
            .iter()
            .filter(|session| {
                filter.all
                    || filter.cwd.as_ref().is_none_or(|cwd| {
                        session
                            .cwd
                            .as_ref()
                            .is_some_and(|session_cwd| same_path(session_cwd, cwd))
                    })
            })
            .filter(|session| filter.include_noninteractive || session.interactive)
            .collect()
    }
}

fn collect_native_home_sessions(
    codex_home: &Path,
    runs: &[(PathBuf, ProviderRunMeta)],
    identity_id: &str,
    sessions: &mut Vec<ProviderSession>,
) -> Result<()> {
    let mut found = HashMap::<String, ProviderSession>::new();
    let mut runtime_runs = HashMap::<&str, (&Path, &ProviderRunMeta)>::new();
    for (path, meta) in runs {
        if let Some(runtime_id) = meta.runtime_provider_id.as_deref() {
            runtime_runs.insert(runtime_id, (path, meta));
        }
    }
    let sessions_root = codex_home.join("sessions");
    if sessions_root.exists() {
        let mut pending = vec![sessions_root];
        while let Some(path) = pending.pop() {
            for entry in std::fs::read_dir(&path)
                .with_context(|| format!("reading Codex sessions {}", path.display()))?
            {
                let entry = entry.with_context(|| format!("reading {}", path.display()))?;
                let child = entry.path();
                let kind = entry
                    .file_type()
                    .with_context(|| format!("reading file type {}", child.display()))?;
                if kind.is_dir() {
                    pending.push(child);
                    continue;
                }
                if !kind.is_file()
                    || !child
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| {
                            name.starts_with("rollout-") && name.ends_with(".jsonl")
                        })
                {
                    continue;
                }
                let Ok(value) = read_rollout_metadata(&child) else {
                    continue;
                };
                let Some(runtime_id) = session_model_provider(&value) else {
                    continue;
                };
                let Some((run_path, meta)) = runtime_runs.get(runtime_id.as_str()) else {
                    continue;
                };
                let run_id = run_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default();
                merge_session_value(
                    &mut found,
                    &value,
                    identity_id,
                    run_path,
                    run_id,
                    Some(meta),
                    SessionMetadataSource::Rollout,
                );
            }
        }
    }

    // Codex's index files usually do not preserve the model provider.  Use
    // them only to enrich a session already proven by its rollout record.
    for value in native_index_values(codex_home)? {
        let payload = value.get("payload").unwrap_or(&value);
        let session_id = json_string(payload, "session_id")
            .or_else(|| json_string(payload, "id"))
            .or_else(|| json_string(&value, "session_id"))
            .or_else(|| json_string(&value, "id"));
        let Some(session_id) = session_id else {
            continue;
        };
        let Some(existing) = found.get(&session_id) else {
            continue;
        };
        let Some((run_path, meta)) = runs.iter().find(|(path, _)| path == &existing.run_path)
        else {
            continue;
        };
        let run_id = run_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        merge_session_value(
            &mut found,
            &value,
            identity_id,
            run_path,
            run_id,
            Some(meta),
            SessionMetadataSource::Index,
        );
    }
    sessions.extend(found.into_values());
    Ok(())
}

fn session_model_provider(value: &serde_json::Value) -> Option<String> {
    let payload = value.get("payload").unwrap_or(value);
    json_string(payload, "model_provider")
        .or_else(|| json_string(value, "model_provider"))
        .or_else(|| json_string(payload, "provider"))
        .or_else(|| json_string(value, "provider"))
}

fn native_index_values(codex_home: &Path) -> Result<Vec<serde_json::Value>> {
    let mut values = Vec::new();
    for name in ["session_meta.json", "session_index.json"] {
        if let Some(value) = read_json_if_present(&codex_home.join(name))? {
            if let Some(items) = value.get("sessions").and_then(serde_json::Value::as_array) {
                values.extend(items.iter().cloned());
            } else {
                values.push(value);
            }
        }
    }
    let jsonl = codex_home.join("session_index.jsonl");
    if jsonl.exists() {
        let file = File::open(&jsonl)
            .with_context(|| format!("opening Codex session index {}", jsonl.display()))?;
        for line in BufReader::new(file).lines() {
            let line =
                line.with_context(|| format!("reading Codex session index {}", jsonl.display()))?;
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                values.push(value);
            }
        }
    }
    Ok(values)
}

fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn collect_run_sessions(
    run_path: &Path,
    identity_id: &str,
    sessions: &mut Vec<ProviderSession>,
) -> Result<()> {
    let run_id = run_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string();
    if run_id.is_empty() {
        return Ok(());
    }
    let run_meta = read_run_meta(run_path)?;
    if let Some(meta) = &run_meta
        && meta.provider_identity_id != identity_id
    {
        return Ok(());
    }
    let mut found = HashMap::<String, ProviderSession>::new();
    if let Some(value) = read_json_if_present(&run_path.join("session_meta.json"))? {
        merge_session_value(
            &mut found,
            &value,
            identity_id,
            run_path,
            &run_id,
            run_meta.as_ref(),
            SessionMetadataSource::Index,
        );
    }
    if let Some(value) = read_json_if_present(&run_path.join("session_index.json"))? {
        if let Some(items) = value.get("sessions").and_then(serde_json::Value::as_array) {
            for item in items {
                merge_session_value(
                    &mut found,
                    item,
                    identity_id,
                    run_path,
                    &run_id,
                    run_meta.as_ref(),
                    SessionMetadataSource::Index,
                );
            }
        } else {
            merge_session_value(
                &mut found,
                &value,
                identity_id,
                run_path,
                &run_id,
                run_meta.as_ref(),
                SessionMetadataSource::Index,
            );
        }
    }
    let jsonl = run_path.join("session_index.jsonl");
    if jsonl.exists() {
        let file = File::open(&jsonl)
            .with_context(|| format!("opening provider session index {}", jsonl.display()))?;
        for line in BufReader::new(file).lines() {
            let line = line
                .with_context(|| format!("reading provider session index {}", jsonl.display()))?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                merge_session_value(
                    &mut found,
                    &value,
                    identity_id,
                    run_path,
                    &run_id,
                    run_meta.as_ref(),
                    SessionMetadataSource::Index,
                );
            }
        }
    }
    scan_rollouts(
        run_path,
        &run_id,
        identity_id,
        run_meta.as_ref(),
        &mut found,
    )?;
    sessions.extend(found.into_values());
    Ok(())
}

fn read_run_meta(path: &Path) -> Result<Option<ProviderRunMeta>> {
    let meta = path.join("provider_run.json");
    if !meta.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&meta)
        .with_context(|| format!("reading provider run metadata {}", meta.display()))?;
    Ok(Some(serde_json::from_str(&raw).with_context(|| {
        format!("parsing provider run metadata {}", meta.display())
    })?))
}

fn read_json_if_present(path: &Path) -> Result<Option<serde_json::Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading provider session metadata {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&raw).with_context(|| {
        format!("parsing provider session metadata {}", path.display())
    })?))
}

fn scan_rollouts(
    run_path: &Path,
    run_id: &str,
    identity_id: &str,
    run_meta: Option<&ProviderRunMeta>,
    found: &mut HashMap<String, ProviderSession>,
) -> Result<()> {
    let sessions_root = run_path.join("sessions");
    if !sessions_root.exists() {
        return Ok(());
    }
    let mut pending = vec![sessions_root];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(&path)
            .with_context(|| format!("reading provider sessions {}", path.display()))?
        {
            let entry = entry.with_context(|| format!("reading {}", path.display()))?;
            let child = entry.path();
            if child.is_dir() {
                pending.push(child);
                continue;
            }
            if child
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
            {
                let file = File::open(&child)
                    .with_context(|| format!("opening provider rollout {}", child.display()))?;
                if let Some(Ok(line)) = BufReader::new(file).lines().next()
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(&line)
                {
                    merge_session_value(
                        found,
                        &value,
                        identity_id,
                        run_path,
                        run_id,
                        run_meta,
                        SessionMetadataSource::Rollout,
                    );
                }
            }
        }
    }
    Ok(())
}

fn merge_session_value(
    found: &mut HashMap<String, ProviderSession>,
    value: &serde_json::Value,
    identity_id: &str,
    run_path: &Path,
    run_id: &str,
    run_meta: Option<&ProviderRunMeta>,
    source: SessionMetadataSource,
) {
    let payload = value.get("payload").unwrap_or(value);
    if let Some(value_identity) = json_string(payload, "provider_identity_id")
        .or_else(|| json_string(value, "provider_identity_id"))
        && value_identity != identity_id
    {
        return;
    }
    let session_id = json_string(payload, "session_id")
        .or_else(|| json_string(payload, "id"))
        .or_else(|| json_string(value, "session_id"))
        .or_else(|| json_string(value, "id"));
    let Some(session_id) = session_id.filter(|id| !id.is_empty()) else {
        return;
    };
    let name = json_string(payload, "name")
        .or_else(|| json_string(payload, "thread_name"))
        .or_else(|| json_string(value, "name"))
        .or_else(|| json_string(value, "thread_name"))
        .unwrap_or_default();
    let updated_at = json_string(payload, "updated_at")
        .or_else(|| json_string(payload, "timestamp"))
        .or_else(|| json_string(value, "updated_at"))
        .or_else(|| json_string(value, "timestamp"))
        .unwrap_or_default();
    let cwd = json_string(payload, "cwd")
        .or_else(|| json_string(value, "cwd"))
        .map(PathBuf::from)
        .or_else(|| run_meta.and_then(|meta| meta.cwd.clone()));
    let interactive = session_interactive(value, payload);
    let model = json_string(payload, "model")
        .or_else(|| json_string(value, "model"))
        .or_else(|| run_meta.and_then(|meta| meta.model.clone()));
    let item = found
        .entry(session_id.clone())
        .or_insert_with(|| ProviderSession {
            session_id: session_id.clone(),
            name: String::new(),
            updated_at: String::new(),
            cwd: None,
            interactive: true,
            model: None,
            run_id: run_id.to_string(),
            run_path: run_path.to_path_buf(),
            name_source: SessionMetadataSource::Rollout,
            updated_at_source: SessionMetadataSource::Rollout,
            cwd_source: SessionMetadataSource::Rollout,
            interactive_source: SessionMetadataSource::Rollout,
            model_source: SessionMetadataSource::Rollout,
        });
    if !name.is_empty() && source >= item.name_source {
        item.name = name;
        item.name_source = source;
    }
    if !updated_at.is_empty() && source >= item.updated_at_source {
        item.updated_at = updated_at;
        item.updated_at_source = source;
    }
    if cwd.is_some() && source >= item.cwd_source {
        item.cwd = cwd;
        item.cwd_source = source;
    }
    if let Some(interactive) = interactive
        && source >= item.interactive_source
    {
        item.interactive = interactive;
        item.interactive_source = source;
    }
    if model.is_some() && source >= item.model_source {
        item.model = model;
        item.model_source = source;
    }
}

fn session_interactive(value: &serde_json::Value, payload: &serde_json::Value) -> Option<bool> {
    if let Some(interactive) = value
        .get("interactive")
        .and_then(serde_json::Value::as_bool)
        .or_else(|| {
            payload
                .get("interactive")
                .and_then(serde_json::Value::as_bool)
        })
    {
        return Some(interactive);
    }
    for source in [
        payload.get("source"),
        value.get("source"),
        payload.get("thread_source"),
        value.get("thread_source"),
    ]
    .into_iter()
    .flatten()
    {
        if source
            .as_object()
            .is_some_and(|object| object.contains_key("subagent"))
        {
            return Some(false);
        }
    }
    let source = json_string(payload, "source")
        .or_else(|| json_string(value, "source"))
        .or_else(|| json_string(payload, "thread_source"))
        .or_else(|| json_string(value, "thread_source"))?;
    let normalized = source.to_ascii_lowercase();
    if normalized == "exec"
        || normalized == "noninteractive"
        || normalized.contains("noninteractive")
    {
        Some(false)
    } else {
        // Unknown sources remain compatible with Codex's interactive picker.
        Some(true)
    }
}

fn json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    match value.get(key) {
        Some(serde_json::Value::String(value)) => Some(value.clone()),
        Some(serde_json::Value::Number(value)) => Some(value.to_string()),
        _ => None,
    }
}

pub(crate) struct ProviderRunLease {
    file: File,
}

impl ProviderRunLease {
    pub(crate) fn acquire(run_path: &Path) -> Result<Self> {
        Self::try_acquire(run_path)?.ok_or_else(|| {
            anyhow::anyhow!(
                "provider session run {} is already being resumed",
                run_path.display()
            )
        })
    }

    pub(crate) fn try_acquire(run_path: &Path) -> Result<Option<Self>> {
        let lock_path = run_path.join("resume.lock");
        ensure_private_dir(run_path)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("opening provider resume lock {}", lock_path.display()))?;
        match FileExt::try_lock(&file) {
            Ok(()) => Ok(Some(Self { file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(err)) => Err(anyhow::Error::from(err))
                .with_context(|| format!("locking provider resume run {}", run_path.display())),
        }
    }
}

impl Drop for ProviderRunLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return true;
    }
    // Read errno straight after the failed call; `__errno_location` is
    // Linux-only, `last_os_error` is portable across Unix targets.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The spawned command may be a `.cmd`/`.sh` wrapper that execs the real
/// Codex as a grandchild; on Windows the recorded pid alone is not enough.
#[cfg(windows)]
fn process_tree_alive(root_pid: u32) -> bool {
    use std::collections::VecDeque;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32, Process32First, Process32Next, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    fn pid_exists(pid: u32) -> bool {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return false;
        }
        unsafe {
            let _ = CloseHandle(handle);
        }
        true
    }

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return pid_exists(root_pid);
    }
    let mut children: Vec<(u32, u32)> = Vec::new();
    let mut entry: PROCESSENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32>() as u32;
    let mut ok = unsafe { Process32First(snapshot, &mut entry) } != 0;
    while ok {
        children.push((entry.th32ProcessID, entry.th32ParentProcessID));
        ok = unsafe { Process32Next(snapshot, &mut entry) } != 0;
    }
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    let mut queue: VecDeque<u32> = VecDeque::from([root_pid]);
    while let Some(pid) = queue.pop_front() {
        if pid_exists(pid) {
            return true;
        }
        queue.extend(
            children
                .iter()
                .filter(|(_, parent)| *parent == pid)
                .map(|(child, _)| *child),
        );
    }
    false
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    process_tree_alive(pid)
}

#[cfg(not(any(unix, windows)))]
fn pid_alive(_pid: u32) -> bool {
    // Unknown platform: assume dead so resume is never blocked by a guess.
    false
}

/// Codex holds `$CODEX_HOME/thread-writer-locks/<thread_id>.lock` (fs lock)
/// for a thread's whole writer lifetime.  A live writer means another Codex
/// process still owns this session's rollout, regardless of how its launcher
/// died.  The file may not exist on older Codex versions; its absence or a
/// free lock only means no writer is active.
fn codex_thread_writer_active(codex_home: &Path, session_id: &str) -> bool {
    let path = codex_home
        .join("thread-writer-locks")
        .join(format!("{session_id}.lock"));
    if !path.exists() {
        return false;
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    else {
        return false;
    };
    matches!(FileExt::try_lock(&file), Err(TryLockError::WouldBlock))
}

/// Whether another Codex still owns a native run's session.  Call before
/// resuming so a second process never appends to a live rollout.
pub(crate) fn native_session_writer_active(codex_home: &Path, session_id: &str) -> bool {
    codex_thread_writer_active(codex_home, session_id)
}
/// Runtime providers referenced by surviving rollouts. An incomplete scan is
/// never evidence that a run is orphaned: losing resume configuration is worse
/// than retaining an unused run directory.
struct LiveRuntimeScan {
    ids: HashSet<String>,
    incomplete: bool,
}

fn scan_live_runtime_ids(codex_home: &Path) -> LiveRuntimeScan {
    let mut scan = LiveRuntimeScan {
        ids: HashSet::new(),
        incomplete: false,
    };
    let home_is_readable_directory =
        || codex_home.is_dir() && std::fs::read_dir(codex_home).is_ok();
    if !home_is_readable_directory() {
        scan.incomplete = true;
        return scan;
    }
    let mut pending = vec![
        codex_home.join("sessions"),
        codex_home.join("archived_sessions"),
    ];
    while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && home_is_readable_directory() =>
            {
                continue;
            }
            Err(_) => {
                scan.incomplete = true;
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                scan.incomplete = true;
                continue;
            };
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                scan.incomplete = true;
                continue;
            };
            if kind.is_symlink() {
                scan.incomplete = true;
                continue;
            }
            if kind.is_dir() {
                pending.push(path);
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                scan.incomplete = true;
                continue;
            };
            if !name.starts_with("rollout-") {
                continue;
            }
            if !kind.is_file() || !name.ends_with(".jsonl") {
                scan.incomplete = true;
                continue;
            }
            match rollout_runtime_provider(&path) {
                Ok(runtime_id) if runtime_id.starts_with("cs_") => {
                    scan.ids.insert(runtime_id);
                }
                Ok(_) => {}
                Err(_) => scan.incomplete = true,
            }
        }
    }
    if !home_is_readable_directory() {
        scan.incomplete = true;
    }
    scan
}

fn read_rollout_metadata(path: &Path) -> Result<serde_json::Value> {
    // Codex writes session metadata as the first JSONL record. Its instructions
    // can exceed 8 KiB; read a complete record without reading the whole history.
    // A larger or truncated record remains unclassified and prevents cleanup.
    const MAX_METADATA_BYTES: usize = 1024 * 1024;
    use std::io::Read;
    let mut reader = BufReader::new(File::open(path)?).take((MAX_METADATA_BYTES + 1) as u64);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line)?;
    anyhow::ensure!(
        line.len() <= MAX_METADATA_BYTES,
        "rollout metadata exceeds scan limit"
    );
    serde_json::from_slice(&line).context("invalid rollout metadata")
}

fn rollout_runtime_provider(path: &Path) -> Result<String> {
    let value = read_rollout_metadata(path)?;
    session_model_provider(&value)
        .filter(|id| !id.trim().is_empty())
        .context("rollout has no classifiable provider metadata")
}

/// Whether a native run is junk rather than resume state: no launcher holds
/// it, its Codex child is gone, and no surviving rollout references its
/// runtime id (the session is gone or never existed). An unreadable rollout
/// keeps it — a false keep costs kilobytes while a false
/// delete loses the only handle back to a session.
fn native_run_is_dead(meta: &ProviderRunMeta, scan: &LiveRuntimeScan) -> bool {
    if meta.child_pid.is_some_and(pid_alive) {
        return false;
    }
    let Some(runtime_id) = meta.runtime_provider_id.as_deref() else {
        return false;
    };
    if scan.incomplete || scan.ids.contains(runtime_id) {
        return false;
    }
    true
}

fn delete_native_profile_file(meta: &ProviderRunMeta) {
    if let (Some(codex_home), Some(profile_name)) = (&meta.codex_home, &meta.profile_name) {
        if !is_valid_native_profile_name(profile_name) {
            return;
        }
        let _ = std::fs::remove_file(codex_home.join(format!("{profile_name}.config.toml")));
    }
}

/// Sweep dead native runs left behind by crashed launches, abandoned
/// `codex exec` runs, or sessions the user deleted inside Codex: remove
/// their `cs-*.config.toml` from the Codex home and their run directories
/// from `provider-runs`.  Runs that still host a live Codex, are being
/// launched or resumed right now, or still own a session keep everything.
/// Each Codex home is scanned once per launch, not once per run.
pub(crate) fn sweep_dead_native_runs() -> Result<()> {
    let root = auth::app_home()?.join("provider-runs");
    if !root.exists() {
        return Ok(());
    }
    struct Candidate {
        path: PathBuf,
        meta: ProviderRunMeta,
        _lease: ProviderRunLease,
    }
    let mut candidates: HashMap<PathBuf, Vec<Candidate>> = HashMap::new();
    for identity in std::fs::read_dir(&root)
        .with_context(|| format!("reading provider runs {}", root.display()))?
    {
        let identity = identity?;
        if !identity.file_type()?.is_dir() {
            continue;
        }
        for run in std::fs::read_dir(identity.path())
            .with_context(|| format!("reading provider runs {}", identity.path().display()))?
        {
            let run = run?;
            let run_path = run.path();
            if !run.file_type()?.is_dir() {
                continue;
            }
            if !ProviderLaunchProfile::is_native_run(&run_path)? {
                continue;
            }
            // Keep the lease across metadata reading, the shared-home scan and
            // deletion. No candidate can start/resume while the scan is in use.
            let Some(lease) = ProviderRunLease::try_acquire(&run_path)? else {
                continue;
            };
            let Some(meta) = read_run_meta(&run_path)? else {
                continue;
            };
            if meta.child_pid.is_some_and(pid_alive) {
                continue;
            }
            let Some(codex_home) = meta.codex_home.clone() else {
                continue;
            };
            candidates.entry(codex_home).or_default().push(Candidate {
                path: run_path,
                meta,
                _lease: lease,
            });
        }
    }
    for (codex_home, runs) in candidates {
        let scan = scan_live_runtime_ids(&codex_home);
        for run in runs {
            if native_run_is_dead(&run.meta, &scan) {
                delete_native_profile_file(&run.meta);
                let _ = std::fs::remove_dir_all(&run.path);
            }
        }
    }
    Ok(())
}

/// Remove this provider's per-run Codex profile files.  Called when the
/// provider itself is removed: without its profile.toml the runs can never be
/// resumed again, so the `cs-*.config.toml` files are pure junk even while a
/// launched Codex is still running — it read its profile once at startup.
fn remove_native_profile_files(identity_id: &str) -> Result<()> {
    let history_root = auth::app_home()?.join("provider-runs").join(identity_id);
    if !history_root.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(&history_root)
        .with_context(|| format!("reading provider runs {}", history_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(meta) = read_run_meta(&entry.path())? {
            delete_native_profile_file(&meta);
        }
    }
    Ok(())
}

fn load_toml_if_present(path: &Path) -> Result<Option<toml::Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading Codex config {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let value =
        toml::from_str(&raw).with_context(|| format!("parsing Codex config {}", path.display()))?;
    Ok(Some(value))
}

fn write_codex_config(path: &Path, value: &toml::Value) -> Result<()> {
    let serialized =
        toml::to_string(value).context("serializing Codex config for provider launch")?;
    crate::auth::atomic_write_private(path, serialized.as_bytes())
        .with_context(|| format!("writing Codex config {}", path.display()))
}

fn strip_provider_session_keys(value: &mut toml::Value) {
    let Some(table) = value.as_table_mut() else {
        return;
    };
    for key in PROVIDER_SESSION_KEYS {
        table.remove(key);
    }
}

fn refresh_existing_run_config(
    run_config_path: &Path,
    current_user_config: Option<&toml::Value>,
) -> Result<()> {
    let run_config = load_toml_if_present(run_config_path)?;
    let mut refreshed = current_user_config
        .cloned()
        .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
    strip_provider_session_keys(&mut refreshed);
    if let Some(run_table) = run_config.and_then(|value| value.as_table().cloned())
        && let Some(refreshed_table) = refreshed.as_table_mut()
    {
        for key in PROVIDER_SESSION_KEYS {
            if let Some(value) = run_table.get(key) {
                refreshed_table.insert(key.to_string(), value.clone());
            }
        }
    }
    if refreshed.as_table().is_some_and(toml::map::Map::is_empty)
        && current_user_config.is_none()
        && !run_config_path.exists()
    {
        return Ok(());
    }
    write_codex_config(run_config_path, &refreshed)
}

fn link_user_entry(src: &Path, dest: &Path) -> Result<()> {
    if !src.exists() {
        return Ok(());
    }
    if dest.exists() || dest.symlink_metadata().is_ok() {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        ensure_private_dir(parent)?;
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(src, dest)
            .with_context(|| format!("linking {} -> {}", dest.display(), src.display()))?;
    }
    #[cfg(windows)]
    {
        let linked = if src.is_dir() {
            std::os::windows::fs::symlink_dir(src, dest)
        } else {
            std::os::windows::fs::symlink_file(src, dest)
        };
        if linked.is_err() {
            copy_tree(src, dest)
                .with_context(|| format!("copying {} -> {}", src.display(), dest.display()))?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn copy_tree(src: &Path, dest: &Path) -> Result<()> {
    if src.is_dir() {
        ensure_private_dir(dest)?;
        for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
            let entry = entry.with_context(|| format!("reading entry in {}", src.display()))?;
            copy_tree(&entry.path(), &dest.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        ensure_private_dir(parent)?;
    }
    std::fs::copy(src, dest)
        .with_context(|| format!("copying {} -> {}", src.display(), dest.display()))?;
    Ok(())
}

fn merge_isolated_config_into_user(
    user_config_path: &Path,
    base: Option<&toml::Value>,
    isolated_config_path: &Path,
) -> Result<()> {
    if base.is_some() && !isolated_config_path.exists() {
        anyhow::bail!(
            "provider launch config {} disappeared before it could be merged",
            isolated_config_path.display()
        );
    }
    let ours = load_toml_if_present(isolated_config_path)?;
    if base.is_none() && ours.is_none() {
        return Ok(());
    }
    let _lock = crate::profile::lock_codex_config_merge()?;
    let theirs = load_toml_if_present(user_config_path)?;
    let merged = merge_user_config(base, ours.as_ref(), theirs.as_ref());
    match merged {
        None => Ok(()),
        Some(value)
            if value.as_table().is_some_and(toml::map::Map::is_empty)
                && !user_config_path.exists() =>
        {
            Ok(())
        }
        Some(value) => write_codex_config(user_config_path, &value),
    }
}

fn merge_user_config(
    base: Option<&toml::Value>,
    ours: Option<&toml::Value>,
    theirs: Option<&toml::Value>,
) -> Option<toml::Value> {
    let empty = toml::map::Map::new();
    let base_table = base.and_then(toml::Value::as_table).unwrap_or(&empty);
    let ours_table = ours.and_then(toml::Value::as_table).unwrap_or(&empty);
    let theirs_table = theirs.and_then(toml::Value::as_table).unwrap_or(&empty);
    let mut keys = HashSet::new();
    keys.extend(base_table.keys().cloned());
    keys.extend(ours_table.keys().cloned());
    keys.extend(theirs_table.keys().cloned());
    let mut out = theirs_table.clone();
    for key in keys {
        if PROVIDER_SESSION_KEYS.contains(&key.as_str()) {
            continue;
        }
        match three_way_merge(
            base_table.get(&key),
            ours_table.get(&key),
            theirs_table.get(&key),
        ) {
            Some(value) => {
                out.insert(key, value);
            }
            None => {
                out.remove(&key);
            }
        }
    }
    if out.is_empty() && theirs.is_none() && ours.is_none() {
        return None;
    }
    Some(toml::Value::Table(out))
}

fn three_way_merge(
    base: Option<&toml::Value>,
    ours: Option<&toml::Value>,
    theirs: Option<&toml::Value>,
) -> Option<toml::Value> {
    if ours == theirs {
        return ours.cloned().or_else(|| theirs.cloned());
    }
    if ours == base {
        return theirs.cloned();
    }
    if theirs == base {
        return ours.cloned();
    }
    match (base, ours, theirs) {
        (
            Some(toml::Value::Table(base)),
            Some(toml::Value::Table(ours)),
            Some(toml::Value::Table(theirs)),
        ) => Some(toml::Value::Table(merge_maps(base, ours, theirs))),
        (Some(toml::Value::Table(base)), Some(toml::Value::Table(ours)), None) => Some(
            toml::Value::Table(merge_maps(base, ours, &toml::map::Map::new())),
        ),
        (Some(toml::Value::Table(base)), None, Some(toml::Value::Table(theirs))) => Some(
            toml::Value::Table(merge_maps(base, &toml::map::Map::new(), theirs)),
        ),
        (_, ours, _) => ours.cloned(),
    }
}

fn merge_maps(
    base: &toml::map::Map<String, toml::Value>,
    ours: &toml::map::Map<String, toml::Value>,
    theirs: &toml::map::Map<String, toml::Value>,
) -> toml::map::Map<String, toml::Value> {
    let mut keys = HashSet::new();
    keys.extend(base.keys().cloned());
    keys.extend(ours.keys().cloned());
    keys.extend(theirs.keys().cloned());
    let mut out = theirs.clone();
    for key in keys {
        match three_way_merge(base.get(&key), ours.get(&key), theirs.get(&key)) {
            Some(value) => {
                out.insert(key, value);
            }
            None => {
                out.remove(&key);
            }
        }
    }
    out
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
    let path = existing_provider_dir(alias)?.join("provider.toml");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading provider profile {}", path.display()))?;
    let mut profile: ProviderProfile = toml::from_str(&raw)
        .with_context(|| format!("parsing provider profile {}", path.display()))?;
    profile.alias = alias.to_string();
    let needs_identity = profile.identity_id.trim().is_empty();
    let legacy_history = auth::app_home()?.join("provider-runs").join(alias);
    if needs_identity || legacy_history.exists() {
        let lock_path = path.with_file_name("identity-migration.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| {
                format!(
                    "opening provider identity migration lock {}",
                    lock_path.display()
                )
            })?;
        FileExt::lock(&lock).with_context(|| {
            format!(
                "locking provider identity migration {}",
                lock_path.display()
            )
        })?;
        let locked_raw = std::fs::read_to_string(&path)
            .with_context(|| format!("rereading provider profile {}", path.display()))?;
        profile = toml::from_str(&locked_raw)
            .with_context(|| format!("parsing provider profile {}", path.display()))?;
        profile.alias = alias.to_string();
        let locked_needs_identity = profile.identity_id.trim().is_empty();
        profile.normalize();
        profile
            .validate()
            .with_context(|| format!("validating provider profile {}", path.display()))?;
        if locked_needs_identity {
            let toml = toml::to_string_pretty(&profile)
                .context("serializing migrated provider profile")?;
            auth::atomic_write_private(&path, toml.as_bytes())
                .with_context(|| format!("migrating provider profile {}", path.display()))?;
        }
        migrate_legacy_provider_history(alias, &profile.identity_id)?;
        drop(lock);
        return Ok(profile);
    }
    profile.normalize();
    profile
        .validate()
        .with_context(|| format!("validating provider profile {}", path.display()))?;
    Ok(profile)
}

fn migrate_legacy_provider_history(alias: &str, identity_id: &str) -> Result<()> {
    let root = auth::app_home()?.join("provider-runs");
    let source = root.join(alias);
    if !source.exists() {
        return Ok(());
    }
    let destination = root.join(identity_id);
    if !destination.exists() {
        std::fs::rename(&source, &destination).with_context(|| {
            format!(
                "migrating provider history {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        return Ok(());
    }
    ensure_private_dir(&destination)?;
    for entry in std::fs::read_dir(&source)
        .with_context(|| format!("reading legacy provider history {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("reading {}", source.display()))?;
        let target = destination.join(entry.file_name());
        if target.exists() {
            anyhow::bail!(
                "refusing to overwrite provider history {} while migrating {}",
                target.display(),
                source.display()
            );
        }
        std::fs::rename(entry.path(), &target)
            .with_context(|| format!("migrating provider history entry to {}", target.display()))?;
    }
    std::fs::remove_dir(&source)
        .with_context(|| format!("removing migrated provider history {}", source.display()))
}

/// Persist a provider profile (directory `0700`, file `0600`).
pub fn save(profile: &ProviderProfile) -> Result<()> {
    let mut stored = profile.clone();
    if stored.responses_support.is_empty()
        && let Ok(existing) = load(&stored.alias)
        && existing.base_url == stored.base_url
    {
        stored.responses_support = existing.responses_support;
    }
    stored.normalize();
    stored.validate()?;
    let dir = provider_dir(&stored.alias)?;
    if dir.exists() {
        existing_provider_dir(&stored.alias)?;
    }
    ensure_private_dir(&dir)?;
    write_profile(&dir.join("provider.toml"), &stored)
}

fn write_profile(path: &Path, profile: &ProviderProfile) -> Result<()> {
    let toml = toml::to_string_pretty(profile).context("serializing provider profile")?;
    auth::atomic_write_private(path, toml.as_bytes())
        .with_context(|| format!("writing provider profile {}", path.display()))
}

/// Whether any of this provider's runs still has a live Codex child.
/// Removing a provider while its Codex is running is legal (the process
/// already read its config), but it is almost always a mistake and leaves
/// the user staring at a session they can no longer resume.
fn provider_has_live_runs(identity_id: &str) -> Result<bool> {
    let history_root = auth::app_home()?.join("provider-runs").join(identity_id);
    if !history_root.exists() {
        return Ok(false);
    }
    for entry in std::fs::read_dir(&history_root)
        .with_context(|| format!("reading provider runs {}", history_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Some(meta) = read_run_meta(&entry.path())?
            && meta.child_pid.is_some_and(pid_alive)
        {
            return Ok(true);
        }
        // A launcher mid-flight (spawn not finished, no child_pid yet) holds
        // the run lease; treat the run as live so removal cannot race it.
        if ProviderRunLease::try_acquire(&entry.path())?.is_none() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Remove a provider profile and its stored key.
pub fn remove(alias: &str) -> Result<()> {
    let profile = load(alias)?;
    let dir = existing_provider_dir(alias)?;
    if provider_has_live_runs(&profile.identity_id)? {
        anyhow::bail!(
            "provider '{alias}' still has a running Codex session; remove it after that Codex exits"
        );
    }
    let history_root = auth::app_home()?
        .join("provider-runs")
        .join(&profile.identity_id);
    ensure_private_dir(&history_root)?;
    let tombstone = serde_json::json!({
        "provider_identity_id": profile.identity_id,
        "alias": alias,
        "removed_at": now_rfc3339(),
    });
    let tombstone =
        serde_json::to_vec_pretty(&tombstone).context("serializing provider tombstone")?;
    auth::atomic_write_private(&history_root.join("tombstone.json"), &tombstone)
        .with_context(|| format!("writing provider tombstone {}", history_root.display()))?;
    remove_native_profile_files(&profile.identity_id)?;
    std::fs::remove_dir_all(&dir)
        .with_context(|| format!("removing provider profile {}", dir.display()))
}

/// Rename a provider directory and re-derive `provider_id` from the new alias.
/// `env_key` is re-derived only when it still matches the old default; a
/// custom key name is kept so launch still injects into the variable the
/// user configured. Display name follows the alias.
pub fn rename(old: &str, new: &str) -> Result<()> {
    crate::profile::validate_alias(old)?;
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
    let old_dir = existing_provider_dir(old)?;
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
        previous_switch: Option<OsString>,
        previous_codex: Option<OsString>,
    }

    impl TestHome {
        fn new() -> Self {
            let lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = tempfile::tempdir().unwrap();
            let previous_switch = std::env::var_os("CODEX_SWITCH_HOME");
            let previous_codex = std::env::var_os("CODEX_HOME");
            let user_codex = home.path().join(".codex");
            std::fs::create_dir_all(&user_codex).unwrap();
            unsafe {
                std::env::set_var("CODEX_SWITCH_HOME", home.path());
                std::env::set_var("CODEX_HOME", &user_codex);
            }
            Self {
                _lock: lock,
                _home: home,
                previous_switch,
                previous_codex,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            unsafe {
                match &self.previous_switch {
                    Some(value) => std::env::set_var("CODEX_SWITCH_HOME", value),
                    None => std::env::remove_var("CODEX_SWITCH_HOME"),
                }
                match &self.previous_codex {
                    Some(value) => std::env::set_var("CODEX_HOME", value),
                    None => std::env::remove_var("CODEX_HOME"),
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
    fn rebuilding_a_provider_with_the_same_alias_gets_a_distinct_stable_identity() {
        let first = sample("same-name");
        let second = sample("same-name");

        assert!(!first.identity_id.is_empty());
        assert_ne!(
            first.identity_id, second.identity_id,
            "the alias is reusable, so it cannot be the provider identity"
        );

        let retained = first.identity_id.clone();
        let cloned = first.clone();
        assert_eq!(cloned.identity_id, retained);
    }

    #[test]
    fn saving_loading_and_renaming_preserve_the_provider_identity() {
        let _home = TestHome::new();
        let profile = sample("stable-name");
        let identity_id = profile.identity_id.clone();
        save(&profile).unwrap();

        assert_eq!(load("stable-name").unwrap().identity_id, identity_id);
        rename("stable-name", "renamed").unwrap();

        let renamed = load("renamed").unwrap();
        assert_eq!(renamed.identity_id, identity_id);
        assert_eq!(renamed.alias, "renamed");
    }

    #[test]
    fn provider_run_root_is_keyed_by_identity_and_not_alias() {
        let _home = TestHome::new();
        let profile = sample("display-name");
        let expected_root = auth::app_home()
            .unwrap()
            .join("provider-runs")
            .join(&profile.identity_id);

        let mut run = ProviderCodexHome::begin(&profile).unwrap();
        assert!(
            run.path.starts_with(&expected_root),
            "run path was {}",
            run.path.display()
        );
        assert!(
            !run.path.starts_with(
                auth::app_home()
                    .unwrap()
                    .join("provider-runs")
                    .join(&profile.alias)
            )
        );
        run.restore().unwrap();
    }

    #[test]
    fn ordinary_provider_launches_get_unique_run_directories() {
        let _home = TestHome::new();
        let profile = sample("parallel");
        let mut first = ProviderCodexHome::begin(&profile).unwrap();
        let mut second = ProviderCodexHome::begin(&profile).unwrap();

        assert_ne!(first.path, second.path);
        assert_eq!(first.path.parent(), second.path.parent());
        first.restore().unwrap();
        second.restore().unwrap();
    }

    #[test]
    fn native_provider_profile_uses_user_home_and_only_writes_its_profile() {
        let _home = TestHome::new();
        let user_home = auth::user_codex_home().unwrap();
        let user_config = user_home.join("config.toml");
        std::fs::write(
            &user_config,
            "model = \"chatgpt\"\n\n[mcp_servers.demo]\ncommand = \"demo\"\n",
        )
        .unwrap();
        let profile = sample("profile-route");
        let run = ProviderLaunchProfile::begin(&profile).unwrap();
        let args = profile
            .codex_config_args(Some("openai/gpt-5.3-codex"))
            .unwrap();
        run.write_config(&profile, &args, "openai/gpt-5.3-codex")
            .unwrap();

        assert_eq!(run.codex_home, user_home);
        assert!(ProviderLaunchProfile::is_native_run(&run.path).unwrap());
        assert_eq!(
            std::fs::read_to_string(&user_config).unwrap(),
            "model = \"chatgpt\"\n\n[mcp_servers.demo]\ncommand = \"demo\"\n"
        );
        let profile_config =
            std::fs::read_to_string(user_home.join(format!("{}.config.toml", run.profile_name)))
                .unwrap();
        assert!(
            profile_config.contains(&format!("model_provider = \"{}\"", run.runtime_provider_id))
        );
        assert!(profile_config.contains(&format!("[model_providers.{}]", run.runtime_provider_id)));
        assert!(!profile_config.contains("mcp_servers"));
        let meta = read_run_meta(&run.path).unwrap().unwrap();
        assert_eq!(
            meta.runtime_provider_id.as_deref(),
            Some(run.runtime_provider_id.as_str())
        );
    }

    #[test]
    fn native_provider_sessions_are_selected_only_by_runtime_provider_id() {
        let _home = TestHome::new();
        let profile = sample("native-history");
        let run = ProviderLaunchProfile::begin(&profile).unwrap();
        let args = profile
            .codex_config_args(Some("openai/gpt-5.3-codex"))
            .unwrap();
        run.write_config(&profile, &args, "openai/gpt-5.3-codex")
            .unwrap();
        let session_dir = run.codex_home.join("sessions").join("2026");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("rollout-native.jsonl"),
            format!(
                "{{\"payload\":{{\"id\":\"provider-session\",\"model_provider\":\"{}\",\"timestamp\":\"2026-01-01T00:00:00Z\"}}}}\n",
                run.runtime_provider_id
            ),
        )
        .unwrap();
        std::fs::write(
            session_dir.join("rollout-account.jsonl"),
            "{\"payload\":{\"id\":\"account-session\",\"model_provider\":\"openai\"}}\n",
        )
        .unwrap();

        let index = ProviderSessionIndex::rebuild(
            &auth::app_home().unwrap().join("provider-runs"),
            &profile.identity_id,
        )
        .unwrap();
        assert!(index.find_by_session_id("provider-session").is_ok());
        assert!(index.find_by_session_id("account-session").is_err());
    }

    fn native_run_with_config(profile: &ProviderProfile) -> ProviderLaunchProfile {
        let run = ProviderLaunchProfile::begin(profile).unwrap();
        let args = profile
            .codex_config_args(Some("openai/gpt-5.3-codex"))
            .unwrap();
        run.write_config(profile, &args, "openai/gpt-5.3-codex")
            .unwrap();
        run
    }

    #[test]
    fn a_fresh_run_self_cleans_until_disarmed() {
        let _home = TestHome::new();
        let profile = sample("abandon");
        let run = native_run_with_config(&profile);
        let config_path = run.config_file_path();
        let run_path = run.path.clone();
        assert!(config_path.exists() && run_path.exists());
        drop(run);
        assert!(!config_path.exists());
        assert!(!run_path.exists());

        let mut run = native_run_with_config(&profile);
        let config_path = run.config_file_path();
        let run_path = run.path.clone();
        run.disarm();
        drop(run);
        assert!(config_path.exists(), "a disarmed run keeps its profile");
        assert!(run_path.exists(), "a disarmed run keeps its run dir");
    }

    #[test]
    fn resume_rejects_a_run_whose_child_is_still_alive() {
        let _home = TestHome::new();
        let profile = sample("live-child");
        let mut run = native_run_with_config(&profile);
        run.set_child_pid(&profile, "openai/gpt-5.3-codex", std::process::id())
            .unwrap();

        let error = match ProviderLaunchProfile::open_existing(&profile, &run.path) {
            Ok(_) => panic!("a live Codex child must block resume"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("still has a live Codex process"));
    }

    #[test]
    fn resume_accepts_a_run_whose_child_is_dead() {
        let _home = TestHome::new();
        let profile = sample("dead-child");
        let mut run = native_run_with_config(&profile);
        // 2^22-1 is outside the pid space on every supported platform.
        run.set_child_pid(&profile, "openai/gpt-5.3-codex", 4_000_000)
            .unwrap();

        ProviderLaunchProfile::open_existing(&profile, &run.path).unwrap();
    }

    #[test]
    fn cleanup_rejects_traversal_in_damaged_native_profile_metadata() {
        let _home = TestHome::new();
        let user_home = auth::user_codex_home().unwrap();
        let separator = std::path::MAIN_SEPARATOR;
        let malicious_name = format!("cs-{separator}..{separator}..{separator}victim");
        std::fs::create_dir_all(user_home.join("cs-")).unwrap();
        let sentinel = user_home.parent().unwrap().join("victim.config.toml");
        std::fs::write(&sentinel, "preserve this unrelated file").unwrap();
        let profile = sample("damaged-meta");
        let meta = ProviderRunMeta {
            provider_identity_id: profile.identity_id,
            alias: profile.alias,
            model: None,
            cwd: None,
            created_at: now_rfc3339(),
            codex_home: Some(user_home),
            profile_name: Some(malicious_name),
            runtime_provider_id: Some("cs_test_run".into()),
            child_pid: None,
        };

        delete_native_profile_file(&meta);

        assert_eq!(
            std::fs::read_to_string(&sentinel).unwrap(),
            "preserve this unrelated file"
        );
    }

    #[test]
    fn sweep_removes_dead_runs_and_keeps_sessions_and_live_children() {
        let _home = TestHome::new();
        let user_home = auth::user_codex_home().unwrap();
        let profile = sample("sweep");

        // Dead run: spawned (disarmed) but no rollout and a dead child ->
        // swept once its launcher is gone.
        let mut dead = native_run_with_config(&profile);
        dead.disarm();
        let dead_config = dead.config_file_path();
        let dead_path = dead.path.clone();
        // While the launcher still holds the run lease it cannot be swept:
        // this is the concurrent-launch race guard.
        sweep_dead_native_runs().unwrap();
        assert!(dead_config.exists(), "a leased run must survive the sweep");
        assert!(dead_path.exists());

        // Run with a recorded rollout -> kept.
        let mut with_session = native_run_with_config(&profile);
        with_session.disarm();
        let with_session_config = with_session.config_file_path();
        let with_session_path = with_session.path.clone();
        let today = chrono::Utc::now();
        let session_dir = user_home
            .join("sessions")
            .join(today.format("%Y").to_string())
            .join(today.format("%m").to_string())
            .join(today.format("%d").to_string());
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("rollout-kept.jsonl"),
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"kept\",\"model_provider\":\"{}\",\"timestamp\":\"{}\"}}}}\n\
                 {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"hello\"}}}}\n",
                with_session.runtime_provider_id,
                today.to_rfc3339()
            ),
        )
        .unwrap();

        // Run whose child is still alive -> kept even without a rollout.
        let mut live = native_run_with_config(&profile);
        live.disarm();
        live.set_child_pid(&profile, "openai/gpt-5.3-codex", std::process::id())
            .unwrap();

        // A launcher that already exited releases its run lease; runs still
        // held open (like `live` here) are protected from the sweep.
        drop(dead);
        drop(with_session);
        sweep_dead_native_runs().unwrap();

        assert!(!dead_config.exists(), "dead run profile must be removed");
        assert!(!dead_path.exists(), "dead run dir must be removed");
        assert!(with_session_config.exists());
        assert!(with_session_path.exists());
        assert!(live.config_file_path().exists());
        assert!(live.path.exists());

        // Once the user deletes that session inside Codex the run becomes
        // orphaned state; the next sweep must reclaim it too.
        std::fs::remove_file(session_dir.join("rollout-kept.jsonl")).unwrap();
        sweep_dead_native_runs().unwrap();
        assert!(
            !with_session_config.exists(),
            "a run whose session is gone must lose its profile file"
        );
        assert!(!with_session_path.exists());
        assert!(live.config_file_path().exists());
    }

    #[test]
    fn sweep_retains_long_metadata_and_archived_sessions() {
        let _home = TestHome::new();
        let user_home = auth::user_codex_home().unwrap();
        let profile = sample("long-rollout");
        for directory in ["sessions", "archived_sessions"] {
            let mut run = native_run_with_config(&profile);
            run.disarm();
            let config = run.config_file_path();
            let path = run.path.clone();
            let payload = serde_json::json!({
                "type": "session_meta",
                "payload": {
                    "id": "long-session",
                    "model_provider": run.runtime_provider_id,
                    "base_instructions": "instructions ".repeat(2048),
                }
            });
            let sessions = user_home.join(directory);
            std::fs::create_dir_all(&sessions).unwrap();
            std::fs::write(
                sessions.join("rollout-long.jsonl"),
                format!("{payload}\n{{\"type\":\"event_msg\",\"payload\":{{}}}}\n"),
            )
            .unwrap();
            drop(run);
            sweep_dead_native_runs().unwrap();
            assert!(config.exists(), "{directory}: metadata must retain config");
            assert!(path.exists(), "{directory}: metadata must retain run");
        }
    }

    #[test]
    fn sweep_never_deletes_on_unclassifiable_rollouts() {
        let _home = TestHome::new();
        let user_home = auth::user_codex_home().unwrap();
        let profile = sample("unknown-rollout");
        let mut run = native_run_with_config(&profile);
        run.disarm();
        let config = run.config_file_path();
        let path = run.path.clone();
        drop(run);
        let sessions = user_home.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let rollout = sessions.join("rollout-incomplete.jsonl");
        let oversized = serde_json::json!({
            "payload": { "model_provider": "openai", "instructions": "x".repeat(1024 * 1024) }
        })
        .to_string();
        for body in ["", "{\"payload\":", "{\"unknown_schema\":true}", &oversized] {
            std::fs::write(&rollout, body).unwrap();
            sweep_dead_native_runs().unwrap();
            assert!(config.exists(), "unclassified rollout must retain config");
            assert!(path.exists(), "unclassified rollout must retain run");
        }
        std::fs::rename(&rollout, sessions.join("rollout-incomplete.jsonl.zst")).unwrap();
        sweep_dead_native_runs().unwrap();
        assert!(config.exists(), "compressed rollout must retain config");
        assert!(path.exists());
    }

    #[test]
    fn sweep_keeps_runs_when_sessions_directory_cannot_be_enumerated() {
        let _home = TestHome::new();
        let user_home = auth::user_codex_home().unwrap();
        let profile = sample("blocked-scan");
        let mut run = native_run_with_config(&profile);
        run.disarm();
        let config = run.config_file_path();
        let path = run.path.clone();
        drop(run);
        // A non-directory at the expected path produces a portable read_dir
        // failure, including under privileged test runners.
        std::fs::write(user_home.join("sessions"), "not a directory").unwrap();
        sweep_dead_native_runs().unwrap();
        assert!(config.exists());
        assert!(path.exists());
    }

    #[test]
    fn runtime_scan_requires_an_available_home_but_allows_missing_session_dirs() {
        let temporary = tempfile::tempdir().unwrap();
        let missing_home = temporary.path().join("missing-home");
        assert!(scan_live_runtime_ids(&missing_home).incomplete);

        let non_directory_home = temporary.path().join("home-file");
        std::fs::write(&non_directory_home, "not a directory").unwrap();
        assert!(scan_live_runtime_ids(&non_directory_home).incomplete);

        let empty_home = temporary.path().join("empty-home");
        std::fs::create_dir(&empty_home).unwrap();
        let scan = scan_live_runtime_ids(&empty_home);
        assert!(!scan.incomplete);
        assert!(scan.ids.is_empty());
    }

    #[test]
    fn sweep_keeps_runs_when_their_codex_home_is_temporarily_unavailable() {
        let _home = TestHome::new();
        let user_home = auth::user_codex_home().unwrap();
        let profile = sample("offline-codex-home");
        let mut run = native_run_with_config(&profile);
        run.disarm();
        let run_path = run.path.clone();
        let offline_home = user_home.with_file_name("offline-codex-home");
        let saved_profile = offline_home.join(format!("{}.config.toml", run.profile_name));
        drop(run);

        std::fs::rename(&user_home, &offline_home).unwrap();
        let scan = scan_live_runtime_ids(&user_home);
        assert!(scan.incomplete);
        sweep_dead_native_runs().unwrap();

        assert!(run_path.exists(), "unavailable home must keep run metadata");
        assert!(
            saved_profile.exists(),
            "profile remains with the offline home"
        );
    }

    #[cfg(unix)]
    #[test]
    fn linked_session_directories_keep_recovery_profiles_and_do_not_loop_indexing() {
        let _home = TestHome::new();
        let user_home = auth::user_codex_home().unwrap();
        let profile = sample("linked-session");
        save(&profile).unwrap();
        let mut run = native_run_with_config(&profile);
        let config_path = run.config_file_path();
        run.disarm();
        drop(run);
        let sessions = user_home.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::os::unix::fs::symlink(&sessions, sessions.join("loop")).unwrap();
        assert!(scan_live_runtime_ids(&user_home).incomplete);
        sweep_dead_native_runs().unwrap();
        assert!(config_path.exists());
        let index = ProviderSessionIndex::rebuild(
            &auth::app_home().unwrap().join("provider-runs"),
            &profile.identity_id,
        )
        .unwrap();
        assert!(index.is_empty());
    }

    #[test]
    fn removing_a_provider_deletes_its_native_profile_files() {
        let _home = TestHome::new();
        let user_home = auth::user_codex_home().unwrap();
        let profile = sample("remove-me");
        save(&profile).unwrap();
        let mut run = native_run_with_config(&profile);
        let config_path = user_home.join(format!("{}.config.toml", run.profile_name));
        let run_path = run.path.clone();
        assert!(config_path.exists());
        // The launcher exited; the run keeps its state but no lease.
        run.disarm();
        drop(run);

        remove(&profile.alias).unwrap();

        assert!(!config_path.exists(), "orphaned cs profile must be deleted");
        assert!(run_path.exists(), "run history stays as tombstoned state");
    }

    #[test]
    fn removing_a_provider_with_a_live_run_is_rejected() {
        let _home = TestHome::new();
        let profile = sample("still-running");
        save(&profile).unwrap();
        let mut run = native_run_with_config(&profile);
        run.set_child_pid(&profile, "openai/gpt-5.3-codex", std::process::id())
            .unwrap();

        let error = remove(&profile.alias).unwrap_err();
        assert!(format!("{error:#}").contains("still has a running Codex session"));
        assert!(
            run.config_file_path().exists(),
            "a refused removal must not touch live state"
        );
    }

    #[test]
    fn removing_a_provider_during_an_in_flight_launch_is_rejected() {
        let _home = TestHome::new();
        let profile = sample("mid-launch");
        save(&profile).unwrap();
        // begin() holds the lease before spawn writes child_pid; removal must
        // see this run as live through the lease alone.
        let run = ProviderLaunchProfile::begin(&profile).unwrap();

        let error = remove(&profile.alias).unwrap_err();
        assert!(format!("{error:#}").contains("still has a running Codex session"));

        drop(run);
        remove(&profile.alias).unwrap();
    }

    #[test]
    fn wire_api_rejects_the_removed_chat_value() {
        let mut profile = sample("wire");
        profile.wire_api = "chat".to_string();
        let error = profile.validate().unwrap_err();
        assert!(format!("{error:#}").contains("wire_api"));
    }

    #[test]
    fn reopening_a_provider_run_refreshes_non_provider_config_from_the_latest_user_snapshot() {
        let _home = TestHome::new();
        let user_config = crate::auth::user_codex_home().unwrap().join("config.toml");
        std::fs::write(
            &user_config,
            "model = \"chatgpt-old\"\n\n[mcp_servers.demo]\ncommand = \"old\"\n",
        )
        .unwrap();
        let profile = sample("resume-config");
        let mut first = ProviderCodexHome::begin(&profile).unwrap();
        std::fs::write(
            first.path.join("config.toml"),
            "model = \"provider-model\"\nmodel_provider = \"resume-config\"\nmodel_catalog_json = \"preserve\"\n\n[mcp_servers.demo]\ncommand = \"stale\"\n",
        )
        .unwrap();
        first.restored = true;

        std::fs::write(
            &user_config,
            "model = \"chatgpt-new\"\n\n[mcp_servers.demo]\ncommand = \"new\"\n",
        )
        .unwrap();

        let mut resumed = ProviderCodexHome::open_existing(&profile, &first.path).unwrap();
        let refreshed = std::fs::read_to_string(first.path.join("config.toml")).unwrap();
        assert!(refreshed.contains("command = \"new\""), "{refreshed}");
        assert!(!refreshed.contains("command = \"stale\""), "{refreshed}");
        assert!(
            refreshed.contains("model = \"provider-model\""),
            "{refreshed}"
        );
        assert!(
            refreshed.contains("model_provider = \"resume-config\""),
            "{refreshed}"
        );
        assert!(
            refreshed.contains("model_catalog_json = \"preserve\""),
            "{refreshed}"
        );

        resumed.restore().unwrap();
        let restored = std::fs::read_to_string(user_config).unwrap();
        assert!(restored.contains("command = \"new\""), "{restored}");
        assert!(!restored.contains("command = \"stale\""), "{restored}");
        assert!(!restored.contains("provider-model"), "{restored}");
    }

    fn write_session_fixture(
        run: &Path,
        provider_id: &str,
        session_id: &str,
        name: &str,
        updated_at: &str,
        cwd: &str,
        interactive: bool,
    ) {
        std::fs::create_dir_all(run).unwrap();
        std::fs::write(
            run.join("session_meta.json"),
            serde_json::to_vec(&serde_json::json!({
                "provider_identity_id": provider_id,
                "session_id": session_id,
                "name": name,
                "updated_at": updated_at,
                "cwd": cwd,
                "interactive": interactive
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            run.join("session_index.json"),
            serde_json::to_vec(&serde_json::json!({
                "provider_identity_id": provider_id,
                "sessions": [{
                    "id": session_id,
                    "name": name,
                    "updated_at": updated_at,
                    "cwd": cwd,
                    "interactive": interactive
                }]
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn provider_session_index_rebuilds_exact_and_unique_name_lookups_per_provider() {
        let _home = TestHome::new();
        let provider = sample("provider-a");
        let other = sample("provider-b");
        let root = auth::app_home().unwrap().join("provider-runs");

        write_session_fixture(
            &root.join(&provider.identity_id).join("run-a"),
            &provider.identity_id,
            "session-a",
            "Alpha",
            "2026-09-09T00:00:01Z",
            "workspace-a",
            true,
        );
        write_session_fixture(
            &root.join(&provider.identity_id).join("run-b"),
            &provider.identity_id,
            "session-b",
            "Beta",
            "2026-09-09T00:00:02Z",
            "workspace-b",
            true,
        );
        write_session_fixture(
            &root.join(&provider.identity_id).join("run-c"),
            &provider.identity_id,
            "session-c",
            "Alpha",
            "2026-09-09T00:00:03Z",
            "workspace-c",
            true,
        );
        write_session_fixture(
            &root.join(&other.identity_id).join("run-other"),
            &other.identity_id,
            "session-other",
            "Alpha",
            "2026-09-09T00:00:04Z",
            "workspace-other",
            true,
        );

        let index = ProviderSessionIndex::rebuild(&root, &provider.identity_id).unwrap();
        assert_eq!(
            index.find_by_session_id("session-a").unwrap().run_id,
            "run-a"
        );
        assert_eq!(index.find_unique_name("Beta").unwrap().run_id, "run-b");
        assert!(index.find_by_session_id("session-other").is_err());
        assert!(index.find_unique_name("Alpha").is_err());
    }

    #[test]
    fn provider_session_index_prefers_index_metadata_over_rollout_creation_metadata() {
        let _home = TestHome::new();
        let provider = sample("index-priority");
        let root = auth::app_home().unwrap().join("provider-runs");
        let run = root.join(&provider.identity_id).join("run-jsonl");
        let sessions = run.join("sessions/2026/09/09");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            run.join("session_index.jsonl"),
            concat!(
                "{\"id\":\"indexed-session\",\"name\":\"Index name\",",
                "\"updated_at\":\"2026-09-09T00:00:10Z\"}\n",
                "{\"id\":\"rollout-only\",\"name\":\"Index fallback\"}\n",
            ),
        )
        .unwrap();
        std::fs::write(
            sessions.join("rollout-indexed-session.jsonl"),
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{",
                "\"id\":\"indexed-session\",\"name\":\"Rollout name\",",
                "\"timestamp\":\"2026-09-09T00:00:01Z\"}}\n",
            ),
        )
        .unwrap();
        std::fs::write(
            sessions.join("rollout-rollout-only.jsonl"),
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{",
                "\"id\":\"rollout-only\",\"timestamp\":",
                "\"2026-09-09T00:00:02Z\"}}\n",
            ),
        )
        .unwrap();

        let index = ProviderSessionIndex::rebuild(&root, &provider.identity_id).unwrap();
        let indexed = index.find_by_session_id("indexed-session").unwrap();
        assert_eq!(indexed.name, "Index name");
        assert_eq!(indexed.updated_at, "2026-09-09T00:00:10Z");
        assert_eq!(
            index.find_by_session_id("rollout-only").unwrap().updated_at,
            "2026-09-09T00:00:02Z"
        );
    }

    #[test]
    fn provider_session_index_classifies_real_session_meta_sources_for_noninteractive_filtering() {
        let _home = TestHome::new();
        let provider = sample("source-filter");
        let root = auth::app_home().unwrap().join("provider-runs");
        let cases = [
            ("exec-session", "source", "exec", "2026-09-09T00:00:04Z"),
            (
                "noninteractive-session",
                "thread_source",
                "noninteractive",
                "2026-09-09T00:00:02Z",
            ),
            (
                "unknown-session",
                "source",
                "future-source",
                "2026-09-09T00:00:03Z",
            ),
        ];
        for (session_id, source_key, source, timestamp) in cases {
            let run = root.join(&provider.identity_id).join(session_id);
            let day = run.join("sessions/2026/09/09");
            std::fs::create_dir_all(&day).unwrap();
            std::fs::write(
                day.join(format!("rollout-{session_id}.jsonl")),
                serde_json::to_string(&serde_json::json!({
                    "type": "session_meta",
                    "payload": {
                        "id": session_id,
                        "name": session_id,
                        source_key: source,
                        "timestamp": timestamp,
                    }
                }))
                .unwrap()
                    + "\n",
            )
            .unwrap();
        }
        let subagent_run = root.join(&provider.identity_id).join("subagent-session");
        let subagent_day = subagent_run.join("sessions/2026/09/09");
        std::fs::create_dir_all(&subagent_day).unwrap();
        std::fs::write(
            subagent_day.join("rollout-subagent-session.jsonl"),
            serde_json::to_string(&serde_json::json!({
                "type": "session_meta",
                "payload": {
                    "id": "subagent-session",
                    "name": "subagent-session",
                    "source": {"subagent": {"thread_spawn": {"parent_thread_id": "parent"}}},
                    "timestamp": "2026-09-09T00:00:05Z",
                }
            }))
            .unwrap()
                + "\n",
        )
        .unwrap();

        let index = ProviderSessionIndex::rebuild(&root, &provider.identity_id).unwrap();
        assert!(
            !index
                .find_by_session_id("exec-session")
                .unwrap()
                .interactive
        );
        assert!(
            !index
                .find_by_session_id("noninteractive-session")
                .unwrap()
                .interactive
        );
        assert!(
            index
                .find_by_session_id("unknown-session")
                .unwrap()
                .interactive
        );
        assert!(
            !index
                .find_by_session_id("subagent-session")
                .unwrap()
                .interactive
        );

        let interactive_only = ProviderResumeFilter {
            cwd: None,
            all: true,
            include_noninteractive: false,
        };
        assert_eq!(
            index.last(&interactive_only).unwrap().session_id,
            "unknown-session"
        );
        let all_sources = ProviderResumeFilter {
            include_noninteractive: true,
            ..interactive_only
        };
        assert_eq!(
            index.last(&all_sources).unwrap().session_id,
            "subagent-session"
        );
    }

    #[test]
    fn provider_last_selection_orders_updated_at_and_applies_scope_filters() {
        let _home = TestHome::new();
        let provider = sample("last-provider");
        let root = auth::app_home().unwrap().join("provider-runs");
        write_session_fixture(
            &root.join(&provider.identity_id).join("old"),
            &provider.identity_id,
            "old-session",
            "Old",
            "2026-09-09T00:00:01Z",
            "workspace-a",
            true,
        );
        write_session_fixture(
            &root.join(&provider.identity_id).join("cwd-new"),
            &provider.identity_id,
            "cwd-session",
            "Cwd",
            "2026-09-09T00:00:03Z",
            "workspace-a",
            true,
        );
        write_session_fixture(
            &root.join(&provider.identity_id).join("other-new"),
            &provider.identity_id,
            "other-session",
            "Other",
            "2026-09-09T00:00:04Z",
            "workspace-b",
            true,
        );
        write_session_fixture(
            &root.join(&provider.identity_id).join("noninteractive"),
            &provider.identity_id,
            "noninteractive-session",
            "Noninteractive",
            "2026-09-09T00:00:05Z",
            "workspace-a",
            false,
        );

        let index = ProviderSessionIndex::rebuild(&root, &provider.identity_id).unwrap();
        let cwd_only = ProviderResumeFilter {
            cwd: Some(PathBuf::from("workspace-a")),
            all: false,
            include_noninteractive: false,
        };
        assert_eq!(index.last(&cwd_only).unwrap().session_id, "cwd-session");
        let all = ProviderResumeFilter {
            cwd: Some(PathBuf::from("workspace-a")),
            all: true,
            include_noninteractive: false,
        };
        assert_eq!(index.last(&all).unwrap().session_id, "other-session");
        let include_noninteractive = ProviderResumeFilter {
            cwd: Some(PathBuf::from("workspace-a")),
            all: false,
            include_noninteractive: true,
        };
        assert_eq!(
            index.last(&include_noninteractive).unwrap().session_id,
            "noninteractive-session"
        );
    }

    #[test]
    fn removing_a_provider_keeps_tombstone_history_but_not_its_key_or_identity() {
        let _home = TestHome::new();
        let profile = sample("reusable");
        let identity_id = profile.identity_id.clone();
        save(&profile).unwrap();
        let mut run = ProviderCodexHome::begin(&profile).unwrap();
        run.restore().unwrap();

        remove("reusable").unwrap();
        assert!(!provider_path("reusable").unwrap().exists());
        assert!(
            auth::app_home()
                .unwrap()
                .join("provider-runs")
                .join(&identity_id)
                .join("tombstone.json")
                .exists(),
            "remove must preserve a tombstone for historical runs"
        );

        let recreated = sample("reusable");
        assert_ne!(recreated.identity_id, identity_id);
        save(&recreated).unwrap();
        assert!(
            ProviderSessionIndex::rebuild(
                &auth::app_home().unwrap().join("provider-runs"),
                &recreated.identity_id
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn a_run_resume_lease_rejects_a_second_open_of_the_same_run() {
        let _home = TestHome::new();
        let run = auth::app_home().unwrap().join("provider-runs/run-lease");
        std::fs::create_dir_all(&run).unwrap();
        let first = ProviderRunLease::acquire(&run).unwrap();
        assert!(
            ProviderRunLease::acquire(&run).is_err(),
            "resume must not open one run concurrently twice"
        );
        drop(first);
        assert!(ProviderRunLease::acquire(&run).is_ok());
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
    fn provider_paths_reject_absolute_and_parent_aliases() {
        assert!(provider_path("/tmp/outside").is_err());
        assert!(provider_path("..").is_err());
        assert!(load("/tmp/outside").is_err());
        assert!(remove("/tmp/outside").is_err());
        assert!(rename("/tmp/outside", "safe").is_err());
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

        let mut insecure_remote = sample("insecure-remote");
        insecure_remote.base_url = "http://api.example.com/v1".to_string();
        assert!(
            insecure_remote.validate().is_err(),
            "remote HTTP would expose the provider API key"
        );
        insecure_remote.allow_insecure_http = true;
        assert!(
            insecure_remote.validate().is_ok(),
            "remote HTTP requires an explicit per-provider opt-in"
        );

        let mut loopback = sample("local-gateway");
        loopback.base_url = "http://127.0.0.1:8080/v1".to_string();
        loopback.allow_insecure_http = true;
        assert!(
            loopback.validate().is_ok(),
            "loopback HTTP must remain usable"
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

        let mut profile = sample("openrouter");
        profile.record_responses_probes(&[ResponsesProbe {
            model: "openai/gpt-5.3-codex".into(),
            url: "https://openrouter.ai/api/v1/responses".into(),
            support: ResponsesSupport::Supported,
            status: 400,
            code: Some("missing_required_parameter".into()),
            message: "Missing required parameter: input".into(),
        }]);
        save(&profile).unwrap();
        let mut form_style_update = profile.clone();
        form_style_update.responses_support.clear();
        save(&form_style_update).unwrap();

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
        assert_eq!(
            loaded.responses_support_for("openai/gpt-5.3-codex"),
            Some(true),
            "a TUI edit rebuilds the public fields and must retain saved probe evidence"
        );

        remove("openrouter").unwrap();
        assert!(!exists("openrouter"));
        assert!(list_providers().unwrap().is_empty());
        assert!(remove("openrouter").is_err(), "removing twice must error");
    }

    #[test]
    fn load_rejects_an_insecure_remote_endpoint_from_disk() {
        let _home = TestHome::new();
        let profile = sample("legacy-http");
        save(&profile).unwrap();
        let path = provider_path("legacy-http").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            raw.replace("https://openrouter.ai/api/v1", "http://api.example.com/v1"),
        )
        .unwrap();

        let error = load("legacy-http").expect_err("remote HTTP must fail closed on load");
        assert!(format!("{error:#}").contains("must use https"));

        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            raw.replace(
                "base_url = \"http://api.example.com/v1\"",
                "base_url = \"http://api.example.com/v1\"\nallow_insecure_http = true",
            ),
        )
        .unwrap();
        assert!(load("legacy-http").unwrap().allow_insecure_http);
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
    fn loading_a_legacy_provider_migrates_alias_history_to_its_stable_identity() {
        let _home = TestHome::new();
        let alias = "legacy-history";
        let dir = provider_dir(alias).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let mut legacy = sample(alias);
        legacy.identity_id.clear();
        std::fs::write(
            dir.join("provider.toml"),
            toml::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();

        let history_root = auth::app_home().unwrap().join("provider-runs");
        let legacy_run = history_root.join(alias).join("run-legacy");
        std::fs::create_dir_all(&legacy_run).unwrap();
        std::fs::write(
            legacy_run.join("session_index.jsonl"),
            "{\"id\":\"legacy-session\",\"name\":\"Legacy history\",\"updated_at\":\"2026-09-09T00:00:01Z\"}\n",
        )
        .unwrap();

        let loaded = load(alias).unwrap();
        assert!(!loaded.identity_id.is_empty());
        assert!(!history_root.join(alias).exists());
        assert!(
            history_root
                .join(&loaded.identity_id)
                .join("run-legacy")
                .exists()
        );
        let index = ProviderSessionIndex::rebuild(&history_root, &loaded.identity_id).unwrap();
        assert_eq!(
            index.find_by_session_id("legacy-session").unwrap().name,
            "Legacy history"
        );
        assert_eq!(load(alias).unwrap().identity_id, loaded.identity_id);
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
    fn runtime_provider_profile_remaps_saved_transport_override_prefixes() {
        let mut profile = sample("alignment");
        profile.codex_config = vec![
            "model_providers.alignment.base_url=\"https://gateway.example/v1\"".into(),
            "model_providers.alignment.http_headers={\"X-Gateway\"=\"value\"}".into(),
            "model_providers.alignment.env_http_headers={\"X-Key\"=\"GATEWAY_KEY\"}".into(),
            "model_providers.alignment.query_params={tenant=\"acme\"}".into(),
            "model_provider=\"alignment\"".into(),
        ];

        let runtime = profile.for_runtime_provider_id("cs_identity_run");
        let args = runtime
            .codex_config_args_with(None, ReasoningLaunch::Saved)
            .unwrap();
        let joined = args.join(" ");

        assert!(
            joined.contains(
                "model_providers.cs_identity_run.base_url=\"https://gateway.example/v1\""
            )
        );
        assert!(
            joined
                .contains("model_providers.cs_identity_run.http_headers={\"X-Gateway\"=\"value\"}")
        );
        assert!(joined.contains(
            "model_providers.cs_identity_run.env_http_headers={\"X-Key\"=\"GATEWAY_KEY\"}"
        ));
        assert!(joined.contains("model_providers.cs_identity_run.query_params={tenant=\"acme\"}"));
        assert!(!joined.contains("model_providers.alignment."));
        assert!(!joined.contains("model_provider=\"alignment\""));
        assert!(joined.contains("model_provider=\"cs_identity_run\""));
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
    fn skip_drops_a_provider_extra_reasoning_override() {
        let mut p = sample("openrouter");
        p.codex_config = vec![
            "model_reasoning_effort=high".to_string(),
            "foo=bar".to_string(),
        ];
        let skipped = p
            .codex_config_args_with(None, ReasoningLaunch::Skip)
            .unwrap();
        assert!(
            !skipped
                .iter()
                .any(|a| a.starts_with("model_reasoning_effort=")),
            "skip must not let extras put thinking back on the wire: {skipped:?}"
        );
        assert!(skipped.iter().any(|a| a == "foo=bar"));

        let saved = p
            .codex_config_args_with(None, ReasoningLaunch::Saved)
            .unwrap();
        assert!(
            saved.iter().any(|a| a == "model_reasoning_effort=high"),
            "saved extras still apply when this launch did not skip"
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

    #[test]
    fn provider_home_leaves_user_model_keys_and_links_prompts() {
        let _home = TestHome::new();
        let home = crate::auth::user_codex_home().unwrap();
        let original = "model = \"gpt-5.3-codex\"\nmodel_reasoning_effort = \"high\"\ndeveloper_instructions = \"be terse\"\n\n[mcp_servers.demo]\ncommand = \"echo\"\n";
        std::fs::write(home.join("config.toml"), original).unwrap();
        std::fs::write(home.join("auth.json"), "{\"tokens\":{}}\n").unwrap();
        std::fs::write(home.join("AGENTS.md"), "# house rules\n").unwrap();
        std::fs::create_dir_all(home.join("prompts")).unwrap();
        std::fs::write(home.join("prompts/review.md"), "review this\n").unwrap();

        let mut session = ProviderCodexHome::begin("or").unwrap();
        let user_live = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert_eq!(
            user_live, original,
            "concurrent launches must not rewrite the user config.toml while Codex runs"
        );

        let isolated = std::fs::read_to_string(session.path.join("config.toml")).unwrap();
        assert!(
            !isolated.contains("model_reasoning_effort") && !isolated.contains("gpt-5.3-codex"),
            "isolated home must not carry leftover ChatGPT model/thinking: {isolated}"
        );
        assert!(
            isolated.contains("demo") && isolated.contains("be terse"),
            "isolated home must keep MCP and prompts config: {isolated}"
        );
        assert_eq!(
            std::fs::read_to_string(session.path.join("AGENTS.md")).unwrap(),
            "# house rules\n"
        );
        assert_eq!(
            std::fs::read_to_string(session.path.join("prompts/review.md")).unwrap(),
            "review this\n"
        );
        assert!(!session.path.join("auth.json").exists());
        assert_eq!(
            std::fs::read_to_string(home.join("auth.json")).unwrap(),
            "{\"tokens\":{}}\n"
        );

        let prompts_link = session.path.join("prompts");
        if prompts_link
            .symlink_metadata()
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
        {
            std::fs::write(prompts_link.join("review.md"), "updated prompt\n").unwrap();
            assert_eq!(
                std::fs::read_to_string(home.join("prompts/review.md")).unwrap(),
                "updated prompt\n",
                "prompt edits through the run dir must land in the user home"
            );
        }

        session.restore().unwrap();
        let restored = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(
            restored.contains("gpt-5.3-codex") && restored.contains("high"),
            "ChatGPT model/reasoning must stay in the user config: {restored}"
        );
        assert!(
            restored.contains("demo") && restored.contains("be terse"),
            "MCP must remain after restore: {restored}"
        );
    }

    #[test]
    fn overlapping_provider_homes_merge_disjoint_mcp_servers() {
        let _home = TestHome::new();
        let home = crate::auth::user_codex_home().unwrap();
        std::fs::write(
            home.join("config.toml"),
            "model = \"gpt-5.3-codex\"\nmodel_reasoning_effort = \"high\"\n\n[mcp_servers.demo]\ncommand = \"echo\"\n",
        )
        .unwrap();

        let mut first = ProviderCodexHome::begin("or").unwrap();
        let mut second = ProviderCodexHome::begin("or").unwrap();
        assert_ne!(first.path, second.path);

        upsert_mcp_server(&first.path.join("config.toml"), "alpha", "true");
        upsert_mcp_server(&second.path.join("config.toml"), "beta", "false");

        first.restore().unwrap();
        second.restore().unwrap();

        let restored = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(
            restored.contains("gpt-5.3-codex") && restored.contains("high"),
            "ChatGPT model/reasoning must survive overlapping launches: {restored}"
        );
        assert!(
            restored.contains("demo") && restored.contains("alpha") && restored.contains("beta"),
            "each session's MCP server must merge in: {restored}"
        );
    }

    #[test]
    fn active_provider_homes_survive_provider_rename_and_remove() {
        let _home = TestHome::new();
        let home = crate::auth::user_codex_home().unwrap();
        std::fs::write(
            home.join("config.toml"),
            "model = \"gpt-5.3-codex\"\n\n[mcp_servers.demo]\ncommand = \"echo\"\n",
        )
        .unwrap();

        save(&sample("rename-me")).unwrap();
        let mut renamed_session = ProviderCodexHome::begin("rename-me").unwrap();
        rename("rename-me", "renamed").unwrap();
        upsert_mcp_server(
            &renamed_session.path.join("config.toml"),
            "renamed-session",
            "true",
        );
        renamed_session.restore().unwrap();

        save(&sample("remove-me")).unwrap();
        let mut removed_session = ProviderCodexHome::begin("remove-me").unwrap();
        remove("remove-me").unwrap();
        upsert_mcp_server(
            &removed_session.path.join("config.toml"),
            "removed-session",
            "true",
        );
        removed_session.restore().unwrap();

        let restored = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(restored.contains("demo"));
        assert!(restored.contains("renamed-session"));
        assert!(restored.contains("removed-session"));
    }

    #[test]
    fn missing_isolated_config_never_deletes_user_config() {
        let _home = TestHome::new();
        let home = crate::auth::user_codex_home().unwrap();
        let config = home.join("config.toml");
        std::fs::write(
            &config,
            "model = \"gpt-5.3-codex\"\n\n[mcp_servers.demo]\ncommand = \"echo\"\n",
        )
        .unwrap();
        let mut session = ProviderCodexHome::begin("or").unwrap();
        std::fs::remove_file(session.path.join("config.toml")).unwrap();

        assert!(session.restore().is_err());
        let restored = std::fs::read_to_string(config).unwrap();
        assert!(restored.contains("gpt-5.3-codex"));
        assert!(restored.contains("demo"));
        session.restored = true;
    }

    #[test]
    fn provider_home_without_config_is_a_noop_then_keeps_mcp_not_gateway_model() {
        let _home = TestHome::new();
        let home = crate::auth::user_codex_home().unwrap();
        let mut session = ProviderCodexHome::begin("or").unwrap();
        assert!(!home.join("config.toml").exists());
        session.restore().unwrap();
        assert!(!home.join("config.toml").exists());

        let mut session = ProviderCodexHome::begin("or").unwrap();
        std::fs::write(
            session.path.join("config.toml"),
            "model = \"glm-5.3-flash\"\n\n[mcp_servers.demo]\ncommand = \"echo\"\n",
        )
        .unwrap();
        session.restore().unwrap();
        let restored = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(
            restored.contains("demo"),
            "MCP added in the isolated home must merge back: {restored}"
        );
        assert!(
            !restored.contains("glm-5.3-flash"),
            "gateway model must not stick in the user config: {restored}"
        );
    }

    fn upsert_mcp_server(path: &Path, name: &str, command: &str) {
        let mut root: toml::Value = std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| toml::from_str(&raw).ok())
            .unwrap_or_else(|| toml::Value::Table(toml::map::Map::new()));
        let table = root
            .as_table_mut()
            .expect("isolated config must be a table");
        let servers = table
            .entry("mcp_servers".to_string())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        let servers = servers.as_table_mut().expect("mcp_servers must be a table");
        let mut server = toml::map::Map::new();
        server.insert(
            "command".to_string(),
            toml::Value::String(command.to_string()),
        );
        servers.insert(name.to_string(), toml::Value::Table(server));
        std::fs::write(path, toml::to_string(&root).unwrap()).unwrap();
    }

    #[test]
    fn launch_args_create_and_tailor_an_offline_catalog() {
        let _home = TestHome::new();
        let mut profile = sample("zai");
        profile.models = vec![ProviderModel {
            id: "glm-5.3-flash".into(),
            reasoning: Some("high".into()),
            no_web_search: false,
        }];
        profile.default_model = "glm-5.3-flash".to_string();
        save(&profile).unwrap();
        let first_run = provider_dir("zai").unwrap().join("runs/first");
        let args = profile
            .codex_config_args_from_saved_catalog_at(None, ReasoningLaunch::Saved, &first_run)
            .unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("model_catalog_json="));
        let catalog_path = provider_dir("zai").unwrap().join("models.json");
        assert!(
            catalog_path.exists(),
            "first launch persists a local base catalog"
        );

        profile
            .write_model_catalog(
                "glm-5.3-flash",
                Some("high"),
                &[remote("glm-5.3-flash")],
                &[],
            )
            .unwrap();
        let saved_catalog = std::fs::read_to_string(&catalog_path).unwrap();
        let skip_run = provider_dir("zai").unwrap().join("runs/skip");
        let args = profile
            .codex_config_args_from_saved_catalog_at(None, ReasoningLaunch::Skip, &skip_run)
            .unwrap();
        let expected_catalog_arg = format!(
            "model_catalog_json={}",
            toml_string(&skip_run.join("models.json").to_string_lossy())
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-c", expected_catalog_arg.as_str()]),
            "launch arguments must pass the tailored catalog path as TOML"
        );
        let catalog: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(skip_run.join("models.json")).unwrap())
                .unwrap();
        assert_eq!(catalog["models"][0]["slug"], "glm-5.3-flash");
        assert_eq!(catalog["models"][0]["visibility"], "list");
        assert_eq!(catalog["models"][0]["context_window"], 8_192);
        assert!(
            catalog["models"][0]["base_instructions"]
                .as_str()
                .is_some_and(|instructions| instructions.contains("coding agent"))
        );
        assert_eq!(
            catalog["models"][0]["model_messages"]["instructions_template"],
            catalog["models"][0]["base_instructions"]
        );
        assert!(catalog["models"][0]["default_reasoning_level"].is_null());
        assert!(
            catalog["models"][0]["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            catalog["models"][0]["supports_reasoning_summary_parameter"],
            false
        );
        assert_eq!(
            std::fs::read_to_string(catalog_path).unwrap(),
            saved_catalog,
            "a one-shot launch must not weaken the persisted remote metadata"
        );
    }

    #[test]
    fn launch_args_honour_an_explicit_catalog_and_do_not_write_one() {
        let _home = TestHome::new();
        let mut profile = sample("zai");
        profile.models = vec![ProviderModel::from_id("glm-5.3-flash")];
        profile.default_model = "glm-5.3-flash".to_string();
        profile.codex_config = vec![r#"model_catalog_json="/tmp/custom-models.json""#.to_string()];
        save(&profile).unwrap();
        let launch_dir = provider_dir("zai").unwrap().join("runs/explicit");
        let args = profile
            .codex_config_args_from_saved_catalog_at(None, ReasoningLaunch::Saved, &launch_dir)
            .unwrap();
        let joined = args.join(" ");
        assert!(joined.contains(r#"model_catalog_json="/tmp/custom-models.json""#));
        assert!(!provider_dir("zai").unwrap().join("models.json").exists());
    }

    fn remote(slug: &str) -> RemoteModel {
        RemoteModel {
            slug: slug.into(),
            display_name: None,
            description: None,
            context_window: Some(8_192),
            input_modalities: vec![],
            catalog_entry: None,
        }
    }

    #[derive(serde::Deserialize)]
    struct Codex0159Catalog {
        models: Vec<Codex0159Model>,
    }

    #[derive(serde::Deserialize)]
    struct Codex0159Model {
        slug: String,
        display_name: String,
        supported_reasoning_levels: Vec<serde_json::Value>,
        shell_type: String,
        visibility: String,
        supported_in_api: bool,
        priority: i32,
        support_verbosity: bool,
        truncation_policy: Codex0159TruncationPolicy,
        experimental_supported_tools: Vec<String>,
        #[serde(default)]
        model_messages: Option<Codex0159ModelMessages>,
        #[serde(flatten)]
        other_fields: serde_json::Map<String, serde_json::Value>,
    }

    #[derive(serde::Deserialize)]
    struct Codex0159TruncationPolicy {
        mode: String,
        limit: i64,
    }

    #[derive(serde::Deserialize)]
    struct Codex0159ModelMessages {
        instructions_template: Option<String>,
    }

    fn validate_codex_0159_model_catalog(catalog: &serde_json::Value) {
        let decoded: Codex0159Catalog = serde_json::from_value(catalog.clone())
            .expect("catalog must deserialize against Codex 0.159.2 ModelInfo's required fields");
        let entries = catalog["models"].as_array().unwrap();
        assert_eq!(decoded.models.len(), entries.len());
        for (model, raw) in decoded.models.iter().zip(entries) {
            assert!(!model.slug.is_empty());
            assert!(!model.display_name.is_empty());
            assert_eq!(model.shell_type, "shell_command");
            assert_eq!(model.visibility, "list");
            assert!(model.supported_in_api);
            assert!(model.priority >= 0);
            assert!(
                model.supported_reasoning_levels.iter().all(|level| {
                    level["effort"].is_string() && level["description"].is_string()
                })
            );
            assert_eq!(
                model.support_verbosity,
                raw["support_verbosity"].as_bool().unwrap()
            );
            assert!(model.truncation_policy.limit > 0);
            assert!(matches!(
                model.truncation_policy.mode.as_str(),
                "bytes" | "tokens"
            ));
            assert!(
                model
                    .experimental_supported_tools
                    .iter()
                    .all(|tool| !tool.is_empty())
            );
            let legacy = model
                .other_fields
                .get("base_instructions")
                .and_then(serde_json::Value::as_str);
            let canonical = model
                .model_messages
                .as_ref()
                .and_then(|messages| messages.instructions_template.as_deref());
            assert!(
                legacy.is_some() || canonical.is_some(),
                "Codex 0.159.2 requires a base instruction template for {}",
                model.slug
            );
        }
    }

    fn native_catalog_row(slug: &str) -> serde_json::Value {
        serde_json::json!({
            "slug": slug,
            "display_name": "Native model",
            "description": "Full native description",
            "default_reasoning_level": "low",
            "supported_reasoning_levels": [
                {"effort": "low", "description": "Native low"},
                {"effort": "high", "description": "Native high"}
            ],
            "shell_type": "shell_command",
            "visibility": "list",
            "supported_in_api": true,
            "priority": 7,
            "model_messages": {
                "instructions_template": "Native canonical instructions",
                "confirmation_policies": {"browser_use": "Native policy"}
            },
            "base_instructions": "Legacy native instructions",
            "support_verbosity": true,
            "default_verbosity": "low",
            "supports_reasoning_summary_parameter": false,
            "supports_image_detail_original": true,
            "supports_reasoning_effort_updates": true,
            "apply_patch_tool_type": "freeform",
            "truncation_policy": {"mode": "tokens", "limit": 8192},
            "context_window": 128000,
            "max_context_window": 256000,
            "effective_context_window_percent": 91,
            "experimental_supported_tools": ["native-tool"],
            "input_modalities": ["text", "image", "audio"],
            "future_catalog_field": {"enabled": true, "version": 2}
        })
    }

    #[test]
    fn native_catalog_rows_preserve_full_metadata_and_bound_context_overrides() {
        let row = native_catalog_row("native-model");
        let remote = parse_gateway_models(&serde_json::json!({"models": [row.clone()]}));
        assert_eq!(remote.len(), 1);
        assert!(remote[0].catalog_entry.is_some());

        let catalog = build_model_catalog(
            &["native-model".into()],
            &[ProviderModel {
                id: "native-model".into(),
                reasoning: Some("high".into()),
                no_web_search: false,
            }],
            &remote,
            &[],
            "native-model",
            Some(400_000),
            Some("high"),
        );
        let model = &catalog["models"][0];
        assert_eq!(model["context_window"], 256_000);
        assert_eq!(model["max_context_window"], 256_000);
        assert_eq!(model["default_reasoning_level"], "high");
        assert_eq!(
            model["supported_reasoning_levels"],
            row["supported_reasoning_levels"]
        );
        assert_eq!(model["supports_reasoning_summary_parameter"], false);
        assert_eq!(model["supports_reasoning_effort_updates"], true);
        assert_eq!(model["supports_image_detail_original"], true);
        assert_eq!(model["input_modalities"], row["input_modalities"]);
        assert_eq!(model["model_messages"], row["model_messages"]);
        assert_eq!(model["base_instructions"], row["base_instructions"]);
        assert_eq!(model["future_catalog_field"], row["future_catalog_field"]);
        validate_codex_0159_model_catalog(&catalog);

        let mut skipped = catalog.clone();
        tailor_saved_catalog(
            &mut skipped,
            &["native-model".into()],
            &[],
            "native-model",
            &ReasoningLaunch::Skip,
            Some(500_000),
        )
        .unwrap();
        let model = &skipped["models"][0];
        assert!(model.get("default_reasoning_level").is_none());
        assert_eq!(
            model["supported_reasoning_levels"],
            row["supported_reasoning_levels"]
        );
        assert_eq!(model["context_window"], 256_000);
        assert_eq!(model["max_context_window"], 256_000);
        validate_codex_0159_model_catalog(&skipped);
    }

    #[test]
    fn missing_metadata_uses_codex_unknown_model_defaults_and_valid_catalog_shape() {
        let catalog = build_model_catalog(
            &["unknown-model".into()],
            &[ProviderModel::from_id("unknown-model")],
            &[],
            &[],
            "unknown-model",
            None,
            None,
        );
        let model = &catalog["models"][0];
        assert_eq!(model["context_window"], 272_000);
        assert!(model["max_context_window"].is_null());
        assert_eq!(model["input_modalities"], serde_json::json!(["text"]));
        assert_eq!(model["supports_image_detail_original"], false);
        assert_eq!(model["supports_reasoning_summary_parameter"], false);
        assert_eq!(model["supported_reasoning_levels"], serde_json::json!([]));
        assert!(
            model["base_instructions"].as_str().is_some_and(|text| {
                text.contains("You are a coding agent") && !text.is_empty()
            })
        );
        assert_eq!(
            model["model_messages"]["instructions_template"],
            model["base_instructions"]
        );
        validate_codex_0159_model_catalog(&catalog);

        let overridden = build_model_catalog(
            &["unknown-model".into()],
            &[ProviderModel::from_id("unknown-model")],
            &[],
            &[],
            "unknown-model",
            Some(1_000_000),
            None,
        );
        assert_eq!(overridden["models"][0]["context_window"], 1_000_000);
        assert!(overridden["models"][0]["max_context_window"].is_null());
        validate_codex_0159_model_catalog(&overridden);

        // A catalog first saved with the fallback value must still allow a
        // later explicit context override when no authoritative max is known.
        let mut saved_then_overridden = catalog.clone();
        tailor_saved_catalog(
            &mut saved_then_overridden,
            &["unknown-model".into()],
            &[ProviderModel::from_id("unknown-model")],
            "unknown-model",
            &ReasoningLaunch::Saved,
            Some(1_000_000),
        )
        .unwrap();
        assert_eq!(
            saved_then_overridden["models"][0]["context_window"],
            1_000_000
        );
        assert!(saved_then_overridden["models"][0]["max_context_window"].is_null());
        validate_codex_0159_model_catalog(&saved_then_overridden);
    }

    #[test]
    fn partial_native_catalogs_get_required_defaults_and_keep_their_fields() {
        let partial = serde_json::json!({
            "slug": "partial-native",
            "supported_reasoning_levels": [
                {"effort": "high", "description": "Native high"}
            ],
            "future_catalog_field": {"kept": true}
        });
        let remote = parse_gateway_models(&serde_json::json!({"models": [partial.clone()]}));
        assert!(remote[0].catalog_entry.is_some());
        let catalog = build_model_catalog(
            &["partial-native".into()],
            &[ProviderModel::from_id("partial-native")],
            &remote,
            &[],
            "partial-native",
            None,
            None,
        );
        let model = &catalog["models"][0];
        assert_eq!(
            model["supported_reasoning_levels"],
            partial["supported_reasoning_levels"]
        );
        assert_eq!(
            model["future_catalog_field"],
            partial["future_catalog_field"]
        );
        assert_eq!(model["shell_type"], "shell_command");
        assert_eq!(model["visibility"], "list");
        assert_eq!(model["supported_in_api"], true);
        assert_eq!(model["support_verbosity"], false);
        assert_eq!(
            model["truncation_policy"],
            serde_json::json!({"mode": "bytes", "limit": 10_000})
        );
        assert_eq!(model["input_modalities"], serde_json::json!(["text"]));
        validate_codex_0159_model_catalog(&catalog);
    }

    #[test]
    fn offline_catalog_migrates_old_generated_empty_prompt_only() {
        let mut catalog = serde_json::json!({
            "models": [
                {
                    "slug": "legacy-generated",
                    "display_name": "legacy-generated",
                    "base_instructions": "",
                    "supports_reasoning_summaries": false,
                    "supported_reasoning_levels": [],
                    "context_window": 8192,
                    "max_context_window": 8192
                },
                {
                    "slug": "native-empty",
                    "display_name": "native-empty",
                    "base_instructions": "",
                    "model_messages": {"instructions_template": ""},
                    "supports_reasoning_summaries": false,
                    "supported_reasoning_levels": [],
                    "context_window": 8192,
                    "max_context_window": 8192
                }
            ]
        });
        tailor_saved_catalog(
            &mut catalog,
            &["legacy-generated".into(), "native-empty".into()],
            &[],
            "legacy-generated",
            &ReasoningLaunch::Saved,
            None,
        )
        .unwrap();
        let generated = &catalog["models"][0];
        assert!(
            generated["base_instructions"]
                .as_str()
                .is_some_and(|instructions| instructions.contains("coding agent"))
        );
        assert!(generated.get("supports_reasoning_summaries").is_none());
        let explicit_empty = &catalog["models"][1];
        assert_eq!(explicit_empty["base_instructions"], "");
        assert_eq!(
            explicit_empty["model_messages"]["instructions_template"],
            ""
        );
    }

    #[test]
    fn catalog_lists_only_saved_slugs_even_when_the_gateway_is_small() {
        let remote = vec![
            RemoteModel {
                slug: "glm-5.3".into(),
                display_name: None,
                description: None,
                context_window: Some(200_000),
                input_modalities: vec![],
                catalog_entry: None,
            },
            RemoteModel {
                slug: "glm-5.3-flash".into(),
                display_name: Some("GLM Flash".into()),
                description: None,
                context_window: Some(1_048_576),
                input_modalities: vec!["text".into()],
                catalog_entry: None,
            },
        ];
        let catalog = build_model_catalog(
            &["glm-5.3-flash".into()],
            &[ProviderModel::from_id("glm-5.3-flash")],
            &remote,
            &[],
            "glm-5.3-flash",
            None,
            None,
        );
        assert_eq!(catalog["models"][0]["slug"], "glm-5.3-flash");
        assert_eq!(catalog["models"].as_array().unwrap().len(), 1);
        assert_eq!(catalog["models"][0]["context_window"], 1_048_576);
        assert_eq!(catalog["models"][0]["display_name"], "GLM Flash");
        assert!(catalog["models"][0]["default_reasoning_level"].is_null());
        assert_eq!(
            catalog["models"][0]["supports_reasoning_summary_parameter"],
            false
        );
        assert!(
            catalog["models"][0]["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn catalog_without_reasoning_does_not_advertise_thinking_levels() {
        let catalog = build_model_catalog(
            &["composer-2.5".into()],
            &[ProviderModel::from_id("composer-2.5")],
            &[remote("composer-2.5")],
            &[],
            "composer-2.5",
            None,
            None,
        );
        let levels = catalog["models"][0]["supported_reasoning_levels"]
            .as_array()
            .unwrap();
        assert!(
            levels.is_empty(),
            "a none default still puts reasoning.effort on Codex 0.150 requests: {levels:?}"
        );
        assert!(catalog["models"][0]["default_reasoning_level"].is_null());
        assert_eq!(
            catalog["models"][0]["supports_reasoning_summary_parameter"],
            false
        );
    }

    #[test]
    fn skip_catalog_does_not_advertise_a_saved_thinking_level() {
        let models = vec![ProviderModel {
            id: "deepseek-v4-flash".into(),
            reasoning: Some("high".into()),
            no_web_search: false,
        }];
        let catalog = build_model_catalog(
            &["deepseek-v4-flash".into()],
            &models,
            &[],
            &[],
            "deepseek-v4-flash",
            None,
            None,
        );
        assert!(catalog["models"][0]["default_reasoning_level"].is_null());
        assert!(
            catalog["models"][0]["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .is_empty(),
            "skip must not leave a default Codex 0.150 can send"
        );
        assert_eq!(
            catalog["models"][0]["supports_reasoning_summary_parameter"],
            false
        );
    }

    #[test]
    fn catalog_with_saved_reasoning_keeps_thinking_levels() {
        let models = vec![
            ProviderModel::from_id("composer-2.5"),
            ProviderModel {
                id: "glm-5.3-flash".into(),
                reasoning: Some("high".into()),
                no_web_search: false,
            },
        ];
        let catalog = build_model_catalog(
            &["composer-2.5".into(), "glm-5.3-flash".into()],
            &models,
            &[remote("composer-2.5"), remote("glm-5.3-flash")],
            &[],
            "composer-2.5",
            None,
            None,
        );
        assert_eq!(catalog["models"][0]["slug"], "composer-2.5");
        assert!(catalog["models"][0]["default_reasoning_level"].is_null());
        assert!(
            catalog["models"][0]["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(catalog["models"][1]["slug"], "glm-5.3-flash");
        assert_eq!(catalog["models"][1]["default_reasoning_level"], "high");
        assert_eq!(
            catalog["models"][1]["supports_reasoning_summary_parameter"],
            false
        );
        assert!(
            catalog["models"][1]["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn none_reasoning_is_not_passed_as_a_codex_override() {
        let mut p = sample("openrouter");
        p.models = vec![ProviderModel {
            id: "composer-2.5".into(),
            reasoning: Some("none".into()),
            no_web_search: false,
        }];
        p.default_model = "composer-2.5".into();
        let args = p.codex_config_args(None).unwrap();
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("model_reasoning_effort=")),
            "effort none must not be sent: {args:?}"
        );
    }

    #[test]
    fn generic_bad_response_404_is_inconclusive() {
        let (message, error_type, code) = openai_error_fields(
            r#"{"error":{"message":"Not Found","type":"bad_response_status_code","param":"","code":"bad_response_status_code"}}"#,
        );
        assert_eq!(message.as_deref(), Some("Not Found"));
        assert_eq!(error_type.as_deref(), Some("bad_response_status_code"));
        assert_eq!(code.as_deref(), Some("bad_response_status_code"));
        assert_eq!(
            classify_responses_probe(
                404,
                code.as_deref(),
                error_type.as_deref(),
                message.as_deref()
            ),
            ResponsesSupport::Unknown
        );
    }

    #[test]
    fn classify_missing_input_400_as_supported() {
        let (message, error_type, code) = openai_error_fields(
            r#"{"error":{"message":"Missing required parameter: 'input'.","type":"invalid_request_error","param":"input","code":"missing_required_parameter"}}"#,
        );
        assert_eq!(
            classify_responses_probe(
                400,
                code.as_deref(),
                error_type.as_deref(),
                message.as_deref()
            ),
            ResponsesSupport::Supported
        );
    }

    #[test]
    fn classify_auth_failure_as_unknown() {
        assert_eq!(
            classify_responses_probe(401, Some("unauthorized"), None, Some("Invalid API key")),
            ResponsesSupport::Unknown
        );
    }

    #[test]
    fn another_required_parameter_does_not_prove_responses_support() {
        assert_eq!(
            classify_responses_probe(
                400,
                Some("missing_required_parameter"),
                Some("invalid_request_error"),
                Some("Missing required parameter: 'model'.")
            ),
            ResponsesSupport::Unknown
        );
    }

    #[test]
    fn ambiguous_404_and_server_statuses_never_deny_a_model() {
        assert_eq!(
            classify_responses_probe(404, Some("model_not_found"), None, Some("Model not found")),
            ResponsesSupport::Unknown
        );
        assert_eq!(
            classify_responses_probe(404, None, Some("not_found"), Some("Not Found")),
            ResponsesSupport::Unknown
        );
        assert_eq!(
            classify_responses_probe(
                502,
                Some("bad_response_status_code"),
                None,
                Some("upstream failure")
            ),
            ResponsesSupport::Unknown
        );
        assert_eq!(
            classify_responses_probe(405, None, None, None),
            ResponsesSupport::Unsupported
        );
    }

    #[test]
    fn responses_verdicts_are_scoped_expiring_and_unknown_clears_denial() {
        let unsupported = ResponsesProbe {
            model: "openai/gpt-5.3-codex".into(),
            url: "https://openrouter.ai/api/v1/responses".into(),
            support: ResponsesSupport::Unsupported,
            status: 404,
            code: Some("bad_response_status_code".into()),
            message: "Not Found".into(),
        };
        let unknown = ResponsesProbe {
            support: ResponsesSupport::Unknown,
            status: 502,
            code: Some("bad_response_status_code".into()),
            message: "upstream failure".into(),
            ..unsupported.clone()
        };
        let mut profile = sample("verdicts");
        profile.record_responses_probes(std::slice::from_ref(&unsupported));
        assert_eq!(
            profile.responses_support_for("openai/gpt-5.3-codex"),
            Some(false)
        );

        profile.record_responses_probes(std::slice::from_ref(&unknown));
        assert_eq!(
            profile.responses_support_for("openai/gpt-5.3-codex"),
            None,
            "an inconclusive result must clear an earlier denial"
        );

        profile.record_responses_probes(std::slice::from_ref(&unsupported));
        profile.api_key = "changed-key".into();
        assert_eq!(
            profile.responses_support_for("openai/gpt-5.3-codex"),
            None,
            "changing the key invalidates the cached result"
        );

        profile.api_key = "sk-secret-1234".into();
        profile.record_responses_probes(std::slice::from_ref(&unsupported));
        profile
            .responses_support
            .get_mut("openai/gpt-5.3-codex")
            .unwrap()
            .checked_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - RESPONSES_SUPPORT_TTL_SECS
            - 1;
        assert_eq!(
            profile.responses_support_for("openai/gpt-5.3-codex"),
            None,
            "expired probe evidence must not block launch"
        );
    }

    #[test]
    fn saved_probe_verdicts_are_kept_for_seven_days() {
        assert_eq!(RESPONSES_SUPPORT_TTL_SECS, 7 * 24 * 60 * 60);
        let mut profile = sample("provider");
        profile.record_responses_probes(&[ResponsesProbe {
            model: "openai/gpt-5.3-codex".into(),
            url: String::new(),
            support: ResponsesSupport::Unsupported,
            status: 405,
            code: None,
            message: String::new(),
        }]);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let record = profile
            .responses_support
            .get_mut("openai/gpt-5.3-codex")
            .unwrap();
        record.checked_at = now - 6 * 24 * 60 * 60;
        assert_eq!(
            profile.responses_support_for("openai/gpt-5.3-codex"),
            Some(false),
            "a verdict from six days ago is still a candidate for a launch re-check"
        );
    }

    #[test]
    fn legacy_boolean_verdicts_deserialize_as_stale_records() {
        #[derive(Deserialize)]
        struct Stored {
            #[serde(deserialize_with = "deserialize_responses_support")]
            responses_support: BTreeMap<String, ResponsesSupportRecord>,
        }

        let stored: Stored =
            serde_json::from_str(r#"{"responses_support":{"openai/gpt-5.3-codex":false}}"#)
                .unwrap();
        let record = &stored.responses_support["openai/gpt-5.3-codex"];
        assert_eq!(record.support, ResponsesSupport::Unsupported);
        assert!(record.fingerprint.is_empty());
        assert_eq!(record.checked_at, 0);
    }

    #[tokio::test]
    async fn probe_posts_model_only_and_classifies_explicit_unsupported_route() {
        use axum::http::StatusCode;
        use axum::routing::post;
        use axum::{Json, Router};
        use serde_json::{Value, json};

        let app = Router::new().route(
            "/v1/responses",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(body, json!({"model": "deepseek-v4-flash"}));
                assert!(body.get("input").is_none());
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "error": {
                            "message": "Cannot POST /responses",
                            "type": "unsupported_endpoint",
                            "param": "",
                            "code": "unsupported_endpoint"
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

        let probe = probe_responses_support(
            &format!("http://{addr}/v1"),
            "sk-test",
            "deepseek-v4-flash",
            true,
        )
        .await
        .unwrap();
        assert_eq!(probe.support, ResponsesSupport::Unsupported);
        assert_eq!(probe.status, 404);
        assert_eq!(probe.code.as_deref(), Some("unsupported_endpoint"));
        assert_eq!(probe.message, "Cannot POST /responses");
        assert!(probe.refusal_message("AI-KR").contains("deepseek-v4-flash"));
    }

    #[tokio::test]
    async fn probe_treats_missing_input_as_supported() {
        use axum::http::StatusCode;
        use axum::routing::post;
        use axum::{Json, Router};
        use serde_json::{Value, json};

        let app = Router::new().route(
            "/v1/responses",
            post(|Json(body): Json<Value>| async move {
                assert!(body.get("input").is_none());
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": {
                            "message": "Missing required parameter: 'input'.",
                            "type": "invalid_request_error",
                            "param": "input",
                            "code": "missing_required_parameter"
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

        let probe = probe_responses_support(
            &format!("http://{addr}/v1"),
            "sk-test",
            "glm-5.3-flash",
            true,
        )
        .await
        .unwrap();
        assert_eq!(probe.support, ResponsesSupport::Supported);
        assert_eq!(probe.status, 400);
    }

    #[test]
    fn model_sync_uses_catalog_url_headers_query_and_client_version() {
        use axum::http::{HeaderMap, Uri};
        use axum::routing::get;
        use axum::{Json, Router};
        use serde_json::json;

        let _env_lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        const ENV_NAME: &str = "CODEX_SWITCH_TEST_PROVIDER_HEADER";
        struct RestoreEnv {
            previous: Option<OsString>,
        }
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                unsafe {
                    match &self.previous {
                        Some(value) => {
                            std::env::set_var("CODEX_SWITCH_TEST_PROVIDER_HEADER", value)
                        }
                        None => std::env::remove_var("CODEX_SWITCH_TEST_PROVIDER_HEADER"),
                    }
                }
            }
        }
        let restore = RestoreEnv {
            previous: std::env::var_os(ENV_NAME),
        };
        unsafe { std::env::set_var(ENV_NAME, "environment-header-value") };

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
        let app = Router::new().route(
            "/catalog",
            get(|headers: HeaderMap, uri: Uri| async move {
                assert_eq!(headers["authorization"], "Bearer stored-key");
                assert_eq!(headers["x-gateway"], "static-header-value");
                assert_eq!(headers["x-env-gateway"], "environment-header-value");
                assert_eq!(headers["x-generated-key"], "stored-key");
                let query = uri.query().unwrap_or_default();
                assert!(query.contains("catalog_token=catalog-secret"));
                assert!(query.contains("tenant=acme"));
                assert!(query.contains(&format!("client_version={}", auth::codex_cli_version())));
                Json(json!({"models": [{"slug": "catalog-model"}]}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let config = vec![
            format!(
                "model_providers.provider.model_catalog_url={}",
                toml_string(&format!("http://{addr}/catalog?catalog_token=catalog-secret"))
            ),
            "model_providers.provider.query_params={tenant=\"acme\"}".into(),
            "model_providers.provider.http_headers={\"X-Gateway\"=\"static-header-value\", Authorization=\"Bearer header-value\"}".into(),
            format!(
                "model_providers.provider.env_http_headers={{\"X-Env-Gateway\"={}, \"X-Generated-Key\"=\"CODEX_SWITCH_PROVIDER_KEY\"}}",
                toml_string(ENV_NAME)
            ),
        ];
        let models = fetch_gateway_models_with_overrides(
            &format!("http://{addr}/v1"),
            "stored-key",
            "CODEX_SWITCH_PROVIDER_KEY",
            true,
            "responses",
            "provider",
            &config,
        )
        .await
        .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "catalog-model");
            });
        drop(restore);
    }

    #[tokio::test]
    async fn responses_probe_uses_query_and_headers_and_does_not_poison_on_502() {
        use axum::http::{HeaderMap, StatusCode, Uri};
        use axum::routing::post;
        use axum::{Json, Router};
        use serde_json::{Value, json};

        let app = Router::new().route(
            "/v1/responses",
            post(
                |headers: HeaderMap, uri: Uri, Json(body): Json<Value>| async move {
                    assert_eq!(body, json!({"model": "model-x"}));
                    assert_eq!(headers["authorization"], "Bearer stored-key");
                    assert_eq!(headers["x-gateway"], "probe-header");
                    assert!(uri.query().unwrap_or_default().contains("tenant=probe"));
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({
                            "error": {
                                "message": "upstream failure included stored-key",
                                "type": "server_error",
                                "code": "bad_response_status_code"
                            }
                        })),
                    )
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base_url = format!("http://{addr}/v1");
        let config = vec![
            "model_providers.provider.query_params={tenant=\"probe\"}".into(),
            "model_providers.provider.http_headers={\"X-Gateway\"=\"probe-header\"}".into(),
        ];
        let connection = resolve_provider_connection_from_parts(
            &base_url,
            "stored-key",
            "CODEX_SWITCH_PROVIDER_KEY",
            true,
            "responses",
            "provider",
            &config,
        )
        .unwrap();
        let probe = probe_responses_support_with_connection(&connection, "model-x")
            .await
            .unwrap();
        assert_eq!(probe.support, ResponsesSupport::Unknown);
        assert_eq!(probe.status, StatusCode::BAD_GATEWAY.as_u16());
        assert!(!probe.url.contains("tenant=probe"));
        assert!(!probe.message.contains("stored-key"));
        assert!(probe.message.contains("[redacted]"));

        let mut profile = sample("provider");
        profile.base_url = base_url;
        profile.api_key = "stored-key".into();
        profile.allow_insecure_http = true;
        profile.codex_config = config;
        profile.models[0].id = "model-x".into();
        profile.default_model = "model-x".into();
        let prior_denial = ResponsesProbe {
            support: ResponsesSupport::Unsupported,
            status: 404,
            code: Some("unsupported_endpoint".into()),
            message: "Cannot POST /responses".into(),
            ..probe.clone()
        };
        profile.record_responses_probes(&[prior_denial]);
        assert_eq!(profile.responses_support_for("model-x"), Some(false));
        profile.record_responses_probes(&[probe]);
        assert_eq!(profile.responses_support_for("model-x"), None);
    }

    #[test]
    fn non_ascii_header_values_resolve_and_reach_the_request_headers() {
        // `HeaderValue::try_from(&str)` accepts obs-text bytes that `to_str`
        // rejects; resolving such a provider must not fail.
        let config =
            vec!["model_providers.provider.http_headers={\"X-Tenant\"=\"caf\u{e9}\"}".to_string()];
        let connection = resolve_provider_connection_from_parts(
            "https://gateway.example/v1",
            "stored-key",
            "CODEX_SWITCH_PROVIDER_KEY",
            false,
            "responses",
            "provider",
            &config,
        )
        .expect("a non-ASCII header value must not break connection resolution");
        assert_eq!(connection.headers["x-tenant"], "caf\u{e9}");
        assert!(!connection.fingerprint().is_empty());
        let headers = provider_http_headers(&connection, false).unwrap();
        assert_eq!(headers["x-tenant"].as_bytes(), "caf\u{e9}".as_bytes());
    }

    #[test]
    fn provider_url_diagnostics_only_show_the_origin() {
        let url = reqwest::Url::parse(
            "https://user:pass@example.com:8443/v1/path-token?api_key=query-secret#fragment-secret",
        )
        .unwrap();
        let display = display_safe_url(&url);

        assert_eq!(display, "https://example.com:8443");
        assert!(!display.contains("user"));
        assert!(!display.contains("pass"));
        assert!(!display.contains("path-token"));
        assert!(!display.contains("query-secret"));
        assert!(!display.contains("fragment-secret"));
    }

    #[test]
    fn full_provider_table_overrides_are_rejected_before_transport() {
        for override_value in [
            r#"model_providers.provider={base_url="https://gateway.example/v1"}"#,
            r#"model_providers={provider={base_url="https://gateway.example/v1"}}"#,
        ] {
            let error = resolve_provider_connection_from_parts(
                "https://api.example.com/v1",
                "stored-key",
                "CODEX_SWITCH_PROVIDER_KEY",
                false,
                "responses",
                "provider",
                &[override_value.into()],
            )
            .unwrap_err();
            assert!(error.to_string().contains("full-table provider overrides"));
        }
    }

    #[test]
    fn provider_http_settings_still_require_the_insecure_http_opt_in() {
        assert!(
            resolve_provider_connection_from_parts(
                "https://api.example.com/v1",
                "stored-key",
                "CODEX_SWITCH_PROVIDER_KEY",
                false,
                "responses",
                "provider",
                &[r#"model_providers.provider.base_url="http://127.0.0.1:8080/v1""#.into()],
            )
            .is_err()
        );
        assert!(
            resolve_provider_connection_from_parts(
                "https://api.example.com/v1",
                "stored-key",
                "CODEX_SWITCH_PROVIDER_KEY",
                false,
                "responses",
                "provider",
                &[
                    r#"model_providers.provider.model_catalog_url="http://catalog.example/models""#
                        .into()
                ],
            )
            .is_err()
        );
    }

    #[test]
    fn fetch_drops_embedding_and_reranker_slugs() {
        let slugs = chat_slugs_from_gateway(&[
            remote("glm-5.3-flash"),
            remote("deepseek-v4-flash"),
            remote("Qwen/Qwen3-Embedding-0.6B"),
            remote("Qwen/Qwen3-Reranker-8B"),
            remote("text-embedding-3-small"),
            remote("nomic-embed-text"),
        ])
        .unwrap();
        assert_eq!(slugs, vec!["glm-5.3-flash", "deepseek-v4-flash"]);
        assert!(!is_vector_model_slug("glm-5.3-flash"));
        assert!(!is_vector_model_slug("remember"));
        assert!(is_vector_model_slug("Qwen/Qwen3-Embedding-4B"));
        assert!(is_vector_model_slug("BAAI/bge-reranker-v2-m3"));
    }

    #[test]
    fn fetch_lists_chat_slugs_even_when_the_catalog_is_large() {
        let remote: Vec<RemoteModel> = (0..SMALL_REMOTE_CATALOG_LIMIT + 1)
            .map(|i| remote(&format!("vendor/model-{i}")))
            .collect();
        let slugs = chat_slugs_from_gateway(&remote).unwrap();
        assert_eq!(slugs.len(), SMALL_REMOTE_CATALOG_LIMIT + 1);
        assert_eq!(slugs[0], "vendor/model-0");
    }

    #[test]
    fn fetch_large_catalog_without_picks_is_refused() {
        let remote: Vec<RemoteModel> = (0..SMALL_REMOTE_CATALOG_LIMIT + 1)
            .map(|i| remote(&format!("vendor/model-{i}")))
            .collect();
        let err = apply_fetched_models(&[], None, &remote, &[])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("pass --model") && err.contains("vendor/model-0"),
            "large catalogs must be picked by hand: {err}"
        );
    }

    #[test]
    fn fetch_large_catalog_keeps_only_cli_picks() {
        let remote: Vec<RemoteModel> = (0..SMALL_REMOTE_CATALOG_LIMIT + 1)
            .map(|i| remote(&format!("vendor/model-{i}")))
            .collect();
        let existing = vec![ProviderModel {
            id: "vendor/model-0".into(),
            reasoning: Some("high".into()),
            no_web_search: true,
        }];
        let picks = vec![
            ProviderModel::from_id("vendor/model-0"),
            ProviderModel {
                id: "vendor/model-2".into(),
                reasoning: Some("low".into()),
                no_web_search: false,
            },
        ];
        let (models, default) =
            apply_fetched_models(&existing, Some("vendor/model-0"), &remote, &picks).unwrap();
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["vendor/model-0", "vendor/model-2"]
        );
        assert_eq!(default, "vendor/model-0");
        assert_eq!(models[0].reasoning.as_deref(), Some("high"));
        assert!(models[0].no_web_search);
        assert_eq!(models[1].reasoning.as_deref(), Some("low"));
    }

    #[test]
    fn fetch_large_catalog_rejects_a_pick_not_on_the_gateway() {
        let remote: Vec<RemoteModel> = (0..SMALL_REMOTE_CATALOG_LIMIT + 1)
            .map(|i| remote(&format!("vendor/model-{i}")))
            .collect();
        let err = apply_fetched_models(
            &[],
            None,
            &remote,
            &[ProviderModel::from_id("missing/slug")],
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("missing/slug") && err.contains("not in gateway"),
            "{err}"
        );
    }

    #[test]
    fn fetch_keeps_cli_models_first_and_reuses_saved_settings() {
        let existing = vec![ProviderModel {
            id: "glm-5.3-flash".into(),
            reasoning: Some("high".into()),
            no_web_search: true,
        }];
        let prepend = vec![ProviderModel::from_id("composer-2.5")];
        let (models, default) = apply_fetched_models(
            &existing,
            Some("composer-2.5"),
            &[remote("glm-5.3-flash"), remote("deepseek-v4-flash")],
            &prepend,
        )
        .unwrap();
        assert_eq!(default, "composer-2.5");
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["composer-2.5", "glm-5.3-flash", "deepseek-v4-flash"]
        );
        assert_eq!(models[1].reasoning.as_deref(), Some("high"));
        assert!(models[1].no_web_search);
    }

    #[test]
    fn fetch_replaces_the_saved_list_and_keeps_a_still_listed_default() {
        let existing = vec![
            ProviderModel::from_id("composer-2.5"),
            ProviderModel {
                id: "glm-5.3-flash".into(),
                reasoning: Some("low".into()),
                no_web_search: false,
            },
        ];
        let (models, default) = apply_fetched_models(
            &existing,
            Some("glm-5.3-flash"),
            &[remote("glm-5.3-flash"), remote("gemini-3-flash")],
            &[],
        )
        .unwrap();
        assert_eq!(default, "glm-5.3-flash");
        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["glm-5.3-flash", "gemini-3-flash"]
        );
        assert_eq!(models[0].reasoning.as_deref(), Some("low"));
    }

    #[tokio::test]
    async fn fetch_gateway_models_at_reads_openai_style_ids() {
        use axum::Json;
        use axum::Router;
        use axum::routing::get;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/v1/models",
            get(|| async {
                Json(serde_json::json!({
                    "data": [
                        {"id": "glm-5.3-flash"},
                        {"id": "Qwen/Qwen3-Embedding-0.6B"}
                    ]
                }))
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let remote = fetch_gateway_models_at(&format!("http://{addr}/v1"), "sk-test", true)
            .await
            .expect("GET /v1/models");
        server.abort();
        assert_eq!(
            chat_slugs_from_gateway(&remote).unwrap(),
            vec!["glm-5.3-flash"]
        );
    }

    #[tokio::test]
    async fn fetch_gateway_models_rejects_remote_http_before_requesting() {
        let error = fetch_gateway_models_at("http://api.example.com/v1", "sk-test", false)
            .await
            .expect_err("Bearer credentials must never be sent over remote HTTP");
        assert!(error.to_string().contains("must use https"));
    }
}
