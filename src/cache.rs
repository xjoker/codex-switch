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
    /// Tracks the last time each profile was selected by `use` (unix seconds).
    #[serde(default)]
    last_used: HashMap<String, i64>,
    /// Tracks the last successful warmup time per profile (unix seconds).
    #[serde(default)]
    warmed_at: HashMap<String, i64>,
}

fn cache_path() -> Result<PathBuf> {
    Ok(auth::app_home()?.join("cache.json"))
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
    let path = match cache_path() {
        Ok(p) => p,
        Err(_) => return CacheFile::default(),
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cache(cache: &CacheFile) -> Result<()> {
    let path = cache_path()?;
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
    let _lock = match CACHE_LOCK.lock() {
        Ok(g) => g,
        Err(_) => {
            tracing::warn!("cache lock poisoned in get()");
            return None;
        }
    };
    let cache = load_cache();
    let entry = cache.entries.get(alias)?;
    if now_secs() - entry.ts > ttl() {
        return None;
    }
    Some(from_entry(entry))
}

/// Store usage result in cache.
pub fn put(alias: &str, usage: &UsageInfo) {
    let _lock = match CACHE_LOCK.lock() {
        Ok(g) => g,
        Err(_) => {
            tracing::warn!("cache lock poisoned in put()");
            return;
        }
    };
    let mut cache = load_cache();
    cache.entries.insert(alias.to_string(), to_entry(usage));
    if let Err(err) = save_cache(&cache) {
        tracing::warn!("Failed to write cache: {err}");
    }
}

/// Get the last-used timestamp for an alias (0 if never used).
pub fn get_last_used(alias: &str) -> i64 {
    let _lock = match CACHE_LOCK.lock() {
        Ok(g) => g,
        Err(_) => {
            tracing::warn!("cache lock poisoned in get_last_used()");
            return 0;
        }
    };
    let cache = load_cache();
    cache.last_used.get(alias).copied().unwrap_or(0)
}

/// Record that an alias was just selected by `use`.
pub fn set_last_used(alias: &str) -> Result<()> {
    let _lock = CACHE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("cache lock poisoned"))?;
    let mut cache = load_cache();
    cache
        .last_used
        .insert(alias.to_string(), crate::auth::now_unix_secs());
    save_cache(&cache).context("writing last_used cache")?;
    Ok(())
}

const WARMUP_TTL_SECS: i64 = 3600;

/// Record that an alias was just successfully warmed up.
pub fn set_warmed(alias: &str) {
    let _lock = match CACHE_LOCK.lock() {
        Ok(g) => g,
        Err(_) => {
            tracing::warn!("cache lock poisoned in set_warmed()");
            return;
        }
    };
    let mut cache = load_cache();
    cache
        .warmed_at
        .insert(alias.to_string(), crate::auth::now_unix_secs());
    if let Err(err) = save_cache(&cache) {
        tracing::warn!("Failed to write warmup cache: {err}");
    }
}

/// Returns true if this alias was successfully warmed up within the last hour.
pub fn is_warmed(alias: &str) -> bool {
    let _lock = match CACHE_LOCK.lock() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let cache = load_cache();
    cache
        .warmed_at
        .get(alias)
        .is_some_and(|&t| crate::auth::now_unix_secs() - t < WARMUP_TTL_SECS)
}

pub fn rename(old: &str, new: &str) -> Result<()> {
    let _lock = CACHE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("cache lock poisoned"))?;
    let mut cache = load_cache();
    // Migrate entries and last_used independently — either may exist without the other.
    let mut changed = false;
    if let Some(entry) = cache.entries.remove(old) {
        cache.entries.insert(new.to_string(), entry);
        changed = true;
    }
    if let Some(ts) = cache.last_used.remove(old) {
        cache.last_used.insert(new.to_string(), ts);
        changed = true;
    }
    if changed {
        save_cache(&cache)?;
    }
    Ok(())
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
