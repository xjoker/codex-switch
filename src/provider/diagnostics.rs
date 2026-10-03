//! Offline evidence about catalogs and saved agent model choices, not a
//! certification of gateway or tool execution support.

use std::io::Read;

use serde_json::{Value, json};

use super::privacy::redact_url;
use super::*;

fn issue(issues: &mut Vec<Value>, code: &str, error: bool, message: impl Into<String>) {
    issues.push(json!({"code": code, "severity": if error {"error"} else {"warning"}, "message": message.into()}));
}

fn read_catalog(path: &Path) -> Result<Value> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take((MAX_GATEWAY_MODELS_BODY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_GATEWAY_MODELS_BODY_BYTES {
        anyhow::bail!("catalog is too large");
    }
    Ok(serde_json::from_slice(&bytes)?)
}

/// Returns only selected capability fields; never echo instructions or arbitrary
/// upstream extension data into diagnostics.
fn capabilities(model: &Value) -> Value {
    let mut result = serde_json::Map::new();
    for field in [
        "multi_agent_version",
        "use_responses_lite",
        "apply_patch_tool_type",
        "input_modalities",
        "supported_reasoning_levels",
        "context_window",
    ] {
        result.insert(
            field.into(),
            model.get(field).cloned().unwrap_or(Value::Null),
        );
    }
    Value::Object(result)
}

pub(crate) fn catalog_report(profile: &ProviderProfile) -> Value {
    let mut issues = Vec::new();
    let explicit =
        override_value(&profile.codex_config, "model_catalog_json").map(provider_override_value);
    let path = if let Some(explicit) = &explicit {
        explicit.as_str().map(PathBuf::from)
    } else {
        provider_dir(&profile.alias)
            .ok()
            .map(|dir| dir.join("models.json"))
    };
    let mut source = if explicit.is_some() {
        "explicit"
    } else {
        "generated"
    }
    .to_string();
    let mut catalog = path.as_deref().and_then(|path| read_catalog(path).ok());
    let age_seconds = path
        .as_deref()
        .and_then(|path| std::fs::metadata(path).ok())
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.elapsed().ok())
        .map(|age| age.as_secs());
    if catalog
        .as_ref()
        .is_some_and(|catalog| catalog.get("models").and_then(Value::as_array).is_none())
    {
        catalog = None;
    }
    if explicit.is_some() && catalog.is_none() {
        issue(
            &mut issues,
            "catalog_unreadable",
            true,
            "Explicit model catalog is unreadable or invalid; inspect model_catalog_json locally.",
        );
    } else if explicit.is_none() {
        let matches = catalog
            .as_ref()
            .and_then(|catalog| catalog["models"].as_array())
            .is_some_and(|entries| {
                let actual: HashSet<_> = entries
                    .iter()
                    .filter_map(|entry| entry["slug"].as_str())
                    .collect();
                let expected: HashSet<_> = profile
                    .models
                    .iter()
                    .map(|model| model.id.as_str())
                    .collect();
                actual == expected
            });
        if !matches {
            if path.as_ref().is_some_and(|path| path.exists()) {
                issue(
                    &mut issues,
                    "catalog_replaced_at_launch",
                    false,
                    "Saved catalog is invalid or does not match saved models; launch will generate fallback metadata. Fetch models to restore gateway metadata.",
                );
            }
            catalog = Some(build_model_catalog(
                &profile.saved_model_slugs(&profile.default_model),
                &profile.models,
                &[],
                &[],
                &profile.default_model,
                override_context_window(&profile.codex_config),
                profile
                    .resolve_model(None)
                    .ok()
                    .and_then(|model| model.reasoning.as_deref()),
            ));
        } else {
            source = "legacy_unknown".into();
        }
    }
    let catalog = catalog.unwrap_or_else(|| json!({"models": []}));
    let provenance = &catalog["_codex_switch"]["sources"];
    let models: Vec<_> = catalog["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let id = model["slug"].as_str()?;
            let model_source = if source == "explicit" {
                "explicit"
            } else {
                provenance
                    .get(id)
                    .and_then(Value::as_str)
                    .unwrap_or(&source)
            };
            Some(json!({"id": id, "source": model_source, "capabilities": capabilities(model)}))
        })
        .collect();
    if source != "explicit" {
        let sources: HashSet<_> = models
            .iter()
            .filter_map(|model| model["source"].as_str())
            .collect();
        if sources.len() == 1 {
            source = sources.into_iter().next().unwrap().into();
        } else if sources.len() > 1 {
            source = "mixed".into();
        }
    }
    if models.iter().any(|model| {
        matches!(
            model["source"].as_str(),
            Some("generated" | "generic_gateway" | "generic_fallback" | "legacy_unknown")
        )
    }) {
        issue(
            &mut issues,
            "metadata_not_native",
            false,
            "Some models use generic or untracked metadata; fetch native gateway metadata before assuming V2, patch, image or Lite support.",
        );
    }
    json!({"source": source, "file_age_seconds": age_seconds, "saved_at": catalog["_codex_switch"]["saved_at"], "models": models, "issues": issues})
}

