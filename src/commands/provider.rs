use std::io::{IsTerminal, Read};

use anyhow::{Context, Result};

use super::render::confirm_default_no;
use crate::cli::ProviderCommand;
use crate::output::{JsonOk, print_json, user_println};
use crate::provider::{self, ProviderProfile};

pub(crate) async fn provider_cmd(cmd: ProviderCommand, json: bool) -> Result<()> {
    match cmd {
        ProviderCommand::Add {
            alias,
            base_url,
            model: _,
            env_key,
            wire_api,
            reasoning: _,
            no_web_search: _,
            set,
            metadata_fallback,
            fetch_models,
            api_key_stdin,
        } => {
            add(
                alias,
                base_url,
                env_key,
                wire_api,
                set,
                metadata_fallback,
                fetch_models,
                api_key_stdin,
                json,
            )
            .await
        }
        ProviderCommand::List => list(json),
        ProviderCommand::Show { alias } => show(&alias, json),
        ProviderCommand::Rename { old, new } => rename(&old, &new, json),
        ProviderCommand::Remove { alias, yes } => remove(&alias, yes, json),
        ProviderCommand::FetchModels { alias, model } => refresh_models(&alias, model, json).await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn add(
    alias: String,
    base_url: String,
    env_key: Option<String>,
    wire_api: String,
    set: Vec<String>,
    metadata_fallback: Option<String>,
    fetch_models: bool,
    api_key_stdin: bool,
    json: bool,
) -> Result<()> {
    crate::profile::validate_alias(&alias)?;
    if provider::exists(&alias) {
        anyhow::bail!("provider '{alias}' already exists");
    }
    if crate::profile::list_profiles()?.iter().any(|p| p == &alias) {
        anyhow::bail!("'{alias}' already names a ChatGPT profile; choose a different alias");
    }

    let argv: Vec<String> = std::env::args().collect();
    let mut models = provider::models_from_cli_args(&argv)?;
    if models.is_empty() && !fetch_models {
        anyhow::bail!("pass --model ID or --fetch-models");
    }

    let api_key = read_api_key(&alias, api_key_stdin)?;

    let mut fetched_default = None;
    if fetch_models {
        let remote = provider::fetch_gateway_models_at(&base_url, &api_key).await?;
        let default_hint = models.first().map(|model| model.id.clone());
        let (merged, default) =
            provider::apply_fetched_models(&[], default_hint.as_deref(), &remote, &models)?;
        models = merged;
        fetched_default = Some(default);
    }
    if models.is_empty() {
        anyhow::bail!("pass --model ID or --fetch-models");
    }

    let mut profile = ProviderProfile::build(&alias, base_url, models, api_key);
    if let Some(env_key) = env_key {
        profile.env_key = env_key;
    }
    profile.wire_api = wire_api;
    profile.codex_config = set;
    if let Some(default) = fetched_default {
        profile.default_model = default;
    }
    if let Some(fallback) = metadata_fallback {
        profile.metadata_fallback = fallback;
    }
    profile.validate()?;
    provider::save(&profile)?;
    print_added(&profile, json)
}

fn print_added(profile: &ProviderProfile, json: bool) -> Result<()> {
    if json {
        print_json(&JsonOk {
            ok: true,
            alias: profile.alias.clone(),
            action: "provider-added".into(),
        });
    } else {
        user_println(&format!(
            "Added provider '{}' -> {}",
            profile.alias, profile.base_url
        ));
        user_println(&format!(
            "  {} model(s); default {}",
            profile.models.len(),
            profile.default_model
        ));
        user_println(&format!(
            "  key stored; Codex reads it from ${} at launch",
            profile.env_key
        ));
    }
    Ok(())
}

async fn refresh_models(alias: &str, picks: Vec<String>, json: bool) -> Result<()> {
    let mut profile = provider::load(alias)?;
    let picks: Vec<provider::ProviderModel> = picks
        .into_iter()
        .map(provider::ProviderModel::from_id)
        .collect();
    let count = provider::fetch_and_apply_models(&mut profile, &picks).await?;
    profile.validate()?;
    provider::save(&profile)?;
    if json {
        print_json(&serde_json::json!({
            "ok": true,
            "alias": profile.alias,
            "action": "provider-models-fetched",
            "default_model": profile.default_model,
            "models": models_json(&profile),
        }));
    } else {
        user_println(&format!(
            "Updated provider '{}' models from {}/models",
            profile.alias,
            profile.base_url.trim_end_matches('/')
        ));
        user_println(&format!(
            "  {count} model(s); default {}",
            profile.default_model
        ));
    }
    Ok(())
}

/// Read the API key without exposing it on the command line: from stdin in
/// `--api-key-stdin` mode, otherwise from a hidden prompt. Refuses
/// to run non-interactively without `--api-key-stdin` rather than echoing.
fn read_api_key(alias: &str, stdin_mode: bool) -> Result<String> {
    let key = if stdin_mode {
        let mut raw = String::new();
        std::io::stdin()
            .read_to_string(&mut raw)
            .context("reading API key from stdin")?;
        raw.trim().to_string()
    } else if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("API key for '{alias}': "))
            .context("reading API key")?
            .trim()
            .to_string()
    } else {
        anyhow::bail!(
            "no interactive terminal for a hidden prompt; pass the key on stdin with --api-key-stdin"
        );
    };
    if key.is_empty() {
        anyhow::bail!("API key cannot be empty");
    }
    Ok(key)
}

