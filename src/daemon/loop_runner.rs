use anyhow::Result;

use super::state::{self, DaemonState, PendingSwitch, SwitchRecord};
use crate::signals::ShutdownListener;
use crate::warmup_schedule::{self, warmup_on_cache_refresh};
use crate::{auth, cache, config, profile, usage, warmup};

async fn shutdown_request_received() {
    #[cfg(target_os = "windows")]
    {
        loop {
            if super::pidfile::shutdown_requested() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    #[cfg(not(target_os = "windows"))]
    std::future::pending::<()>().await;
}

/// Outcome of one monitor poll.
enum PollOutcome {
    NoAction,
    Switched {
        from: String,
        to: String,
        score: f64,
    },
    Deferred {
        to: String,
    },
}

/// Backoff after `consecutive_failures` failed polls, capped at 16 poll intervals.
fn poll_backoff_secs(poll_secs: u64, consecutive_failures: u32) -> u64 {
    poll_secs * 2u64.pow(consecutive_failures.min(4))
}

fn current_usage_percent_for_switch(current_usage: &usage::UsageInfo) -> f64 {
    if current_usage.account_limited {
        return 100.0;
    }

    current_usage
        .primary
        .as_ref()
        .or(current_usage.secondary.as_ref())
        .and_then(|w| w.used_percent)
        .unwrap_or(0.0)
}

/// Main daemon event loop: periodically checks usage and switches account when needed.
pub async fn run_daemon_loop() -> Result<()> {
    // Registered before anything else can block: from here on every signal is
    // recorded, even while a branch body is busy.
    let mut shutdown = ShutdownListener::new()?;

    let cfg = config::get();
    let mut poll_secs = cfg.daemon.poll_interval_secs.max(1);
    let mut cache_refresh_secs = cfg.daemon.cache_refresh_interval_secs.max(1);

    // First poll fires immediately so a freshly started daemon checks soon;
    // cache refresh waits one full period so startup does not hammer the API.
    let mut poll_interval = tokio::time::interval(std::time::Duration::from_secs(poll_secs));
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut cache_refresh_interval = delayed_interval(cache_refresh_secs);
    let mut warmup_interval = tokio::time::interval(std::time::Duration::from_secs(60));
    warmup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let previous = state::read();
    let mut st = DaemonState {
        pid: std::process::id(),
        started_at: auth::now_unix_secs(),
        last_warmup_slot: previous.and_then(|snap| snap.last_warmup_slot),
        ..DaemonState::default()
    };
    state::write(&mut st);

    tracing::info!(
        "Daemon loop started: poll={}s, cache_refresh={}s, auto_warmup={}, warmup_times={:?}, timezone={}, threshold={}%",
        poll_secs,
        cache_refresh_secs,
        cfg.daemon.auto_warmup,
        cfg.daemon.warmup_times,
        warmup_schedule::timezone_label(&cfg.daemon.timezone),
        cfg.daemon.switch_threshold,
    );

    loop {
        tokio::select! {
            _ = poll_interval.tick() => {
                sync_runtime_timers(
                    &mut poll_secs,
                    &mut cache_refresh_secs,
                    &mut poll_interval,
                    &mut cache_refresh_interval,
                );
                // Failure backoff suspends polling only; cache refresh and
                // scheduled warmup keep running.
                let now = auth::now_unix_secs();
                if let Some(until) = st.backoff_until {
                    if now < until {
                        tracing::debug!("Poll suspended by backoff for {}s more", until - now);
                        continue;
                    }
                    st.backoff_until = None;
                }

                match check_and_switch().await {
                    Ok(outcome) => {
                        st.consecutive_failures = 0;
                        st.last_error = None;
                        st.last_poll_at = Some(auth::now_unix_secs());
                        match outcome {
                            PollOutcome::Switched { from, to, score } => {
                                tracing::info!("Account switch completed");
                                st.pending_switch = None;
                                st.last_switch = Some(SwitchRecord {
                                    from,
                                    to,
                                    at: auth::now_unix_secs(),
                                    score,
                                });
                            }
                            PollOutcome::Deferred { to } => {
                                // Keep the original `since` while the same target stays pending.
                                let since = st
                                    .pending_switch
                                    .as_ref()
                                    .filter(|p| p.to == to)
                                    .map(|p| p.since)
                                    .unwrap_or_else(auth::now_unix_secs);
                                st.pending_switch = Some(PendingSwitch { to, since });
                            }
                            PollOutcome::NoAction => {
                                st.pending_switch = None;
                            }
                        }
                    }
                    Err(e) => {
                        st.consecutive_failures += 1;
                        st.last_poll_at = Some(auth::now_unix_secs());
                        st.last_error = Some(e.to_string());
                        let backoff_secs = poll_backoff_secs(poll_secs, st.consecutive_failures);
                        st.backoff_until = Some(auth::now_unix_secs() + backoff_secs as i64);
                        tracing::error!(
                            "Monitor cycle failed ({}x): {e}, backing off {backoff_secs}s",
                            st.consecutive_failures
                        );
                    }
                }
                state::write(&mut st);
            }
            _ = cache_refresh_interval.tick() => {
                sync_runtime_timers(
                    &mut poll_secs,
                    &mut cache_refresh_secs,
                    &mut poll_interval,
                    &mut cache_refresh_interval,
                );
                let daemon = config::get().daemon;
                let warm = warmup_on_cache_refresh(daemon.auto_warmup, &daemon.warmup_times);
                match refresh_profile_cache(warm).await {
                    Ok(summary) => tracing::debug!(
                        "Cache refresh completed: refreshed={}, warmed={}, failed={}",
                        summary.refreshed,
                        summary.warmed,
                        summary.failed
                    ),
                    Err(e) => tracing::warn!("Cache refresh skipped: {e}"),
                }
                st.last_cache_refresh_at = Some(auth::now_unix_secs());
                state::write(&mut st);
            }
            _ = warmup_interval.tick() => {
                // Always-on ~60s tick: picks up Settings saves even when poll/cache
                // periods are long, and rebuilds those timers when intervals change.
                sync_runtime_timers(
                    &mut poll_secs,
                    &mut cache_refresh_secs,
                    &mut poll_interval,
                    &mut cache_refresh_interval,
                );
                run_due_scheduled_warmup(&mut st).await;
            }
            _ = shutdown.recv() => {
                tracing::info!("Received shutdown signal, exiting daemon loop");
                break;
            }
            _ = shutdown_request_received() => {
                tracing::info!("Received Windows shutdown request, exiting daemon loop");
                break;
            }
        }
    }
    Ok(())
}

fn delayed_interval(secs: u64) -> tokio::time::Interval {
    let period = std::time::Duration::from_secs(secs.max(1));
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval
}

/// Diff used when rebuilding daemon timers after a config.toml re-read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IntervalDelta {
    poll: Option<u64>,
    cache_refresh: Option<u64>,
}

fn interval_delta(old_poll: u64, old_cache: u64, new_poll: u64, new_cache: u64) -> IntervalDelta {
    IntervalDelta {
        poll: (new_poll != old_poll).then_some(new_poll),
        cache_refresh: (new_cache != old_cache).then_some(new_cache),
    }
}

/// Re-read `config.toml` into the process snapshot and rebuild poll/cache timers
/// when those intervals changed. Other daemon keys (threshold, notify, warmup, …)
/// take effect on the next poll/refresh via `config::get()`.
///
/// A missing file is left alone: `load_current` maps NotFound to defaults, and
/// applying those would wipe a last-known-good snapshot after a temporary
/// `mv`/`rm` while editing.
fn sync_runtime_timers(
    poll_secs: &mut u64,
    cache_refresh_secs: &mut u64,
    poll_interval: &mut tokio::time::Interval,
    cache_refresh_interval: &mut tokio::time::Interval,
) {
    let path = match config::config_path() {
        Ok(path) => path,
        Err(e) => {
            tracing::debug!("Using in-memory config; failed to resolve config path: {e}");
            return;
        }
    };
    if !path.is_file() {
        tracing::debug!("Using in-memory config; {} is absent", path.display());
        return;
    }
    let cfg = match config::load_current() {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::debug!("Using in-memory config; failed to re-read file: {e}");
            return;
        }
    };
    let new_poll = cfg.daemon.poll_interval_secs.max(1);
    let new_cache = cfg.daemon.cache_refresh_interval_secs.max(1);
    let delta = interval_delta(*poll_secs, *cache_refresh_secs, new_poll, new_cache);
    if let Some(secs) = delta.poll {
        tracing::info!("Hot-reloaded poll_interval_secs: {} -> {secs}", *poll_secs);
        *poll_secs = secs;
        *poll_interval = delayed_interval(secs);
    }
    if let Some(secs) = delta.cache_refresh {
        tracing::info!(
            "Hot-reloaded cache_refresh_interval_secs: {} -> {secs}",
            *cache_refresh_secs
        );
        *cache_refresh_secs = secs;
        *cache_refresh_interval = delayed_interval(secs);
    }
    config::replace_runtime(cfg);
}

