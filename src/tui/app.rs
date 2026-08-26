use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::DefaultTerminal;
use tokio::sync::Semaphore;

use crate::auth;
use crate::cache;
use crate::jwt::AccountInfo;
use crate::login;
use crate::output::{format_local_datetime, format_local_timestamp, reset_credits_count};
use crate::profile::{
    self, cmd_delete, list_profiles, profile_auth_path, read_current, rename_profile,
    switch_profile, sync_current_from_live, validate_alias,
};
use crate::usage::{
    ConsumedResetCredit, Refresh, UsageError, UsageInfo, fetch_usage_retried,
    fetch_usage_retried_force, fetch_usage_retried_unattended,
};
use crate::warmup::ModelEntry;

#[derive(Debug, Clone)]
pub struct AccountEntry {
    pub alias: String,
    pub info: AccountInfo,
    pub usage: UsageStatus,
    pub is_current: bool,
}

#[derive(Debug, Clone)]
pub enum UsageStatus {
    Idle,
    Loading,
    Loaded(Box<UsageInfo>),
    Error(UsageError),
}

fn retained_usage_by_alias(accounts: Vec<AccountEntry>) -> HashMap<String, UsageStatus> {
    accounts
        .into_iter()
        .map(|account| (account.alias, account.usage))
        .collect()
}

fn refresh_fetches_loaded_usage(refresh: Refresh) -> bool {
    !matches!(refresh, Refresh::Cached)
}

fn refresh_forces_negative_caches(refresh: Refresh) -> bool {
    matches!(refresh, Refresh::Forced)
}

fn refresh_priority(refresh: Refresh) -> u8 {
    match refresh {
        Refresh::Cached => 0,
        Refresh::Unattended => 1,
        Refresh::Forced => 2,
    }
}

#[derive(Debug)]
pub struct ResetCardFailure {
    message: String,
    invalidate_cache: bool,
    outcome_unknown: bool,
}

fn map_reset_card_failure(message: String, invalidate_cache: bool) -> ResetCardFailure {
    ResetCardFailure {
        message,
        invalidate_cache,
        outcome_unknown: invalidate_cache,
    }
}

/// The actual `outcome_unknown_after_request` -> `invalidate_cache` routing decision,
/// isolated from `ConsumeResetCreditError` so it can be unit-tested directly instead of
/// only through a literal struct construction (a reset card is a non-renewable resource:
/// routing an unknown outcome to "definite failure" would let the UI offer to burn a
/// second card after the first attempt may have already consumed one).
fn reset_card_failure_from_outcome(
    unknown: bool,
    unknown_message: String,
    definite_message: String,
) -> ResetCardFailure {
    if unknown {
        map_reset_card_failure(unknown_message, true)
    } else {
        map_reset_card_failure(definite_message, false)
    }
}

#[derive(Debug, Clone)]
pub enum ModelStatus {
    Loading,
    Loaded(Vec<ModelEntry>),
    Error(String),
}

fn wrap_account_detail_line(line: String) -> Vec<String> {
    const MAX_WIDTH: usize = 68;
    if line.chars().count() <= MAX_WIDTH {
        return vec![line];
    }
    let indent = "    ";
    let mut remaining = line.as_str();
    let mut wrapped = Vec::new();
    while remaining.chars().count() > MAX_WIDTH {
        let split = remaining
            .char_indices()
            .take(MAX_WIDTH + 1)
            .filter(|(_, ch)| ch.is_whitespace() || matches!(ch, '·' | ','))
            .map(|(index, _)| index)
            .last()
            .unwrap_or_else(|| {
                remaining
                    .char_indices()
                    .nth(MAX_WIDTH)
                    .map(|(index, _)| index)
                    .unwrap_or(remaining.len())
            });
        let (head, tail) = remaining.split_at(split);
        wrapped.push(head.trim_end().to_string());
        remaining = tail.trim_start_matches(|ch: char| ch.is_whitespace() || ch == '·');
    }
    if !remaining.is_empty() {
        wrapped.push(format!("{indent}{remaining}"));
    }
    wrapped
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Name,
    Quota,
    Status,
}

impl SortMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            SortMode::Name => "name",
            SortMode::Quota => "quota",
            SortMode::Status => "status",
        }
    }
}

pub enum ConfirmAction {
    Delete(String),
    BatchDelete(Vec<String>),
    ConsumeResetCard {
        alias: String,
        credit_id: String,
        expires_at: String,
    },
    RemoveProvider(String),
}

pub struct RenameState {
    pub old_alias: String,
    pub input: String,
    pub cursor: usize,
}

#[derive(Debug, Clone)]
pub struct SearchState {
    pub query: String,
    pub cursor: usize,
}

type ResetCardRefreshResult = (
    String,
    u64,
    Result<(Option<u64>, Vec<crate::usage::ResetCredit>), String>,
);

/// Which top-level TUI tab is active. Accounts (ChatGPT OAuth) and Providers
/// (third-party API + key) are isolated so their very different semantics
/// (quota/scoring vs base_url/key) and key bindings never mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Accounts,
    Providers,
}

pub struct App {
    pub accounts: Vec<AccountEntry>,
    /// Custom API provider profiles (OpenRouter, etc.), shown on the Providers
    /// tab; they carry no OAuth/usage and never join `accounts`.
    pub providers: Vec<crate::provider::ProviderProfile>,
    /// Selected row within the Providers tab.
    pub provider_selected: usize,
    /// Active add/edit provider form.
    pub provider_form: Option<super::provider_form::ProviderFormState>,
    /// Active launch picker (Providers tab, `o`).
    pub provider_launch: Option<super::provider_launch::ProviderLaunchState>,
    /// Active top-level tab.
    pub active_tab: Tab,
    pub selected: usize,
    pub search: Option<SearchState>,
    pub search_active: bool,
    pub sort_mode: SortMode,
    pub view_indices: Vec<usize>,
    pub marked: BTreeSet<String>,
    pub status_msg: Option<String>,
    pub status_is_error: bool,
    pub status_expiry: Option<Instant>,
    pub refreshing_requests: HashMap<String, (u64, Refresh)>,
    pub pending_usage_refreshes: HashMap<String, Refresh>,
    pub usage_next_id: u64,
    pub pending_results: tokio::sync::mpsc::Receiver<(String, u64, Result<UsageInfo, UsageError>)>,
    pub result_sender: tokio::sync::mpsc::Sender<(String, u64, Result<UsageInfo, UsageError>)>,
    pub pending_workspace: tokio::sync::mpsc::Receiver<String>,
    pub workspace_sender: tokio::sync::mpsc::Sender<String>,
    pub pending_warmup: tokio::sync::mpsc::Receiver<(u64, String, Result<(), String>)>,
    pub warmup_sender: tokio::sync::mpsc::Sender<(u64, String, Result<(), String>)>,
    pub pending_reset_cards:
        tokio::sync::mpsc::Receiver<(String, Result<ConsumedResetCredit, ResetCardFailure>)>,
    pub reset_card_sender:
        tokio::sync::mpsc::Sender<(String, Result<ConsumedResetCredit, ResetCardFailure>)>,
    pub pending_reset_card_refreshes: tokio::sync::mpsc::Receiver<ResetCardRefreshResult>,
    pub reset_card_refresh_sender: tokio::sync::mpsc::Sender<ResetCardRefreshResult>,
    pub reset_card_refresh_tasks: HashMap<String, u64>,
    pub usage_generations: HashMap<String, u64>,
    pub reset_card_cooldown_until: Option<Instant>,
    /// Prevents duplicate confirmations from starting two irreversible consumes.
    pub reset_card_tasks: BTreeSet<String>,
    /// Tracks in-flight warmup tasks: task_id → (alias, start_time).
    /// Each spawn gets a unique `warmup_next_id`; results are matched by ID
    /// so a late-arriving result from a timed-out task cannot clear a newer task.
    pub warmup_tasks: HashMap<u64, (String, Instant)>,
    pub warmup_next_id: u64,
    pub confirm: Option<ConfirmAction>,
    pub rename: Option<RenameState>,
    pub usage_limiter: Arc<Semaphore>,
    pub update_available: Option<String>,
    pub update_rx: Option<tokio::sync::oneshot::Receiver<String>>,
    pub auto_refresh_enabled: bool,
    pub auto_refresh_interval: Duration,
    pub next_auto_refresh: Option<Instant>,
    pub auto_warmup_enabled: bool,
    pub detail_visible: bool,
    pub help_popup: Option<super::popup::PopupState>,
    pub menu: Option<super::menu::MenuState>,
    /// Session-level per-alias model list cache (no TTL). Populated lazily
    /// for the selected account or when its account details are opened.
    pub model_cache: HashMap<String, ModelStatus>,
    pub pending_models: tokio::sync::mpsc::Receiver<(String, Result<Vec<ModelEntry>, String>)>,
    pub model_sender: tokio::sync::mpsc::Sender<(String, Result<Vec<ModelEntry>, String>)>,
}

