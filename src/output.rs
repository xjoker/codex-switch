use std::io::{self, IsTerminal, Write};
use std::sync::OnceLock;

use chrono::{DateTime, Local, TimeZone, Utc};
use serde::Serialize;

use crate::jwt::AccountInfo;
use crate::usage::{UsageInfo, WindowUsage};

// ── JSON types ───────────────────────────────────────────

#[derive(Serialize)]
pub struct JsonAccount {
    pub email: Option<String>,
    pub plan: Option<String>,
    pub account_id: Option<String>,
}

#[derive(Serialize)]
pub struct JsonWindow {
    pub label: String,
    pub used_percent: f64,
    pub resets_at: Option<i64>,
    pub resets_in_seconds: Option<i64>,
    pub remaining_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pace_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub over_pace: Option<bool>,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum JsonUsage {
    Ok {
        fetched_at: String,
        primary: Option<Box<JsonWindow>>,
        secondary: Option<Box<JsonWindow>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        credits_balance: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        unlimited_credits: Option<bool>,
    },
    Err {
        error: String,
    },
}

#[derive(Serialize)]
pub struct JsonProfileWithUsage {
    pub alias: String,
    pub is_current: bool,
    pub account: JsonAccount,
    pub usage: JsonUsage,
}

#[derive(Serialize)]
pub struct JsonUsageResult {
    pub profiles: Vec<JsonProfileWithUsage>,
}

#[derive(Serialize)]
pub struct JsonBest {
    pub switched_to: String,
    pub account: JsonAccount,
    pub usage: JsonUsage,
    pub score: f64,
    pub mode: String,
}

#[derive(Serialize)]
pub struct JsonOk {
    pub ok: bool,
    pub alias: String,
    pub action: String,
}

#[derive(Serialize)]
pub struct JsonError {
    pub ok: bool,
    pub error: String,
}

#[derive(Serialize)]
pub struct JsonImportEntry {
    pub source: String,
    pub alias: String,
    pub action: String,
    pub account: JsonAccount,
    pub usage: JsonUsage,
}

#[derive(Serialize)]
pub struct JsonImportFailure {
    pub source: String,
    pub stage: String,
    pub error: String,
}

#[derive(Serialize)]
pub struct JsonImportReport {
    pub imported: Vec<JsonImportEntry>,
    pub skipped: Vec<JsonImportFailure>,
}

#[derive(Serialize)]
pub struct JsonSelfUpdate {
    pub ok: bool,
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub updated: bool,
    pub install_source: String,
    pub action: String,
}

// ── Conversion helpers ───────────────────────────────────

pub fn account_to_json(info: &AccountInfo) -> JsonAccount {
    JsonAccount {
        email: info.email.clone(),
        plan: info.plan_type.clone(),
        account_id: info.account_id.clone(),
    }
}

fn window_to_json(w: &WindowUsage, label: &str, window_secs: i64) -> JsonWindow {
    let resets_in_seconds = w.resets_at.map(|ts| ts - crate::auth::now_unix_secs());
    let used = w.used_percent.unwrap_or(0.0);
    let pace = crate::usage::pace_percent(w, window_secs);
    JsonWindow {
        label: label.to_string(),
        used_percent: used,
        resets_at: w.resets_at,
        resets_in_seconds,
        remaining_percent: (100.0 - used).max(0.0),
        pace_percent: pace,
        over_pace: pace.map(|p| used > p),
    }
}

pub fn usage_to_json(result: Result<&UsageInfo, &str>) -> JsonUsage {
    match result {
        Err(e) => JsonUsage::Err {
            error: e.to_string(),
        },
        Ok(u) => {
            let fetched_at = u
                .fetched_at
                .map(format_iso8601)
                .unwrap_or_else(|| Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string());
            JsonUsage::Ok {
                fetched_at,
                primary: u
                    .primary
                    .as_ref()
                    .map(|w| Box::new(window_to_json(w, "5h", crate::usage::WINDOW_5H_SECS))),
                secondary: u
                    .secondary
                    .as_ref()
                    .map(|w| Box::new(window_to_json(w, "7d", crate::usage::WINDOW_7D_SECS))),
                credits_balance: u.credits_balance,
                unlimited_credits: u.unlimited_credits,
            }
        }
    }
}

pub fn format_iso8601(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

/// Shared timestamp formatter: "2h30m (14:30)" or "1d12h (03-27 14:30)"
pub fn format_reset_time(ts: i64) -> String {
    let now = Local::now();
    let dt: DateTime<Local> = match Local.timestamp_opt(ts, 0).single() {
        Some(d) => d,
        None => return "--".into(),
    };
    if dt <= now {
        return "expired".into();
    }
    let secs = (dt - now).num_seconds().max(0) as u64;
    let relative = if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86400, (secs % 86400) / 3600)
    };
    let local_fmt = if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M").to_string()
    } else {
        dt.format("%m-%d %H:%M").to_string()
    };
    format!("{relative} ({local_fmt})")
}

