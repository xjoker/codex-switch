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
//! (`$CODEX_SWITCH_HOME/providers/<alias>/provider.toml`, mode `0600`). At
//! launch the profile is translated into `codex -c …` overrides (model and
//! endpoint) while the key is injected into the child process environment
//! under `env_key` — never onto the command line. Each launch gets its own
//! Codex home under `$CODEX_SWITCH_HOME/providers/<alias>/runs/` so concurrent
//! models do not share sqlite or rewrite the user's `config.toml` model keys.
//! `prompts/`, `skills/`, and `AGENTS.md` are linked to the user home; MCP and
//! other non-model keys are copied into the run `config.toml` and three-way
//! merged back on exit. `auth.json` is not swapped. Model and endpoint come
//! from `-c`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
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
    /// Last conclusive result from the explicit `provider probe` command.
    /// Missing means unknown; launch never performs this network request.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    responses_support: BTreeMap<String, bool>,
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
        self.responses_support.get(model).copied()
    }

    pub(crate) fn record_responses_probes(&mut self, probes: &[ResponsesProbe]) {
        for probe in probes {
            match probe.support {
                ResponsesSupport::Supported => {
                    self.responses_support.insert(probe.model.clone(), true);
                }
                ResponsesSupport::Unsupported => {
                    self.responses_support.insert(probe.model.clone(), false);
                }
                ResponsesSupport::Unknown => {}
            }
        }
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
        apply_catalog_reasoning(&mut entry, effort)?;
        let object = entry
            .as_object_mut()
            .context("saved provider catalog model is not an object")?;
        object.insert(
            "priority".into(),
            serde_json::Value::from(i64::try_from(priority).unwrap_or(i64::MAX)),
        );
        if slug == selected_slug
            && let Some(context_window) = selected_context_window
        {
            object.insert("context_window".into(), context_window.into());
            object.insert("max_context_window".into(), context_window.into());
        }
        ordered.push(entry);
    }
    *entries = ordered;
    Ok(())
}

/// Codex's fallback for an unknown slug is a 272k window. Custom models such as
/// GLM-5.3 Flash are 1M; using the fallback is what the metadata warning means
/// by "degrade performance".
const DEFAULT_PROVIDER_CONTEXT_WINDOW: i64 = 1_048_576;

/// Gateways at or under this size can be imported wholesale with
/// `--fetch-models` / TUI `f`. Larger catalogs (OpenRouter is hundreds) must
/// be picked with `--model` or the TUI picker.
pub(crate) const SMALL_REMOTE_CATALOG_LIMIT: usize = 48;

const GATEWAY_MODELS_TIMEOUT: Duration = Duration::from_secs(8);

const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

/// Fields from a gateway `GET {base_url}/models` row that Codex's catalog uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteModel {
    pub slug: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub context_window: Option<i64>,
    pub input_modalities: Vec<String>,
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

/// `GET {base_url}/models` with the provider key for an explicit sync action.
pub(crate) async fn fetch_gateway_models(profile: &ProviderProfile) -> Result<Vec<RemoteModel>> {
    let models = fetch_gateway_models_at(&profile.base_url, &profile.api_key).await?;
    debug!(
        "provider '{}' gateway /models returned {} entries",
        profile.alias,
        models.len()
    );
    Ok(models)
}

pub(crate) async fn fetch_gateway_models_at(
    base_url: &str,
    api_key: &str,
) -> Result<Vec<RemoteModel>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    fetch_models_url(&url, Some(api_key)).await
}

/// Same as [`fetch_gateway_models_at`] from a sync caller (CLI add, TUI `f`).
pub(crate) fn fetch_gateway_models_blocking(
    base_url: &str,
    api_key: &str,
) -> Result<Vec<RemoteModel>> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| {
            handle.block_on(fetch_gateway_models_at(base_url, api_key))
        }),
        Err(_) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("starting runtime for gateway /models")?;
            runtime.block_on(fetch_gateway_models_at(base_url, api_key))
        }
    }
}