impl App {
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        let (workspace_tx, workspace_rx) = tokio::sync::mpsc::channel(128);
        let (warmup_tx, warmup_rx) = tokio::sync::mpsc::channel(64);
        let (reset_card_tx, reset_card_rx) = tokio::sync::mpsc::channel(16);
        let (reset_card_refresh_tx, reset_card_refresh_rx) = tokio::sync::mpsc::channel(64);
        let (model_tx, model_rx) = tokio::sync::mpsc::channel(32);
        let cfg = crate::config::get();
        App {
            accounts: vec![],
            providers: vec![],
            provider_selected: 0,
            provider_form: None,
            provider_launch: None,
            active_tab: Tab::default(),
            selected: 0,
            search: None,
            search_active: false,
            sort_mode: SortMode::Name,
            view_indices: vec![],
            marked: BTreeSet::new(),
            status_msg: None,
            status_is_error: false,
            status_expiry: None,
            refreshing_requests: HashMap::new(),
            pending_usage_refreshes: HashMap::new(),
            usage_next_id: 0,
            pending_results: rx,
            result_sender: tx,
            pending_workspace: workspace_rx,
            workspace_sender: workspace_tx,
            pending_warmup: warmup_rx,
            warmup_sender: warmup_tx,
            pending_reset_cards: reset_card_rx,
            reset_card_sender: reset_card_tx,
            pending_reset_card_refreshes: reset_card_refresh_rx,
            reset_card_refresh_sender: reset_card_refresh_tx,
            reset_card_refresh_tasks: HashMap::new(),
            usage_generations: HashMap::new(),
            reset_card_cooldown_until: None,
            reset_card_tasks: BTreeSet::new(),
            warmup_tasks: HashMap::new(),
            warmup_next_id: 0,
            confirm: None,
            rename: None,
            usage_limiter: Arc::new(Semaphore::new(cfg.network.max_concurrent)),
            update_available: None,
            update_rx: None,
            auto_refresh_enabled: false,
            auto_refresh_interval: Duration::from_secs(cfg.tui.auto_refresh_interval_secs),
            next_auto_refresh: None,
            auto_warmup_enabled: false,
            detail_visible: true,
            help_popup: None,
            menu: None,
            model_cache: HashMap::new(),
            pending_models: model_rx,
            model_sender: model_tx,
        }
    }

    /// Kick off a model-list fetch for `alias` if the detail panel needs it
    /// and it isn't already loaded or in flight. Idempotent — safe to call
    /// every frame.
    pub fn ensure_models_loaded(&mut self, alias: &str) {
        if matches!(
            self.model_cache.get(alias),
            Some(ModelStatus::Loaded(_)) | Some(ModelStatus::Loading)
        ) {
            return;
        }
        let path = match profile_auth_path(alias) {
            Ok(p) => p,
            Err(_) => return,
        };
        self.model_cache
            .insert(alias.to_string(), ModelStatus::Loading);
        let alias_owned = alias.to_string();
        let tx = self.model_sender.clone();
        let limiter = self.usage_limiter.clone();
        tokio::spawn(async move {
            let _permit = limiter.acquire().await;
            let result = crate::warmup::fetch_models_for_profile(&alias_owned, &path)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send((alias_owned, result)).await;
        });
    }

    /// Fetch the model list for the currently-selected account, if the
    /// detail panel is visible. No-op when nothing is selected.
    pub fn ensure_models_loaded_for_selected(&mut self) {
        if !self.detail_visible {
            return;
        }
        if let Some(alias) = self
            .selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
            .map(|e| e.alias.clone())
        {
            self.ensure_models_loaded(&alias);
        }
    }

    pub fn poll_model_results(&mut self) {
        let mut refresh_open_account = false;
        while let Ok((alias, result)) = self.pending_models.try_recv() {
            refresh_open_account |= matches!(
                self.menu.as_ref(),
                Some(super::menu::MenuState::Account { info, .. }) if info.alias == alias
            );
            self.model_cache.insert(
                alias,
                match result {
                    Ok(models) => ModelStatus::Loaded(models),
                    Err(e) => ModelStatus::Error(e),
                },
            );
        }
        if refresh_open_account {
            self.rebuild_open_account_menu();
        }
    }

    fn rebuild_open_account_menu(&mut self) {
        let scroll = match self.menu.as_ref() {
            Some(super::menu::MenuState::Account { popup, .. }) => popup.scroll,
            _ => return,
        };
        self.open_account_menu();
        if let Some(super::menu::MenuState::Account { popup, .. }) = self.menu.as_mut() {
            popup.scroll = scroll;
        }
    }

    pub fn open_help(&mut self) {
        self.help_popup = Some(super::popup::PopupState::new());
    }

    pub fn close_help(&mut self) {
        self.help_popup = None;
    }

    pub fn open_account_menu(&mut self) {
        let Some(account_idx) = self.selected_account_idx() else {
            return;
        };
        let alias = self.accounts[account_idx].alias.clone();
        self.ensure_models_loaded(&alias);
        let entry = &self.accounts[account_idx];
        let loaded_usage = match &entry.usage {
            UsageStatus::Loaded(u) => Some(u.as_ref()),
            _ => None,
        };
        let plan = loaded_usage
            .and_then(|u| u.plan_type.as_deref())
            .or(entry.info.plan_type.as_deref());
        let reset_cards = loaded_usage.and_then(reset_credits_count);
        let reset_card_expiries = loaded_usage
            .map(|u| {
                let mut credits: Vec<_> = u.reset_credits.iter().collect();
                credits.sort_by_key(|credit| {
                    credit
                        .expires_at
                        .as_deref()
                        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                        .map(|dt| dt.timestamp())
                        .unwrap_or(i64::MAX)
                });
                credits
                    .into_iter()
                    .map(|credit| {
                        let granted = credit
                            .granted_at
                            .as_deref()
                            .map(format_local_datetime)
                            .unwrap_or_else(|| "grant date unavailable".to_string());
                        let expires = credit
                            .expires_at
                            .as_deref()
                            .map(format_local_datetime)
                            .unwrap_or_else(|| "no expiry date".to_string());
                        format!("expires {expires} · granted {granted}")
                    })
                    .collect()
            })
            .unwrap_or_default();
        let can_consume_reset_card = loaded_usage
            .and_then(|u| crate::usage::earliest_reset_credit(&u.reset_credits))
            .is_some();
        let usage_meta: Vec<String> = loaded_usage
            .map(|usage| {
                let mut items = Vec::new();
                if usage.account_limited || usage.rate_limit_reached_type.is_some() {
                    let reason = usage
                        .rate_limit_reached_type
                        .as_deref()
                        .map(|value| format!(" · {}", value.replace(['_', '-'], " ")))
                        .unwrap_or_default();
                    items.push(format!("  Status  limited{reason}"));
                }
                if usage.reset_credits_error.is_some() {
                    items.push("  Reset-card details are temporarily unavailable".to_string());
                }
                if let Some(limit) = &usage.individual_limit {
                    let mut parts = vec!["  Monthly API".to_string()];
                    if let Some(value) = &limit.limit {
                        parts.push(format!("{value} total"));
                    }
                    if let Some(value) = &limit.used {
                        parts.push(format!("{value} used"));
                    }
                    if let Some(value) = &limit.remaining {
                        parts.push(format!("{value} remaining"));
                    }
                    if let Some(value) = limit.remaining_percent {
                        parts.push(format!("{value:.0}% left"));
                    }
                    if let Some(value) = limit.resets_at {
                        parts.push(format!("resets {}", format_local_timestamp(value)));
                    }
                    if parts.len() > 1 {
                        items.push(parts.join(" · "));
                    }
                }
                items
            })
            .unwrap_or_default()
            .into_iter()
            .flat_map(wrap_account_detail_line)
            .collect();
        let models: Vec<String> = match self.model_cache.get(&entry.alias) {
            Some(ModelStatus::Loaded(models)) => crate::warmup::sorted_models_for_display(models)
                .into_iter()
                .map(|model| {
                    let label = match &model.display_name {
                        Some(name) => name.clone(),
                        None => model.slug.clone(),
                    };
                    let default = model
                        .default_reasoning_effort
                        .as_deref()
                        .unwrap_or("not reported");
                    let allowed = if model.supported_reasoning_efforts.is_empty() {
                        "not reported".to_string()
                    } else {
                        model.supported_reasoning_efforts.join(", ")
                    };
                    format!("  {label} · default {default} · allowed {allowed}")
                })
                .collect(),
            Some(ModelStatus::Error(error)) => vec![format!("  error: {error}")],
            _ => vec!["  loading...".to_string()],
        };
        let auth_expiries = profile_auth_path(&entry.alias)
            .ok()
            .and_then(|path| auth::read_auth(&path).ok())
            .map(|auth| {
                let mut expiries = Vec::new();
                if let Some(token) = auth::extract_id_token(&auth) {
                    let expiry = crate::jwt::token_expires_at(&token)
                        .map(crate::output::format_token_expiry)
                        .unwrap_or_else(|| "not reported".into());
                    expiries.push(format!("ID token · {expiry}"));
                }
                if let Some(token) = auth
                    .pointer("/tokens/access_token")
                    .and_then(serde_json::Value::as_str)
                {
                    let expiry = crate::jwt::token_expires_at(token)
                        .map(crate::output::format_token_expiry)
                        .unwrap_or_else(|| "not reported".into());
                    expiries.push(format!("Access token · {expiry}"));
                }
                expiries
            })
            .unwrap_or_default();
        self.menu = Some(super::menu::MenuState::account(
            super::menu::AccountMenuInfo {
                alias: entry.alias.clone(),
                email: entry.info.email.clone(),
                account_id: entry.info.account_id.clone(),
                user_id: entry.info.user_id.clone(),
                workspace_name: entry.info.workspace_name.clone(),
                is_fedramp: entry.info.is_fedramp,
                plan_label: entry.info.plan_label_with(plan),
                plan_type: plan.map(str::to_string),
                is_current: entry.is_current,
                organizations: entry
                    .info
                    .organizations
                    .iter()
                    .filter(|organization| !organization.title.is_empty())
                    .map(|organization| {
                        let role = organization
                            .role
                            .split(['_', '-'])
                            .filter(|part| !part.is_empty())
                            .map(|part| {
                                let mut chars = part.chars();
                                chars
                                    .next()
                                    .map(|first| {
                                        first.to_uppercase().collect::<String>() + chars.as_str()
                                    })
                                    .unwrap_or_default()
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        format!(
                            "{} · {}{}",
                            organization.title,
                            if role.is_empty() { "Member" } else { &role },
                            if organization.is_default {
                                " · default workspace"
                            } else {
                                ""
                            }
                        )
                    })
                    .flat_map(wrap_account_detail_line)
                    .collect(),
                auth_expiries,
                usage: loaded_usage.cloned().map(Box::new),
                usage_meta,
                models,
                reset_cards,
                reset_card_expiries,
                can_consume_reset_card,
            },
        ));
    }

    pub fn open_batch_menu(&mut self) {
        let count = self.marked.len();
        if count == 0 {
            return;
        }
        self.menu = Some(super::menu::MenuState::batch(count));
    }

    pub fn open_batch_relogin_flow(&mut self) {
        let count = self.marked.len();
        if count == 0 {
            return;
        }
        self.menu = Some(super::menu::MenuState::batch_relogin_flow(count));
    }

    pub fn open_add_menu(&mut self) {
        self.menu = Some(super::menu::MenuState::add());
    }

    /// Switch between the Accounts and Providers tabs.
    pub fn toggle_tab(&mut self) {
        self.active_tab = match self.active_tab {
            Tab::Accounts => Tab::Providers,
            Tab::Providers => Tab::Accounts,
        };
    }

    pub fn provider_select_next(&mut self) {
        if !self.providers.is_empty() && self.provider_selected + 1 < self.providers.len() {
            self.provider_selected += 1;
        }
    }

    pub fn provider_select_prev(&mut self) {
        if self.provider_selected > 0 {
            self.provider_selected -= 1;
        }
    }

    /// Open the add-provider form (Providers tab, `a`).
    pub fn open_provider_add(&mut self) {
        self.provider_form = Some(super::provider_form::ProviderFormState::add());
    }

    /// Open the edit-provider form (Providers tab, `e` / Enter).
    /// Enter opens the row; it does not launch Codex.
    pub fn open_provider_edit(&mut self) {
        match self.providers.get(self.provider_selected) {
            Some(p) => {
                self.provider_form = Some(super::provider_form::ProviderFormState::edit(p));
            }
            None => self.set_status_error("No provider selected".to_string(), 3),
        }
    }

    /// Ask to remove the selected provider (Providers tab, `d`).
    pub fn request_remove_provider(&mut self) {
        match self.providers.get(self.provider_selected) {
            Some(p) => self.confirm = Some(ConfirmAction::RemoveProvider(p.alias.clone())),
            None => self.set_status_error("No provider selected".to_string(), 3),
        }
    }

    /// Providers list keys. Launch is `o` (same as Accounts). `l` is re-login
    /// on Accounts, so it never launches from this tab.
    pub fn handle_provider_list_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Down | KeyCode::Char('j') => self.provider_select_next(),
            KeyCode::Up | KeyCode::Char('k') => self.provider_select_prev(),
            KeyCode::Char('a') => self.open_provider_add(),
            KeyCode::Enter | KeyCode::Char('e') => self.open_provider_edit(),
            KeyCode::Char('n') => self.start_provider_rename(),
            KeyCode::Char('d') => self.request_remove_provider(),
            KeyCode::Char('o') => self.open_provider_launch(),
            KeyCode::Char('l') => {
                self.set_status("o launches Codex; l is re-login on Accounts".to_string(), 4)
            }
            _ => {}
        }
    }

    pub fn open_provider_launch(&mut self) {
        match self.providers.get(self.provider_selected) {
            Some(p) => {
                self.provider_launch =
                    Some(super::provider_launch::ProviderLaunchState::from_profile(p));
            }
            None => self.set_status_error("No provider selected".to_string(), 3),
        }
    }

    pub fn handle_provider_launch_key(
        &mut self,
        code: KeyCode,
    ) -> Option<(String, String, crate::provider::ReasoningLaunch)> {
        let picker = self.provider_launch.as_mut()?;
        match picker.handle_key(code) {
            super::provider_launch::LaunchPickerOutcome::Continue => None,
            super::provider_launch::LaunchPickerOutcome::Cancel => {
                self.provider_launch = None;
                None
            }
            super::provider_launch::LaunchPickerOutcome::Launch {
                alias,
                model,
                reasoning,
            } => {
                self.provider_launch = None;
                Some((alias, model, reasoning))
            }
        }
    }

    pub fn start_provider_rename(&mut self) {
        match self.providers.get(self.provider_selected) {
            Some(p) => {
                let old = p.alias.clone();
                let len = old.chars().count();
                self.rename = Some(RenameState {
                    old_alias: old.clone(),
                    input: old,
                    cursor: len,
                });
            }
            None => self.set_status_error("No provider selected".to_string(), 3),
        }
    }

    /// Keys for the add/edit provider form (raw, case-sensitive input).
    pub fn handle_provider_form_key(&mut self, code: KeyCode) {
        let Some(form) = self.provider_form.as_mut() else {
            return;
        };
        match form.handle_key(code) {
            super::provider_form::FormOutcome::Continue => {}
            super::provider_form::FormOutcome::Cancel => self.provider_form = None,
            super::provider_form::FormOutcome::Saved(profile) => {
                let action = if crate::provider::exists(&profile.alias) {
                    "Updated"
                } else {
                    "Added"
                };
                match crate::provider::save(&profile) {
                    Ok(()) => {
                        self.provider_form = None;
                        self.set_status(format!("{action} provider '{}'", profile.alias), 4);
                        self.active_tab = Tab::Providers;
                        self.load_profiles();
                        if let Some(idx) =
                            self.providers.iter().position(|p| p.alias == profile.alias)
                        {
                            self.provider_selected = idx;
                        }
                    }
                    Err(e) => self.set_status_error(format!("{action} provider failed: {e}"), 6),
                }
            }
        }
    }

    pub fn open_relogin_flow_menu(&mut self, alias: String, email: Option<String>) {
        self.menu = Some(super::menu::MenuState::relogin_flow(alias, email));
    }

    pub fn close_menu(&mut self) {
        self.menu = None;
    }

    /// Warmup just one alias.
    pub fn warmup_one(&mut self, alias: &str) {
        let target_indices: Vec<usize> = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.alias == alias)
            .map(|(i, _)| i)
            .collect();
        let (count, _, skipped) = self.warmup_indices(target_indices);
        if count == 0 {
            if skipped > 0 {
                self.set_status(format!("{alias}: already active or in flight"), 4);
            } else {
                self.set_status(format!("{alias}: nothing to warm up"), 4);
            }
        } else {
            self.set_status(format!("Warming up {alias}..."), 6);
        }
    }

    pub fn request_consume_reset_card(&mut self, alias: &str) {
        if self.reset_card_tasks.contains(alias) {
            self.set_status_error(
                format!("{alias}: reset card consumption already in progress"),
                5,
            );
            return;
        }
        let Some(entry) = self.accounts.iter().find(|a| a.alias == alias) else {
            return;
        };
        let UsageStatus::Loaded(u) = &entry.usage else {
            self.set_status(format!("{alias}: refresh usage before using reset card"), 4);
            return;
        };
        let Some(credit) = crate::usage::earliest_reset_credit(&u.reset_credits) else {
            self.set_status(format!("{alias}: no available reset cards"), 4);
            return;
        };
        self.confirm = Some(ConfirmAction::ConsumeResetCard {
            alias: alias.to_string(),
            credit_id: credit.id.clone(),
            expires_at: credit
                .expires_at
                .as_deref()
                .map(format_local_datetime)
                .unwrap_or_else(|| "no expiry".to_string()),
        });
    }

    /// Request delete confirmation for a specific alias (called from menu).
    pub fn request_delete_alias(&mut self, alias: &str) {
        let Some(entry) = self.accounts.iter().find(|a| a.alias == alias) else {
            return;
        };
        if entry.is_current {
            self.set_status_error("Cannot delete the active profile".to_string(), 3);
            return;
        }
        self.confirm = Some(ConfirmAction::Delete(entry.alias.clone()));
    }

    /// Begin rename for a specific alias (called from menu).
    pub fn start_rename_alias(&mut self, alias: &str) {
        let Some(entry) = self.accounts.iter().find(|a| a.alias == alias) else {
            return;
        };
        let old = entry.alias.clone();
        let len = old.len();
        self.rename = Some(RenameState {
            old_alias: old.clone(),
            input: old,
            cursor: len,
        });
    }

    pub fn load_profiles(&mut self) {
        let mut retained_usage = retained_usage_by_alias(std::mem::take(&mut self.accounts));
        let profiles = list_profiles().unwrap_or_else(|e| {
            tracing::warn!("failed to load profiles: {e}");
            Vec::new()
        });
        let current = sync_current_from_live().unwrap_or_else(read_current);
        self.accounts = profiles
            .into_iter()
            .filter_map(|alias| {
                let path = match profile_auth_path(&alias) {
                    Ok(p) => p,
                    Err(_) => return None,
                };
                let info = auth::read_account_info(&path);
                let is_current = alias == current;
                Some(AccountEntry {
                    usage: retained_usage.remove(&alias).unwrap_or(UsageStatus::Idle),
                    alias,
                    info,
                    is_current,
                })
            })
            .collect();
        self.providers = crate::provider::list_providers()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|alias| crate::provider::load(&alias).ok())
            .collect();
        if self.provider_selected >= self.providers.len() {
            self.provider_selected = self.providers.len().saturating_sub(1);
        }
        self.marked
            .retain(|alias| self.accounts.iter().any(|account| &account.alias == alias));
        // A reload can follow credential replacement for an existing alias.
        // Invalidate old generations so their late results cannot bind to the
        // newly loaded profile; the caller starts the replacement refresh.
        self.refreshing_requests.clear();
        self.pending_usage_refreshes.clear();
        self.selected = 0;
        self.view_indices.clear();
        self.update_view();
        if let Some(account_idx) = self.accounts.iter().position(|a| a.is_current)
            && let Some(view_idx) = self.view_indices.iter().position(|&idx| idx == account_idx)
        {
            self.selected = view_idx;
        }
    }

    pub fn load_profiles_preserving_selection(&mut self) {
        let selected_alias = self
            .selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
            .map(|entry| entry.alias.clone());

        self.load_profiles();

        if let Some(alias) = selected_alias
            && let Some(account_idx) = self.accounts.iter().position(|a| a.alias == alias)
            && let Some(view_idx) = self.view_indices.iter().position(|&idx| idx == account_idx)
        {
            self.selected = view_idx;
        }
    }

    /// Recompute `view_indices` based on the current search query.
    pub fn update_view(&mut self) {
        let selected_account_idx = self.selected_account_idx();

        self.view_indices = match &self.search {
            None => (0..self.accounts.len()).collect(),
            Some(s) if s.query.is_empty() => (0..self.accounts.len()).collect(),
            Some(s) => {
                let q = s.query.to_lowercase();
                self.accounts
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| {
                        entry.alias.to_lowercase().contains(&q)
                            || entry
                                .info
                                .email
                                .as_deref()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(&q)
                            || entry
                                .info
                                .plan_type
                                .as_deref()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(&q)
                    })
                    .map(|(i, _)| i)
                    .collect()
            }
        };

        match self.sort_mode {
            SortMode::Name => {}
            SortMode::Quota => {
                let quotas: Vec<f64> = (0..self.accounts.len())
                    .map(|idx| self.get_5h_used_pct(idx))
                    .collect();
                self.view_indices.sort_by(|&a, &b| {
                    quotas[a]
                        .partial_cmp(&quotas[b])
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            SortMode::Status => {
                let statuses: Vec<u8> = (0..self.accounts.len())
                    .map(|idx| self.status_order(idx))
                    .collect();
                self.view_indices
                    .sort_by(|&a, &b| statuses[a].cmp(&statuses[b]));
            }
        }

        if let Some(account_idx) = selected_account_idx
            && let Some(view_idx) = self.view_indices.iter().position(|&idx| idx == account_idx)
        {
            self.selected = view_idx;
            return;
        }

        if self.view_indices.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.view_indices.len() {
            self.selected = self.view_indices.len() - 1;
        }
    }

    /// Get the selected index in `accounts`.
    pub fn selected_account_idx(&self) -> Option<usize> {
        self.view_indices.get(self.selected).copied()
    }

    pub fn loading_count(&self) -> usize {
        self.refreshing_requests.len()
    }

    pub fn is_refreshing(&self, alias: &str) -> bool {
        self.refreshing_requests.contains_key(alias)
    }

    pub fn cycle_sort(&mut self) {
        self.sort_mode = match self.sort_mode {
            SortMode::Name => SortMode::Quota,
            SortMode::Quota => SortMode::Status,
            SortMode::Status => SortMode::Name,
        };
        self.update_view();
    }

    pub fn toggle_mark(&mut self) {
        if let Some(idx) = self.selected_account_idx() {
            let alias = self.accounts[idx].alias.clone();
            if !self.marked.remove(&alias) {
                self.marked.insert(alias);
            }
        }

        if self.selected + 1 < self.view_indices.len() {
            self.selected += 1;
        }
    }

    pub fn clear_marks(&mut self) {
        self.marked.clear();
    }

    /// Returns true if usage data proves an active rate-limit window.
    ///
    /// A window that appears "just started" (elapsed < 5 min) likely means the previous warmup
    /// ping didn't consume real quota — allow the user to retry.
    fn is_already_warmed(&self, alias: &str) -> bool {
        let now = crate::auth::now_unix_secs();

        // Prefer in-memory loaded usage — most authoritative.
        for a in &self.accounts {
            if a.alias != alias {
                continue;
            }
            if let UsageStatus::Loaded(u) = &a.usage {
                return crate::usage::usage_has_active_warmup_window(u, now);
            }
        }

        // No loaded data: fall back to disk-cached usage.
        if let Some(u) = crate::cache::get(alias) {
            return crate::usage::usage_has_active_warmup_window(&u, now);
        }

        false
    }

    fn is_warmup_in_flight(&self, alias: &str) -> bool {
        self.warmup_tasks.values().any(|(a, _)| a == alias)
    }

    fn warmup_indices(&mut self, target_indices: Vec<usize>) -> (usize, usize, usize) {
        let candidate_count = target_indices.len();
        let aliases: Vec<String> = target_indices
            .iter()
            .filter_map(|&idx| self.accounts.get(idx))
            .filter(|a| {
                !matches!(a.usage, UsageStatus::Error(_))
                    && !self.is_already_warmed(&a.alias)
                    && !self.is_warmup_in_flight(&a.alias)
            })
            .map(|a| a.alias.clone())
            .collect();
        let skipped = candidate_count.saturating_sub(aliases.len());

        let count = aliases.len();
        for alias in aliases {
            self.spawn_warmup(alias);
        }

        (count, candidate_count, skipped)
    }

    pub fn warmup_all(&mut self) -> usize {
        let target_indices: Vec<usize> = (0..self.accounts.len()).collect();
        let (count, _, _) = self.warmup_indices(target_indices);
        count
    }

    pub fn refresh_one(&mut self, alias: &str) {
        let Some(idx) = self
            .accounts
            .iter()
            .position(|account| account.alias == alias)
        else {
            return;
        };
        self.model_cache.remove(alias);
        self.fetch_usage_for(idx, Refresh::Forced);
        self.ensure_models_loaded(alias);
        self.set_status(format!("Refreshing {alias}"), 3);
    }

    fn spawn_warmup(&mut self, alias: String) {
        // Skip if this alias already has an in-flight warmup task.
        if self.is_warmup_in_flight(&alias) {
            return;
        }
        let task_id = self.warmup_next_id;
        self.warmup_next_id += 1;
        self.warmup_tasks
            .insert(task_id, (alias.clone(), Instant::now()));
        let path = match profile_auth_path(&alias) {
            Ok(p) => p,
            Err(e) => {
                self.warmup_tasks.remove(&task_id);
                self.set_status_error(format!("Path error for {alias}: {e}"), 5);
                return;
            }
        };
        let tx = self.warmup_sender.clone();
        let limiter = self.usage_limiter.clone();
        tokio::spawn(async move {
            let _permit = limiter.acquire().await;
            let result = crate::warmup::warmup_account(&alias, &path)
                .await
                .map_err(|e| {
                    tracing::error!(alias = %alias, error = %format!("{e:#}"), "warmup failed");
                    format!("{e:#}")
                });
            let _ = tx.send((task_id, alias, result)).await;
        });
    }

    pub fn poll_update(&mut self) {
        if let Some(rx) = &mut self.update_rx {
            match rx.try_recv() {
                Ok(version) => {
                    self.update_available = Some(version);
                    self.update_rx = None;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    // Sender dropped without sending (no update or check failed)
                    self.update_rx = None;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    // Still waiting, keep polling
                }
            }
        }
    }

    pub fn start_update_check(&mut self) {
        if self.update_rx.is_some() || self.update_available.is_some() {
            return;
        }

        let (tx, rx) = tokio::sync::oneshot::channel();
        self.update_rx = Some(rx);
        let is_dev = crate::update::current_version().contains("-dev");
        tokio::spawn(async move {
            let result = if is_dev {
                crate::update::check_for_dev_update().await
            } else {
                crate::update::check_for_update(false).await
            };
            if let Ok(Some(info)) = result {
                let _ = tx.send(info.latest_version);
            }
        });
    }

    pub fn poll_warmup_results(&mut self) {
        let mut to_refresh = std::collections::BTreeSet::<String>::new();
        while let Ok((task_id, alias, result)) = self.pending_warmup.try_recv() {
            // Only accept results whose task_id is still tracked.
            // A timed-out task's late result is silently ignored.
            if self.warmup_tasks.remove(&task_id).is_none() {
                continue;
            }
            match result {
                Ok(()) => {
                    self.set_status(format!("Warmed up {alias} — refreshing usage..."), 4);
                    to_refresh.insert(alias);
                }
                Err(e) => {
                    self.set_status_error(format!("Warmup failed ({alias}): {e}"), 6);
                }
            }
        }
        for alias in to_refresh {
            if let Some(idx) = self.accounts.iter().position(|a| a.alias == alias) {
                // Always force a fresh fetch after warmup while keeping the previous
                // quota visible until the replacement arrives.
                self.fetch_usage_for(idx, Refresh::Forced);
            }
        }
    }

    pub fn poll_reset_card_results(&mut self) {
        let mut to_refresh = std::collections::BTreeSet::<String>::new();
        while let Ok((alias, result)) = self.pending_reset_cards.try_recv() {
            match result {
                Ok(consumed) => {
                    if let Err(err) = cache::invalidate(&alias) {
                        tracing::warn!("Failed to invalidate usage cache for {alias}: {err}");
                    }
                    self.set_status(
                        format!(
                            "Used reset card for {alias} (was expiring {})",
                            consumed
                                .credit
                                .expires_at
                                .as_deref()
                                .map(format_local_datetime)
                                .unwrap_or_else(|| "no expiry".to_string())
                        ),
                        6,
                    );
                    to_refresh.insert(alias);
                }
                Err(e) => {
                    if !e.outcome_unknown {
                        self.reset_card_tasks.remove(&alias);
                    }
                    if e.invalidate_cache
                        && let Err(err) = cache::invalidate(&alias)
                    {
                        tracing::warn!("Failed to invalidate usage cache for {alias}: {err}");
                    }
                    self.set_status_error(e.message, 7);
                }
            }
        }
        for alias in to_refresh {
            if let Some(idx) = self.accounts.iter().position(|a| a.alias == alias) {
                self.fetch_usage_for(idx, Refresh::Forced);
            }
        }
    }

    fn request_reset_card_refresh(&mut self, alias: &str, generation: u64) {
        if self
            .reset_card_cooldown_until
            .is_some_and(|until| Instant::now() < until)
        {
            return;
        }
        if self.reset_card_refresh_tasks.get(alias) == Some(&generation) {
            return;
        }
        self.reset_card_refresh_tasks
            .insert(alias.to_string(), generation);
        let path = match profile_auth_path(alias) {
            Ok(path) => path,
            Err(error) => {
                self.reset_card_refresh_tasks.remove(alias);
                tracing::debug!("[{alias}] reset-card detail path unavailable: {error}");
                return;
            }
        };
        let alias = alias.to_string();
        let sender = self.reset_card_refresh_sender.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            self.reset_card_refresh_tasks.remove(&alias);
            return;
        };
        runtime.spawn(async move {
            let result = crate::usage::refresh_reset_credits_for_profile(&alias, &path)
                .await
                .map_err(|error| error.to_string());
            let _ = sender.send((alias, generation, result)).await;
        });
    }

    pub fn poll_reset_card_refreshes(&mut self) {
        let mut changed = false;
        while let Ok((alias, generation, result)) = self.pending_reset_card_refreshes.try_recv() {
            if self.reset_card_refresh_tasks.get(&alias) == Some(&generation) {
                self.reset_card_refresh_tasks.remove(&alias);
            }
            if self.usage_generations.get(&alias) != Some(&generation) {
                continue;
            }
            let rate_limited = matches!(
                &result,
                Err(error) if error.contains("HTTP 429") || error.contains("cooling down")
            );
            if rate_limited {
                self.reset_card_cooldown_until = Some(Instant::now() + Duration::from_secs(30));
                self.set_status_error(
                    "Reset Card service rate-limited; card refresh is cooling down".to_string(),
                    8,
                );
            }
            let Some(entry) = self.accounts.iter_mut().find(|entry| entry.alias == alias) else {
                continue;
            };
            let UsageStatus::Loaded(usage) = &mut entry.usage else {
                continue;
            };
            match result {
                Ok((available_count, credits)) => {
                    if available_count == Some(0) {
                        usage.reset_credits_available_count = Some(0);
                        usage.reset_credits.clear();
                    } else {
                        if let Some(count) = available_count {
                            usage.reset_credits_available_count = Some(count);
                        }
                        if !credits.is_empty() {
                            if available_count.is_none() {
                                usage.reset_credits_available_count = Some(credits.len() as u64);
                            }
                            usage.reset_credits = credits;
                        }
                    }
                    usage.reset_credits_error = None;
                }
                Err(error) => {
                    usage.reset_credits_error = Some(error);
                }
            }
            let available_count = usage.reset_credits_available_count;
            let credits = usage.reset_credits.clone();
            let error = usage.reset_credits_error.clone();
            let expected_fetched_at = usage.fetched_at;
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn_blocking(move || {
                    crate::cache::put_reset_credits(
                        &alias,
                        expected_fetched_at,
                        available_count,
                        &credits,
                        error.as_deref(),
                    );
                });
            } else {
                crate::cache::put_reset_credits(
                    &alias,
                    expected_fetched_at,
                    available_count,
                    &credits,
                    error.as_deref(),
                );
            }
            changed = true;
        }
        if changed {
            self.update_view();
            self.rebuild_open_account_menu();
        }
    }

    pub fn run_due_reset_card_cooldown(&mut self) {
        let Some(until) = self.reset_card_cooldown_until else {
            return;
        };
        if Instant::now() < until {
            return;
        }
        self.reset_card_cooldown_until = None;
        let aliases: Vec<(String, u64)> = self
            .accounts
            .iter()
            .filter_map(|entry| match &entry.usage {
                UsageStatus::Loaded(usage)
                    if crate::usage::should_fetch_reset_credit_details(usage) =>
                {
                    self.usage_generations
                        .get(&entry.alias)
                        .map(|generation| (entry.alias.clone(), *generation))
                }
                _ => None,
            })
            .collect();
        for (alias, generation) in aliases {
            self.request_reset_card_refresh(&alias, generation);
        }
    }

    fn get_5h_used_pct(&self, idx: usize) -> f64 {
        match &self.accounts[idx].usage {
            UsageStatus::Loaded(u) => u
                .primary
                .as_ref()
                .and_then(|w| w.used_percent)
                // free accounts have no 5h window — fall back to 7d usage for sorting
                .or_else(|| u.secondary.as_ref().and_then(|w| w.used_percent))
                .unwrap_or(999.0),
            _ => 999.0,
        }
    }

    fn status_order(&self, idx: usize) -> u8 {
        match &self.accounts[idx].usage {
            UsageStatus::Error(_) => 0,
            UsageStatus::Loaded(u) if !crate::usage::is_available(u) => 1,
            UsageStatus::Loaded(_) => 2,
            UsageStatus::Loading => 3,
            UsageStatus::Idle => 4,
        }
    }

    fn fetch_usage_for(&mut self, idx: usize, refresh: Refresh) {
        let entry = match self.accounts.get(idx) {
            Some(e) => e,
            None => return,
        };
        if self.refreshing_requests.contains_key(&entry.alias) {
            if refresh_fetches_loaded_usage(refresh) {
                self.pending_usage_refreshes
                    .entry(entry.alias.clone())
                    .and_modify(|queued| {
                        if refresh_priority(refresh) > refresh_priority(*queued) {
                            *queued = refresh;
                        }
                    })
                    .or_insert(refresh);
            }
            return;
        }
        let needs_usage =
            refresh_fetches_loaded_usage(refresh) || !matches!(entry.usage, UsageStatus::Loaded(_));
        let force_negative_caches = refresh_forces_negative_caches(refresh);
        let needs_workspace = force_negative_caches
            || entry
                .info
                .account_id
                .as_deref()
                .is_some_and(|id| !crate::cache::workspace_name_is_known(id));
        if !needs_usage && !needs_workspace {
            return;
        }

        let alias = entry.alias.clone();
        let path = match profile_auth_path(&alias) {
            Ok(p) => p,
            Err(e) => {
                self.set_status_error(format!("Path error for {alias}: {e}"), 5);
                return;
            }
        };
        let current = read_current();
        let limiter = self.usage_limiter.clone();

        if needs_usage && !matches!(self.accounts[idx].usage, UsageStatus::Loaded(_)) {
            self.accounts[idx].usage = UsageStatus::Loading;
        }

        let usage_tx = self.result_sender.clone();
        let workspace_tx = self.workspace_sender.clone();
        let request_id = needs_usage.then(|| {
            let request_id = self.usage_next_id;
            self.usage_next_id = self.usage_next_id.wrapping_add(1);
            self.refreshing_requests
                .insert(alias.clone(), (request_id, refresh));
            request_id
        });
        tokio::spawn(async move {
            let _permit = limiter.acquire().await;
            if needs_usage {
                let result = match refresh {
                    Refresh::Cached => fetch_usage_retried(&alias, &path, &current).await,
                    Refresh::Unattended => {
                        fetch_usage_retried_unattended(&alias, &path, &current).await
                    }
                    Refresh::Forced => fetch_usage_retried_force(&alias, &path, &current).await,
                };
                // Usage is independent of best-effort workspace metadata.
                let _ = usage_tx
                    .send((alias.clone(), request_id.expect("usage request id"), result))
                    .await;
            }
            if needs_workspace {
                // Read auth after usage because that path may have refreshed the token.
                if let Ok(auth) = crate::auth::read_auth(&path)
                    && let Err(err) =
                        crate::workspace::refresh_for_auth_if_needed(&auth, force_negative_caches)
                            .await
                {
                    tracing::debug!("[{alias}] workspace metadata unavailable: {err}");
                }
                let _ = workspace_tx.send(alias).await;
            }
        });
    }

    fn refresh_indices(&mut self, target_indices: &[usize], refresh: Refresh) {
        let mut card_refreshes = Vec::new();
        for &i in target_indices {
            let entry = &mut self.accounts[i];
            if let UsageStatus::Error(_) = &entry.usage {
                entry.usage = UsageStatus::Idle;
            }
            if matches!(refresh, Refresh::Cached)
                && let Some(cached) = crate::cache::get(&entry.alias)
            {
                let should_refresh_cards = crate::usage::should_fetch_reset_credit_details(&cached);
                entry.usage = UsageStatus::Loaded(Box::new(cached));
                if should_refresh_cards {
                    let generation = self.usage_next_id;
                    self.usage_next_id = self.usage_next_id.wrapping_add(1);
                    self.usage_generations
                        .insert(entry.alias.clone(), generation);
                    card_refreshes.push((entry.alias.clone(), generation));
                }
            }
        }
        for &i in target_indices {
            self.fetch_usage_for(i, refresh);
        }
        for (alias, generation) in card_refreshes {
            self.request_reset_card_refresh(&alias, generation);
        }
        self.update_view();
    }

    /// Refresh usage for all visible accounts (search-filtered view).
    /// Batch refresh of just the marked accounts is exposed separately
    /// via the Enter > Batch menu so the implicit "marks change scope"
    /// behavior is gone.
    pub fn refresh(&mut self, refresh: Refresh) {
        let target_indices: Vec<usize> = self.view_indices.clone();
        self.refresh_indices(&target_indices, refresh);
    }

    pub fn refresh_all(&mut self, refresh: Refresh) {
        let target_indices: Vec<usize> = (0..self.accounts.len()).collect();
        self.refresh_indices(&target_indices, refresh);
    }

    pub fn poll_results(&mut self) {
        let mut changed = false;
        let open_account_alias = match self.menu.as_ref() {
            Some(super::menu::MenuState::Account { info, .. }) => Some(info.alias.clone()),
            _ => None,
        };
        let mut refresh_open_account = false;
        while let Ok((alias, request_id, result)) = self.pending_results.try_recv() {
            let is_current_request = self
                .refreshing_requests
                .get(&alias)
                .is_some_and(|(active_id, _)| *active_id == request_id);
            if !is_current_request {
                continue;
            }
            self.refreshing_requests.remove(&alias);
            let Some(idx) = self.accounts.iter().position(|entry| entry.alias == alias) else {
                continue;
            };
            self.accounts[idx].usage = match result {
                Ok(u) => UsageStatus::Loaded(Box::new(u)),
                Err(e) => UsageStatus::Error(e),
            };
            self.usage_generations.insert(alias.clone(), request_id);
            let should_refresh_cards = matches!(
                &self.accounts[idx].usage,
                UsageStatus::Loaded(usage) if crate::usage::should_fetch_reset_credit_details(usage)
            );
            crate::cache::apply_workspace_name(&mut self.accounts[idx].info);
            refresh_open_account |= open_account_alias.as_deref() == Some(alias.as_str());
            changed = true;
            if let Some(refresh) = self.pending_usage_refreshes.remove(&alias) {
                self.fetch_usage_for(idx, refresh);
            }
            if should_refresh_cards {
                self.request_reset_card_refresh(&alias, request_id);
            }
        }
        while let Ok(alias) = self.pending_workspace.try_recv() {
            if let Some(entry) = self.accounts.iter_mut().find(|entry| entry.alias == alias) {
                crate::cache::apply_workspace_name(&mut entry.info);
                refresh_open_account |= open_account_alias.as_deref() == Some(alias.as_str());
                changed = true;
            }
        }
        if changed {
            self.update_view();
        }
        if refresh_open_account {
            self.rebuild_open_account_menu();
        }
    }

    pub fn switch_selected(&mut self) {
        if let Some(entry) = self
            .selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
        {
            let alias = entry.alias.clone();
            match switch_profile(&alias) {
                Ok(()) => {
                    let _ = cache::set_last_used(&alias);
                    let current = read_current();
                    for a in &mut self.accounts {
                        a.is_current = a.alias == current;
                    }
                    self.set_status(format!("Switched to {alias}"), 3);
                }
                Err(e) => self.set_status_error(format!("Switch failed: {e}"), 5),
            }
        }
    }

    pub fn confirm_action(&mut self) {
        let action = match self.confirm.take() {
            Some(a) => a,
            None => return,
        };
        match action {
            ConfirmAction::Delete(alias) => match cmd_delete(&alias) {
                Ok(()) => {
                    self.set_status(format!("Deleted {alias} (recoverable)"), 3);
                    self.load_profiles();
                    self.refresh(Refresh::Forced);
                }
                Err(e) => self.set_status_error(format!("Delete failed: {e}"), 5),
            },
            ConfirmAction::RemoveProvider(alias) => match crate::provider::remove(&alias) {
                Ok(()) => {
                    self.set_status(format!("Removed provider {alias}"), 3);
                    self.load_profiles();
                }
                Err(e) => self.set_status_error(format!("Remove provider failed: {e}"), 5),
            },
            ConfirmAction::BatchDelete(aliases) => {
                let mut ok = 0usize;
                let mut errors: Vec<String> = Vec::new();
                let current = read_current();
                for alias in &aliases {
                    if alias == &current {
                        errors.push(format!("{alias}: active, skipped"));
                        continue;
                    }
                    match cmd_delete(alias) {
                        Ok(()) => ok += 1,
                        Err(e) => errors.push(format!("{alias}: {e}")),
                    }
                }
                self.marked.clear();
                self.load_profiles();
                self.refresh(Refresh::Forced);
                let msg = if errors.is_empty() {
                    format!("Deleted {ok} account(s) (recoverable)")
                } else {
                    format!("Deleted {ok} ok, {} failed", errors.len())
                };
                if errors.is_empty() {
                    self.set_status(msg, 6);
                } else {
                    self.set_status_error(msg, 6);
                }
            }
            ConfirmAction::ConsumeResetCard {
                alias, credit_id, ..
            } => {
                self.consume_reset_card(&alias, &credit_id);
            }
        }
    }

    fn consume_reset_card(&mut self, alias: &str, credit_id: &str) {
        if !self.reset_card_tasks.insert(alias.to_string()) {
            self.set_status_error(
                format!("{alias}: reset card consumption already in progress"),
                5,
            );
            return;
        }
        let path = match profile_auth_path(alias) {
            Ok(p) => p,
            Err(e) => {
                self.reset_card_tasks.remove(alias);
                self.set_status_error(format!("Path error for {alias}: {e}"), 5);
                return;
            }
        };
        let alias_owned = alias.to_string();
        let credit_id = credit_id.to_string();
        let tx = self.reset_card_sender.clone();
        self.set_status(format!("Using reset card for {alias}..."), 6);
        tokio::spawn(async move {
            let result = crate::usage::consume_reset_credit_by_id(&alias_owned, &path, &credit_id)
                .await
                .map_err(|error| {
                    let unknown = error.outcome_unknown_after_request();
                    reset_card_failure_from_outcome(
                        unknown,
                        error.user_facing_unknown_message(&alias_owned),
                        format!("Reset card failed ({alias_owned}): {error}"),
                    )
                });
            let _ = tx.send((alias_owned, result)).await;
        });
    }

    pub fn request_batch_delete(&mut self) {
        if self.marked.is_empty() {
            return;
        }
        let aliases: Vec<String> = self.marked.iter().cloned().collect();
        self.confirm = Some(ConfirmAction::BatchDelete(aliases));
    }

    /// Refresh all marked accounts (force).
    pub fn refresh_marked(&mut self) {
        if self.marked.is_empty() {
            return;
        }
        let target_indices: Vec<usize> = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| self.marked.contains(&a.alias))
            .map(|(i, _)| i)
            .collect();
        let count = target_indices.len();
        self.refresh_indices(&target_indices, Refresh::Forced);
        self.set_status(format!("Refreshing {count} marked account(s)..."), 3);
    }

    /// Warmup all marked accounts (skipping already-active / in-flight / errored).
    pub fn warmup_marked(&mut self) {
        if self.marked.is_empty() {
            return;
        }
        let target_indices: Vec<usize> = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| self.marked.contains(&a.alias))
            .map(|(i, _)| i)
            .collect();
        let candidate = target_indices.len();
        let (count, _, skipped) = self.warmup_indices(target_indices);
        if count == 0 {
            self.set_status(
                format!("All {candidate} marked already active or skipped"),
                4,
            );
        } else {
            let mut msg = format!("Warming up {count} marked account(s)");
            if skipped > 0 {
                msg.push_str(&format!(" ({skipped} skipped)"));
            }
            self.set_status(msg, 6);
        }
    }

    pub fn cancel_confirm(&mut self) {
        self.confirm = None;
    }

    pub fn handle_rename_key(&mut self, code: KeyCode) -> bool {
        let state = match &mut self.rename {
            Some(s) => s,
            None => return false,
        };
        match code {
            KeyCode::Esc => {
                self.rename = None;
                return false;
            }
            KeyCode::Enter => {
                let old = state.old_alias.clone();
                let new = state.input.trim().to_string();
                self.rename = None;
                if new.is_empty() || new == old {
                    return false;
                }
                if let Err(err) = validate_alias(&new) {
                    self.set_status_error(format!("Invalid alias: {err}"), 3);
                    return false;
                }
                match self.active_tab {
                    Tab::Providers => match crate::provider::rename(&old, &new) {
                        Ok(()) => {
                            self.set_status(format!("Renamed provider {old} -> {new}"), 3);
                            self.load_profiles();
                            if let Some(idx) = self.providers.iter().position(|p| p.alias == new) {
                                self.provider_selected = idx;
                            }
                        }
                        Err(e) => self.set_status_error(format!("Rename failed: {e}"), 5),
                    },
                    Tab::Accounts => match rename_profile(&old, &new) {
                        Ok(()) => {
                            let was_marked = self.marked.remove(&old);
                            if was_marked {
                                self.marked.insert(new.clone());
                            }
                            self.set_status(format!("Renamed {old} -> {new}"), 3);
                            self.load_profiles();
                            if let Some(account_idx) =
                                self.accounts.iter().position(|a| a.alias == new)
                                && let Some(view_idx) =
                                    self.view_indices.iter().position(|&idx| idx == account_idx)
                            {
                                self.selected = view_idx;
                            }
                            self.refresh(Refresh::Forced);
                        }
                        Err(e) => self.set_status_error(format!("Rename failed: {e}"), 5),
                    },
                }
                return false;
            }
            KeyCode::Backspace if state.cursor > 0 => {
                state.cursor -= 1;
                let byte_pos = char_to_byte(&state.input, state.cursor);
                state.input.remove(byte_pos);
            }
            KeyCode::Delete => {
                let char_count = state.input.chars().count();
                if state.cursor < char_count {
                    let byte_pos = char_to_byte(&state.input, state.cursor);
                    state.input.remove(byte_pos);
                }
            }
            KeyCode::Left if state.cursor > 0 => {
                state.cursor -= 1;
            }
            KeyCode::Right => {
                let char_count = state.input.chars().count();
                if state.cursor < char_count {
                    state.cursor += 1;
                }
            }
            KeyCode::Home => {
                state.cursor = 0;
            }
            KeyCode::End => {
                state.cursor = state.input.chars().count();
            }
            KeyCode::Char(c) => {
                let byte_pos = char_to_byte(&state.input, state.cursor);
                state.input.insert(byte_pos, c);
                state.cursor += 1;
            }
            _ => {}
        }
        true
    }

    pub fn handle_search_key(&mut self, code: KeyCode) -> bool {
        let mut clear_search = false;
        let mut accept_search = false;

        {
            let state = match &mut self.search {
                Some(s) => s,
                None => return false,
            };

            match code {
                KeyCode::Esc => {
                    clear_search = true;
                }
                KeyCode::Enter => {
                    accept_search = true;
                }
                KeyCode::Backspace if state.cursor > 0 => {
                    state.cursor -= 1;
                    let byte_pos = char_to_byte(&state.query, state.cursor);
                    state.query.remove(byte_pos);
                }
                KeyCode::Delete => {
                    let char_count = state.query.chars().count();
                    if state.cursor < char_count {
                        let byte_pos = char_to_byte(&state.query, state.cursor);
                        state.query.remove(byte_pos);
                    }
                }
                KeyCode::Left if state.cursor > 0 => {
                    state.cursor -= 1;
                }
                KeyCode::Right => {
                    let char_count = state.query.chars().count();
                    if state.cursor < char_count {
                        state.cursor += 1;
                    }
                }
                KeyCode::Home => {
                    state.cursor = 0;
                }
                KeyCode::End => {
                    state.cursor = state.query.chars().count();
                }
                KeyCode::Char(c) => {
                    let byte_pos = char_to_byte(&state.query, state.cursor);
                    state.query.insert(byte_pos, c);
                    state.cursor += 1;
                }
                _ => {}
            }
        }

        if clear_search {
            self.search = None;
            self.search_active = false;
            self.update_view();
            return false;
        }

        if accept_search {
            self.search_active = false;
            if self
                .search
                .as_ref()
                .is_some_and(|state| state.query.is_empty())
            {
                self.search = None;
            }
            self.update_view();
            return false;
        }

        self.update_view();
        true
    }

    fn set_status(&mut self, msg: String, secs: u64) {
        self.status_msg = Some(msg);
        self.status_is_error = false;
        self.status_expiry = Some(Instant::now() + Duration::from_secs(secs));
    }

    fn set_status_error(&mut self, msg: String, secs: u64) {
        self.status_msg = Some(msg);
        self.status_is_error = true;
        self.status_expiry = Some(Instant::now() + Duration::from_secs(secs));
    }

    pub fn auto_refresh_interval_secs(&self) -> u64 {
        self.auto_refresh_interval.as_secs()
    }

    pub fn auto_refresh_remaining_secs(&self) -> Option<u64> {
        if !self.auto_refresh_enabled {
            return None;
        }
        Some(
            self.next_auto_refresh
                .map(|next| next.saturating_duration_since(Instant::now()).as_secs())
                .unwrap_or(0),
        )
    }

    pub fn toggle_auto_refresh(&mut self) {
        self.auto_refresh_enabled = !self.auto_refresh_enabled;
        if self.auto_refresh_enabled {
            self.next_auto_refresh = Some(Instant::now());
            self.set_status(
                format!(
                    "Auto refresh on (every {}s)",
                    self.auto_refresh_interval_secs()
                ),
                4,
            );
        } else {
            self.next_auto_refresh = None;
            self.set_status("Auto refresh off".to_string(), 3);
        }
    }

    pub fn toggle_detail_panel(&mut self) {
        self.detail_visible = !self.detail_visible;
        if self.detail_visible {
            self.set_status("Account details shown".to_string(), 3);
        } else {
            self.set_status("Account details hidden".to_string(), 3);
        }
    }

    /// Toggle auto-warmup. Auto-warmup piggybacks on the auto-refresh tick: every
    /// refresh cycle it calls `warmup_all`, which spawns warmup for any account
    /// whose 5h window has expired (paid) or 7d window has expired (free).
    /// Enabling auto-warmup turns on auto-refresh if it is off — without refresh,
    /// the warmup pass has no fresh usage data to decide eligibility.
    pub fn toggle_auto_warmup(&mut self) {
        self.auto_warmup_enabled = !self.auto_warmup_enabled;
        if self.auto_warmup_enabled {
            let mut msg = "Auto warmup on".to_string();
            if !self.auto_refresh_enabled {
                self.auto_refresh_enabled = true;
                self.next_auto_refresh = Some(Instant::now());
                msg.push_str(&format!(
                    " (also enabled auto-refresh every {}s)",
                    self.auto_refresh_interval_secs()
                ));
            }
            self.set_status(msg, 4);
        } else {
            self.set_status("Auto warmup off".to_string(), 3);
        }
    }

    pub fn run_due_auto_refresh(&mut self) {
        if !self.auto_refresh_enabled {
            return;
        }

        let now = Instant::now();
        if self.next_auto_refresh.is_some_and(|next| now < next) {
            return;
        }

        if self.loading_count() > 0 || !self.warmup_tasks.is_empty() {
            self.next_auto_refresh = Some(now + Duration::from_secs(5));
            return;
        }

        self.load_profiles_preserving_selection();
        let account_count = self.accounts.len();
        let warmup_count = if self.auto_warmup_enabled {
            self.warmup_all()
        } else {
            0
        };
        self.refresh_all(Refresh::Unattended);
        self.next_auto_refresh = Some(now + self.auto_refresh_interval);

        let mut msg = format!("Auto refresh: refreshing {account_count} account(s)");
        if warmup_count > 0 {
            msg.push_str(&format!(", warming {warmup_count}"));
        }
        self.set_status(msg, 4);
    }

    pub fn tick(&mut self) {
        if let Some(expiry) = self.status_expiry
            && Instant::now() >= expiry
        {
            self.status_msg = None;
            self.status_expiry = None;
        }

        // Evict warmup tasks that have been in-flight too long (panic / channel drop).
        // Late-arriving results for evicted IDs are ignored in poll_warmup_results.
        const WARMUP_TASK_TIMEOUT: Duration = Duration::from_secs(60);
        let now = Instant::now();
        self.warmup_tasks
            .retain(|_, (_, started)| now.duration_since(*started) < WARMUP_TASK_TIMEOUT);
    }
}

