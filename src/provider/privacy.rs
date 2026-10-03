//! Display redaction and child-only transport for saved literal HTTP headers.

use super::*;

fn sensitive_name(name: &str) -> bool {
    let name = name.trim_matches(['\'', '"']).to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "env_key" | "env_key_instructions" | "env_http_headers" | "bearer_token_env_var"
    ) {
        return false;
    }
    matches!(
        name.as_str(),
        "http_headers" | "query_params" | "env" | "auth" | "credentials"
    ) || [
        "token",
        "secret",
        "password",
        "api_key",
        "apikey",
        "authorization",
        "cookie",
    ]
    .iter()
    .any(|part| name.contains(part))
}

fn sensitive_value(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => table
            .iter()
            .any(|(key, value)| sensitive_name(key) || sensitive_value(value)),
        toml::Value::Array(values) => values.iter().any(sensitive_value),
        toml::Value::String(value) => redact_url(value) != *value,
        _ => false,
    }
}

pub(crate) fn redact_url(value: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(value) else {
        return value.to_string();
    };
    if !matches!(url.scheme(), "http" | "https") {
        return value.to_string();
    }
    let mut changed = false;
    if !url.username().is_empty() || url.password().is_some() {
        let _ = url.set_username("REDACTED");
        let _ = url.set_password(None);
        changed = true;
    }
    if url.query().is_some() {
        url.set_query(Some("REDACTED"));
        changed = true;
    }
    if url.fragment().is_some() {
        url.set_fragment(Some("REDACTED"));
        changed = true;
    }
    if changed {
        url.to_string()
    } else {
        value.to_string()
    }
}

pub(crate) fn redacted_overrides(entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| {
            let Some((key, raw)) = entry.split_once('=') else {
                return "[REDACTED invalid override]".into();
            };
            if key.split('.').any(|part| part == "env_http_headers")
                || matches!(
                    key.rsplit('.').next(),
                    Some("env_key" | "bearer_token_env_var")
                )
            {
                return entry.clone();
            }
            if key.split('.').any(sensitive_name)
                || sensitive_value(&provider_override_value(raw.trim()))
            {
                format!("{key}=\"[REDACTED]\"")
            } else {
                entry.clone()
            }
        })
        .collect()
}