async fn run_due_scheduled_warmup(st: &mut DaemonState) {
    let daemon = config::get().daemon;
    if !daemon.auto_warmup || daemon.warmup_times.is_empty() {
        return;
    }
    let now = warmup_schedule::schedule_now(&daemon.timezone);
    let Some(slot) =
        warmup_schedule::latest_due_slot(&daemon.warmup_times, now, st.last_warmup_slot.as_deref())
    else {
        return;
    };
    let completed = match refresh_profile_cache(true).await {
        Ok(summary) => {
            tracing::info!(
                "Scheduled warmup for {slot}: refreshed={}, warmed={}, failed={}",
                summary.refreshed,
                summary.warmed,
                summary.failed
            );
            summary.completed()
        }
        Err(e) => {
            tracing::warn!("Scheduled warmup for {slot} skipped: {e}");
            false
        }
    };
    if completed {
        st.last_warmup_slot = Some(warmup_schedule::slot_stamp_for(now, &slot));
    }
    st.last_cache_refresh_at = Some(auth::now_unix_secs());
    state::write(st);
}

/// Check current account usage and switch to a better candidate if threshold exceeded.
async fn check_and_switch() -> Result<PollOutcome> {
    let profiles = profile::list_profiles()?;
    if profiles.len() < 2 {
        return Ok(PollOutcome::NoAction);
    }

    let current = profile::read_current();
    if current.is_empty() {
        return Ok(PollOutcome::NoAction);
    }

    let cfg = config::get();
    let safety_7d = cfg.use_cfg.safety_margin_7d;
    let threshold = cfg.daemon.switch_threshold;
    let now = auth::now_unix_secs();

    // 1. Force-fetch current account's usage (bypass cache)
    let current_path = profile::profile_auth_path(&current)?;
    let current_usage = usage::fetch_usage_retried_unattended(&current, &current_path, &current)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.detail))?;

    // 2. Check if current account exceeds threshold
    // Free accounts have no primary window (7d is remapped to secondary),
    // so fall back to secondary when primary is absent.
    let current_used = current_usage_percent_for_switch(&current_usage);

    if current_used < threshold {
        tracing::debug!(
            "Current account '{}' at {:.1}%, below threshold {:.1}%",
            current,
            current_used,
            threshold,
        );
        return Ok(PollOutcome::NoAction);
    }

    tracing::info!(
        "Current account '{}' at {:.1}%, above threshold {:.1}% -- searching for better candidate",
        current,
        current_used,
        threshold,
    );

    // 3. Fetch all other candidates concurrently
    let team_priority = cfg.use_cfg.team_priority;
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(cfg.network.max_concurrent));
    let mut tasks = tokio::task::JoinSet::new();

    for alias in &profiles {
        if alias == &current {
            continue;
        }
        let path = match profile::profile_auth_path(alias) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let alias = alias.clone();
        let current = current.clone();
        let semaphore = semaphore.clone();
        tasks.spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .expect("candidate usage limiter must stay open");
            let u = usage::fetch_usage_retried(&alias, &path, &current).await;
            (alias, path, u)
        });
    }

    // 4. Score everything uniformly (same helper as CLI `use`); the current
    // account goes first so it can be split back off after scoring.
    let mut items = vec![(
        current.clone(),
        current_usage.clone(),
        auth::read_account_info(&current_path),
        cache::get_last_used(&current),
    )];
    while let Some(res) = tasks.join_next().await {
        let (alias, path, u) = match res {
            Ok(v) => v,
            Err(_) => continue,
        };
        let u = match u {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("[{alias}] fetch failed: {}", e.summary);
                continue;
            }
        };
        let info = auth::read_account_info(&path);
        let last_used = cache::get_last_used(&alias);
        items.push((alias, u, info, last_used));
    }

    let mut scored = usage::score_candidates(items, now, safety_7d, team_priority);
    let current_score = scored.remove(0).score;

    // 5. Switch if a better candidate was found
    if let Some((best_alias, best_score)) =
        usage::pick_switch_target(current_score, &scored, safety_7d)
    {
        let (best_alias, best_score) = (best_alias.to_string(), best_score);
        // A switch replaces the live auth.json; doing that under an active
        // Codex session would swap accounts mid-conversation. Hold the
        // switch and let the next poll retry once the session ends.
        if cfg.daemon.defer_switch_while_codex_running
            && super::codex_process::codex_process_running()
        {
            tracing::info!(
                "Deferring switch '{}' -> '{}': a Codex session is running",
                current,
                best_alias,
            );
            return Ok(PollOutcome::Deferred { to: best_alias });
        }

        tracing::info!(
            "Switching: '{}' (score {:.1}) -> '{}' (score {:.1})",
            current,
            current_score,
            best_alias,
            best_score,
        );
        if !profile::switch_profile_if_current(&current, &best_alias)? {
            tracing::info!(
                "Skipping stale switch decision '{} -> {}': current profile changed during polling",
                current,
                best_alias,
            );
            return Ok(PollOutcome::NoAction);
        }
        if let Err(error) = cache::set_last_used(&best_alias) {
            tracing::warn!(
                "Switched to '{best_alias}', but failed to record last-used metadata: {error:#}"
            );
        }

        if cfg.daemon.notify {
            super::notify::send_notification(&format!(
                "Switched to '{}' (score: {:.0})",
                best_alias, best_score
            ));
        }
        return Ok(PollOutcome::Switched {
            from: current,
            to: best_alias,
            score: best_score,
        });
    }

    tracing::debug!("No better candidate found");
    Ok(PollOutcome::NoAction)
}