pub async fn run() -> Result<()> {
    // auth-change detection runs before dispatch(), so auto_track is already handled.

    // Ensure terminal is restored even on panic
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        original_hook(info);
    }));

    let mut terminal = ratatui::init();
    let result = run_app(&mut terminal).await;
    ratatui::restore();
    result
}

async fn run_app(terminal: &mut DefaultTerminal) -> Result<()> {
    let mut app = App::new();
    app.load_profiles();
    app.update_view();

    if !app.accounts.is_empty() {
        app.refresh(Refresh::Cached);
    }
    app.start_update_check();

    loop {
        app.poll_results();
        app.poll_warmup_results();
        app.poll_reset_card_results();
        app.poll_reset_card_refreshes();
        app.run_due_reset_card_cooldown();
        app.poll_model_results();
        app.poll_update();
        app.tick();
        app.run_due_auto_refresh();
        app.ensure_models_loaded_for_selected();

        terminal
            .draw(|f| super::ui::render(f, &mut app))
            .context("drawing TUI")?;

        if event::poll(Duration::from_millis(100)).context("polling terminal events")?
            && let Event::Key(key) = event::read().context("reading terminal event")?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            // Search and rename inputs need raw case-sensitive keystrokes.
            if app.rename.is_some() {
                app.handle_rename_key(key.code);
                continue;
            }
            if app.search_active {
                app.handle_search_key(key.code);
                continue;
            }
            // The provider form needs raw, case-sensitive keystrokes.
            if app.provider_form.is_some() {
                app.handle_provider_form_key(key.code);
                continue;
            }
            if app.provider_launch.is_some() {
                if let Some((alias, model, reasoning)) = app.handle_provider_launch_key(key.code) {
                    perform_launch(terminal, &mut app, alias, Some(model), reasoning).await;
                }
                continue;
            }

            // Capital 'W' is a distinct global binding (toggle auto-warmup),
            // separate from menu 'w' (per-account warmup). Detect it before
            // case normalization so it survives the lowercase dispatch below.
            // Only meaningful in the main view (no popup/menu/confirm overlay).
            if matches!(key.code, KeyCode::Char('W'))
                && app.active_tab == Tab::Accounts
                && app.help_popup.is_none()
                && app.menu.is_none()
                && app.confirm.is_none()
            {
                app.toggle_auto_warmup();
                continue;
            }

            // Normalize letter case for top-level dispatch:
            // any uppercase letter is treated as its lowercase equivalent.
            let code = match key.code {
                KeyCode::Char(c) if c.is_ascii_uppercase() => KeyCode::Char(c.to_ascii_lowercase()),
                other => other,
            };

            // Help popup: any key (esc/q/h preferred) closes it; arrows scroll.
            if app.help_popup.is_some() {
                handle_help_key(&mut app, code);
                continue;
            }

            // Active menu intercepts everything.
            if app.menu.is_some() {
                handle_menu_key(&mut app, terminal, code).await;
                continue;
            }

            if app.confirm.is_some() {
                match code {
                    KeyCode::Char('y') => app.confirm_action(),
                    _ => app.cancel_confirm(),
                }
                continue;
            }

            match code {
                KeyCode::Char('q') => break,
                KeyCode::Char('h') => app.open_help(),
                KeyCode::Tab | KeyCode::BackTab => app.toggle_tab(),
                _ => match app.active_tab {
                    Tab::Accounts => match code {
                        KeyCode::Esc => {
                            if app.search.is_some() {
                                app.search = None;
                                app.update_view();
                            } else if !app.marked.is_empty() {
                                app.clear_marks();
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j')
                            if app.selected + 1 < app.view_indices.len() =>
                        {
                            app.selected += 1;
                        }
                        KeyCode::Up | KeyCode::Char('k') if app.selected > 0 => {
                            app.selected -= 1;
                        }
                        KeyCode::Enter => {
                            if app.marked.is_empty() {
                                app.open_account_menu();
                            } else {
                                app.open_batch_menu();
                            }
                        }
                        KeyCode::Char('a') => app.open_add_menu(),
                        KeyCode::Char('o') if app.marked.is_empty() => {
                            if let Some(alias) = app
                                .selected_account_idx()
                                .and_then(|idx| app.accounts.get(idx))
                                .map(|entry| entry.alias.clone())
                            {
                                perform_launch(
                                    terminal,
                                    &mut app,
                                    alias,
                                    None,
                                    crate::provider::ReasoningLaunch::Saved,
                                )
                                .await;
                            } else {
                                app.set_status_error("No account selected".to_string(), 3);
                            }
                        }
                        KeyCode::Char('r') => app.refresh(Refresh::Forced),
                        KeyCode::Char('t') => app.toggle_auto_refresh(),
                        KeyCode::Char('i') => app.toggle_detail_panel(),
                        KeyCode::Char('s') => app.cycle_sort(),
                        KeyCode::Char(' ') => app.toggle_mark(),
                        KeyCode::Char('/') => {
                            if let Some(search) = &mut app.search {
                                search.cursor = search.query.chars().count();
                            } else {
                                app.search = Some(SearchState {
                                    query: String::new(),
                                    cursor: 0,
                                });
                                app.update_view();
                            }
                            app.search_active = true;
                        }
                        _ => {}
                    },
                    Tab::Providers => app.handle_provider_list_key(code),
                },
            }
        }
    }

    Ok(())
}

async fn handle_menu_key(app: &mut App, terminal: &mut DefaultTerminal, code: KeyCode) {
    let Some(menu) = app.menu.as_mut() else {
        return;
    };
    let action = menu.handle_key(code);
    use super::menu::MenuAction;
    match action {
        MenuAction::Noop => {}
        MenuAction::Close => app.close_menu(),
        MenuAction::Use(alias) => {
            app.close_menu();
            // Reuse switch_selected logic by selecting the alias first.
            if let Some(account_idx) = app.accounts.iter().position(|a| a.alias == alias)
                && let Some(view_idx) = app.view_indices.iter().position(|&i| i == account_idx)
            {
                app.selected = view_idx;
            }
            app.switch_selected();
        }
        MenuAction::Launch(alias) => {
            app.close_menu();
            perform_launch(
                terminal,
                app,
                alias,
                None,
                crate::provider::ReasoningLaunch::Saved,
            )
            .await;
        }
        MenuAction::ReloginRequest(alias, email) => {
            app.open_relogin_flow_menu(alias, email);
        }
        MenuAction::Relogin { alias, device } => {
            app.close_menu();
            perform_oauth(terminal, app, OAuthMode::Relogin(alias), device).await;
        }
        MenuAction::Add { device } => {
            app.close_menu();
            perform_oauth(terminal, app, OAuthMode::Add, device).await;
        }
        MenuAction::RefreshOne(alias) => {
            app.close_menu();
            app.refresh_one(&alias);
        }
        MenuAction::Rename(alias) => {
            app.close_menu();
            app.start_rename_alias(&alias);
        }
        MenuAction::WarmupOne(alias) => {
            app.close_menu();
            app.warmup_one(&alias);
        }
        MenuAction::ConsumeResetCard(alias) => {
            app.close_menu();
            app.request_consume_reset_card(&alias);
        }
        MenuAction::DeleteRequest(alias) => {
            app.close_menu();
            app.request_delete_alias(&alias);
        }
        MenuAction::BatchRefresh => {
            app.close_menu();
            app.refresh_marked();
        }
        MenuAction::BatchWarmup => {
            app.close_menu();
            app.warmup_marked();
        }
        MenuAction::BatchReloginRequest => {
            app.open_batch_relogin_flow();
        }
        MenuAction::BatchRelogin { device } => {
            app.close_menu();
            perform_batch_relogin(terminal, app, device).await;
        }
        MenuAction::BatchDeleteRequest => {
            app.close_menu();
            app.request_batch_delete();
        }
    }
}

enum OAuthMode {
    Add,
    Relogin(String),
}

fn reset_plain_terminal_view() {
    let mut stdout = std::io::stdout();
    let _ = crossterm::execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
    );
    let _ = std::io::Write::flush(&mut stdout);
}

