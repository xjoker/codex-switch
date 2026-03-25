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
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum JsonUsage {
    Ok {
        fetched_at: String,
        primary: Option<JsonWindow>,
        secondary: Option<JsonWindow>,
    },
    Err {
        error: String,
    },
}

#[derive(Serialize)]
pub struct JsonProfile {
    pub alias: String,
    pub is_current: bool,
    pub account: JsonAccount,
}

#[derive(Serialize)]
pub struct JsonProfileWithUsage {
    pub alias: String,
    pub is_current: bool,
    pub account: JsonAccount,
    pub usage: JsonUsage,
}

#[derive(Serialize)]
pub struct JsonList {
    pub current: Option<String>,
    pub count: usize,
    pub profiles: Vec<JsonProfile>,
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

// ── Conversion helpers ───────────────────────────────────

pub fn account_to_json(info: &AccountInfo) -> JsonAccount {
    JsonAccount {
        email: info.email.clone(),
        plan: info.plan_type.clone(),
        account_id: info.account_id.clone(),
    }
}

fn window_to_json(w: &WindowUsage, label: &str) -> JsonWindow {
    let resets_in_seconds = w.resets_at.map(|ts| ts - crate::auth::now_unix_secs());
    JsonWindow {
        label: label.to_string(),
        used_percent: w.used_percent.unwrap_or(0.0),
        resets_at: w.resets_at,
        resets_in_seconds,
    }
}

pub fn usage_to_json(result: Result<&UsageInfo, &str>) -> JsonUsage {
    match result {
        Err(e) => JsonUsage::Err {
            error: e.to_string(),
        },
        Ok(u) => {
            let now_iso = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
            JsonUsage::Ok {
                fetched_at: now_iso,
                primary: u.primary.as_ref().map(|w| window_to_json(w, "5h")),
                secondary: u.secondary.as_ref().map(|w| window_to_json(w, "7d")),
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
        None => return "—".into(),
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
        None => return "—".into(),
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

pub fn print_json<T: serde::Serialize>(val: &T) {
    println!(
        "{}",
        serde_json::to_string_pretty(val)
            .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}")),
    );
}

pub fn print_error(msg: &str) {
    let e = JsonError {
        ok: false,
        error: msg.to_string(),
    };
    println!("{}", serde_json::to_string_pretty(&e).unwrap());
}
