use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::DefaultTerminal;
use tokio::sync::Semaphore;

use crate::auth;
use crate::jwt::AccountInfo;
use crate::profile::{
    cmd_delete, list_profiles, profile_auth_path, read_current, rename_profile, switch_profile,
    validate_alias,
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
}

impl App {
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        let (warmup_tx, warmup_rx) = tokio::sync::mpsc::channel(64);
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
            usage_limiter: Arc::new(Semaphore::new(crate::config::get().network.max_concurrent)),
        }
    }

    pub fn load_profiles(&mut self) {
        let profiles = list_profiles().unwrap_or_default();
        let current = read_current();
        self.accounts = profiles
            .into_iter()
            .map(|alias| {
                let path = profile_auth_path(&alias);
                let info = auth::read_account_info(&path);
                let is_current = alias == current;
                AccountEntry {
                    alias,
                    info,
                    usage: UsageStatus::Idle,
                    is_current,
                }
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

    pub fn batch_refresh(&mut self) {
        if self.marked.is_empty() {
            self.set_status("No accounts marked (use Space to mark)".to_string(), 3);
            return;
        }

        let aliases: Vec<String> = self.marked.iter().cloned().collect();
        let count = aliases.len();
        for alias in &aliases {
            if let Some(idx) = self.accounts.iter().position(|a| &a.alias == alias) {
                self.accounts[idx].usage = UsageStatus::Idle;
                self.fetch_usage_for(idx, true);
            }
        }
        self.update_view();
        self.set_status(format!("Refreshing {count} marked account(s)..."), 3);
    }

    pub fn warmup_selected(&mut self) {
        let entry = match self
            .selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
        {
            Some(e) => e,
            None => return,
        };
        let alias = entry.alias.clone();
        self.spawn_warmup(alias.clone());
        self.set_status(format!("Warming up {alias}..."), 10);
    }

    pub fn warmup_all(&mut self) {
        let aliases: Vec<String> = self.accounts.iter().map(|a| a.alias.clone()).collect();
        if aliases.is_empty() {
            return;
        }
        let count = aliases.len();
        for alias in aliases {
            self.spawn_warmup(alias);
        }
        self.set_status(format!("Warming up {count} account(s)..."), 10);
    }

    fn spawn_warmup(&mut self, alias: String) {
        // Skip if this alias already has an in-flight warmup task.
        if self.warmup_tasks.values().any(|(a, _)| *a == alias) {
            return;
        }
        let task_id = self.warmup_next_id;
        self.warmup_next_id += 1;
        self.warmup_tasks
            .insert(task_id, (alias.clone(), Instant::now()));
        let path = profile_auth_path(&alias);
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
        let path = profile_auth_path(&alias);
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

    /// Fetch all accounts. If `force` is true, bypass disk cache.
    pub fn refresh_all(&mut self, force: bool) {
        for entry in &mut self.accounts {
            match &entry.usage {
                UsageStatus::Error(_) => entry.usage = UsageStatus::Idle,
                UsageStatus::Loaded(_) if force => entry.usage = UsageStatus::Idle,
                _ => {}
            }

            if !force && let Some(cached) = crate::cache::get(&entry.alias) {
                entry.usage = UsageStatus::Loaded(cached);
            }
        }
        let count = self.accounts.len();
        for i in 0..count {
            self.fetch_usage_for(i, force);
        }
        self.update_view();
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

    pub fn request_delete(&mut self) {
        if let Some(entry) = self
            .selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
        {
            if entry.is_current {
                self.set_status("Cannot delete the active profile".to_string(), 3);
                return;
            }
            self.confirm = Some(ConfirmAction::Delete(entry.alias.clone()));
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
                    self.refresh_all(true);
                }
                Err(e) => self.set_status(format!("Delete failed: {e}"), 5),
            },
        }
    }

    pub fn cancel_confirm(&mut self) {
        self.confirm = None;
    }

    pub fn start_rename(&mut self) {
        if let Some(entry) = self
            .selected_account_idx()
            .and_then(|idx| self.accounts.get(idx))
        {
            let old = entry.alias.clone();
            let len = old.len();
            self.rename = Some(RenameState {
                old_alias: old.clone(),
                input: old,
                cursor: len,
            });
        }
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
                        self.refresh_all(true);
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
    crate::profile::auto_track_current();

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

    app.refresh_all(false);

    loop {
        app.poll_results();
        app.poll_warmup_results();
        app.tick();

        terminal
            .draw(|f| super::ui::render(f, &app))
            .context("drawing TUI")?;

        if event::poll(Duration::from_millis(100)).context("polling terminal events")?
            && let Event::Key(key) = event::read().context("reading terminal event")?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            if app.rename.is_some() {
                app.handle_rename_key(key.code);
                continue;
            }

            if app.confirm.is_some() {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => app.confirm_action(),
                    _ => app.cancel_confirm(),
                }
                continue;
            }

            if app.search_active {
                app.handle_search_key(key.code);
                continue;
            }

            match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Esc => {
                    if app.search.is_some() {
                        app.search = None;
                        app.update_view();
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
                KeyCode::Enter => app.switch_selected(),
                KeyCode::Char('r') => app.refresh_all(true),
                KeyCode::Char('b') => app.batch_refresh(),
                KeyCode::Char('d') => app.request_delete(),
                KeyCode::Char('n') => app.start_rename(),
                KeyCode::Char('s') => app.cycle_sort(),
                KeyCode::Char('c') => app.clear_marks(),
                KeyCode::Char('w') => app.warmup_selected(),
                KeyCode::Char('W') => app.warmup_all(),
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

/// Convert a char-based cursor position to a byte offset in a string.
fn char_to_byte(s: &str, char_pos: usize) -> usize {
    s.char_indices()
        .nth(char_pos)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(s.len())
}