fn suspend_tui_for_plain_output() {
    ratatui::restore();
    reset_plain_terminal_view();
}

fn resume_tui_after_plain_output(terminal: &mut DefaultTerminal) {
    reset_plain_terminal_view();
    *terminal = ratatui::init();
    let _ = terminal.clear();
}

async fn perform_launch(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    alias: String,
    model: Option<String>,
    reasoning: crate::provider::ReasoningLaunch,
) {
    suspend_tui_for_plain_output();
    crate::output::set_message_mode(crate::output::MessageMode::Stdout);

    match &model {
        Some(model) => println!("\n=== Launch Codex: {alias} / {model} ===\n"),
        None => println!("\n=== Launch Codex: {alias} ===\n"),
    }

    let result = crate::launch::launch_for_tui(&alias, model.as_deref(), reasoning).await;

    let _ = std::io::Write::flush(&mut std::io::stdout());

    match &result {
        Ok(exit_code) if *exit_code == 0 => println!("\nCodex exited successfully."),
        Ok(exit_code) => println!("\nCodex exited with code {exit_code}."),
        Err(e) => eprintln!("\nError: {e}"),
    }
    println!("\nReturning to TUI...");
    if result.is_err() || result.as_ref().is_ok_and(|code| *code != 0) {
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }

    crate::output::set_message_mode(crate::output::MessageMode::Silent);
    resume_tui_after_plain_output(terminal);

    match result {
        Ok(0) => {
            app.set_status(format!("Codex session ended ({alias})"), 4);
            app.load_profiles_preserving_selection();
            app.refresh(Refresh::Cached);
            if app.auto_refresh_enabled {
                app.next_auto_refresh = Some(Instant::now() + app.auto_refresh_interval);
            }
        }
        Ok(exit_code) => {
            app.set_status_error(format!("Codex exited with code {exit_code}"), 5);
        }
        Err(e) => app.set_status_error(format!("Launch failed: {e}"), 6),
    }
}