#[cfg(test)]
mod tests {
    use super::{
        CacheRefreshSummary, IntervalDelta, current_usage_percent_for_switch, interval_delta,
        poll_backoff_secs,
    };
    use crate::usage::{UsageInfo, WindowUsage};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn poll_backoff_doubles_and_caps_at_sixteen_intervals() {
        assert_eq!(poll_backoff_secs(60, 1), 120);
        assert_eq!(poll_backoff_secs(60, 2), 240);
        assert_eq!(poll_backoff_secs(60, 4), 960);
        assert_eq!(poll_backoff_secs(60, 10), 960);
    }

    #[test]
    fn interval_delta_only_reports_changed_timers() {
        assert_eq!(
            interval_delta(60, 300, 60, 300),
            IntervalDelta {
                poll: None,
                cache_refresh: None,
            }
        );
        assert_eq!(
            interval_delta(60, 300, 90, 300),
            IntervalDelta {
                poll: Some(90),
                cache_refresh: None,
            }
        );
        assert_eq!(
            interval_delta(60, 300, 60, 120),
            IntervalDelta {
                poll: None,
                cache_refresh: Some(120),
            }
        );
        assert_eq!(
            interval_delta(60, 300, 15, 45),
            IntervalDelta {
                poll: Some(15),
                cache_refresh: Some(45),
            }
        );
    }