fn models_json(p: &ProviderProfile) -> Vec<serde_json::Value> {
    p.models
        .iter()
        .map(|model| {
            serde_json::json!({
                "id": model.id,
                "reasoning": model.reasoning,
                "no_web_search": model.no_web_search,
                "default": model.id == p.default_model,
            })
        })
        .collect()
}

fn list(json: bool) -> Result<()> {
    let aliases = provider::list_providers()?;
    if json {
        let items: Vec<serde_json::Value> = aliases
            .iter()
            .filter_map(|alias| provider::load(alias).ok())
            .map(|p| {
                serde_json::json!({
                    "alias": p.alias,
                    "provider_id": p.provider_id,
                    "base_url": p.base_url,
                    "default_model": p.default_model,
                    "models": models_json(&p),
                    "wire_api": p.wire_api,
                    "env_key": p.env_key,
                    "codex_config": p.codex_config,
                    "has_key": !p.api_key.is_empty(),
                })
            })
            .collect();
        print_json(&serde_json::json!({ "providers": items }));
        return Ok(());
    }
    if aliases.is_empty() {
        user_println("(no providers)");
        return Ok(());
    }
    for alias in aliases {
        match provider::load(&alias) {
            Ok(p) => user_println(&format!(
                "{}  {}  [{}]",
                p.alias,
                p.models_label(),
                p.base_url
            )),
            Err(e) => user_println(&format!("{alias}  (error: {e})")),
        }
    }
    Ok(())
}

fn show(alias: &str, json: bool) -> Result<()> {
    let p = provider::load(alias)?;
    if json {
        print_json(&serde_json::json!({
            "alias": p.alias,
            "provider_id": p.provider_id,
            "base_url": p.base_url,
            "default_model": p.default_model,
            "models": models_json(&p),
            "wire_api": p.wire_api,
            "env_key": p.env_key,
            "codex_config": p.codex_config,
            "key": p.redacted_key(),
        }));
        return Ok(());
    }
    user_println(&format!("alias       {}", p.alias));
    user_println(&format!("provider_id {}", p.provider_id));
    user_println(&format!("base_url    {}", p.base_url));
    user_println(&format!("default     {}", p.default_model));
    for model in &p.models {
        let mut extras = Vec::new();
        if let Some(effort) = &model.reasoning {
            extras.push(format!("reasoning {effort}"));
        }
        if model.no_web_search {
            extras.push("no-web-search".to_string());
        }
        if model.id == p.default_model {
            extras.push("default".to_string());
        }
        if extras.is_empty() {
            user_println(&format!("model       {}", model.id));
        } else {
            user_println(&format!(
                "model       {}  ({})",
                model.id,
                extras.join(", ")
            ));
        }
    }
    user_println(&format!("wire_api    {}", p.wire_api));
    user_println(&format!("env_key     {}", p.env_key));
    if p.codex_config.is_empty() {
        user_println("codex_config (none)");
    } else {
        for entry in &p.codex_config {
            user_println(&format!("codex_config {entry}"));
        }
    }
    user_println(&format!("key         {}", p.redacted_key()));
    Ok(())
}

fn rename(old: &str, new: &str, json: bool) -> Result<()> {
    provider::rename(old, new)?;
    if json {
        print_json(&JsonOk {
            ok: true,
            alias: new.to_string(),
            action: "provider-renamed".into(),
        });
    } else {
        user_println(&format!("Renamed provider: {old} -> {new}"));
    }
    Ok(())
}

fn remove(alias: &str, yes: bool, json: bool) -> Result<()> {
    if !provider::exists(alias) {
        anyhow::bail!("provider '{alias}' not found");
    }
    if !yes {
        if json || !std::io::stdin().is_terminal() {
            anyhow::bail!("confirmation required; rerun with --yes to remove provider '{alias}'");
        }
        if !confirm_default_no(&format!("Remove provider '{alias}'? [y/N] ")) {
            user_println("Removal cancelled.");
            return Ok(());
        }
    }
    provider::remove(alias)?;
    if json {
        print_json(&JsonOk {
            ok: true,
            alias: alias.to_string(),
            action: "provider-removed".into(),
        });
    } else {
        user_println(&format!("Removed provider '{alias}'"));
    }
    Ok(())
}