/// Suspend the TUI, run OAuth (browser PKCE or device code), persist the
/// resulting auth.json to the appropriate profile, then restore the TUI.
///
/// Always restores the terminal even on error so the caller can keep running.
async fn perform_oauth(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    mode: OAuthMode,
    device: bool,
) {
    // Tear down TUI: restore cooked mode + clear screen so the OAuth output
    // (browser prompts, device user_code, polling progress) is visible.
    suspend_tui_for_plain_output();
    // TUI starts with MessageMode::Silent; switch to Stdout so login.rs
    // user_println calls (device code URL, user_code) are actually shown.
    crate::output::set_message_mode(crate::output::MessageMode::Stdout);

    let mode_name = match &mode {
        OAuthMode::Add => "Add new account".to_string(),
        OAuthMode::Relogin(alias) => format!("Re-login: {alias}"),
    };
    println!("\n=== {mode_name} ===");
    if device {
        println!("Flow: device code\n");
    } else {
        println!("Flow: browser (PKCE)\n");
    }

    let result = run_oauth_inner(mode, device).await;

    // Flush stdout so any buffered output (e.g. device code URL) appears
    // before TUI repaints, particularly important on Windows.
    let _ = std::io::Write::flush(&mut std::io::stdout());

    if result.is_ok() {
        println!("\nReturning to TUI...");
    } else {
        if let Err(ref e) = result {
            eprintln!("\nError: {e}");
        }
        println!("\nReturning to TUI...");
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }

    // Restore silent mode before reinitializing TUI.
    crate::output::set_message_mode(crate::output::MessageMode::Silent);
    resume_tui_after_plain_output(terminal);

    match result {
        Ok(msg) => {
            app.set_status(msg, 5);
            app.load_profiles_preserving_selection();
            app.refresh(Refresh::Forced);
            // Reset auto-refresh timer so it doesn't fire immediately.
            if app.auto_refresh_enabled {
                app.next_auto_refresh = Some(Instant::now() + app.auto_refresh_interval);
            }
        }
        Err(e) => {
            app.set_status_error(format!("OAuth failed: {e}"), 7);
        }
    }
}

