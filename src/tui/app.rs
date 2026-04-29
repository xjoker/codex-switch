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
use crate::profile::{
    self, cmd_delete, list_profiles, profile_auth_path, read_current, rename_profile,
    sync_current_from_live, switch_profile, validate_alias,
};
use crate::usage::{UsageError, UsageInfo, fetch_usage_retried, fetch_usage_retried_force};

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
    Loaded(UsageInfo),
    Error(UsageError),
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

pub struct App {
    pub accounts: Vec<AccountEntry>,
    pub selected: usize,
    pub search: Option<SearchState>,
    pub search_active: bool,
    pub sort_mode: SortMode,
    pub view_indices: Vec<usize>,
    pub marked: BTreeSet<String>,
    pub status_msg: Option<String>,
    pub status_expiry: Option<Instant>,
    pub pending_results: tokio::sync::mpsc::Receiver<(String, Result<UsageInfo, UsageError>)>,
    pub result_sender: tokio::sync::mpsc::Sender<(String, Result<UsageInfo, UsageError>)>,
    pub pending_warmup: tokio::sync::mpsc::Receiver<(u64, String, Result<(), String>)>,
    pub warmup_sender: tokio::sync::mpsc::Sender<(u64, String, Result<(), String>)>,
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
    pub help_popup: Option<super::popup::PopupState>,
    pub menu: Option<super::menu::MenuState>,
}

