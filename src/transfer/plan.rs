//! Export collection and import classification. Classification is pure with
//! respect to disk writes: it reads the local store and returns a plan that
//! `--dry-run` prints and `apply` executes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use super::{Entry, EntryKind, LogicalKey, Payload, parse_key};
use crate::error::CsError;
use crate::profile;

// ── export ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ExportOptions {
    pub no_providers: bool,
    pub no_config: bool,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct ExportSummary {
    pub profiles: Vec<String>,
    pub providers: Vec<String>,
    pub config: bool,
}

fn read_utf8(path: &Path) -> Result<String> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    String::from_utf8(raw).map_err(|_| {
        anyhow::anyhow!(
            "{} is not valid UTF-8; fix or remove it before exporting",
            path.display()
        )
    })
}

pub(crate) fn collect_export(opts: ExportOptions) -> Result<(Payload, ExportSummary)> {
    let app = crate::auth::app_home()?;
    let mut entries = Vec::new();
    let mut summary = ExportSummary::default();

    for alias in profile::list_profiles()? {
        if profile::validate_alias(&alias).is_err() {
            continue;
        }
        let key = LogicalKey::ProfileAuth(alias.clone());
        let path = key.path_in(&app);
        if !path.is_file() {
            continue;
        }
        let text = read_utf8(&path)?;
        serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}'))
            .with_context(|| format!("parsing {}", path.display()))?;
        entries.push(Entry {
            key: key.render(),
            kind: EntryKind::ProfileAuth,
            text,
        });
        summary.profiles.push(alias);
    }

    if !opts.no_providers {
        for alias in crate::provider::list_providers()? {
            if profile::validate_alias(&alias).is_err() {
                continue;
            }
            let key = LogicalKey::Provider(alias.clone());
            let text = read_utf8(&key.path_in(&app))?;
            entries.push(Entry {
                key: key.render(),
                kind: EntryKind::Provider,
                text,
            });
            let models = LogicalKey::ProviderModels(alias.clone());
            let models_path = models.path_in(&app);
            if models_path.is_file() {
                entries.push(Entry {
                    key: models.render(),
                    kind: EntryKind::ProviderModels,
                    text: read_utf8(&models_path)?,
                });
            }
            summary.providers.push(alias);
        }
    }

    if !opts.no_config {
        let path = LogicalKey::AppConfig.path_in(&app);
        if path.is_file() {
            entries.push(Entry {
                key: LogicalKey::AppConfig.render(),
                kind: EntryKind::AppConfig,
                text: read_utf8(&path)?,
            });
            summary.config = true;
        }
    }

    if entries.is_empty() {
        anyhow::bail!("nothing to export: no saved profiles, providers or config.toml");
    }
    let current = profile::read_current();
    let source_current =
        (!current.is_empty() && summary.profiles.contains(&current)).then_some(current);
    Ok((
        Payload {
            source_current,
            entries,
        },
        summary,
    ))
}

// ── import classification ─────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// Keep the local copy on conflict (default)
    #[default]
    Skip,
    /// Replace the local copy with the imported one (a backup is kept)
    Overwrite,
    /// Profiles: keep the newer credential; config.toml: add missing keys only
    Merge,
}

impl Strategy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Strategy::Skip => "skip",
            Strategy::Overwrite => "overwrite",
            Strategy::Merge => "merge",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    /// New item, written under its own alias.
    Create,
    /// New item whose alias is taken locally; written under `target`.
    Rename,
    /// Local content is already identical.
    Unchanged,
    /// Conflict; local kept.
    Skip,
    /// Conflict; imported content replaces local.
    Overwrite,
    /// Conflict; merged content replaces local.
    Merge,
    /// Identity matches several local profiles or only by email; never touched.
    Ambiguous,
}