/// Sequentially re-login every marked alias. The TUI is suspended for the
/// duration; OAuth output goes to the cooked terminal so the user sees
/// browser prompts / device codes / progress.
///
/// User can abort the whole batch with Ctrl+C between rounds (handled by
/// the underlying login::run_device_*) or by closing the browser tab.
fn batch_relogin_not_attempted(total: usize, ok: usize, failed: usize, cancelled: bool) -> usize {
    total.saturating_sub(ok + failed + usize::from(cancelled))
}

async fn finish_login_or_cancel<T, LoginFuture, CancelFuture>(
    login_future: LoginFuture,
    cancel_future: CancelFuture,
) -> Result<T>
where
    LoginFuture: std::future::Future<Output = Result<T>>,
    CancelFuture: std::future::Future<Output = std::io::Result<()>>,
{
    tokio::pin!(login_future);
    tokio::pin!(cancel_future);
    tokio::select! {
        biased;
        result = &mut login_future => result,
        signal = &mut cancel_future => {
            signal.context("listening for Ctrl+C during batch re-login")?;
            Err(login::LoginCancelled.into())
        }
    }
}

async fn finish_refresh_then_commit<T, RefreshFuture, Commit>(
    refresh_future: RefreshFuture,
    commit: Commit,
) -> Result<T>
where
    RefreshFuture: std::future::Future<Output = ()>,
    Commit: FnOnce() -> Result<T>,
{
    refresh_future.await;
    commit()
}

