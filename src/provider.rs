//! Custom API provider profiles.
//!
//! A provider profile lets `codex-switch launch` run Codex against a third-party
//! OpenAI-compatible endpoint (OpenRouter, an LLM proxy, …) instead of a ChatGPT
//! OAuth account. Unlike an OAuth profile it carries no `auth.json`; it holds the
//! Codex model-provider definition plus a bearer API key.
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

/// A saved custom-provider profile. `alias` is the codex-switch-facing name and
/// the on-disk directory; it is derived from the path on load, never stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderProfile {
    #[serde(skip)]
    pub alias: String,
    /// The `[model_providers.<id>]` key Codex sees.
    pub provider_id: String,
    /// Human-readable provider name (Codex requires a non-empty value).
    pub name: String,
    /// API base URL, e.g. `https://openrouter.ai/api/v1`.
    pub base_url: String,
    /// Environment variable Codex reads the key from. Derived from the alias and
    /// owned by codex-switch, so it never collides with a provider's own var.
    pub env_key: String,
    /// Default model id (for OpenRouter, the full slug incl. provider prefix).
    pub model: String,
    /// Extra slugs shown in Codex `/model` besides [`model`](Self::model).
    /// Empty means just the default, plus the gateway's full `/models` list
    /// when that list is small enough to inject wholesale.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Catalog metadata fallback: HTTP(S) URL, local JSON path, or `none` to
    /// skip. Empty means env (`CODEX_SWITCH_METADATA_FALLBACK`, then
    /// `CODEX_SWITCH_OPENROUTER_MODELS_URL`) or the public OpenRouter list.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub metadata_fallback: String,
    #[serde(default = "default_wire_api")]
    pub wire_api: String,
    /// Extra `codex -c key=value` overrides applied at launch, stored verbatim as
    /// `"key=value"` strings. Lets a provider carry model-specific Codex settings
    /// (e.g. `web_search=disabled`, `model_reasoning_effort=medium`) so the user
    /// need not retype them. Values pass through untouched; Codex — not
    /// codex-switch — is the source of truth for which keys and values are valid.
    #[serde(default)]
    pub codex_config: Vec<String>,
    /// Bearer API key. Secret: stored `0600`, injected as an env var at launch,
    /// and never printed or placed on the command line.
    pub api_key: String,
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

