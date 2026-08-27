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
//! the process table. The Codex child also gets its own `CODEX_HOME` under
//! `providers/<alias>/codex-home`, so sessions, sqlite, and project trust stay
//! in `$CODEX_SWITCH_HOME` rather than the user's `~/.codex`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

    pub(crate) fn has_explicit_model_catalog(&self) -> bool {
        override_value(&self.codex_config, "model_catalog_json").is_some()
    }

    /// Launch-time `-c` overrides plus a generated model catalog unless the
    /// provider already has an explicit `model_catalog_json` override.
    pub(crate) fn codex_config_args_from_remote(
        &self,
        model_id: Option<&str>,
        reasoning: ReasoningLaunch,
        remote: &[RemoteModel],
        fallback: &[RemoteModel],
    ) -> Result<Vec<String>> {
        let mut args = self.codex_config_args_with(model_id, reasoning.clone())?;
        if self.has_explicit_model_catalog() {
            return Ok(args);
        }
        let model = self.resolve_model(model_id)?;
        let effort = match &reasoning {
            ReasoningLaunch::Saved => model.reasoning.as_deref(),
            ReasoningLaunch::Skip => None,
            ReasoningLaunch::Effort(value) => Some(value.as_str()),
        };
        let path = self.write_model_catalog(&model.id, effort, remote, fallback)?;
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

/// Codex's fallback for an unknown slug is a 272k window. Custom models such as
/// GLM-5.3 Flash are 1M; using the fallback is what the metadata warning means
/// by "degrade performance".
const DEFAULT_PROVIDER_CONTEXT_WINDOW: i64 = 1_048_576;

/// Gateways at or under this size are injected wholesale into Codex `/model`.
/// Larger catalogs (OpenRouter is hundreds) stay limited to saved slugs.
const SMALL_REMOTE_CATALOG_LIMIT: usize = 48;

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

/// `GET {base_url}/models` with the provider key. Failure is the caller's to
/// swallow: launch must still proceed with generated defaults.
pub(crate) async fn fetch_gateway_models(profile: &ProviderProfile) -> Result<Vec<RemoteModel>> {
    let url = format!("{}/models", profile.base_url.trim_end_matches('/'));
    let models = fetch_models_url(&url, Some(profile.api_key.as_str())).await?;
    debug!(
        "provider '{}' gateway /models returned {} entries",
        profile.alias,
        models.len()
    );
    Ok(models)
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

/// Primary gateway list plus fallback metadata when the gateway did not cover
/// every catalog slug's `context_window`. Fallback never decides which slugs
/// are injected into `/model`.
pub(crate) async fn load_remote_catalog(
    profile: &ProviderProfile,
) -> (Vec<RemoteModel>, Vec<RemoteModel>) {
    let primary = match fetch_gateway_models(profile).await {
        Ok(models) => models,
        Err(err) => {
            debug!("provider gateway /models unavailable: {err:#}");
            Vec::new()
        }
    };
    let source = metadata_fallback_source(profile);
    if !needs_metadata_fallback(
        &profile.saved_model_slugs(&profile.default_model),
        &primary,
        &profile.base_url,
        &source,
    ) {
        return (primary, Vec::new());
    }
    let fallback = match fetch_fallback_models(&source).await {
        Ok(models) => models,
        Err(err) => {
            debug!("metadata fallback unavailable ({source}): {err:#}");
            Vec::new()
        }
    };
    (primary, fallback)
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
    select_catalog_slugs(saved, primary).iter().any(|slug| {
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

fn select_catalog_slugs(saved: &[String], remote: &[RemoteModel]) -> Vec<String> {
    let mut out = Vec::new();
    for slug in saved {
        if !slug.is_empty() && !out.iter().any(|existing| existing == slug) {
            out.push(slug.clone());
        }
    }
    if !remote.is_empty() && remote.len() <= SMALL_REMOTE_CATALOG_LIMIT {
        for model in remote {
            if !out.iter().any(|existing| existing == &model.slug) {
                out.push(model.slug.clone());
            }
        }
    }
    out
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
    remote: &[RemoteModel],
    fallback: &[RemoteModel],
    default_slug: &str,
    user_context: Option<i64>,
    default_reasoning: Option<&str>,
) -> serde_json::Value {
    let models: Vec<serde_json::Value> = select_catalog_slugs(saved, remote)
        .iter()
        .enumerate()
        .map(|(index, slug)| {
            let owned = overlay_remote_metadata(slug, remote, fallback);
            let meta = owned.as_ref();
            catalog_entry(
                slug,
                entry_context_window(slug, default_slug, user_context, meta),
                (slug == default_slug)
                    .then_some(default_reasoning)
                    .flatten(),
                meta,
                i64::try_from(index).unwrap_or(i64::MAX),
            )
        })
        .collect();
    serde_json::json!({ "models": models })
}

fn catalog_entry(
    slug: &str,
    context_window: i64,
    default_reasoning: Option<&str>,
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
        "supported_reasoning_levels": [
            {"effort": "low", "description": "Light reasoning"},
            {"effort": "medium", "description": "Balanced"},
            {"effort": "high", "description": "Enhanced reasoning"},
            {"effort": "xhigh", "description": "Extra high reasoning"},
            {"effort": "max", "description": "Deep reasoning"},
        ],
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
    if let Some(effort) = default_reasoning.filter(|value| !value.is_empty()) {
        entry["default_reasoning_level"] = serde_json::Value::String(effort.to_string());
    }
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

/// Codex runtime dir for one provider. Sessions, sqlite, and project trust
/// live here so `launch` does not write them into the user's `$CODEX_HOME`.
pub(crate) fn isolated_codex_home(alias: &str) -> Result<PathBuf> {
    Ok(provider_dir(alias)?.join("codex-home"))
}

/// Create the isolated Codex home under `$CODEX_SWITCH_HOME`. Nothing is
/// copied from the user's `$CODEX_HOME`.
pub(crate) fn prepare_isolated_codex_home(alias: &str) -> Result<PathBuf> {
    let dest = isolated_codex_home(alias)?;
    ensure_private_dir(&dest)?;
    Ok(dest)
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

    #[test]
    fn isolated_codex_home_lives_under_switch_home_and_does_not_copy_user_files() {
        let _home = TestHome::new();
        let user_codex = tempfile::tempdir().unwrap();
        std::fs::write(
            user_codex.path().join("config.toml"),
            "mcp_servers = { demo = { command = \"echo\" } }\n",
        )
        .unwrap();
        std::fs::write(user_codex.path().join("auth.json"), "{\"tokens\":{}}\n").unwrap();

        let previous = std::env::var_os("CODEX_HOME");
        unsafe {
            std::env::set_var("CODEX_HOME", user_codex.path());
        }
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(value) => std::env::set_var("CODEX_HOME", value),
                        None => std::env::remove_var("CODEX_HOME"),
                    }
                }
            }
        }
        let _restore = Restore(previous);

        save(&sample("zai")).unwrap();
        let isolated = prepare_isolated_codex_home("zai").unwrap();
        assert!(isolated.starts_with(crate::auth::app_home().unwrap()));
        assert!(isolated.ends_with(std::path::Path::new("providers/zai/codex-home")));
        assert!(!isolated.join("config.toml").exists());
        assert!(!isolated.join("auth.json").exists());
        assert_eq!(
            std::fs::read_to_string(user_codex.path().join("config.toml")).unwrap(),
            "mcp_servers = { demo = { command = \"echo\" } }\n"
        );
    }

    #[test]
    fn launch_args_write_a_catalog_for_an_unknown_slug() {
        let _home = TestHome::new();
        let mut profile = sample("zai");
        profile.models = vec![ProviderModel::from_id("glm-5.3-flash")];
        profile.default_model = "glm-5.3-flash".to_string();
        save(&profile).unwrap();
        let args = profile
            .codex_config_args_from_remote(None, ReasoningLaunch::Saved, &[], &[])
            .unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("model_catalog_json="));
        let catalog_path = provider_dir("zai").unwrap().join("models.json");
        let catalog: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&catalog_path).unwrap()).unwrap();
        assert_eq!(catalog["models"][0]["slug"], "glm-5.3-flash");
        assert_eq!(catalog["models"][0]["visibility"], "list");
        assert_eq!(catalog["models"][0]["context_window"], 1_048_576);
        assert_eq!(catalog["models"][0]["base_instructions"], "");
    }

    #[test]
    fn launch_args_honour_an_explicit_catalog_and_do_not_write_one() {
        let _home = TestHome::new();
        let mut profile = sample("zai");
        profile.models = vec![ProviderModel::from_id("glm-5.3-flash")];
        profile.default_model = "glm-5.3-flash".to_string();
        profile.codex_config = vec![r#"model_catalog_json="/tmp/custom-models.json""#.to_string()];
        save(&profile).unwrap();
        let args = profile
            .codex_config_args_from_remote(None, ReasoningLaunch::Saved, &[], &[])
            .unwrap();
        let joined = args.join(" ");
        assert!(joined.contains(r#"model_catalog_json="/tmp/custom-models.json""#));
        assert!(!provider_dir("zai").unwrap().join("models.json").exists());
    }

    #[test]
    fn small_remote_catalog_is_injected_with_the_default_first() {
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
            &remote,
            &[],
            "glm-5.3-flash",
            None,
            None,
        );
        assert_eq!(catalog["models"][0]["slug"], "glm-5.3-flash");
        assert_eq!(catalog["models"][1]["slug"], "glm-5.3");
        assert_eq!(catalog["models"].as_array().unwrap().len(), 2);
    }
}