async fn perform_batch_relogin(terminal: &mut DefaultTerminal, app: &mut App, device: bool) {
    let aliases: Vec<String> = app.marked.iter().cloned().collect();
    if aliases.is_empty() {
        return;
    }

    suspend_tui_for_plain_output();
    crate::output::set_message_mode(crate::output::MessageMode::Stdout);

    let total = aliases.len();
    println!("\n=== Batch re-login: {total} account(s) ===");
    if device {
        println!("Flow: device code\n");
    } else {
        println!("Flow: browser (PKCE)\n");
    }

    let mut ok = 0usize;
    let mut failed: Vec<(String, String)> = Vec::new();
    let mut cancelled = false;

    for (i, alias) in aliases.iter().enumerate() {
        println!("\n--- [{}/{}] {alias} ---", i + 1, total);
        let mode = OAuthMode::Relogin(alias.clone());
        match finish_login_or_cancel(run_oauth_inner(mode, device), tokio::signal::ctrl_c()).await {
            Ok(_) => ok += 1,
            Err(e) if login::is_login_cancelled(&e) => {
                eprintln!("[cancelled] Batch re-login stopped by user");
                cancelled = true;
                break;
            }
            Err(e) => {
                eprintln!("[err] {alias}: {e}");
                failed.push((alias.clone(), e.to_string()));
            }
        }
    }

    let _ = std::io::Write::flush(&mut std::io::stdout());
    if cancelled {
        let not_attempted = batch_relogin_not_attempted(total, ok, failed.len(), true);
        println!(
            "\n=== Batch cancelled: {ok} ok, {} failed, 1 cancelled, {not_attempted} not attempted ===",
            failed.len()
        );
    } else {
        println!("\n=== Batch complete: {ok} ok, {} failed ===", failed.len());
    }
    if !failed.is_empty() {
        for (a, e) in &failed {
            println!("  - {a}: {e}");
        }
    }
    println!("\nReturning to TUI...");
    tokio::time::sleep(Duration::from_millis(1200)).await;

    crate::output::set_message_mode(crate::output::MessageMode::Silent);
    resume_tui_after_plain_output(terminal);

    app.marked.clear();
    let summary = if cancelled {
        let not_attempted = batch_relogin_not_attempted(total, ok, failed.len(), true);
        format!("Batch re-login cancelled: {ok} ok, 1 cancelled, {not_attempted} not attempted")
    } else if failed.is_empty() {
        format!("Batch re-login: {ok} ok")
    } else {
        format!("Batch re-login: {ok} ok, {} failed", failed.len())
    };
    if failed.is_empty() && !cancelled {
        app.set_status(summary, 8);
    } else {
        app.set_status_error(summary, 8);
    }
    app.load_profiles_preserving_selection();
    app.refresh(Refresh::Forced);
    if app.auto_refresh_enabled {
        app.next_auto_refresh = Some(Instant::now() + app.auto_refresh_interval);
    }
}

async fn run_oauth_inner(mode: OAuthMode, device: bool) -> Result<String> {
    let tokens = if device {
        login::run_device_code_auth().await?
    } else {
        login::run_device_auth().await?
    };
    let (auth_val, info) = login::build_auth_from_tokens(&tokens);

    match mode {
        OAuthMode::Add => {
            let refresh_auth = auth_val.clone();
            finish_refresh_then_commit(
                async {
                    if let Err(err) = crate::workspace::refresh_for_auth(&refresh_auth).await {
                        tracing::debug!(
                            "workspace metadata unavailable before TUI login save: {err}"
                        );
                    }
                },
                || {
                    let action = profile::save_auth_value(auth_val, None)?;
                    let alias = action.alias().to_string();
                    let verb = action.action(); // "created" / "updated"
                    let email_disp = info.email.as_deref().unwrap_or("unknown");
                    println!("[ok] Account {verb}: {alias} ({email_disp})");
                    Ok(format!("Account {verb}: {alias}"))
                },
            )
            .await
        }
        OAuthMode::Relogin(alias) => {
            finish_refresh_then_commit(
                async {
                    if let Err(err) = crate::workspace::refresh_for_auth(&auth_val).await {
                        tracing::debug!(
                            "workspace metadata unavailable before TUI re-login save: {err}"
                        );
                    }
                },
                || {
                    profile::replace_profile_auth_and_live_if_current(&alias, &auth_val)?;
                    let email_disp = info.email.as_deref().unwrap_or("unknown");
                    println!("[ok] Re-logged in: {alias} ({email_disp})");
                    Ok(format!("Re-logged in: {alias}"))
                },
            )
            .await
        }
    }
}

fn handle_help_key(app: &mut App, code: KeyCode) {
    let Some(state) = app.help_popup.as_mut() else {
        return;
    };
    match code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') => app.close_help(),
        KeyCode::Down | KeyCode::Char('j') => state.scroll_down(u16::MAX),
        KeyCode::Up | KeyCode::Char('k') => state.scroll_up(),
        KeyCode::PageDown => state.page_down(5, u16::MAX),
        KeyCode::PageUp => state.page_up(5),
        KeyCode::Home => state.reset(),
        _ => app.close_help(),
    }
}