impl Action {
    pub(crate) fn writes(self) -> bool {
        matches!(
            self,
            Action::Create | Action::Rename | Action::Overwrite | Action::Merge
        )
    }
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Action::Create => "create",
            Action::Rename => "rename",
            Action::Unchanged => "unchanged",
            Action::Skip => "skip",
            Action::Overwrite => "overwrite",
            Action::Merge => "merge",
            Action::Ambiguous => "ambiguous",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PlanItem {
    pub key: String,
    pub kind: &'static str,
    pub action: Action,
    /// Local alias that will receive the item (profiles/providers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub reason: String,
    /// Content to write when `action.writes()`. Never serialised.
    #[serde(skip)]
    pub write: Option<(LogicalKey, String)>,
    /// Whether this write replaces the live Codex auth.json too.
    #[serde(skip)]
    pub updates_live: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ImportPlan {
    pub items: Vec<PlanItem>,
    pub source_current: Option<String>,
    /// `source_current` after any rename on this machine.
    pub source_current_target: Option<String>,
    pub notices: Vec<String>,
}

impl ImportPlan {
    pub(crate) fn needs_confirmation(&self) -> bool {
        self.items
            .iter()
            .any(|i| matches!(i.action, Action::Overwrite | Action::Merge))
    }
    pub(crate) fn has_writes(&self) -> bool {
        self.items.iter().any(|i| i.action.writes())
    }
    /// A comparable fingerprint of the decisions, to detect that the store
    /// changed between the confirmation and taking the lock.
    pub(crate) fn decisions(&self) -> Vec<(String, Action, Option<String>)> {
        self.items
            .iter()
            .map(|i| (i.key.clone(), i.action, i.target.clone()))
            .collect()
    }
}

/// Lowercased aliases already used locally by profiles and providers. macOS and
/// Windows file systems are case-insensitive, so `Work` and `work` collide.
fn local_taken() -> Result<BTreeSet<String>> {
    let mut taken = BTreeSet::new();
    for alias in profile::list_profiles()? {
        taken.insert(alias.to_ascii_lowercase());
    }
    for alias in crate::provider::list_providers()? {
        taken.insert(alias.to_ascii_lowercase());
    }
    Ok(taken)
}

const MAX_ALIAS_LEN: usize = 64;

fn unique_alias(base: &str, taken: &BTreeSet<String>) -> Result<String> {
    for n in 2..1000u32 {
        let suffix = format!("_{n}");
        let prefix: String = base
            .chars()
            .take(MAX_ALIAS_LEN.saturating_sub(suffix.len()))
            .collect();
        let candidate = format!("{prefix}{suffix}");
        if !taken.contains(&candidate.to_ascii_lowercase()) {
            return Ok(candidate);
        }
    }
    anyhow::bail!("could not generate a unique alias for '{base}'")
}

#[cfg(windows)]
fn windows_reserved(alias: &str) -> bool {
    let stem = alias
        .split('.')
        .next()
        .unwrap_or(alias)
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit())
}
#[cfg(not(windows))]
fn windows_reserved(_alias: &str) -> bool {
    false
}

struct LocalProfile {
    alias: String,
    value: serde_json::Value,
    identity: profile::AccountIdentity,
}

fn local_profiles() -> Result<Vec<LocalProfile>> {
    let mut out = Vec::new();
    for alias in profile::list_profiles()? {
        let Ok(path) = profile::profile_auth_path(&alias) else {
            continue;
        };
        let Ok(value) = crate::auth::read_auth(&path) else {
            continue;
        };
        let identity = profile::extract_identity(&value);
        out.push(LocalProfile {
            alias,
            value,
            identity,
        });
    }
    Ok(out)
}

fn parse_auth_text(key: &str, text: &str) -> Result<serde_json::Value> {
    serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| CsError::InvalidBundle(format!("{key}: invalid auth JSON: {e}")).into())
}

fn item(
    key: &LogicalKey,
    action: Action,
    target: Option<String>,
    reason: impl Into<String>,
) -> PlanItem {
    PlanItem {
        key: key.render(),
        kind: key.kind().as_str(),
        action,
        target,
        reason: reason.into(),
        write: None,
        updates_live: false,
    }
}