/// Short reset time for table columns: "14:30" or "03-27 14:30"
pub fn format_reset_short(ts: i64) -> String {
    let now = Local::now();
    let dt: DateTime<Local> = match Local.timestamp_opt(ts, 0).single() {
        Some(d) => d,
        None => return "--".into(),
    };
    if dt <= now {
        return "reset".into();
    }
    if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M").to_string()
    } else {
        dt.format("%m-%d %H:%M").to_string()
    }
}

// ── Output ───────────────────────────────────────────────

static JSON_PRETTY: OnceLock<bool> = OnceLock::new();
static MESSAGE_MODE: OnceLock<MessageMode> = OnceLock::new();

#[derive(Debug, Clone, Copy)]
pub enum MessageMode {
    Stdout,
    Stderr,
    Silent,
}

/// Set JSON output mode. Call once at startup.
pub fn set_json_pretty(pretty: bool) {
    let _ = JSON_PRETTY.set(pretty);
}

pub fn set_message_mode(mode: MessageMode) {
    let _ = MESSAGE_MODE.set(mode);
}

fn is_pretty() -> bool {
    *JSON_PRETTY.get().unwrap_or(&false)
}

fn message_mode() -> MessageMode {
    *MESSAGE_MODE.get().unwrap_or(&MessageMode::Stdout)
}

fn serialize<T: serde::Serialize>(val: &T) -> String {
    if is_pretty() {
        serde_json::to_string_pretty(val)
    } else {
        serde_json::to_string(val)
    }
    .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

pub fn print_json<T: serde::Serialize>(val: &T) {
    println!("{}", serialize(val));
}

pub fn print_error(msg: &str) {
    let e = JsonError {
        ok: false,
        error: msg.to_string(),
    };
    println!("{}", serialize(&e));
}

pub fn user_print(msg: &str) {
    match message_mode() {
        MessageMode::Stdout => {
            print!("{msg}");
            let _ = io::stdout().flush();
        }
        MessageMode::Stderr => {
            eprint!("{msg}");
            let _ = io::stderr().flush();
        }
        MessageMode::Silent => {}
    }
}

pub fn user_println(msg: &str) {
    match message_mode() {
        MessageMode::Stdout => println!("{msg}"),
        MessageMode::Stderr => eprintln!("{msg}"),
        MessageMode::Silent => {}
    }
}

pub struct ProgressReporter {
    enabled: bool,
    label: String,
    total: usize,
    last_width: usize,
}

impl ProgressReporter {
    pub fn new(label: &str, total: usize) -> Self {
        let enabled = progress_enabled() && total > 0;
        let mut reporter = Self {
            enabled,
            label: label.to_string(),
            total,
            last_width: 0,
        };
        if reporter.enabled {
            reporter.advance(0);
        }
        reporter
    }

    pub fn advance(&mut self, completed: usize) {
        if !self.enabled {
            return;
        }

        let line = render_progress_line(&self.label, completed.min(self.total), self.total);
        self.last_width = line.chars().count();
        eprint!("\r{line}");
        let _ = io::stderr().flush();
    }

    pub fn finish(&mut self) {
        if !self.enabled {
            return;
        }

        eprint!("\r{}\r", " ".repeat(self.last_width.max(1)));
        let _ = io::stderr().flush();
        self.enabled = false;
    }
}

impl Drop for ProgressReporter {
    fn drop(&mut self) {
        self.finish();
    }
}

pub fn render_progress_line(label: &str, completed: usize, total: usize) -> String {
    let total = total.max(1);
    let completed = completed.min(total);
    let width = 24usize;
    let filled = completed.saturating_mul(width) / total;
    let bar = format!(
        "{}{}",
        "#".repeat(filled),
        "-".repeat(width.saturating_sub(filled))
    );
    format!("{label} [{bar}] {completed}/{total}")
}

fn progress_enabled() -> bool {
    if matches!(message_mode(), MessageMode::Silent) {
        return false;
    }
    if std::env::var("CS_PROGRESS_FORCE").ok().as_deref() == Some("1") {
        return true;
    }
    io::stderr().is_terminal()
}