/// Convert a char-based cursor position to a byte offset in a string.
fn char_to_byte(s: &str, char_pos: usize) -> usize {
    s.char_indices()
        .nth(char_pos)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::{
        AccountEntry, App, ModelStatus, UsageStatus, batch_relogin_not_attempted,
        finish_login_or_cancel, finish_refresh_then_commit, refresh_fetches_loaded_usage,
        refresh_forces_negative_caches, reset_card_failure_from_outcome, retained_usage_by_alias,
    };
    use super::{ConfirmAction, Tab};
    use crate::{
        jwt::{AccountInfo, OrgInfo},
        usage::{Refresh, ResetCredit, UsageInfo},
        warmup::ModelEntry,
    };
    use crossterm::event::KeyCode;

    /// Isolate `CODEX_SWITCH_HOME`/`CODEX_HOME` for tests that touch provider
    /// storage. Serialized via the shared env lock so it can't race sibling
    /// tests that also relocate these variables.
    struct EnvHome {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        prev_cs: Option<std::ffi::OsString>,
        prev_ch: Option<std::ffi::OsString>,
    }

    impl EnvHome {
        fn new() -> Self {
            let lock = crate::profile::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let prev_cs = std::env::var_os("CODEX_SWITCH_HOME");
            let prev_ch = std::env::var_os("CODEX_HOME");
            unsafe {
                std::env::set_var("CODEX_SWITCH_HOME", dir.path());
                std::env::set_var("CODEX_HOME", dir.path().join("codex"));
            }
            Self {
                _lock: lock,
                _dir: dir,
                prev_cs,
                prev_ch,
            }
        }
    }

    impl Drop for EnvHome {
        fn drop(&mut self) {
            unsafe {
                match &self.prev_cs {
                    Some(v) => std::env::set_var("CODEX_SWITCH_HOME", v),
                    None => std::env::remove_var("CODEX_SWITCH_HOME"),
                }
                match &self.prev_ch {
                    Some(v) => std::env::set_var("CODEX_HOME", v),
                    None => std::env::remove_var("CODEX_HOME"),
                }
            }
        }
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_provider_form_key(KeyCode::Char(c));
        }
    }

    #[test]
    fn provider_form_add_saves_a_provider() {
        let _home = EnvHome::new();
        let mut app = App::new();
        app.open_provider_add();

        app.handle_provider_form_key(KeyCode::Enter);
        type_str(&mut app, "myrouter");
        app.handle_provider_form_key(KeyCode::Enter);
        app.handle_provider_form_key(KeyCode::Tab);
        app.handle_provider_form_key(KeyCode::Enter);
        type_str(&mut app, "https://openrouter.ai/api/v1");
        app.handle_provider_form_key(KeyCode::Enter);
        app.handle_provider_form_key(KeyCode::Tab);
        app.handle_provider_form_key(KeyCode::Enter);
        type_str(&mut app, "sk-secret-xyz");
        app.handle_provider_form_key(KeyCode::Enter);
        app.handle_provider_form_key(KeyCode::Tab);
        app.handle_provider_form_key(KeyCode::Enter);
        type_str(&mut app, "openai/gpt-5.3-codex");
        app.handle_provider_form_key(KeyCode::Enter);
        app.handle_provider_form_key(KeyCode::Char('s'));

        assert!(app.provider_form.is_none(), "form should close after save");
        let p = crate::provider::load("myrouter").expect("provider must be saved");
        assert_eq!(p.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(p.default_model, "openai/gpt-5.3-codex");
        assert_eq!(p.models.len(), 1);
        assert_eq!(p.env_key, "CODEX_SWITCH_MYROUTER_KEY");
        assert_eq!(p.api_key, "sk-secret-xyz");
        assert_eq!(p.wire_api, "responses");
        assert_eq!(app.active_tab, Tab::Providers);
        assert!(app.providers.iter().any(|x| x.alias == "myrouter"));
    }

    #[test]
    fn provider_launch_picker_picks_a_saved_model_without_writing() {
        let mut app = App::new();
        app.providers.push(crate::provider::ProviderProfile::build(
            "or",
            "https://openrouter.ai/api/v1",
            vec![
                crate::provider::ProviderModel::from_id("minimax/minimax-m3:free"),
                crate::provider::ProviderModel {
                    id: "liquid/lfm-2.5-2.6b:free".into(),
                    reasoning: Some("high".into()),
                    no_web_search: true,
                },
            ],
            "sk-test",
        ));
        app.handle_provider_list_key(KeyCode::Char('o'));
        assert!(app.provider_launch.is_some());
        assert!(app.handle_provider_launch_key(KeyCode::Down).is_none());
        let (alias, model, reasoning) = app
            .handle_provider_launch_key(KeyCode::Enter)
            .expect("enter launches");
        assert_eq!(alias, "or");
        assert_eq!(model, "liquid/lfm-2.5-2.6b:free");
        assert_eq!(
            reasoning,
            crate::provider::ReasoningLaunch::Effort("high".into())
        );
        assert!(app.provider_launch.is_none());
        assert_eq!(
            app.providers[0].models[1].reasoning.as_deref(),
            Some("high"),
            "picker must not persist a launch-only reasoning change"
        );
    }

    #[test]
    fn provider_enter_still_opens_the_edit_form() {
        let mut app = App::new();
        app.providers.push(crate::provider::ProviderProfile::build(
            "or",
            "https://openrouter.ai/api/v1",
            vec![crate::provider::ProviderModel::from_id("m")],
            "k",
        ));
        app.handle_provider_list_key(KeyCode::Enter);
        assert!(app.provider_form.is_some());
        assert!(app.provider_launch.is_none());
    }

    #[test]
    fn provider_o_opens_launch_picker_and_l_does_not() {
        let mut app = App::new();
        app.providers.push(crate::provider::ProviderProfile::build(
            "or",
            "https://openrouter.ai/api/v1",
            vec![crate::provider::ProviderModel::from_id("m")],
            "k",
        ));
        app.handle_provider_list_key(KeyCode::Char('l'));
        assert!(app.provider_launch.is_none());
        assert!(app.provider_form.is_none());
        let hint = app
            .status_msg
            .clone()
            .expect("l should explain the launch key");
        assert!(hint.contains("o launches Codex"), "{hint}");
        assert!(hint.contains("re-login"), "{hint}");

        app.status_msg = None;
        app.handle_provider_list_key(KeyCode::Char('o'));
        assert!(app.provider_launch.is_some());
        assert!(app.provider_form.is_none());
    }

    #[test]
    fn provider_rename_from_the_list() {
        let _home = EnvHome::new();
        crate::provider::save(&crate::provider::ProviderProfile::build(
            "old",
            "https://example.com/v1",
            vec![crate::provider::ProviderModel::from_id("m")],
            "k",
        ))
        .unwrap();
        let mut app = App::new();
        app.load_profiles();
        app.active_tab = Tab::Providers;
        app.provider_selected = 0;
        app.start_provider_rename();
        app.handle_rename_key(KeyCode::End);
        app.handle_rename_key(KeyCode::Backspace);
        app.handle_rename_key(KeyCode::Backspace);
        app.handle_rename_key(KeyCode::Backspace);
        app.handle_rename_key(KeyCode::Char('n'));
        app.handle_rename_key(KeyCode::Char('e'));
        app.handle_rename_key(KeyCode::Char('w'));
        app.handle_rename_key(KeyCode::Enter);
        assert!(crate::provider::exists("new"));
        assert!(!crate::provider::exists("old"));
        assert!(app.providers.iter().any(|p| p.alias == "new"));
    }

    #[test]
    fn request_and_confirm_remove_provider_deletes_it() {
        let _home = EnvHome::new();
        let profile = crate::provider::ProviderProfile::build(
            "gone",
            "https://example.com/v1",
            vec![crate::provider::ProviderModel::from_id("m")],
            "k",
        );
        crate::provider::save(&profile).unwrap();

        let mut app = App::new();
        app.load_profiles();
        app.active_tab = Tab::Providers;
        app.provider_selected = 0;
        assert!(app.providers.iter().any(|x| x.alias == "gone"));

        app.request_remove_provider();
        assert!(
            matches!(&app.confirm, Some(ConfirmAction::RemoveProvider(a)) if a == "gone"),
            "remove must ask for confirmation first"
        );
        app.confirm_action();
        assert!(!crate::provider::exists("gone"));
        assert!(app.providers.iter().all(|x| x.alias != "gone"));
    }

    #[test]
    fn cancelled_batch_counts_the_current_account_as_attempted() {
        assert_eq!(batch_relogin_not_attempted(3, 1, 0, true), 1);
        assert_eq!(batch_relogin_not_attempted(3, 1, 1, false), 1);
    }

    #[tokio::test]
    async fn completed_batch_login_wins_over_a_simultaneous_cancel() {
        let result = finish_login_or_cancel(async { Ok("saved") }, async { Ok(()) }).await;

        assert_eq!(result.unwrap(), "saved");
    }

    #[tokio::test]
    async fn cancel_stops_an_unfinished_batch_login_round() {
        let login = std::future::pending::<anyhow::Result<&'static str>>();
        let result = finish_login_or_cancel(login, async { Ok(()) }).await;

        assert!(crate::login::is_login_cancelled(&result.unwrap_err()));
    }

    #[tokio::test]
    async fn cancellation_before_workspace_refresh_finishes_does_not_commit_credentials() {
        let committed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let committed_by_save = committed.clone();
        let login = finish_refresh_then_commit(std::future::pending(), move || {
            committed_by_save.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok("saved")
        });

        let result = finish_login_or_cancel(login, async { Ok(()) }).await;

        assert!(crate::login::is_login_cancelled(&result.unwrap_err()));
        assert!(!committed.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn model_result_rebuilds_an_open_account_detail() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Idle,
            is_current: false,
        });
        app.view_indices.push(0);
        app.model_cache
            .insert("account".into(), ModelStatus::Loading);
        app.open_account_menu();

        app.model_sender
            .try_send((
                "account".into(),
                Ok(vec![ModelEntry {
                    slug: "official-slug".into(),
                    display_name: Some("Official Name".into()),
                    description: Some("Official description".into()),
                    visibility: Some("list".into()),
                    supported_in_api: Some(true),
                    context_window: Some(372_000),
                    default_reasoning_effort: Some("medium".into()),
                    supported_reasoning_efforts: vec!["low".into(), "medium".into(), "high".into()],
                    ..ModelEntry::default()
                }]),
            ))
            .unwrap();
        app.poll_model_results();

        let Some(super::super::menu::MenuState::Account { info, .. }) = app.menu else {
            panic!("account detail should remain open");
        };
        assert!(info.models.iter().any(|line| {
            line.trim() == "Official Name · default medium · allowed low, medium, high"
        }));
        assert!(!info.models.iter().any(|line| {
            line.contains("official-slug")
                || line.contains("visibility=")
                || line.contains("context=")
        }));
    }

    #[test]
    fn reset_card_confirmation_is_blocked_while_consume_is_in_flight() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::new(UsageInfo {
                reset_credits: vec![ResetCredit {
                    id: "credit-1".into(),
                    granted_at: None,
                    expires_at: None,
                }],
                ..UsageInfo::default()
            })),
            is_current: false,
        });
        app.reset_card_tasks.insert("account".into());

        app.request_consume_reset_card("account");

        assert!(app.confirm.is_none());
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("already in progress"))
        );
    }

    #[test]
    fn usage_result_rebuilds_an_open_account_detail() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loading,
            is_current: false,
        });
        app.view_indices.push(0);
        app.model_cache
            .insert("account".into(), ModelStatus::Loaded(Vec::new()));
        app.refreshing_requests
            .insert("account".into(), (1, Refresh::Cached));
        app.open_account_menu();

        app.result_sender
            .try_send(("account".into(), 1, Ok(UsageInfo::default())))
            .unwrap();
        app.poll_results();
        assert_eq!(app.loading_count(), 0);

        let Some(super::super::menu::MenuState::Account { info, .. }) = app.menu else {
            panic!("account detail should remain open");
        };
        assert!(info.usage.is_some());
    }

    #[test]
    fn reset_card_rate_limit_keeps_cards_visible_and_enters_cooldown() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::new(UsageInfo {
                reset_credits_available_count: Some(1),
                reset_credits: vec![ResetCredit {
                    id: "credit-1".into(),
                    granted_at: None,
                    expires_at: None,
                }],
                ..UsageInfo::default()
            })),
            is_current: false,
        });
        app.view_indices.push(0);
        app.usage_generations.insert("account".into(), 7);
        app.reset_card_refresh_tasks.insert("account".into(), 7);
        app.reset_card_refresh_sender
            .try_send((
                "account".into(),
                7,
                Err("reset credits request failed (HTTP 429 Too Many Requests)".into()),
            ))
            .unwrap();

        app.poll_reset_card_refreshes();

        let UsageStatus::Loaded(usage) = &app.accounts[0].usage else {
            panic!("main usage must stay visible after a card-only rate limit");
        };
        assert_eq!(usage.reset_credits_available_count, Some(1));
        assert_eq!(usage.reset_credits.len(), 1);
        assert!(app.reset_card_cooldown_until.is_some());
        assert!(!app.reset_card_refresh_tasks.contains_key("account"));
        assert!(
            app.status_msg
                .as_deref()
                .is_some_and(|message| message.contains("cooling down"))
        );
    }

    #[test]
    fn ambiguous_empty_card_refresh_does_not_clear_last_known_cards() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::new(UsageInfo {
                reset_credits_available_count: Some(1),
                reset_credits: vec![ResetCredit {
                    id: "credit-1".into(),
                    granted_at: None,
                    expires_at: None,
                }],
                ..UsageInfo::default()
            })),
            is_current: false,
        });
        app.view_indices.push(0);
        app.usage_generations.insert("account".into(), 7);
        app.reset_card_refresh_sender
            .try_send(("account".into(), 7, Ok((None, Vec::new()))))
            .unwrap();

        app.poll_reset_card_refreshes();

        let UsageStatus::Loaded(usage) = &app.accounts[0].usage else {
            panic!("main usage must stay visible after an ambiguous card response");
        };
        assert_eq!(usage.reset_credits_available_count, Some(1));
        assert_eq!(usage.reset_credits.len(), 1);
    }

    #[test]
    fn stale_card_refresh_cannot_restore_cards_after_a_newer_explicit_zero() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::new(UsageInfo {
                reset_credits_available_count: Some(0),
                ..UsageInfo::default()
            })),
            is_current: false,
        });
        app.view_indices.push(0);
        app.usage_generations.insert("account".into(), 2);
        app.reset_card_refresh_tasks.insert("account".into(), 1);
        app.reset_card_refresh_sender
            .try_send((
                "account".into(),
                1,
                Ok((
                    Some(1),
                    vec![ResetCredit {
                        id: "stale-credit".into(),
                        granted_at: None,
                        expires_at: None,
                    }],
                )),
            ))
            .unwrap();

        app.poll_reset_card_refreshes();

        let UsageStatus::Loaded(usage) = &app.accounts[0].usage else {
            panic!("newer main usage must remain loaded");
        };
        assert_eq!(usage.reset_credits_available_count, Some(0));
        assert!(usage.reset_credits.is_empty());
    }

    #[test]
    fn stale_usage_result_is_ignored_after_a_new_request_generation_starts() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: true,
        });
        app.view_indices.push(0);
        app.refreshing_requests
            .insert("account".into(), (2, Refresh::Forced));

        app.result_sender
            .try_send((
                "account".into(),
                1,
                Err(crate::usage::UsageError {
                    summary: "old request".into(),
                    detail: "must be ignored".into(),
                }),
            ))
            .unwrap();
        app.poll_results();

        assert!(matches!(app.accounts[0].usage, UsageStatus::Loaded(_)));
        assert_eq!(app.loading_count(), 1);
    }

    #[test]
    fn forced_follow_up_is_queued_when_usage_request_is_already_in_flight() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: true,
        });
        app.view_indices.push(0);
        app.refreshing_requests
            .insert("account".into(), (1, Refresh::Cached));

        app.fetch_usage_for(0, Refresh::Forced);

        assert_eq!(
            app.pending_usage_refreshes.get("account"),
            Some(&Refresh::Forced)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn force_refresh_keeps_last_loaded_usage_visible_while_request_is_in_flight() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: true,
        });
        app.view_indices.push(0);

        app.refresh_indices(&[0], Refresh::Forced);

        assert!(
            matches!(app.accounts[0].usage, UsageStatus::Loaded(_)),
            "force refresh must retain the last value until its replacement arrives"
        );
        assert_eq!(app.loading_count(), 1);
    }

    #[test]
    fn profile_reload_retains_loaded_usage_by_alias() {
        let retained = retained_usage_by_alias(vec![AccountEntry {
            alias: "account".into(),
            info: AccountInfo::default(),
            usage: UsageStatus::Loaded(Box::default()),
            is_current: false,
        }]);

        assert!(matches!(
            retained.get("account"),
            Some(UsageStatus::Loaded(_))
        ));
    }

    #[test]
    fn unattended_refresh_refetches_loaded_usage_without_forcing_negative_caches() {
        assert!(refresh_fetches_loaded_usage(Refresh::Unattended));
        assert!(!refresh_forces_negative_caches(Refresh::Unattended));
        assert!(refresh_forces_negative_caches(Refresh::Forced));
    }

    #[test]
    fn account_detail_formats_workspaces_and_reset_cards_without_raw_ids() {
        let mut app = App::new();
        app.accounts.push(AccountEntry {
            alias: "account".into(),
            info: AccountInfo {
                organizations: vec![OrgInfo {
                    id: "org-secret-looking-id".into(),
                    title: "Night City".into(),
                    role: "owner".into(),
                    is_default: true,
                }],
                ..Default::default()
            },
            usage: UsageStatus::Loaded(Box::new(UsageInfo {
                reset_credits_available_count: Some(1),
                reset_credits: vec![ResetCredit {
                    id: "credit-secret-looking-id".into(),
                    granted_at: Some("2026-07-01T08:00:00Z".into()),
                    expires_at: Some("2026-07-20T08:00:00Z".into()),
                }],
                ..Default::default()
            })),
            is_current: false,
        });
        app.view_indices.push(0);
        app.model_cache
            .insert("account".into(), ModelStatus::Loaded(Vec::new()));

        app.open_account_menu();

        let Some(super::super::menu::MenuState::Account { info, .. }) = app.menu else {
            panic!("account detail should open");
        };
        assert_eq!(
            info.organizations,
            vec!["Night City · Owner · default workspace"]
        );
        assert!(info.reset_card_expiries[0].contains("expires 2026-07-20"));
        assert!(!info.reset_card_expiries[0].contains("credit-secret-looking-id"));
        assert!(!info.organizations[0].contains("org-secret-looking-id"));
    }

    #[test]
    fn unknown_reset_card_outcome_invalidates_cache_and_uses_safe_message() {
        let failure = reset_card_failure_from_outcome(
            true,
            "account: reset-card consumption may have occurred; verify before retry".to_string(),
            "Reset card failed (account): HTTP 400".to_string(),
        );

        // Unknown outcome must invalidate the cache: the card may have been consumed,
        // so a stale "still available" cache entry could let the UI burn a second one.
        assert!(failure.invalidate_cache);
        assert!(failure.message.contains("account"));
        assert!(failure.message.contains("consumption may have occurred"));
        assert!(failure.message.contains("verify before retry"));
        // Must route to the safe message, never the raw definite-failure text.
        assert!(!failure.message.contains("HTTP 400"));
    }

    #[test]
    fn definite_reset_card_outcome_keeps_accurate_error_without_invalidation() {
        let failure = reset_card_failure_from_outcome(
            false,
            "account: reset-card consumption may have occurred; verify before retry".to_string(),
            "Reset card failed (account): HTTP 400".to_string(),
        );

        // Definite (unconsumed) outcome must NOT invalidate the cache, and must surface
        // the accurate error rather than the unknown-outcome safe message.
        assert!(!failure.invalidate_cache);
        assert_eq!(failure.message, "Reset card failed (account): HTTP 400");
    }
}