/// Validate the whole payload and decide what happens to each entry. Any
/// validation failure aborts the import before anything is written.
pub(crate) fn classify(payload: &Payload, strategy: Strategy) -> Result<ImportPlan> {
    let app = crate::auth::app_home()?;
    let mut taken = local_taken()?;
    let locals = local_profiles()?;
    let local_current = profile::read_current();
    let mut notices = Vec::new();

    // Parse and validate every key first, and reject duplicates.
    let mut seen = BTreeSet::new();
    let mut keyed: Vec<(LogicalKey, &Entry)> = Vec::new();
    for entry in &payload.entries {
        let key = parse_key(&entry.key)?;
        if key.kind() != entry.kind {
            return Err(CsError::InvalidBundle(format!(
                "entry '{}' has kind '{}' but its key is a {}",
                entry.key,
                entry.kind.as_str(),
                key.kind().as_str()
            ))
            .into());
        }
        if !seen.insert(entry.key.to_ascii_lowercase()) {
            return Err(CsError::InvalidBundle(format!("duplicate entry '{}'", entry.key)).into());
        }
        keyed.push((key, entry));
    }
    let bundle_providers: BTreeSet<String> = keyed
        .iter()
        .filter_map(|(k, _)| match k {
            LogicalKey::Provider(a) => Some(a.clone()),
            _ => None,
        })
        .collect();
    for (key, _) in &keyed {
        if let LogicalKey::ProviderModels(a) = key
            && !bundle_providers.contains(a)
        {
            return Err(CsError::InvalidBundle(format!(
                "{} has no matching provider.toml",
                key.render()
            ))
            .into());
        }
    }

    let mut items = Vec::new();
    let mut renames: BTreeMap<String, String> = BTreeMap::new();
    let mut provider_targets: BTreeMap<String, (Action, String)> = BTreeMap::new();

    // Profiles.
    for (key, entry) in &keyed {
        let LogicalKey::ProfileAuth(alias) = key else {
            continue;
        };
        let value = parse_auth_text(&entry.key, &entry.text)?;
        let identity = profile::extract_identity(&value);
        if identity.account_id.as_deref().is_none_or(str::is_empty) {
            return Err(CsError::InvalidBundle(format!(
                "{}: imported auth must contain a non-empty account_id",
                entry.key
            ))
            .into());
        }
        crate::auth::validate_managed_auth_value(&value)
            .with_context(|| format!("{} is not allowed by the managed policy", entry.key))?;

        let exact = locals.iter().find(|l| {
            l.identity.account_id == identity.account_id
                && l.identity.email.is_some()
                && l.identity.email == identity.email
        });
        let email_only: Vec<&LocalProfile> = locals
            .iter()
            .filter(|l| {
                l.identity.account_id.is_none()
                    && l.identity.email.is_some()
                    && l.identity.email == identity.email
            })
            .collect();

        if let Some(local) = exact {
            if local.value == value {
                items.push(item(
                    key,
                    Action::Unchanged,
                    Some(local.alias.clone()),
                    "same account, identical credentials",
                ));
                renames.insert(alias.clone(), local.alias.clone());
                continue;
            }
            let imported_ts = profile::parse_last_refresh(&value);
            let local_ts = profile::parse_last_refresh(&local.value);
            let (action, reason) = match strategy {
                Strategy::Skip => (
                    Action::Skip,
                    "same account with different credentials; local kept (use --strategy)"
                        .to_string(),
                ),
                Strategy::Overwrite => {
                    let mut reason = "same account; imported credentials replace local".to_string();
                    if matches!((imported_ts, local_ts), (Some(i), Some(l)) if i < l) {
                        reason.push_str(
                            " (warning: imported last_refresh is older and may hold an already-rotated refresh token)",
                        );
                    }
                    (Action::Overwrite, reason)
                }
                Strategy::Merge => match (imported_ts, local_ts) {
                    (Some(i), Some(l)) if i > l => (
                        Action::Overwrite,
                        "same account; imported credentials are newer (last_refresh)".to_string(),
                    ),
                    _ => (
                        Action::Skip,
                        "same account; local credentials are newer or not comparable".to_string(),
                    ),
                },
            };
            let mut it = item(key, action, Some(local.alias.clone()), reason);
            if action.writes() {
                it.write = Some((
                    LogicalKey::ProfileAuth(local.alias.clone()),
                    entry.text.clone(),
                ));
                it.updates_live = local_current == local.alias;
            }
            renames.insert(alias.clone(), local.alias.clone());
            items.push(it);
            continue;
        }
        if !email_only.is_empty() {
            items.push(item(
                key,
                Action::Ambiguous,
                None,
                format!(
                    "email matches local profile(s) {} without a provable account_id; skipped",
                    email_only
                        .iter()
                        .map(|l| l.alias.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
            continue;
        }
        let lower = alias.to_ascii_lowercase();
        let (action, target, reason) = if taken.contains(&lower) || windows_reserved(alias) {
            let target = unique_alias(alias, &taken)?;
            (
                Action::Rename,
                target,
                format!("alias '{alias}' is taken locally by a different account or provider"),
            )
        } else {
            (Action::Create, alias.clone(), "new account".to_string())
        };
        taken.insert(target.to_ascii_lowercase());
        renames.insert(alias.clone(), target.clone());
        let mut it = item(key, action, Some(target.clone()), reason);
        it.write = Some((LogicalKey::ProfileAuth(target), entry.text.clone()));
        items.push(it);
    }

    // Providers.
    for (key, entry) in &keyed {
        let LogicalKey::Provider(alias) = key else {
            continue;
        };
        let imported = crate::provider::parse_and_validate(alias, &entry.text)?;
        let local_path = key.path_in(&app);
        // Parse rather than `provider::load`, which may migrate files on read.
        let local = std::fs::read_to_string(&local_path)
            .ok()
            .and_then(|text| crate::provider::parse_and_validate(alias, &text).ok());
        match local {
            Some(local) if local.identity_id == imported.identity_id => {
                let local_text = std::fs::read_to_string(&local_path).unwrap_or_default();
                let (action, reason) = if local_text == entry.text {
                    (Action::Unchanged, "identical provider".to_string())
                } else {
                    match strategy {
                        Strategy::Skip => (
                            Action::Skip,
                            "same provider with different settings; local kept".to_string(),
                        ),
                        Strategy::Overwrite => (
                            Action::Overwrite,
                            "same provider; imported settings replace local".to_string(),
                        ),
                        Strategy::Merge => (
                            Action::Skip,
                            "provider merge is not supported yet; local kept (use overwrite to replace)"
                                .to_string(),
                        ),
                    }
                };
                let mut it = item(key, action, Some(alias.clone()), reason);
                if action.writes() {
                    it.write = Some((key.clone(), entry.text.clone()));
                }
                provider_targets.insert(alias.clone(), (action, alias.clone()));
                items.push(it);
            }
            _ => {
                let lower = alias.to_ascii_lowercase();
                let (action, target, text, reason) =
                    if taken.contains(&lower) || windows_reserved(alias) {
                        let target = unique_alias(alias, &taken)?;
                        let text =
                            crate::provider::retarget_provider_text(alias, &target, &entry.text)?;
                        (
                            Action::Rename,
                            target,
                            text,
                            format!("alias '{alias}' is taken locally by something else"),
                        )
                    } else {
                        (
                            Action::Create,
                            alias.clone(),
                            entry.text.clone(),
                            "new provider".to_string(),
                        )
                    };
                taken.insert(target.to_ascii_lowercase());
                let mut it = item(key, action, Some(target.clone()), reason);
                it.write = Some((LogicalKey::Provider(target.clone()), text));
                provider_targets.insert(alias.clone(), (action, target));
                items.push(it);
            }
        }
    }

    // Provider model catalogs follow their provider.
    for (key, entry) in &keyed {
        let LogicalKey::ProviderModels(alias) = key else {
            continue;
        };
        serde_json::from_str::<serde_json::Value>(&entry.text)
            .map_err(|e| CsError::InvalidBundle(format!("{}: invalid JSON: {e}", entry.key)))?;
        let (provider_action, target) = provider_targets
            .get(alias)
            .cloned()
            .unwrap_or((Action::Skip, alias.clone()));
        let target_key = LogicalKey::ProviderModels(target.clone());
        let local_text = std::fs::read_to_string(target_key.path_in(&app)).ok();
        let (action, reason) = if provider_action.writes() {
            if local_text.as_deref() == Some(entry.text.as_str()) {
                (Action::Unchanged, "identical model catalog".to_string())
            } else {
                (provider_action, "follows its provider".to_string())
            }
        } else if local_text.as_deref() == Some(entry.text.as_str()) {
            (Action::Unchanged, "identical model catalog".to_string())
        } else {
            (
                Action::Skip,
                "provider not written; catalog kept".to_string(),
            )
        };
        let mut it = item(key, action, Some(target), reason);
        if action.writes() {
            it.write = Some((target_key, entry.text.clone()));
        }
        items.push(it);
    }

    // App config.
    for (key, entry) in &keyed {
        if *key != LogicalKey::AppConfig {
            continue;
        }
        crate::config::load_from_str_with_warnings(&entry.text)
            .map_err(|e| CsError::InvalidBundle(format!("config.toml: {e}")))?;
        let imported: toml::Table = toml::from_str(&entry.text)
            .map_err(|e| CsError::InvalidBundle(format!("config.toml: {e}")))?;
        if imported
            .get("proxy")
            .and_then(|p| p.as_table())
            .is_some_and(|p| !p.is_empty())
        {
            notices
                .push("config.toml [proxy]: machine-specific, review it after import".to_string());
        }
        let local_path = key.path_in(&app);
        let local_text = std::fs::read_to_string(&local_path).ok();
        let (action, text, reason) = match local_text {
            None => (
                Action::Create,
                entry.text.clone(),
                "no local config.toml".to_string(),
            ),
            Some(local) if local == entry.text => {
                (Action::Unchanged, String::new(), "identical".to_string())
            }
            Some(local) => match strategy {
                Strategy::Skip => (
                    Action::Skip,
                    String::new(),
                    "local config.toml differs; kept".to_string(),
                ),
                Strategy::Overwrite => (
                    Action::Overwrite,
                    entry.text.clone(),
                    "imported config.toml replaces local".to_string(),
                ),
                Strategy::Merge => {
                    let mut local_table: toml::Table = toml::from_str(&local)
                        .with_context(|| format!("parsing {}", local_path.display()))?;
                    let added = merge_missing(&mut local_table, &imported);
                    if added == 0 {
                        (
                            Action::Unchanged,
                            String::new(),
                            "local config.toml already has every imported key".to_string(),
                        )
                    } else {
                        let merged = toml::to_string_pretty(&local_table)
                            .context("serializing merged config.toml")?;
                        crate::config::load_from_str_with_warnings(&merged)
                            .map_err(|e| anyhow::anyhow!("merged config.toml is invalid: {e}"))?;
                        (
                            Action::Merge,
                            merged,
                            format!("added {added} missing key(s); local values kept"),
                        )
                    }
                }
            },
        };
        let mut it = item(key, action, None, reason);
        if action.writes() {
            it.write = Some((LogicalKey::AppConfig, text));
        }
        items.push(it);
    }

    let source_current_target = payload
        .source_current
        .as_ref()
        .and_then(|a| renames.get(a).cloned());
    Ok(ImportPlan {
        items,
        source_current: payload.source_current.clone(),
        source_current_target,
        notices,
    })
}

/// Add keys from `incoming` that `local` lacks, recursively. Returns how many
/// leaf keys were added. Local values always win.
fn merge_missing(local: &mut toml::Table, incoming: &toml::Table) -> usize {
    let mut added = 0;
    for (k, v) in incoming {
        match (local.get_mut(k), v) {
            (None, v) => {
                local.insert(k.clone(), v.clone());
                added += 1;
            }
            (Some(toml::Value::Table(l)), toml::Value::Table(i)) => {
                added += merge_missing(l, i);
            }
            _ => {}
        }
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keeps_local_values() {
        let mut local: toml::Table = toml::from_str("[cache]\nttl = 10\n").unwrap();
        let incoming: toml::Table =
            toml::from_str("[cache]\nttl = 99\n[network]\nmax_concurrent = 4\n").unwrap();
        assert_eq!(merge_missing(&mut local, &incoming), 1);
        assert_eq!(local["cache"]["ttl"].as_integer(), Some(10));
        assert_eq!(local["network"]["max_concurrent"].as_integer(), Some(4));
    }

    #[test]
    fn unique_alias_is_case_insensitive() {
        let taken: BTreeSet<String> = ["work".into(), "work_2".into()].into_iter().collect();
        assert_eq!(unique_alias("Work", &taken).unwrap(), "Work_3");
    }
}