impl ProviderProfile {
    /// Do not mutate the saved profile or the launcher's environment. Resolve
    /// static/env header precedence before replacing literals with references.
    pub(crate) fn with_private_headers(&self) -> Result<(Self, Vec<(String, String)>)> {
        let mut runtime = self.clone();
        let active_prefix = format!("model_providers.{}", self.provider_id);
        let static_prefix = format!("{active_prefix}.http_headers");
        let has_static_headers = self
            .codex_config
            .iter()
            .filter_map(|entry| entry.split_once('='))
            .any(|(key, _)| {
                key.trim() == static_prefix || key.trim().starts_with(&format!("{static_prefix}."))
            });
        let mut groups: BTreeMap<String, toml::map::Map<String, toml::Value>> = BTreeMap::new();
        let mut rest = Vec::new();
        for entry in &self.codex_config {
            let Some((key, raw)) = entry.split_once('=') else {
                rest.push(entry.clone());
                continue;
            };
            let parts: Vec<_> = key.trim().split('.').collect();
            let header = parts.iter().position(|part| {
                matches!(
                    part.trim_matches(['\'', '"']),
                    "http_headers" | "env_http_headers"
                )
            });
            if let Some(index) = header {
                if parts[index + 1..]
                    .iter()
                    .any(|part| part.contains(['\'', '"']))
                {
                    anyhow::bail!(
                        "quoted header leaf keys require a http_headers or env_http_headers inline table"
                    );
                }
                let prefix = parts[..index].join(".");
                if prefix != active_prefix || !has_static_headers {
                    if parts[index].trim_matches(['\'', '"']) == "http_headers" {
                        anyhow::bail!(
                            "literal HTTP headers must use unquoted dotted keys for the active provider; use env_http_headers for MCP or other providers"
                        );
                    }
                    rest.push(entry.clone());
                    continue;
                }
                let group = groups.entry(prefix).or_default();
                insert_toml_override(
                    group,
                    &parts[index..].join("."),
                    provider_override_value(raw.trim()),
                )?;
            } else {
                // Whole parent tables cannot be remapped without changing -c
                // precedence. Refuse secret-bearing forms rather than leak them.
                fn contains_headers(value: &toml::Value) -> bool {
                    value.as_table().is_some_and(|table| {
                        table.contains_key("http_headers") || table.values().any(contains_headers)
                    })
                }
                if contains_headers(&provider_override_value(raw.trim())) {
                    anyhow::bail!(
                        "nested literal HTTP headers require dotted http_headers overrides or env_http_headers"
                    );
                }
                rest.push(entry.clone());
            }
        }
        let mut env = vec![self.launch_env()];
        let namespace = new_identity_id().replace('-', "").to_ascii_uppercase();
        for (prefix, group) in groups {
            let static_headers = string_table(group.get("http_headers"), "http_headers")?;
            let env_headers = string_table(group.get("env_http_headers"), "env_http_headers")?;
            let mut headers = BTreeMap::new();
            for (name, value) in static_headers {
                if let (Ok(name), Ok(_)) = (
                    reqwest::header::HeaderName::try_from(name.as_str()),
                    reqwest::header::HeaderValue::try_from(value.as_str()),
                ) {
                    headers.insert(name.as_str().to_string(), value);
                }
            }
            for (name, variable) in env_headers {
                let value = if variable == self.env_key {
                    Some(self.api_key.clone())
                } else {
                    std::env::var(&variable).ok()
                };
                if let Some(value) = value.filter(|value| !value.trim().is_empty())
                    && let (Ok(name), Ok(_)) = (
                        reqwest::header::HeaderName::try_from(name.as_str()),
                        reqwest::header::HeaderValue::try_from(value.as_str()),
                    )
                {
                    headers.insert(name.as_str().to_string(), value);
                }
            }
            let mut references = toml::map::Map::new();
            let mut empty_headers = toml::map::Map::new();
            for (name, value) in headers {
                // Codex ignores empty env header values. Keep intentional
                // empty/whitespace literals, which contain no credential.
                if value.trim().is_empty() {
                    empty_headers.insert(name, toml::Value::String(value));
                    continue;
                }
                let variable = format!("CODEX_SWITCH_HEADER_{namespace}_{}", env.len());
                references.insert(name, toml::Value::String(variable.clone()));
                env.push((variable, value));
            }
            rest.push(format!(
                "{prefix}.http_headers={}",
                toml::Value::Table(empty_headers)
            ));
            rest.push(format!(
                "{prefix}.env_http_headers={}",
                toml::Value::Table(references)
            ));
        }
        runtime.codex_config = rest;
        Ok((runtime, env))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_headers_keep_env_precedence_and_missing_env_fallback() {
        let mut profile = ProviderProfile::build(
            "test",
            "https://example.invalid/v1",
            vec![ProviderModel::from_id("model")],
            "primary-key",
        );
        profile.codex_config = vec![
            r#"model_providers.test.http_headers={X-Token="static-secret",X-Fallback="fallback-secret",X-Blank=" "}"#.into(),
            format!("model_providers.test.env_http_headers.X-Token={}", toml_string(&profile.env_key)),
            format!("model_providers.test.env_http_headers.X-Fallback={}", toml_string(&format!("CS_MISSING_{}", new_identity_id()))),
        ];
        let original = profile.codex_config.clone();
        let (runtime, env) = profile.with_private_headers().unwrap();
        assert_eq!(profile.codex_config, original);
        assert!(!runtime.codex_config.join(" ").contains("secret"));
        assert!(runtime.codex_config.iter().any(|entry| {
            entry.starts_with("model_providers.test.http_headers=") && entry.contains("x-blank")
        }));
        assert!(env.iter().any(
            |(name, value)| name.starts_with("CODEX_SWITCH_HEADER_") && value == "primary-key"
        ));
        assert!(env.iter().any(|(_, value)| value == "fallback-secret"));
        assert!(!env.iter().any(|(_, value)| value == "static-secret"));
    }

    #[test]
    fn redaction_handles_quoted_keys_nested_tables_and_url_credentials() {
        for entry in [
            r#"model_providers.test.http_headers."custom"="opaque""#,
            r#"mcp_servers.test={env={KEY="opaque"}}"#,
            r#"model_providers.test.base_url="https://user:opaque@host/v1?token=opaque""#,
        ] {
            assert!(!redacted_overrides(&[entry.into()])[0].contains("opaque"));
        }
        let reference =
            r#"model_providers.test.env_http_headers.X-Auth-Token="MY_SECRET_ENV""#.to_string();
        assert_eq!(
            redacted_overrides(std::slice::from_ref(&reference)),
            [reference]
        );
    }

    #[test]
    fn env_only_headers_keep_inherited_settings_and_other_services_use_references() {
        let mut profile = ProviderProfile::build(
            "test",
            "https://example.invalid/v1",
            vec![ProviderModel::from_id("model")],
            "key",
        );
        profile.codex_config = vec![
            "mcp_servers.demo.env_http_headers.X-Token=DEMO_TOKEN".into(),
            "model_providers.test.env_http_headers.X-Token=TOKEN".into(),
        ];
        let (runtime, env) = profile.with_private_headers().unwrap();
        assert_eq!(runtime.codex_config, profile.codex_config);
        assert_eq!(env, [profile.launch_env()]);
        profile
            .codex_config
            .push("mcp_servers.demo.http_headers.X-Token=secret".into());
        assert!(profile.with_private_headers().is_err());
    }
}
