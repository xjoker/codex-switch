use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::DefaultTerminal;

use crate::auth;
use crate::jwt::AccountInfo;
use crate::profile::{
    cmd_delete, list_profiles, profile_auth_path, read_current, rename_profile, switch_profile,
};
use crate::usage::{fetch_usage_retried, UsageInfo};

const CACHE_TTL_SECS: u64 = 60;

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
    Loaded(UsageInfo, Instant),
    Error(String),
}

pub enum ConfirmAction {
    Delete(String),
}

pub struct RenameState {
    pub old_alias: String,
    pub input: String,
    pub cursor: usize,
}

pub struct App {
    pub accounts: Vec<AccountEntry>,
    pub selected: usize,
    pub status_msg: Option<String>,
    pub status_expiry: Option<Instant>,
    pub pending_results: tokio::sync::mpsc::Receiver<(usize, Result<UsageInfo, String>)>,
    pub result_sender: tokio::sync::mpsc::Sender<(usize, Result<UsageInfo, String>)>,
    pub confirm: Option<ConfirmAction>,
    pub rename: Option<RenameState>,
}

impl App {
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        App {
            accounts: vec![],
            selected: 0,
            status_msg: None,
            status_expiry: None,
            pending_results: rx,
            result_sender: tx,
            confirm: None,
            rename: None,
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
                AccountEntry { alias, info, usage: UsageStatus::Idle, is_current }
            })
            .collect();
        if let Some(idx) = self.accounts.iter().position(|a| a.is_current) {
            self.selected = idx;
        }
    }

    pub fn loading_count(&self) -> usize {
        self.accounts.iter().filter(|a| matches!(a.usage, UsageStatus::Loading)).count()
    }

    pub fn fetch_usage_for(&mut self, idx: usize) {
        let entry = match self.accounts.get(idx) {
            Some(e) => e,
            None => return,
        };
        if matches!(entry.usage, UsageStatus::Loading) {
            return;
        }
        if let UsageStatus::Loaded(_, fetched_at) = &entry.usage {
            if fetched_at.elapsed().as_secs() < CACHE_TTL_SECS {
                return;
            }
        }

        let alias = entry.alias.clone();
        let path = profile_auth_path(&alias);
        let current = read_current();

        self.accounts[idx].usage = UsageStatus::Loading;

        let tx = self.result_sender.clone();
        tokio::spawn(async move {
            let result = fetch_usage_retried(&alias, &path, &current).await;
            let _ = tx.send((idx, result)).await;
        });
    }

    pub fn refresh_all(&mut self) {
        for entry in &mut self.accounts {
            match &entry.usage {
                UsageStatus::Error(_) | UsageStatus::Loaded(_, _) => {
                    entry.usage = UsageStatus::Idle;
                }
                _ => {}
            }
        }
        let count = self.accounts.len();
        for i in 0..count {
            self.fetch_usage_for(i);
        }
    }

    pub fn poll_results(&mut self) {
        while let Ok((idx, result)) = self.pending_results.try_recv() {
            if let Some(entry) = self.accounts.get_mut(idx) {
                entry.usage = match result {
                    Ok(u) => UsageStatus::Loaded(u, Instant::now()),
                    Err(e) => UsageStatus::Error(e),
                };
            }
        }
    }

    pub fn switch_selected(&mut self) {
        if let Some(entry) = self.accounts.get(self.selected) {
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
        if let Some(entry) = self.accounts.get(self.selected) {
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
                    if self.selected >= self.accounts.len() && !self.accounts.is_empty() {
                        self.selected = self.accounts.len() - 1;
                    }
                    self.refresh_all();
                }
                Err(e) => self.set_status(format!("Delete failed: {e}"), 5),
            },
        }
    }

    pub fn cancel_confirm(&mut self) {
        self.confirm = None;
    }

    pub fn start_rename(&mut self) {
        if let Some(entry) = self.accounts.get(self.selected) {
            let old = entry.alias.clone();
            let len = old.len();
            self.rename = Some(RenameState { old_alias: old.clone(), input: old, cursor: len });
        }
    }

    pub fn handle_rename_key(&mut self, code: KeyCode) -> bool {
        let state = match &mut self.rename {
            Some(s) => s,
            None => return false,
        };
        match code {
            KeyCode::Esc => { self.rename = None; return false; }
            KeyCode::Enter => {
                let old = state.old_alias.clone();
                let new = state.input.trim().to_string();
                self.rename = None;
                if new.is_empty() || new == old { return false; }
                if !new.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
                    self.set_status("Invalid alias (use letters, digits, - or _)".to_string(), 3);
                    return false;
                }
                match rename_profile(&old, &new) {
                    Ok(()) => {
                        self.set_status(format!("Renamed {old} → {new}"), 3);
                        self.load_profiles();
                        if let Some(idx) = self.accounts.iter().position(|a| a.alias == new) {
                            self.selected = idx;
                        }
                        self.refresh_all();
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
            KeyCode::Left => { if state.cursor > 0 { state.cursor -= 1; } }
            KeyCode::Right => {
                let char_count = state.input.chars().count();
                if state.cursor < char_count { state.cursor += 1; }
            }
            KeyCode::Home => { state.cursor = 0; }
            KeyCode::End => { state.cursor = state.input.chars().count(); }
            KeyCode::Char(c) => {
                let byte_pos = char_to_byte(&state.input, state.cursor);
                state.input.insert(byte_pos, c);
                state.cursor += 1;
            }
            _ => {}
        }
        true
    }

    fn set_status(&mut self, msg: String, secs: u64) {
        self.status_msg = Some(msg);
        self.status_expiry = Some(Instant::now() + Duration::from_secs(secs));
    }

    pub fn tick(&mut self) {
        if let Some(expiry) = self.status_expiry {
            if Instant::now() >= expiry {
                self.status_msg = None;
                self.status_expiry = None;
            }
        }
    }
}

pub async fn run() -> Result<()> {
    let mut terminal = ratatui::init();
    let result = run_app(&mut terminal).await;
    ratatui::restore();
    result
}

async fn run_app(terminal: &mut DefaultTerminal) -> Result<()> {
    crate::profile::auto_track_current();

    let mut app = App::new();
    app.load_profiles();

    if app.accounts.is_empty() {
        ratatui::restore();
        println!("No saved profiles. Run `codex-switch login` to add an account first.");
        return Ok(());
    }

    app.refresh_all();

    loop {
        app.poll_results();
        app.tick();

        terminal.draw(|f| super::ui::render(f, &app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press { continue; }

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

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Down | KeyCode::Char('j') => {
                        if app.selected + 1 < app.accounts.len() { app.selected += 1; }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        if app.selected > 0 { app.selected -= 1; }
                    }
                    KeyCode::Enter => app.switch_selected(),
                    KeyCode::Char('r') => app.refresh_all(),
                    KeyCode::Char('d') => app.request_delete(),
                    KeyCode::Char('n') => app.start_rename(),
                    _ => {}
                }
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
