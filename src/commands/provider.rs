use std::io::{IsTerminal, Read};

use anyhow::{Context, Result};

use super::render::confirm_default_no;
use crate::cli::ProviderCommand;
use crate::output::{JsonOk, print_json, user_println};
use crate::provider::{self, ProviderProfile};

pub(crate) fn provider_cmd(cmd: ProviderCommand, json: bool) -> Result<()> {
    match cmd {
        ProviderCommand::Add {
            alias,
            base_url,
            model,
            name,
            env_key,
            wire_api,
            reasoning,
            no_web_search,
            set,
            api_key_stdin,
        } => add(
            alias,
            base_url,
            model,
            name,
            env_key,
            wire_api,
            merge_codex_config(reasoning, no_web_search, set),
            api_key_stdin,
            json,
        ),
        ProviderCommand::List => list(json),
        ProviderCommand::Show { alias } => show(&alias, json),
        ProviderCommand::Remove { alias, yes } => remove(&alias, yes, json),
    }
}

#[allow(clippy::too_many_arguments)]
fn add(
    alias: String,
    base_url: String,
    model: String,
    name: Option<String>,
    env_key: Option<String>,
    wire_api: String,
    codex_config: Vec<String>,
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

    let api_key = read_api_key(&alias, api_key_stdin)?;

    let profile = ProviderProfile {
        provider_id: provider::sanitize_provider_id(&alias),
        name: name.unwrap_or_else(|| alias.clone()),
        base_url,
        env_key: env_key.unwrap_or_else(|| provider::derive_env_key(&alias)),
        model,
        wire_api,
        codex_config,
        api_key,
        alias: alias.clone(),
    };
    profile.validate()?;
    provider::save(&profile)?;

    if json {
        print_json(&JsonOk {
            ok: true,
            alias,
            action: "provider-added".into(),
        });
    } else {
        user_println(&format!(
            "Added provider '{}' ({}) -> {}",
            profile.alias, profile.name, profile.base_url
        ));
        user_println(&format!(
            "  key stored; Codex reads it from ${} at launch",
            profile.env_key
        ));
    }
    Ok(())
}

/// Translate the convenience `--reasoning` / `--no-web-search` flags into
/// `codex -c KEY=VALUE` overrides, then append the raw `--set` overrides last so
/// an explicit `--set` wins over a convenience flag for the same key (Codex
/// takes the last `-c` when a key repeats). The convenience flags are just
/// shortcuts; any value is passed through, and `--set` remains the escape hatch
/// for arbitrary keys.
fn merge_codex_config(
    reasoning: Option<String>,
    no_web_search: bool,
    set: Vec<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(effort) = reasoning {
        out.push(format!("model_reasoning_effort={effort}"));
    }
    if no_web_search {
        out.push("web_search=disabled".to_string());
    }
    out.extend(set);
    out
}

/// Read the API key without exposing it on the command line: from stdin in
/// `--api-key-stdin` mode, otherwise from a hidden interactive prompt. Refuses
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
                    "name": p.name,
                    "base_url": p.base_url,
                    "model": p.model,
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
                "{}  {}  {}  [{}]",
                p.alias, p.name, p.model, p.base_url
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
            "name": p.name,
            "base_url": p.base_url,
            "model": p.model,
            "wire_api": p.wire_api,
            "env_key": p.env_key,
            "codex_config": p.codex_config,
            "key": p.redacted_key(),
        }));
        return Ok(());
    }
    user_println(&format!("alias       {}", p.alias));
    user_println(&format!("name        {}", p.name));
    user_println(&format!("provider_id {}", p.provider_id));
    user_println(&format!("base_url    {}", p.base_url));
    user_println(&format!("model       {}", p.model));
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

#[cfg(test)]
mod tests {
    use super::merge_codex_config;

    #[test]
    fn convenience_flags_translate_to_codex_overrides() {
        let out = merge_codex_config(Some("medium".to_string()), true, vec![]);
        assert_eq!(
            out,
            vec![
                "model_reasoning_effort=medium".to_string(),
                "web_search=disabled".to_string(),
            ]
        );
    }

    #[test]
    fn no_flags_yield_no_overrides() {
        assert!(merge_codex_config(None, false, vec![]).is_empty());
    }

    #[test]
    fn explicit_set_is_appended_last_so_it_wins_over_a_convenience_flag() {
        let out = merge_codex_config(
            Some("high".to_string()),
            false,
            vec!["model_reasoning_effort=low".to_string()],
        );
        // Both survive; Codex takes the last `-c` for a repeated key, so the
        // explicit --set (last) wins.
        assert_eq!(
            out,
            vec![
                "model_reasoning_effort=high".to_string(),
                "model_reasoning_effort=low".to_string(),
            ]
        );
    }

    #[test]
    fn an_unknown_reasoning_value_is_passed_through_unchecked() {
        // The effort set is Codex's to define; codex-switch must not reject a
        // value it does not recognize.
        let out = merge_codex_config(Some("ultra".to_string()), false, vec![]);
        assert_eq!(out, vec!["model_reasoning_effort=ultra".to_string()]);
    }
}
