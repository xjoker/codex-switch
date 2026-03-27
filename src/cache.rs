use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::auth;
use crate::usage::UsageInfo;

static CACHE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    ts: u64,
    primary_used: Option<f64>,
    primary_reset: Option<i64>,
    secondary_used: Option<f64>,
    secondary_reset: Option<i64>,
    #[serde(default)]
    credits_balance: Option<f64>,
    #[serde(default)]
    unlimited_credits: Option<bool>,
}

#[derive(Serialize, Deserialize, Default)]
struct CacheFile {
    entries: HashMap<String, CacheEntry>,
}

fn cache_path() -> PathBuf {
    auth::app_home().join("cache.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ttl() -> u64 {
    crate::config::get().cache.ttl
}

fn load_cache() -> CacheFile {
    let path = cache_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cache(cache: &CacheFile) -> Result<()> {
    let path = cache_path();
    let json = serde_json::to_string(cache).context("serializing cache")?;

    // Atomic write: write to temp file then rename.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)
        .with_context(|| format!("writing cache temp file {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| {
        format!(
            "renaming cache temp file {} -> {}",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(())
}

fn to_entry(u: &UsageInfo) -> CacheEntry {
    CacheEntry {
        ts: now_secs(),
        primary_used: u.primary.as_ref().and_then(|w| w.used_percent),
        primary_reset: u.primary.as_ref().and_then(|w| w.resets_at),
        secondary_used: u.secondary.as_ref().and_then(|w| w.used_percent),
        secondary_reset: u.secondary.as_ref().and_then(|w| w.resets_at),
        credits_balance: u.credits_balance,
        unlimited_credits: u.unlimited_credits,
    }
}

fn from_entry(e: &CacheEntry) -> UsageInfo {
    use crate::usage::WindowUsage;
    let primary = if e.primary_used.is_some() || e.primary_reset.is_some() {
        Some(WindowUsage {
            used_percent: e.primary_used,
            resets_at: e.primary_reset,
        })
    } else {
        None
    };
    let secondary = if e.secondary_used.is_some() || e.secondary_reset.is_some() {
        Some(WindowUsage {
            used_percent: e.secondary_used,
            resets_at: e.secondary_reset,
        })
    } else {
        None
    };
    UsageInfo {
        fetched_at: Some(e.ts as i64),
        primary,
        secondary,
        credits_balance: e.credits_balance,
        unlimited_credits: e.unlimited_credits,
    }
}

/// Get cached usage for an alias if within TTL.
pub fn get(alias: &str) -> Option<UsageInfo> {
    let _lock = CACHE_LOCK.lock().ok()?;
    let cache = load_cache();
    let entry = cache.entries.get(alias)?;
    if now_secs() - entry.ts > ttl() {
        return None;
    }
    Some(from_entry(entry))
}

/// Store usage result in cache.
pub fn put(alias: &str, usage: &UsageInfo) {
    let _lock = CACHE_LOCK.lock().ok();
    let mut cache = load_cache();
    cache.entries.insert(alias.to_string(), to_entry(usage));
    if let Err(err) = save_cache(&cache) {
        tracing::warn!("Failed to write cache: {err}");
    }
}

pub fn rename(old: &str, new: &str) -> Result<()> {
    let _lock = CACHE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("cache lock poisoned"))?;
    let mut cache = load_cache();
    let Some(entry) = cache.entries.remove(old) else {
        return Ok(());
    };
    cache.entries.insert(new.to_string(), entry);
    save_cache(&cache)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_cache_entry_deserialize_without_credits() {
        let entry: CacheEntry = serde_json::from_value(json!({
            "ts": 123,
            "primary_used": 25.0,
            "primary_reset": 456,
            "secondary_used": 75.0,
            "secondary_reset": 789
        }))
        .unwrap();

        assert_eq!(entry.credits_balance, None);
        assert_eq!(entry.unlimited_credits, None);

        let usage = from_entry(&entry);
        assert_eq!(usage.credits_balance, None);
        assert_eq!(usage.unlimited_credits, None);
    }
}