    #[tokio::test]
    async fn sync_runtime_timers_reloads_intervals_from_disk() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let prev_cs = std::env::var_os("CODEX_SWITCH_HOME");
        unsafe {
            std::env::set_var("CODEX_SWITCH_HOME", dir.path());
        }

        let mut cfg = crate::config::AppConfig::default();
        cfg.daemon.poll_interval_secs = 90;
        cfg.daemon.cache_refresh_interval_secs = 120;
        cfg.daemon.switch_threshold = 55.0;
        crate::config::save(&cfg).expect("write config.toml");

        let mut poll_secs = 60u64;
        let mut cache_secs = 300u64;
        let mut poll_interval = super::delayed_interval(poll_secs);
        let mut cache_interval = super::delayed_interval(cache_secs);
        super::sync_runtime_timers(
            &mut poll_secs,
            &mut cache_secs,
            &mut poll_interval,
            &mut cache_interval,
        );
        assert_eq!(poll_secs, 90);
        assert_eq!(cache_secs, 120);

        // Unchanged intervals must not be reported as a delta on the next sync.
        let before_poll = poll_secs;
        let before_cache = cache_secs;
        super::sync_runtime_timers(
            &mut poll_secs,
            &mut cache_secs,
            &mut poll_interval,
            &mut cache_interval,
        );
        assert_eq!(poll_secs, before_poll);
        assert_eq!(cache_secs, before_cache);

        // Missing config.toml must keep the in-memory timers, not apply defaults.
        std::fs::remove_file(crate::config::config_path().unwrap()).unwrap();
        super::sync_runtime_timers(
            &mut poll_secs,
            &mut cache_secs,
            &mut poll_interval,
            &mut cache_interval,
        );
        assert_eq!(poll_secs, 90);
        assert_eq!(cache_secs, 120);