pub(crate) fn diagnose(profile: &ProviderProfile, requested_children: &[String]) -> Value {
    let catalog = catalog_report(profile);
    let mut issues = catalog["issues"].as_array().cloned().unwrap_or_default();
    let models = catalog["models"].as_array().cloned().unwrap_or_default();
    if !models
        .iter()
        .any(|model| model["id"] == profile.default_model)
    {
        issue(
            &mut issues,
            "default_model_missing",
            true,
            "The provider default model is absent from the launch catalog.",
        );
    }
    let effective_base_url = match resolve_provider_connection(profile) {
        Ok(connection) => Some(redact_url(&connection.base_url)),
        Err(_) => {
            issue(
                &mut issues,
                "connection_config_invalid",
                true,
                "Connection settings cannot be resolved. Check wire_api, URLs, env_key, headers and conflicting authentication locally.",
            );
            None
        }
    };
    if profile.with_private_headers().is_err() {
        issue(
            &mut issues,
            "header_config_invalid",
            true,
            "Header overrides cannot be transported safely; use dotted http_headers or env_http_headers settings.",
        );
    }
    let codex_home = auth::user_codex_home().ok();
    let config_path = codex_home.as_ref().map(|home| home.join("config.toml"));
    let mut config = match config_path
        .as_ref()
        .map(|path| load_toml_if_present(path))
        .transpose()
    {
        Ok(value) => value
            .flatten()
            .and_then(|value| value.as_table().cloned())
            .unwrap_or_default(),
        Err(_) => {
            issue(
                &mut issues,
                "codex_config_unreadable",
                true,
                "CODEX_HOME/config.toml could not be read or parsed; its contents have been omitted.",
            );
            toml::map::Map::new()
        }
    };
    let mut saved_overrides = toml::map::Map::new();
    for entry in &profile.codex_config {
        if let Some((key, raw)) = entry.split_once('=') {
            let _ = insert_toml_override(
                &mut saved_overrides,
                key.trim(),
                provider_override_value(raw.trim()),
            );
        }
        if let Some((key, raw)) = entry.split_once('=')
            && insert_toml_override(&mut config, key.trim(), provider_override_value(raw.trim()))
                .is_err()
        {
            issue(
                &mut issues,
                "override_conflict",
                true,
                "Saved overrides contain a conflicting key path; inspect Extra -c locally.",
            );
        }
    }
    let mut children: Vec<(String, String, Option<String>)> = requested_children
        .iter()
        .map(|id| ("requested".into(), id.clone(), None))
        .collect();
    if let Some(agents) = config.get("agents").and_then(toml::Value::as_table) {
        let default_model = agents
            .get("default_subagent_model")
            .and_then(toml::Value::as_str)
            .unwrap_or(&profile.default_model);
        let default_effort = agents
            .get("default_subagent_reasoning_effort")
            .and_then(toml::Value::as_str);
        if agents.contains_key("default_subagent_model") || default_effort.is_some() {
            children.push((
                "agents.default_subagent_model".into(),
                default_model.into(),
                default_effort.map(str::to_string),
            ));
        }
        for (role, settings) in agents {
            if let Some(file) = settings.get("config_file").and_then(toml::Value::as_str) {
                let path = PathBuf::from(file);
                let path = if path.is_absolute() {
                    path
                } else if saved_overrides
                    .get("agents")
                    .and_then(|agents| agents.get(role))
                    .and_then(|settings| settings.get("config_file"))
                    .is_some()
                {
                    std::env::current_dir().unwrap_or_default().join(path)
                } else {
                    codex_home.clone().unwrap_or_default().join(path)
                };
                match load_toml_if_present(&path) {
                    Ok(Some(role_config)) => {
                        if role_config.get("model_provider").is_some() {
                            issue(
                                &mut issues,
                                "role_provider_override",
                                true,
                                format!(
                                    "Agent role {role} sets model_provider; provider routing through custom roles is not supported by the aligned Codex version."
                                ),
                            );
                        }
                        let model = role_config
                            .get("model")
                            .and_then(toml::Value::as_str)
                            .unwrap_or(default_model);
                        children.push((
                            format!("agents.{role}"),
                            model.into(),
                            role_config
                                .get("model_reasoning_effort")
                                .and_then(toml::Value::as_str)
                                .or(default_effort)
                                .map(str::to_string),
                        ));
                    }
                    _ => issue(
                        &mut issues,
                        "agent_config_unreadable",
                        true,
                        format!(
                            "Agent role {role} config_file could not be read or parsed; contents omitted."
                        ),
                    ),
                }
            }
        }
    }
    if children.is_empty() {
        children.push((
            "inherited_default".into(),
            profile.default_model.clone(),
            None,
        ));
    }
    let child_models: Vec<_> = children.into_iter().map(|(origin, id, effort)| {
        let model = models.iter().find(|model| model["id"] == id);
        if model.is_none() {
            issue(&mut issues, "child_model_missing", true, format!("Child model {id} ({origin}) is absent from the launch catalog; save/fetch it or change the agent model."));
        }
        if let (Some(model), Some(effort)) = (model, effort.as_deref()) {
            let supported = model["capabilities"]["supported_reasoning_levels"].as_array().is_some_and(|levels| levels.iter().any(|level| level["effort"].as_str() == Some(effort)));
            if !supported { issue(&mut issues, "child_reasoning_unadvertised", false, format!("Child model {id} does not advertise reasoning effort {effort}; Codex may reject the override.")); }
        }
        json!({"origin": origin, "model": id, "available": model.is_some(), "reasoning": effort})
    }).collect();
    let ok = !issues.iter().any(|issue| issue["severity"] == "error");
    json!({
        "ok": ok, "alias": profile.alias, "network_checked": false,
        "scope": "Saved provider and CODEX_HOME/config.toml plus referenced roles; project/system policy, launch CLI and resumed profiles are not evaluated.",
        "capability_evidence": "Catalog metadata only; tools, SSE, gateway translation and model reachability are not tested.",
        "effective_base_url": effective_base_url,
        "catalog": {"source": catalog["source"], "file_age_seconds": catalog["file_age_seconds"], "saved_at": catalog["saved_at"]},
        "models": models, "child_models": child_models, "issues": issues
    })
}