pub(crate) async fn fetch_fallback_models(source: &str) -> Result<Vec<RemoteModel>> {
    let source = source.trim();
    if is_none_fallback(source) {
        return Ok(Vec::new());
    }
    if source.starts_with("http://") || source.starts_with("https://") {
        let models = fetch_models_url(source, None).await?;
        debug!(
            "metadata fallback GET {source} returned {} entries",
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
    let mut request = client.get(url).timeout(GATEWAY_MODELS_TIMEOUT);
    if let Some(key) = bearer {
        request = request.header("Authorization", format!("Bearer {key}"));
    }
    let response = request.send().await.with_context(|| format!("GET {url}"))?;
    let status = response.status();
    let body = response.bytes().await.context("reading /models body")?;
    if !status.is_success() {
        anyhow::bail!("GET {url} returned {status}");
    }
    let value: serde_json::Value = serde_json::from_slice(&body).context("parsing /models JSON")?;
    Ok(parse_gateway_models(&value))
}

/// Whether `{base_url}/responses` will accept this slug for Codex.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
) -> Result<ResponsesProbe> {
    let url = format!("{}/responses", base_url.trim_end_matches('/'));
    let client = auth::build_http_client()?;
    let response = client
        .post(&url)
        .timeout(GATEWAY_MODELS_TIMEOUT)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "model": model }))
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = response.status().as_u16();
    let body = response
        .bytes()
        .await
        .context("reading /responses probe body")?;
    let body_text = String::from_utf8_lossy(&body);
    let (message, error_type, code) = openai_error_fields(&body_text);
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
        url,
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
    let slugs: Vec<String> = match model {
        Some(id) => {
            let selected = profile.resolve_model(Some(id))?;
            vec![selected.id.clone()]
        }
        None => profile.models.iter().map(|m| m.id.clone()).collect(),
    };
    let mut results = Vec::with_capacity(slugs.len());
    for slug in slugs {
        results.push(probe_responses_support(&profile.base_url, &profile.api_key, &slug).await?);
    }
    Ok(results)
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

    if status == 404
        || status == 405
        || blob.contains("bad_response_status_code")
        || ((400..500).contains(&status) && blob.contains("not found"))
    {
        return ResponsesSupport::Unsupported;
    }
    if matches!(status, 400 | 422) {
        if blob.contains("model_not_found") || blob.contains("model not found") {
            return ResponsesSupport::Unsupported;
        }
        return ResponsesSupport::Supported;
    }
    if status == 200 {
        return ResponsesSupport::Supported;
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
            debug!("metadata fallback unavailable ({source}): {err:#}");
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
    }
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
        if (name == "text" || name == "image") && !out.iter().any(|existing| existing == name) {
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
    Some(RemoteModel {
        slug: slug.to_string(),
        display_name: pick_text(
            primary.and_then(|model| model.display_name.as_ref()),
            fallback.and_then(|model| model.display_name.as_ref()),
        ),
        description: pick_text(
            primary.and_then(|model| model.description.as_ref()),
            fallback.and_then(|model| model.description.as_ref()),
        ),
        context_window: primary
            .and_then(|model| model.context_window)
            .or_else(|| fallback.and_then(|model| model.context_window)),
        input_modalities: if primary_modalities.is_empty() {
            fallback_modalities.to_vec()
        } else {
            primary_modalities.to_vec()
        },
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
    if slug == default_slug
        && let Some(value) = user_context
    {
        return value;
    }
    if let Some(value) = remote.and_then(|model| model.context_window) {
        return value;
    }
    DEFAULT_PROVIDER_CONTEXT_WINDOW
}

/// A Codex `model_catalog_json` body. Each `slug` is listed with
/// `visibility: list` so `/model` can show it. Codex 0.149 requires
/// `base_instructions` (an empty string is accepted).
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

const THINKING_REASONING_LEVELS: &[(&str, &str)] = &[
    ("low", "Light reasoning"),
    ("medium", "Balanced"),
    ("high", "Enhanced reasoning"),
    ("xhigh", "Extra high reasoning"),
    ("max", "Deep reasoning"),
];

fn apply_catalog_reasoning(entry: &mut serde_json::Value, reasoning: Option<&str>) -> Result<()> {
    let thinking = thinking_effort(reasoning);
    // Codex 0.150 always puts `reasoning.effort` on POST /responses when the
    // catalog has a default (including `none`). Skip/plain-chat slugs must
    // advertise no levels and omit the default so the field stays off the wire.
    let levels: Vec<serde_json::Value> = match thinking {
        Some(effort) => {
            let mut levels: Vec<serde_json::Value> = THINKING_REASONING_LEVELS
                .iter()
                .map(|(level, description)| {
                    serde_json::json!({ "effort": level, "description": description })
                })
                .collect();
            if !levels.iter().any(|level| level["effort"] == effort) {
                levels.insert(
                    0,
                    serde_json::json!({ "effort": effort, "description": effort }),
                );
            }
            levels
        }
        None => Vec::new(),
    };
    let object = entry
        .as_object_mut()
        .context("provider catalog model is not an object")?;
    object.insert("supported_reasoning_levels".into(), levels.into());
    object.insert(
        "supports_reasoning_summaries".into(),
        thinking.is_some().into(),
    );
    match thinking {
        Some(effort) => {
            object.insert("default_reasoning_level".into(), effort.into());
        }
        None => {
            object.remove("default_reasoning_level");
        }
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
    let modalities: Vec<String> = meta
        .map(|model| model.input_modalities.clone())
        .filter(|values| !values.is_empty())
        .unwrap_or_else(|| vec!["text".to_string(), "image".to_string()]);
    let mut entry = serde_json::json!({
        "slug": slug,
        "display_name": display_name,
        "description": description,
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": priority,
        "base_instructions": "",
        "default_reasoning_summary": "none",
        "support_verbosity": false,
        "apply_patch_tool_type": "freeform",
        "truncation_policy": {"mode": "bytes", "limit": 10000},
        "context_window": context_window,
        "max_context_window": context_window,
        "effective_context_window_percent": 95,
        "experimental_supported_tools": [],
        "input_modalities": modalities,
    });
    apply_catalog_reasoning(&mut entry, reasoning).expect("catalog entry is an object");
    entry
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

/// Keys that provider `launch` supplies via `codex -c`. They stay in the user's
/// `config.toml` (ChatGPT) and are omitted from the per-launch Codex home.
const PROVIDER_SESSION_KEYS: [&str; 6] = [
    "model",
    "model_provider",
    "model_reasoning_effort",
    "model_catalog_json",
    "model_providers",
    "web_search",
];

const USER_PROMPT_LINKS: [&str; 3] = ["AGENTS.md", "prompts", "skills"];

/// Per-launch Codex home for a custom provider.
///
/// Concurrent `launch` processes must not share sqlite or rewrite the user's
/// `config.toml` model keys. Each run directory links `prompts/`, `skills/`,
/// and `AGENTS.md` to the user home, copies non-model config (MCP, …), and
/// three-way-merges those keys back on exit.
pub(crate) struct ProviderCodexHome {
    pub path: PathBuf,
    user_config_path: PathBuf,
    base_config: Option<toml::Value>,
    restored: bool,
}

impl ProviderCodexHome {
    pub(crate) fn begin(alias: &str) -> Result<Self> {
        let user_home = auth::user_codex_home()?;
        let user_config_path = user_home.join("config.toml");
        let base_config = load_toml_if_present(&user_config_path)?;
        let path = unique_run_dir(alias)?;
        ensure_private_dir(&path)?;
        for name in USER_PROMPT_LINKS {
            link_user_entry(&user_home.join(name), &path.join(name))?;
        }
        if let Some(base) = &base_config {
            let mut live = base.clone();
            strip_provider_session_keys(&mut live);
            write_codex_config(&path.join("config.toml"), &live)?;
        }
        Ok(Self {
            path,
            user_config_path,
            base_config,
            restored: false,
        })
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
    Ok(provider_dir(alias)?
        .join("runs")
        .join(format!("{}-{nanos}-{seq}", std::process::id())))
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
    if stored.responses_support.is_empty()
        && let Ok(existing) = load(&stored.alias)
        && existing.base_url == stored.base_url
    {
        stored.responses_support = existing.responses_support;
    }
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

        let mut profile = sample("openrouter");
        profile
            .responses_support
            .insert("openai/gpt-5.3-codex".into(), true);
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
        assert!(args.join(" ").contains(&skip_run.display().to_string()));
        let catalog: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(skip_run.join("models.json")).unwrap())
                .unwrap();
        assert_eq!(catalog["models"][0]["slug"], "glm-5.3-flash");
        assert_eq!(catalog["models"][0]["visibility"], "list");
        assert_eq!(catalog["models"][0]["context_window"], 8_192);
        assert_eq!(catalog["models"][0]["base_instructions"], "");
        assert!(catalog["models"][0]["default_reasoning_level"].is_null());
        assert!(
            catalog["models"][0]["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(catalog["models"][0]["supports_reasoning_summaries"], false);
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
        }
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
            },
            RemoteModel {
                slug: "glm-5.3-flash".into(),
                display_name: Some("GLM Flash".into()),
                description: None,
                context_window: Some(1_048_576),
                input_modalities: vec!["text".into()],
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
        assert_eq!(catalog["models"][0]["supports_reasoning_summaries"], false);
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
        assert_eq!(catalog["models"][0]["supports_reasoning_summaries"], false);
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
        assert_eq!(catalog["models"][0]["supports_reasoning_summaries"], false);
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
        assert_eq!(catalog["models"][1]["supports_reasoning_summaries"], true);
        assert!(
            catalog["models"][1]["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .iter()
                .any(|level| level["effort"] == "high")
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
    fn classify_new_api_404_as_unsupported() {
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
            ResponsesSupport::Unsupported
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

    #[tokio::test]
    async fn probe_posts_model_only_and_treats_new_api_404_as_unsupported() {
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
                            "message": "Not Found",
                            "type": "bad_response_status_code",
                            "param": "",
                            "code": "bad_response_status_code"
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

        let probe =
            probe_responses_support(&format!("http://{addr}/v1"), "sk-test", "deepseek-v4-flash")
                .await
                .unwrap();
        assert_eq!(probe.support, ResponsesSupport::Unsupported);
        assert_eq!(probe.status, 404);
        assert_eq!(probe.code.as_deref(), Some("bad_response_status_code"));
        assert_eq!(probe.message, "Not Found");
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

        let probe =
            probe_responses_support(&format!("http://{addr}/v1"), "sk-test", "glm-5.3-flash")
                .await
                .unwrap();
        assert_eq!(probe.support, ResponsesSupport::Supported);
        assert_eq!(probe.status, 400);
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
        let remote = fetch_gateway_models_at(&format!("http://{addr}/v1"), "sk-test")
            .await
            .expect("GET /v1/models");
        server.abort();
        assert_eq!(
            chat_slugs_from_gateway(&remote).unwrap(),
            vec!["glm-5.3-flash"]
        );
    }
}