        unsafe {
            match prev_cs {
                Some(v) => std::env::set_var("CODEX_SWITCH_HOME", v),
                None => std::env::remove_var("CODEX_SWITCH_HOME"),
            }
        }
    }

    #[tokio::test]
    async fn sync_runtime_timers_skips_when_config_file_is_absent() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let prev_cs = std::env::var_os("CODEX_SWITCH_HOME");
        unsafe {
            std::env::set_var("CODEX_SWITCH_HOME", dir.path());
        }

        let mut poll_secs = 45u64;
        let mut cache_secs = 180u64;
        let mut poll_interval = super::delayed_interval(poll_secs);
        let mut cache_interval = super::delayed_interval(cache_secs);
        assert!(!crate::config::config_path().unwrap().exists());
        super::sync_runtime_timers(
            &mut poll_secs,
            &mut cache_secs,
            &mut poll_interval,
            &mut cache_interval,
        );
        assert_eq!(poll_secs, 45);
        assert_eq!(cache_secs, 180);

        unsafe {
            match prev_cs {
                Some(v) => std::env::set_var("CODEX_SWITCH_HOME", v),
                None => std::env::remove_var("CODEX_SWITCH_HOME"),
            }
        }
    }

    #[test]
    fn account_limited_usage_bypasses_low_usage_switch_threshold() {
        let usage = UsageInfo {
            account_limited: true,
            primary: Some(WindowUsage {
                used_percent: Some(1.0),
                ..WindowUsage::default()
            }),
            ..UsageInfo::default()
        };

        assert!(current_usage_percent_for_switch(&usage) >= 80.0);
    }

    #[test]
    fn scheduled_warmup_is_complete_only_when_every_account_succeeds() {
        assert!(CacheRefreshSummary::default().completed());
        assert!(
            CacheRefreshSummary {
                refreshed: 2,
                warmed: 1,
                failed: 0,
            }
            .completed()
        );
        assert!(
            !CacheRefreshSummary {
                refreshed: 1,
                warmed: 0,
                failed: 1,
            }
            .completed()
        );
    }

    #[tokio::test]
    async fn candidate_usage_requests_never_exceed_network_limit() {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(2));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = tokio::task::JoinSet::new();

        for _ in 0..6 {
            let semaphore = semaphore.clone();
            let active = active.clone();
            let peak = peak.clone();
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await.unwrap();
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
        while tasks.join_next().await.is_some() {}
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }
}

#[derive(Default)]
struct CacheRefreshSummary {
    refreshed: usize,
    warmed: usize,
    failed: usize,
}

impl CacheRefreshSummary {
    fn completed(&self) -> bool {
        self.failed == 0
    }
}

async fn refresh_profile_cache(auto_warmup: bool) -> Result<CacheRefreshSummary> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        return Ok(CacheRefreshSummary::default());
    }

    let current = profile::read_current();
    let now = auth::now_unix_secs();
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        config::get().network.max_concurrent,
    ));
    let mut tasks = tokio::task::JoinSet::new();

    for alias in profiles {
        let current = current.clone();
        let sem = semaphore.clone();
        tasks.spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                return (alias, None, false, Some("usage limiter closed".to_string()));
            };
            let path = match profile::profile_auth_path(&alias) {
                Ok(path) => path,
                Err(e) => return (alias, None, false, Some(e.to_string())),
            };

            let usage =
                match usage::fetch_usage_retried_unattended_deferred_cache(&alias, &path, &current)
                    .await
                {
                    Ok(usage) => usage,
                    Err(e) => return (alias, None, false, Some(e.summary)),
                };

            if !auto_warmup || usage::usage_has_active_warmup_window(&usage, now) {
                return (alias, Some(usage), false, None);
            }

            if let Err(e) = warmup::warmup_account(&alias, &path).await {
                return (
                    alias,
                    Some(usage),
                    false,
                    Some(format!("warmup failed: {e}")),
                );
            }

            let usage =
                match usage::fetch_usage_retried_unattended_deferred_cache(&alias, &path, &current)
                    .await
                {
                    Ok(updated) => updated,
                    Err(e) => {
                        tracing::warn!("[{alias}] post-warmup cache refresh failed: {}", e.summary);
                        usage
                    }
                };
            (alias, Some(usage), true, None)
        });
    }

    let mut summary = CacheRefreshSummary::default();
    let mut updates = Vec::new();
    while let Some(res) = tasks.join_next().await {
        let (alias, usage, warmed, err) = match res {
            Ok(value) => value,
            Err(e) => {
                summary.failed += 1;
                tracing::warn!("Cache refresh worker failed: {e}");
                continue;
            }
        };
        if let Some(usage) = usage {
            summary.refreshed += 1;
            updates.push((alias.clone(), usage));
        }
        if warmed {
            summary.warmed += 1;
        }
        if let Some(err) = err {
            summary.failed += 1;
            tracing::warn!("[{alias}] cache refresh failed: {err}");
        }
    }

    cache::put_many_async(updates).await?;

    Ok(summary)
}