impl App {
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        let (warmup_tx, warmup_rx) = tokio::sync::mpsc::channel(64);
        let cfg = crate::config::get();
        App {
            accounts: vec![],
            selected: 0,
            search: None,
            search_active: false,
            sort_mode: SortMode::Name,
            view_indices: vec![],
            marked: BTreeSet::new(),
            status_msg: None,
            status_expiry: None,
            pending_results: rx,
            result_sender: tx,
            pending_warmup: warmup_rx,
            warmup_sender: warmup_tx,
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
            help_popup: None,
            menu: None,
        }
    }

    pub fn open_help(&mut self) {
        self.help_popup = Some(super::popup::PopupState::new());
    }

    pub fn close_help(&mut self) {
        self.help_popup = None;
    }

    pub fn open_account_menu(&mut self) {
        let Some(entry) = self
            .selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
        else {
            return;
        };
        self.menu = Some(super::menu::MenuState::account(
            entry.alias.clone(),
            entry.info.email.clone(),
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

    pub fn open_relogin_flow_menu(&mut self, alias: String, email: Option<String>) {
        self.menu = Some(super::menu::MenuState::relogin_flow(alias, email));
    }

    pub fn close_menu(&mut self) {
        self.menu = None;
    }

    /// Refresh just one alias unconditionally.
    pub fn refresh_one(&mut self, alias: &str) {
        if let Some(idx) = self.accounts.iter().position(|a| a.alias == alias) {
            self.refresh_indices(&[idx], true);
            self.set_status(format!("Refreshing {alias}..."), 3);
        }
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

    /// Request delete confirmation for a specific alias (called from menu).
    pub fn request_delete_alias(&mut self, alias: &str) {
        let Some(entry) = self.accounts.iter().find(|a| a.alias == alias) else {
            return;
        };
        if entry.is_current {
            self.set_status("Cannot delete the active profile".to_string(), 3);
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
                    alias,
                    info,
                    usage: UsageStatus::Idle,
                    is_current,
                })
            })
            .collect();
        self.marked
            .retain(|alias| self.accounts.iter().any(|account| &account.alias == alias));
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
        self.accounts
            .iter()
            .filter(|a| matches!(a.usage, UsageStatus::Loading))
            .count()
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

    /// Returns true if the account's 5h window is still active (reset time in the future).
    /// Real usage data is authoritative: if a fresh fetch shows `used == 0`, the warmup
    /// did not stick, even if `cache::is_warmed` flagged a recent attempt as successful
    /// (server can return 200 OK without actually consuming quota — see #warmup-stuck).
    /// The disk `warmed_at` flag is only used as a fallback when no usage data exists.
    fn is_already_warmed(&self, alias: &str) -> bool {
        let now = crate::auth::now_unix_secs();

        // Prefer in-memory loaded usage — most authoritative.
        for a in &self.accounts {
            if a.alias != alias {
                continue;
            }
            if let UsageStatus::Loaded(u) = &a.usage {
                return u.primary.as_ref().is_some_and(|w| {
                    w.resets_at.is_some_and(|t| t > now)
                        && w.used_percent.is_some_and(|p| p > 0.0)
                });
            }
        }

        // No loaded data: fall back to disk-cached usage.
        if let Some(u) = crate::cache::get(alias) {
            return u.primary.is_some_and(|w| {
                w.resets_at.is_some_and(|t| t > now)
                    && w.used_percent.is_some_and(|p| p > 0.0)
            });
        }

        // No usage data anywhere: trust the recent-warmup flag.
        crate::cache::is_warmed(alias)
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
                self.set_status(format!("Path error for {alias}: {e}"), 5);
                return;
            }
        };
        let tx = self.warmup_sender.clone();
        let limiter = self.usage_limiter.clone();
        tokio::spawn(async move {
            let _permit = limiter.acquire().await;
            let result = crate::warmup::warmup_account(&alias, &path)
                .await
                .map_err(|e| e.to_string());
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
                    crate::cache::set_warmed(&alias);
                    self.set_status(format!("Warmed up {alias} — refreshing usage..."), 4);
                    to_refresh.insert(alias);
                }
                Err(e) => {
                    self.set_status(format!("Warmup failed ({alias}): {e}"), 6);
                }
            }
        }
        for alias in to_refresh {
            if let Some(idx) = self.accounts.iter().position(|a| a.alias == alias) {
                // Always force a fresh fetch after warmup — reset to Idle even if currently
                // Loading, so the post-warmup fetch reflects the newly opened quota window.
                self.accounts[idx].usage = UsageStatus::Idle;
                self.fetch_usage_for(idx, true);
            }
        }
    }

    fn get_5h_used_pct(&self, idx: usize) -> f64 {
        match &self.accounts[idx].usage {
            UsageStatus::Loaded(u) => u
                .primary
                .as_ref()
                .and_then(|w| w.used_percent)
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

    fn fetch_usage_for(&mut self, idx: usize, force: bool) {
        let entry = match self.accounts.get(idx) {
            Some(e) => e,
            None => return,
        };
        if matches!(entry.usage, UsageStatus::Loading) {
            return;
        }
        if !force && matches!(entry.usage, UsageStatus::Loaded(_)) {
            return;
        }

        let alias = entry.alias.clone();
        let path = match profile_auth_path(&alias) {
            Ok(p) => p,
            Err(e) => {
                self.set_status(format!("Path error for {alias}: {e}"), 5);
                return;
            }
        };
        let current = read_current();
        let limiter = self.usage_limiter.clone();

        self.accounts[idx].usage = UsageStatus::Loading;

        let tx = self.result_sender.clone();
        tokio::spawn(async move {
            let _permit = limiter.acquire().await;
            let result = if force {
                fetch_usage_retried_force(&alias, &path, &current).await
            } else {
                fetch_usage_retried(&alias, &path, &current).await
            };
            let _ = tx.send((alias, result)).await;
        });
    }

    fn refresh_indices(&mut self, target_indices: &[usize], force: bool) {
        for &i in target_indices {
            let entry = &mut self.accounts[i];
            match &entry.usage {
                UsageStatus::Error(_) => entry.usage = UsageStatus::Idle,
                UsageStatus::Loaded(_) if force => entry.usage = UsageStatus::Idle,
                _ => {}
            }
            if !force && let Some(cached) = crate::cache::get(&entry.alias) {
                entry.usage = UsageStatus::Loaded(cached);
            }
        }
        for &i in target_indices {
            self.fetch_usage_for(i, force);
        }
        self.update_view();
    }

    /// Refresh usage for all visible accounts (search-filtered view).
    /// Batch refresh of just the marked accounts is exposed separately
    /// via the Enter > Batch menu so the implicit "marks change scope"
    /// behavior is gone.
    pub fn refresh(&mut self, force: bool) {
        let target_indices: Vec<usize> = self.view_indices.clone();
        self.refresh_indices(&target_indices, force);
    }

    pub fn refresh_all(&mut self, force: bool) {
        let target_indices: Vec<usize> = (0..self.accounts.len()).collect();
        self.refresh_indices(&target_indices, force);
    }

    pub fn poll_results(&mut self) {
        let mut changed = false;
        while let Ok((alias, result)) = self.pending_results.try_recv() {
            let Some(idx) = self.accounts.iter().position(|entry| entry.alias == alias) else {
                continue;
            };
            self.accounts[idx].usage = match result {
                Ok(u) => UsageStatus::Loaded(u),
                Err(e) => UsageStatus::Error(e),
            };
            changed = true;
        }
        if changed {
            self.update_view();
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
                Err(e) => self.set_status(format!("Switch failed: {e}"), 5),
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
                    self.set_status(format!("Deleted {alias}"), 3);
                    self.load_profiles();
                    self.refresh(true);
                }
                Err(e) => self.set_status(format!("Delete failed: {e}"), 5),
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
                self.refresh(true);
                let msg = if errors.is_empty() {
                    format!("Deleted {ok} account(s)")
                } else {
                    format!("Deleted {ok} ok, {} failed", errors.len())
                };
                self.set_status(msg, 6);
            }
        }
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
        self.refresh_indices(&target_indices, true);
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
            self.set_status(format!("All {candidate} marked already active or skipped"), 4);
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
                    self.set_status(format!("Invalid alias: {err}"), 3);
                    return false;
                }
                match rename_profile(&old, &new) {
                    Ok(()) => {
                        let was_marked = self.marked.remove(&old);
                        if was_marked {
                            self.marked.insert(new.clone());
                        }
                        self.set_status(format!("Renamed {old} -> {new}"), 3);
                        self.load_profiles();
                        if let Some(account_idx) = self.accounts.iter().position(|a| a.alias == new)
                            && let Some(view_idx) =
                                self.view_indices.iter().position(|&idx| idx == account_idx)
                        {
                            self.selected = view_idx;
                        }
                        self.refresh(true);
                    }
                    Err(e) => self.set_status(format!("Rename failed: {e}"), 5),
                }
                return false;
            }
            KeyCode::Backspace => {
                if state.cursor > 0 {
                    state.cursor -= 1;
                    let byte_pos = char_to_byte(&state.input, state.cursor);
                    state.input.remove(byte_pos);
                }
            }
            KeyCode::Delete => {
                let char_count = state.input.chars().count();
                if state.cursor < char_count {
                    let byte_pos = char_to_byte(&state.input, state.cursor);
                    state.input.remove(byte_pos);
                }
            }
            KeyCode::Left => {
                if state.cursor > 0 {
                    state.cursor -= 1;
                }
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
                KeyCode::Backspace => {
                    if state.cursor > 0 {
                        state.cursor -= 1;
                        let byte_pos = char_to_byte(&state.query, state.cursor);
                        state.query.remove(byte_pos);
                    }
                }
                KeyCode::Delete => {
                    let char_count = state.query.chars().count();
                    if state.cursor < char_count {
                        let byte_pos = char_to_byte(&state.query, state.cursor);
                        state.query.remove(byte_pos);
                    }
                }
                KeyCode::Left => {
                    if state.cursor > 0 {
                        state.cursor -= 1;
                    }
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
        let warmup_count = self.warmup_all();
        self.refresh_all(true);
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

    if app.accounts.is_empty() {
        ratatui::restore();
        println!("No saved profiles. Run `codex-switch login` to add an account first.");
        return Ok(());
    }

    app.refresh(false);
    app.start_update_check();

    loop {
        app.poll_results();
        app.poll_warmup_results();
        app.poll_update();
        app.tick();
        app.run_due_auto_refresh();

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

            // Normalize letter case for top-level dispatch:
            // any uppercase letter is treated as its lowercase equivalent.
            let code = match key.code {
                KeyCode::Char(c) if c.is_ascii_uppercase() => {
                    KeyCode::Char(c.to_ascii_lowercase())
                }
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
                KeyCode::Esc => {
                    if app.search.is_some() {
                        app.search = None;
                        app.update_view();
                    } else if !app.marked.is_empty() {
                        app.clear_marks();
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if app.selected + 1 < app.view_indices.len() {
                        app.selected += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if app.selected > 0 {
                        app.selected -= 1;
                    }
                }
                KeyCode::Enter => {
                    if app.marked.is_empty() {
                        app.open_account_menu();
                    } else {
                        app.open_batch_menu();
                    }
                }
                KeyCode::Char('a') => app.open_add_menu(),
                KeyCode::Char('r') => app.refresh(true),
                KeyCode::Char('t') => app.toggle_auto_refresh(),
                KeyCode::Char('s') => app.cycle_sort(),
                KeyCode::Char('h') => app.open_help(),
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
            }
        }
    }

    Ok(())
}

async fn handle_menu_key(app: &mut App, terminal: &mut DefaultTerminal, code: KeyCode) {
    let Some(menu) = app.menu.as_ref() else { return };
    let action = menu.handle_key(code);
    use super::menu::MenuAction;
    match action {
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
        MenuAction::Rename(alias) => {
            app.close_menu();
            app.start_rename_alias(&alias);
        }
        MenuAction::RefreshOne(alias) => {
            app.close_menu();
            app.refresh_one(&alias);
        }
        MenuAction::WarmupOne(alias) => {
            app.close_menu();
            app.warmup_one(&alias);
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
    ratatui::restore();

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

    // Wait briefly so user can read the result line before TUI repaints.
    if result.is_ok() {
        println!("\nReturning to TUI...");
    } else {
        println!("\nPress Enter to return to TUI...");
        let _ = tokio::task::spawn_blocking(|| {
            let mut buf = String::new();
            let _ = std::io::stdin().read_line(&mut buf);
        })
        .await;
    }

    // Restore TUI.
    *terminal = ratatui::init();

    match result {
        Ok(msg) => {
            app.set_status(msg, 5);
            app.load_profiles_preserving_selection();
            app.refresh(true);
            // Reset auto-refresh timer so it doesn't fire immediately.
            if app.auto_refresh_enabled {
                app.next_auto_refresh = Some(Instant::now() + app.auto_refresh_interval);
            }
        }
        Err(e) => {
            app.set_status(format!("OAuth failed: {e}"), 7);
        }
    }
}

/// Sequentially re-login every marked alias. The TUI is suspended for the
/// duration; OAuth output goes to the cooked terminal so the user sees
/// browser prompts / device codes / progress.
///
/// User can abort the whole batch with Ctrl+C between rounds (handled by
/// the underlying login::run_device_*) or by closing the browser tab.
async fn perform_batch_relogin(terminal: &mut DefaultTerminal, app: &mut App, device: bool) {
    let aliases: Vec<String> = app.marked.iter().cloned().collect();
    if aliases.is_empty() {
        return;
    }

    ratatui::restore();

    let total = aliases.len();
    println!("\n=== Batch re-login: {total} account(s) ===");
    if device {
        println!("Flow: device code\n");
    } else {
        println!("Flow: browser (PKCE)\n");
    }

    let mut ok = 0usize;
    let mut failed: Vec<(String, String)> = Vec::new();

    for (i, alias) in aliases.iter().enumerate() {
        println!("\n--- [{}/{}] {alias} ---", i + 1, total);
        let mode = OAuthMode::Relogin(alias.clone());
        match run_oauth_inner(mode, device).await {
            Ok(_) => ok += 1,
            Err(e) => {
                eprintln!("[err] {alias}: {e}");
                failed.push((alias.clone(), e.to_string()));
            }
        }
    }

    println!(
        "\n=== Batch complete: {ok} ok, {} failed ===",
        failed.len()
    );
    if !failed.is_empty() {
        for (a, e) in &failed {
            println!("  - {a}: {e}");
        }
    }
    println!("\nPress Enter to return to TUI...");
    let _ = tokio::task::spawn_blocking(|| {
        let mut buf = String::new();
        let _ = std::io::stdin().read_line(&mut buf);
    })
    .await;

    *terminal = ratatui::init();

    app.marked.clear();
    let summary = if failed.is_empty() {
        format!("Batch re-login: {ok} ok")
    } else {
        format!("Batch re-login: {ok} ok, {} failed", failed.len())
    };
    app.set_status(summary, 8);
    app.load_profiles_preserving_selection();
    app.refresh(true);
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
            let action = profile::save_auth_value(auth_val, None)?;
            let alias = action.alias().to_string();
            let verb = action.action(); // "created" / "updated"
            let email_disp = info.email.as_deref().unwrap_or("unknown");
            println!("[ok] Account {verb}: {alias} ({email_disp})");
            Ok(format!("Account {verb}: {alias}"))
        }
        OAuthMode::Relogin(alias) => {
            let dst = profile::profile_auth_path(&alias)?;
            auth::write_auth(&dst, &auth_val)?;
            // If this profile is currently active, also refresh the live auth.json.
            if profile::read_current() == alias {
                let live = auth::codex_auth_path()?;
                let _ = auth::backup_auth(&live);
                auth::write_auth(&live, &auth_val)?;
            }
            let email_disp = info.email.as_deref().unwrap_or("unknown");
            println!("[ok] Re-logged in: {alias} ({email_disp})");
            Ok(format!("Re-logged in: {alias}"))
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