impl ProviderProfile {
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
        if self.name.trim().is_empty() {
            anyhow::bail!("provider name cannot be empty (Codex requires it)");
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
        if self.model.trim().is_empty() {
            anyhow::bail!("model cannot be empty");
        }
        if let Err(err) = validate_metadata_fallback(&self.metadata_fallback) {
            anyhow::bail!("{err}");
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

    /// A display-safe rendering of the key: never the raw value.
    pub fn redacted_key(&self) -> String {
        redact_key(&self.api_key)
    }

    /// The provider-defining `-c` overrides without writing a catalog. Tests
    /// use this when they want argv shape without filesystem side effects.
    /// Launch uses [`launch_config_args`](Self::launch_config_args).
    #[cfg(test)]
    pub fn codex_config_args(&self) -> Vec<String> {
        self.config_pairs(None)
    }

    /// Launch-time `-c` overrides, including a generated model catalog unless
    /// the provider already has an explicit `model_catalog_json` override.
    ///
    /// The catalog is written under this provider's directory (not
    /// `$CODEX_HOME`) so Codex can resolve the selected model slug. Does not
    /// fetch the gateway; callers that have a `/models` response should use
    /// [`launch_config_args_from_remote`](Self::launch_config_args_from_remote).
    #[cfg(test)]
    pub fn launch_config_args(&self) -> Result<Vec<String>> {
        self.launch_config_args_from_remote(&[], &[])
    }

    /// Like [`launch_config_args`](Self::launch_config_args), filling catalog
    /// rows from a gateway `/models` listing when one is available.
    /// `fallback` is OpenRouter metadata only: it never changes which slugs
    /// appear in `/model`.
    pub(crate) fn launch_config_args_from_remote(
        &self,
        remote: &[RemoteModel],
        fallback: &[RemoteModel],
    ) -> Result<Vec<String>> {
        let catalog_path = if self.has_explicit_model_catalog() {
            None
        } else {
            Some(self.write_model_catalog(remote, fallback)?)
        };
        let catalog_utf8 = catalog_path
            .as_ref()
            .map(|path| {
                path.to_str().map(str::to_string).with_context(|| {
                    format!("model catalog path {} is not valid UTF-8", path.display())
                })
            })
            .transpose()?;
        Ok(self.config_pairs(catalog_utf8.as_deref()))
    }

    fn config_pairs(&self, catalog_path: Option<&str>) -> Vec<String> {
        let id = &self.provider_id;
        let mut pairs = vec![
            format!("model_providers.{id}.name={}", toml_string(&self.name)),
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
            format!("model={}", toml_string(&self.model)),
        ];
        if let Some(path) = catalog_path {
            pairs.push(format!("model_catalog_json={}", toml_string(path)));
        }
        // Provider-saved overrides layer on top, after the model is selected, and
        // pass through verbatim (the user is responsible for their TOML form).
        pairs.extend(self.codex_config.iter().cloned());
        pairs
            .into_iter()
            .flat_map(|kv| ["-c".to_string(), kv])
            .collect()
    }

    pub(crate) fn has_explicit_model_catalog(&self) -> bool {
        override_value(&self.codex_config, "model_catalog_json").is_some()
    }

    fn write_model_catalog(
        &self,
        remote: &[RemoteModel],
        fallback: &[RemoteModel],
    ) -> Result<PathBuf> {
        let dir = provider_dir(&self.alias)?;
        ensure_private_dir(&dir)?;
        let path = dir.join("models.json");
        let json = build_model_catalog(
            &self.saved_model_slugs(),
            remote,
            fallback,
            &self.model,
            override_context_window(&self.codex_config),
            override_value(&self.codex_config, "model_reasoning_effort"),
        );
        let body =
            serde_json::to_vec_pretty(&json).context("serializing provider model catalog")?;
        auth::atomic_write_private(&path, &body)
            .with_context(|| format!("writing provider model catalog {}", path.display()))?;
        Ok(path)
    }

    /// Default [`model`](Self::model) first, then extra [`models`](Self::models),
    /// de-duplicated. Empty strings are skipped.
    fn saved_model_slugs(&self) -> Vec<String> {
        let mut out = Vec::new();
        for slug in
            std::iter::once(self.model.as_str()).chain(self.models.iter().map(String::as_str))
        {
            let slug = slug.trim();
            if slug.is_empty() || out.iter().any(|existing| existing == slug) {
                continue;
            }
            out.push(slug.to_string());
        }
        out
    }

    /// The single environment override that hands Codex the API key under the
    /// profile's `env_key`. Injected into the child process only.
    pub fn launch_env(&self) -> (String, String) {
        (self.env_key.clone(), self.api_key.clone())
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

fn same_http_host(left: &str, right: &str) -> bool {
    match (host_from_url(left), host_from_url(right)) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

fn host_from_url(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let hostport = rest.split(['/', '?', '#']).next()?;
    let host = hostport.rsplit('@').next()?;
    if host.starts_with('[') {
        return None;
    }
    host.split(':').next()
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
        &profile.saved_model_slugs(),
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
    if same_http_host(base_url, fallback_source) {
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
    Ok(profile)
}

/// Persist a provider profile (directory `0700`, file `0600`).
pub fn save(profile: &ProviderProfile) -> Result<()> {
    let dir = provider_dir(&profile.alias)?;
    ensure_private_dir(&dir)?;
    let path = dir.join("provider.toml");
    let toml = toml::to_string_pretty(profile).context("serializing provider profile")?;
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

    fn model_catalog_json(
        model: &str,
        context_window: i64,
        default_reasoning: Option<&str>,
    ) -> serde_json::Value {
        build_model_catalog(
            &[model.to_string()],
            &[],
            &[],
            model,
            Some(context_window),
            default_reasoning,
        )
    }

    fn sample(alias: &str) -> ProviderProfile {
        ProviderProfile {
            alias: alias.to_string(),
            provider_id: sanitize_provider_id(alias),
            name: "OpenRouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            env_key: derive_env_key(alias),
            model: "openai/gpt-5.3-codex".to_string(),
            models: Vec::new(),
            metadata_fallback: String::new(),
            wire_api: default_wire_api(),
            codex_config: Vec::new(),
            api_key: "sk-secret-1234".to_string(),
        }
    }

    struct FallbackEnvGuard {
        previous_meta: Option<String>,
        previous_or: Option<String>,
    }

    impl FallbackEnvGuard {
        fn snapshot() -> Self {
            Self {
                previous_meta: std::env::var("CODEX_SWITCH_METADATA_FALLBACK").ok(),
                previous_or: std::env::var("CODEX_SWITCH_OPENROUTER_MODELS_URL").ok(),
            }
        }

        fn restore_var(name: &str, previous: &Option<String>) {
            unsafe {
                match previous {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }

        fn clear() -> Self {
            let guard = Self::snapshot();
            unsafe {
                std::env::remove_var("CODEX_SWITCH_METADATA_FALLBACK");
                std::env::remove_var("CODEX_SWITCH_OPENROUTER_MODELS_URL");
            }
            guard
        }

        fn set_openrouter_alias(value: &str) -> Self {
            let guard = Self::clear();
            unsafe {
                std::env::set_var("CODEX_SWITCH_OPENROUTER_MODELS_URL", value);
            }
            guard
        }

        fn set_both(meta: &str, openrouter: &str) -> Self {
            let guard = Self::snapshot();
            unsafe {
                std::env::set_var("CODEX_SWITCH_METADATA_FALLBACK", meta);
                std::env::set_var("CODEX_SWITCH_OPENROUTER_MODELS_URL", openrouter);
            }
            guard
        }
    }

    impl Drop for FallbackEnvGuard {
        fn drop(&mut self) {
            Self::restore_var("CODEX_SWITCH_METADATA_FALLBACK", &self.previous_meta);
            Self::restore_var("CODEX_SWITCH_OPENROUTER_MODELS_URL", &self.previous_or);
        }
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
        assert!(no_name.validate().is_err(), "empty name must be rejected");

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
        assert_eq!(loaded.base_url, profile.base_url);
        assert_eq!(loaded.env_key, "CODEX_SWITCH_OPENROUTER_KEY");
        assert_eq!(loaded.api_key, "sk-secret-1234");
        assert_eq!(loaded.wire_api, "responses");

        remove("openrouter").unwrap();
        assert!(!exists("openrouter"));
        assert!(list_providers().unwrap().is_empty());
        assert!(remove("openrouter").is_err(), "removing twice must error");
    }

    #[test]
    fn codex_config_args_define_and_select_the_provider_without_the_key() {
        let p = sample("openrouter");
        let args = p.codex_config_args();
        let joined = args.join(" ");

        // Every override is introduced by its own `-c`.
        assert_eq!(args.iter().filter(|a| a.as_str() == "-c").count(), 6);
        assert!(joined.contains(r#"model_providers.openrouter.name="OpenRouter""#));
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

        // The secret must never travel on the command line.
        assert!(
            !args.iter().any(|a| a.contains("sk-secret-1234")),
            "the API key must never appear in argv"
        );
    }

    #[test]
    fn codex_config_overrides_are_appended_after_the_model_selection() {
        let mut p = sample("openrouter");
        p.codex_config = vec![
            "web_search=disabled".to_string(),
            "model_reasoning_effort=medium".to_string(),
        ];
        let args = p.codex_config_args();

        // Two extra `-c` overrides beyond the six that define/select the provider.
        assert_eq!(args.iter().filter(|a| a.as_str() == "-c").count(), 8);

        let model_pos = args.iter().position(|a| a.starts_with("model=")).unwrap();
        let web_pos = args
            .iter()
            .position(|a| a == "web_search=disabled")
            .unwrap();
        let reasoning_pos = args
            .iter()
            .position(|a| a == "model_reasoning_effort=medium")
            .unwrap();
        assert!(
            model_pos < web_pos && model_pos < reasoning_pos,
            "overrides must layer on top of the model selection"
        );
        // Passed through verbatim, not re-quoted as TOML strings.
        assert!(args.iter().any(|a| a == "web_search=disabled"));
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
        ok.codex_config = vec!["web_search=disabled".to_string()];
        assert!(
            ok.validate().is_ok(),
            "a KEY=VALUE override must be accepted"
        );
    }

    #[test]
    fn codex_config_survives_a_save_load_round_trip() {
        let _home = TestHome::new();
        let mut profile = sample("openrouter");
        profile.codex_config = vec![
            "web_search=disabled".to_string(),
            "model_reasoning_effort=medium".to_string(),
        ];
        save(&profile).unwrap();

        let loaded = load("openrouter").unwrap();
        assert_eq!(
            loaded.codex_config,
            vec![
                "web_search=disabled".to_string(),
                "model_reasoning_effort=medium".to_string(),
            ]
        );
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

    #[test]
    fn generated_catalog_slug_matches_the_selected_model_exactly() {
        let catalog = model_catalog_json("glm-5.3-flash", 1_048_576, None);
        let models = catalog["models"].as_array().expect("models array");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["slug"], "glm-5.3-flash");
        assert_eq!(models[0]["base_instructions"], "");
        assert_eq!(models[0]["context_window"], 1_048_576);
        assert_eq!(models[0]["max_context_window"], 1_048_576);
        assert!(models[0].get("default_reasoning_level").is_none());
    }

    #[test]
    fn generated_catalog_copies_reasoning_effort_when_set() {
        let catalog = model_catalog_json("glm-5.3-flash", 1_048_576, Some("max"));
        assert_eq!(catalog["models"][0]["default_reasoning_level"], "max");
    }

    #[test]
    fn launch_args_write_a_catalog_for_an_unknown_slug() {
        let _home = TestHome::new();
        let mut profile = sample("zai");
        profile.model = "glm-5.3-flash".to_string();
        save(&profile).unwrap();

        let args = profile.launch_config_args().unwrap();
        let joined = args.join(" ");
        assert!(
            joined.contains("model_catalog_json="),
            "launch must point Codex at a catalog, got: {joined}"
        );
        assert!(
            joined.contains(r#"model="glm-5.3-flash""#),
            "the selected slug must be passed through, got: {joined}"
        );
        assert!(
            !args.iter().any(|a| a.contains("sk-secret-1234")),
            "the API key must never appear in argv"
        );

        let catalog_path = super::provider_dir("zai").unwrap().join("models.json");
        let catalog: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&catalog_path).unwrap()).unwrap();
        assert_eq!(catalog["models"][0]["slug"], "glm-5.3-flash");
        assert_eq!(catalog["models"][0]["context_window"], 1_048_576);
        let expected_path = toml_string(catalog_path.to_str().unwrap());
        assert!(
            args.iter()
                .any(|a| a == &format!("model_catalog_json={expected_path}")),
            "the generated catalog path must be on argv, got: {joined}"
        );
    }

    #[test]
    fn launch_args_honour_an_explicit_catalog_and_do_not_write_one() {
        let _home = TestHome::new();
        let mut profile = sample("zai");
        profile.model = "glm-5.3-flash".to_string();
        profile.codex_config = vec![r#"model_catalog_json="/tmp/custom-models.json""#.to_string()];
        save(&profile).unwrap();

        let args = profile.launch_config_args().unwrap();
        let joined = args.join(" ");
        assert!(joined.contains(r#"model_catalog_json="/tmp/custom-models.json""#));
        assert!(
            !joined.contains("providers/zai/models.json"),
            "an explicit catalog must not be replaced, got: {joined}"
        );
        assert!(
            !super::provider_dir("zai")
                .unwrap()
                .join("models.json")
                .exists()
        );
    }

    #[test]
    fn launch_catalog_uses_a_saved_context_window_override() {
        let _home = TestHome::new();
        let mut profile = sample("zai");
        profile.model = "glm-5.3-flash".to_string();
        profile.codex_config = vec!["model_context_window=272000".to_string()];
        save(&profile).unwrap();

        let _args = profile.launch_config_args().unwrap();
        let catalog_path = super::provider_dir("zai").unwrap().join("models.json");
        let catalog: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(catalog_path).unwrap()).unwrap();
        assert_eq!(catalog["models"][0]["context_window"], 272000);
        assert_eq!(catalog["models"][0]["max_context_window"], 272000);
    }

    fn remote(slug: &str, display: &str, context_window: i64, modalities: &[&str]) -> RemoteModel {
        RemoteModel {
            slug: slug.to_string(),
            display_name: Some(display.to_string()),
            description: Some(format!("{display} description")),
            context_window: Some(context_window),
            input_modalities: modalities
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
        }
    }

    #[test]
    fn parse_openai_and_openrouter_models_bodies() {
        let openai = serde_json::json!({
            "object": "list",
            "data": [
                {"id": "glm-5.3-flash", "object": "model"},
                {"id": "  ", "object": "model"},
            ]
        });
        let parsed = parse_gateway_models(&openai);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].slug, "glm-5.3-flash");
        assert_eq!(parsed[0].context_window, None);

        let openrouter = serde_json::json!({
            "data": [{
                "id": "z-ai/glm-5.3-flash",
                "name": "Z.ai: GLM 5.3 Flash",
                "description": "Flash",
                "context_length": 1_310_720,
                "supported_parameters": ["reasoning"],
                "architecture": {"input_modalities": ["text", "image", "video"]},
                "top_provider": {"context_length": 1_310_720}
            }]
        });
        let parsed = parse_gateway_models(&openrouter);
        assert_eq!(parsed[0].slug, "z-ai/glm-5.3-flash");
        assert_eq!(
            parsed[0].display_name.as_deref(),
            Some("Z.ai: GLM 5.3 Flash")
        );
        assert_eq!(parsed[0].context_window, Some(1_310_720));
        assert_eq!(parsed[0].input_modalities, vec!["text", "image"]);
    }

    #[test]
    fn parse_codex_shaped_models_body() {
        let body = serde_json::json!({
            "models": [{
                "slug": "glm-5.3",
                "display_name": "GLM 5.3",
                "context_window": 200000,
                "input_modalities": ["text"]
            }]
        });
        let parsed = parse_gateway_models(&body);
        assert_eq!(parsed[0].slug, "glm-5.3");
        assert_eq!(parsed[0].display_name.as_deref(), Some("GLM 5.3"));
        assert_eq!(parsed[0].context_window, Some(200000));
        assert_eq!(parsed[0].input_modalities, vec!["text"]);
    }

    #[test]
    fn unrecognized_or_error_bodies_yield_no_remote_models() {
        assert!(parse_gateway_models(&serde_json::json!({"code": 1001})).is_empty());
        assert!(parse_gateway_models(&serde_json::json!({"data": "nope"})).is_empty());
    }

    #[test]
    fn small_remote_catalog_is_injected_with_the_default_first() {
        let remote = vec![
            remote("glm-5.3", "GLM 5.3", 200_000, &["text"]),
            remote("glm-5.3-flash", "GLM Flash", 1_048_576, &["text", "image"]),
        ];
        let catalog = build_model_catalog(
            &["glm-5.3-flash".to_string()],
            &remote,
            &[],
            "glm-5.3-flash",
            None,
            Some("max"),
        );
        let slugs: Vec<&str> = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["slug"].as_str().unwrap())
            .collect();
        assert_eq!(slugs, vec!["glm-5.3-flash", "glm-5.3"]);
        assert_eq!(catalog["models"][0]["display_name"], "GLM Flash");
        assert_eq!(catalog["models"][0]["context_window"], 1_048_576);
        assert_eq!(catalog["models"][0]["default_reasoning_level"], "max");
        assert_eq!(catalog["models"][0]["visibility"], "list");
        assert_eq!(catalog["models"][1]["context_window"], 200_000);
        assert!(
            catalog["models"][1]
                .get("default_reasoning_level")
                .is_none()
        );
        assert_eq!(
            catalog["models"][1]["input_modalities"],
            serde_json::json!(["text"])
        );
    }

    #[test]
    fn large_remote_catalog_stays_limited_to_saved_slugs_but_fills_metadata() {
        let remote: Vec<RemoteModel> = (0..=SMALL_REMOTE_CATALOG_LIMIT)
            .map(|i| {
                remote(
                    &format!("m{i}"),
                    &format!("M{i}"),
                    32_000 + i as i64,
                    &["text"],
                )
            })
            .collect();
        assert!(remote.len() > SMALL_REMOTE_CATALOG_LIMIT);

        let catalog = build_model_catalog(
            &["m3".to_string(), "m7".to_string()],
            &remote,
            &[],
            "m3",
            None,
            None,
        );
        let models = catalog["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["slug"], "m3");
        assert_eq!(models[0]["display_name"], "M3");
        assert_eq!(models[0]["context_window"], 32_003);
        assert_eq!(models[1]["slug"], "m7");
        assert_eq!(models[1]["context_window"], 32_007);
    }

    #[test]
    fn user_context_window_override_wins_for_the_default_slug_only() {
        let remote = vec![
            remote("glm-5.3-flash", "Flash", 1_310_720, &["text", "image"]),
            remote("glm-5.3", "GLM", 200_000, &["text"]),
        ];
        let catalog = build_model_catalog(
            &["glm-5.3-flash".to_string(), "glm-5.3".to_string()],
            &remote,
            &[],
            "glm-5.3-flash",
            Some(272_000),
            None,
        );
        assert_eq!(catalog["models"][0]["context_window"], 272_000);
        assert_eq!(catalog["models"][1]["context_window"], 200_000);
    }

    #[test]
    fn launch_args_write_a_multi_model_catalog_from_remote_metadata() {
        let _home = TestHome::new();
        let mut profile = sample("zai");
        profile.model = "glm-5.3-flash".to_string();
        profile.models = vec!["glm-5.3".to_string()];
        save(&profile).unwrap();

        let remote = vec![
            remote("glm-5.3-flash", "GLM Flash", 1_310_720, &["text", "image"]),
            remote("glm-5.3", "GLM 5.3", 200_000, &["text"]),
            remote("glm-4.7", "GLM 4.7", 128_000, &["text"]),
        ];
        let args = profile
            .launch_config_args_from_remote(&remote, &[])
            .unwrap();
        let catalog_path = super::provider_dir("zai").unwrap().join("models.json");
        let catalog: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&catalog_path).unwrap()).unwrap();
        let slugs: Vec<&str> = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["slug"].as_str().unwrap())
            .collect();
        assert_eq!(slugs, vec!["glm-5.3-flash", "glm-5.3", "glm-4.7"]);
        assert_eq!(catalog["models"][0]["context_window"], 1_310_720);
        assert_eq!(catalog["models"][0]["display_name"], "GLM Flash");
        assert!(
            !args.iter().any(|a| a.contains("sk-secret-1234")),
            "the API key must never appear in argv"
        );
    }

    #[test]
    fn openrouter_fallback_matches_vendor_prefixed_id_and_keeps_the_provider_slug() {
        let fallback = vec![
            remote("z-ai/glm-5.3-flash:free", "Free Flash", 32_000, &["text"]),
            remote(
                "z-ai/glm-5.3-flash",
                "Z.ai: GLM 5.3 Flash",
                1_310_720,
                &["text", "image"],
            ),
        ];
        let found = lookup_fallback_model(&fallback, "glm-5.3-flash").unwrap();
        assert_eq!(found.slug, "z-ai/glm-5.3-flash");
        assert_eq!(found.context_window, Some(1_310_720));

        let catalog = build_model_catalog(
            &["glm-5.3-flash".to_string()],
            &[],
            &fallback,
            "glm-5.3-flash",
            None,
            None,
        );
        assert_eq!(catalog["models"].as_array().unwrap().len(), 1);
        assert_eq!(catalog["models"][0]["slug"], "glm-5.3-flash");
        assert_eq!(catalog["models"][0]["display_name"], "Z.ai: GLM 5.3 Flash");
        assert_eq!(catalog["models"][0]["context_window"], 1_310_720);
    }

    #[test]
    fn ambiguous_openrouter_suffix_is_not_guessed() {
        let fallback = vec![
            remote("z-ai/glm-5.3-flash", "Z", 1_310_720, &["text"]),
            remote("other/glm-5.3-flash", "O", 200_000, &["text"]),
        ];
        assert!(lookup_fallback_model(&fallback, "glm-5.3-flash").is_none());
    }

    #[test]
    fn openrouter_fallback_does_not_inject_its_catalog_into_the_picker() {
        let fallback: Vec<RemoteModel> = (0..80)
            .map(|i| remote(&format!("vendor/m{i}"), &format!("M{i}"), 8_000, &["text"]))
            .collect();
        let catalog = build_model_catalog(
            &["glm-5.3-flash".to_string()],
            &[],
            &fallback,
            "glm-5.3-flash",
            None,
            None,
        );
        assert_eq!(catalog["models"].as_array().unwrap().len(), 1);
        assert_eq!(catalog["models"][0]["slug"], "glm-5.3-flash");
        assert_eq!(
            catalog["models"][0]["context_window"],
            DEFAULT_PROVIDER_CONTEXT_WINDOW
        );
    }

    #[test]
    fn primary_context_window_wins_over_openrouter_fallback() {
        let primary = vec![remote("glm-5.3-flash", "Local", 999_999, &["text"])];
        let fallback = vec![remote(
            "z-ai/glm-5.3-flash",
            "Z.ai: GLM 5.3 Flash",
            1_310_720,
            &["text", "image"],
        )];
        let catalog = build_model_catalog(
            &["glm-5.3-flash".to_string()],
            &primary,
            &fallback,
            "glm-5.3-flash",
            None,
            None,
        );
        assert_eq!(catalog["models"][0]["context_window"], 999_999);
        assert_eq!(catalog["models"][0]["display_name"], "Local");
    }

    #[tokio::test]
    async fn metadata_fallback_source_order() {
        let _lock = crate::auth::URL_ENV_LOCK.lock().await;
        let mut profile = sample("zai");
        profile.base_url = "https://api.z.ai/api/v1".to_string();

        {
            let _env = FallbackEnvGuard::clear();
            assert_eq!(metadata_fallback_source(&profile), OPENROUTER_MODELS_URL);
        }

        {
            let _env = FallbackEnvGuard::set_openrouter_alias("https://alias.example/models");
            assert_eq!(
                metadata_fallback_source(&profile),
                "https://alias.example/models"
            );
        }

        {
            let _env = FallbackEnvGuard::set_both(
                "https://meta.example/models",
                "https://alias.example/models",
            );
            assert_eq!(
                metadata_fallback_source(&profile),
                "https://meta.example/models"
            );

            profile.metadata_fallback = "https://example.com/models.json".to_string();
            assert_eq!(
                metadata_fallback_source(&profile),
                "https://example.com/models.json"
            );
        }

        profile.metadata_fallback = "none".to_string();
        assert_eq!(metadata_fallback_source(&profile), "none");
        assert!(!needs_metadata_fallback(
            &["glm-5.3-flash".to_string()],
            &[],
            "https://api.z.ai/api/v1",
            "none"
        ));

        profile.metadata_fallback.clear();
        let _env = FallbackEnvGuard::set_both("none", "https://should-not-use.example/models");
        assert_eq!(metadata_fallback_source(&profile), "none");
    }

    #[test]
    fn metadata_fallback_rejects_non_http_urls() {
        let mut profile = sample("zai");
        profile.base_url = "https://api.z.ai/api/v1".to_string();
        profile.metadata_fallback = "ftp://example.com/models".to_string();
        let err = profile.validate().unwrap_err().to_string();
        assert!(err.contains("metadata fallback"));
        profile.metadata_fallback = "none".to_string();
        profile.validate().unwrap();
        profile.metadata_fallback = "/tmp/models.json".to_string();
        profile.validate().unwrap();
    }

    #[test]
    fn openrouter_base_url_skips_the_public_fallback() {
        assert!(!needs_metadata_fallback(
            &["openai/gpt-5.3-codex".to_string()],
            &[],
            "https://openrouter.ai/api/v1",
            OPENROUTER_MODELS_URL
        ));
        assert!(needs_metadata_fallback(
            &["glm-5.3-flash".to_string()],
            &[],
            "https://api.z.ai/api/v1",
            OPENROUTER_MODELS_URL
        ));
        assert!(!needs_metadata_fallback(
            &["glm-5.3-flash".to_string()],
            &[remote("glm-5.3-flash", "Flash", 1_048_576, &["text"])],
            "https://api.z.ai/api/v1",
            OPENROUTER_MODELS_URL
        ));
    }

    #[tokio::test]
    async fn metadata_fallback_reads_a_local_json_file() {
        let _home = TestHome::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fallback.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "data": [{
                    "id": "z-ai/glm-5.3-flash",
                    "name": "From file",
                    "context_length": 1_310_720
                }]
            })
            .to_string(),
        )
        .unwrap();

        let mut profile = sample("zai");
        profile.model = "glm-5.3-flash".to_string();
        profile.base_url = "http://127.0.0.1:9/v1".to_string();
        profile.metadata_fallback = path.to_str().unwrap().to_string();
        save(&profile).unwrap();

        let fallback = fetch_fallback_models(&profile.metadata_fallback)
            .await
            .unwrap();
        assert_eq!(fallback[0].display_name.as_deref(), Some("From file"));
        let _args = profile
            .launch_config_args_from_remote(&[], &fallback)
            .unwrap();
        let catalog_path = super::provider_dir("zai").unwrap().join("models.json");
        let catalog: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(catalog_path).unwrap()).unwrap();
        assert_eq!(catalog["models"][0]["slug"], "glm-5.3-flash");
        assert_eq!(catalog["models"][0]["display_name"], "From file");
        assert_eq!(catalog["models"][0]["context_window"], 1_310_720);
    }

    #[test]
    fn extra_saved_models_survive_a_save_load_round_trip() {
        let _home = TestHome::new();
        let mut profile = sample("zai");
        profile.model = "glm-5.3-flash".to_string();
        profile.models = vec!["glm-5.3".to_string()];
        profile.metadata_fallback = "/tmp/my-models.json".to_string();
        save(&profile).unwrap();
        let loaded = load("zai").unwrap();
        assert_eq!(loaded.model, "glm-5.3-flash");
        assert_eq!(loaded.models, vec!["glm-5.3".to_string()]);
        assert_eq!(loaded.metadata_fallback, "/tmp/my-models.json");
    }

    #[tokio::test]
    async fn fetch_gateway_models_sends_bearer_and_parses_openrouter_shape() {
        use axum::Json;
        use axum::extract::State;
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::get;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Clone)]
        struct MockState {
            hits: Arc<AtomicUsize>,
        }

        async fn handler(
            State(state): State<MockState>,
            headers: HeaderMap,
        ) -> (StatusCode, Json<serde_json::Value>) {
            state.hits.fetch_add(1, Ordering::SeqCst);
            let auth = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            if auth != "Bearer sk-secret-1234" {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({"error": "bad key"})),
                );
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "data": [
                        {
                            "id": "glm-5.3-flash",
                            "name": "GLM Flash",
                            "context_length": 1_048_576
                        },
                        {
                            "id": "glm-5.3",
                            "name": "GLM 5.3",
                            "context_length": 200000
                        }
                    ]
                })),
            )
        }

        let hits = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route("/v1/models", get(handler))
            .with_state(MockState { hits: hits.clone() });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut profile = sample("zai");
        profile.base_url = format!("http://{addr}/v1");
        profile.model = "glm-5.3-flash".to_string();
        let fetched = fetch_gateway_models(&profile).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(
            fetched.iter().map(|m| m.slug.as_str()).collect::<Vec<_>>(),
            vec!["glm-5.3-flash", "glm-5.3"]
        );
        assert_eq!(fetched[0].context_window, Some(1_048_576));
        server.abort();
    }

    #[tokio::test]
    async fn fetch_gateway_models_errors_on_http_failure_so_launch_can_fall_back() {
        use axum::http::StatusCode;
        use axum::routing::get;

        let app =
            axum::Router::new().route("/v1/models", get(|| async { StatusCode::UNAUTHORIZED }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut profile = sample("zai");
        profile.base_url = format!("http://{addr}/v1");
        let err = fetch_gateway_models(&profile).await.unwrap_err();
        assert!(
            err.to_string().contains("401") || format!("{err:#}").contains("401"),
            "expected HTTP 401 in {err:#}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn load_remote_catalog_fills_from_openrouter_without_forwarding_the_provider_key() {
        use axum::Json;
        use axum::extract::State;
        use axum::http::{HeaderMap, StatusCode};
        use axum::routing::get;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        #[derive(Clone)]
        struct MockState {
            saw_auth_on_openrouter: Arc<AtomicBool>,
        }

        async fn zai_models() -> StatusCode {
            StatusCode::UNAUTHORIZED
        }

        async fn openrouter_models(
            State(state): State<MockState>,
            headers: HeaderMap,
        ) -> (StatusCode, Json<serde_json::Value>) {
            if headers.contains_key(axum::http::header::AUTHORIZATION) {
                state.saw_auth_on_openrouter.store(true, Ordering::SeqCst);
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "data": [{
                        "id": "z-ai/glm-5.3-flash",
                        "name": "Z.ai: GLM 5.3 Flash",
                        "context_length": 1_310_720
                    }]
                })),
            )
        }

        let _home = TestHome::new();
        let _url_lock = crate::auth::URL_ENV_LOCK.lock().await;
        let saw_auth = Arc::new(AtomicBool::new(false));
        let app = axum::Router::new()
            .route("/v1/models", get(zai_models))
            .route("/api/v1/models", get(openrouter_models))
            .with_state(MockState {
                saw_auth_on_openrouter: saw_auth.clone(),
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let _url = FallbackEnvGuard::set_openrouter_alias(&format!("http://{addr}/api/v1/models"));
        let mut profile = sample("zai");
        profile.model = "glm-5.3-flash".to_string();
        profile.base_url = format!("http://{addr}/v1");
        save(&profile).unwrap();

        let (primary, fallback) = load_remote_catalog(&profile).await;
        assert!(primary.is_empty(), "401 gateway must not yield rows");
        assert!(
            !saw_auth.load(Ordering::SeqCst),
            "OpenRouter fallback must not receive the provider key"
        );
        assert_eq!(fallback[0].slug, "z-ai/glm-5.3-flash");

        let _args = profile
            .launch_config_args_from_remote(&primary, &fallback)
            .unwrap();
        let catalog_path = super::provider_dir("zai").unwrap().join("models.json");
        let catalog: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(catalog_path).unwrap()).unwrap();
        assert_eq!(catalog["models"][0]["slug"], "glm-5.3-flash");
        assert_eq!(catalog["models"][0]["context_window"], 1_310_720);
        assert_eq!(catalog["models"][0]["display_name"], "Z.ai: GLM 5.3 Flash");
        server.abort();
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
