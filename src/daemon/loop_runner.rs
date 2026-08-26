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
    let poll_secs = cfg.daemon.poll_interval_secs;
    let token_secs = cfg.daemon.token_check_interval_secs;
    let cache_refresh_secs = cfg.daemon.cache_refresh_interval_secs;

    let mut poll_interval = tokio::time::interval(std::time::Duration::from_secs(poll_secs));
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut token_interval = tokio::time::interval(std::time::Duration::from_secs(token_secs));
    token_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let cache_refresh_period = std::time::Duration::from_secs(cache_refresh_secs);
    let mut cache_refresh_interval = tokio::time::interval_at(
        tokio::time::Instant::now() + cache_refresh_period,
        cache_refresh_period,
    );
    cache_refresh_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
        "Daemon loop started: poll={}s, token_check={}s, cache_refresh={}s, auto_warmup={}, warmup_times={:?}, threshold={}%",
        poll_secs,
        token_secs,
        cache_refresh_secs,
        cfg.daemon.auto_warmup,
        cfg.daemon.warmup_times,
        cfg.daemon.switch_threshold,
    );

    loop {
        tokio::select! {
            _ = poll_interval.tick() => {
                // Failure backoff suspends polling only; token and cache
                // timers keep running.
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
            _ = token_interval.tick() => {
                // Runs unattended on a timer: a lost write here bricks the
                // profile with nobody watching, so it gets ERROR, not debug.
                for failure in usage::refresh_expiring_tokens().await {
                    // `detail` already opens with `[alias]` and carries the
                    // underlying IO/permission cause; the field makes the
                    // affected profile filterable in structured log output.
                    tracing::error!(alias = %failure.alias, "{}", failure.error.detail);
                }
            }
            _ = cache_refresh_interval.tick() => {
                let daemon = live_daemon_cfg();
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

fn live_daemon_cfg() -> crate::config::DaemonConfig {
    match config::load_current() {
        Ok(cfg) => cfg.daemon,
        Err(e) => {
            tracing::debug!("Using in-memory daemon config; failed to re-read file: {e}");
            config::get().daemon
        }
    }
}

async fn run_due_scheduled_warmup(st: &mut DaemonState) {
    let daemon = live_daemon_cfg();
    if !daemon.auto_warmup || daemon.warmup_times.is_empty() {
        return;
    }
    let now = chrono::Local::now();
    let Some(slot) =
        warmup_schedule::latest_due_slot(&daemon.warmup_times, now, st.last_warmup_slot.as_deref())
    else {
        return;
    };
    match refresh_profile_cache(true).await {
        Ok(summary) => tracing::info!(
            "Scheduled warmup for {slot}: refreshed={}, warmed={}, failed={}",
            summary.refreshed,
            summary.warmed,
            summary.failed
        ),
        Err(e) => tracing::warn!("Scheduled warmup for {slot} skipped: {e}"),
    }
    st.last_warmup_slot = Some(warmup_schedule::slot_stamp_for(now, &slot));
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
        profile::switch_profile(&best_alias)?;
        cache::set_last_used(&best_alias)?;

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
    use super::{current_usage_percent_for_switch, poll_backoff_secs};
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
                return (
                    alias,
                    false,
                    false,
                    Some("usage limiter closed".to_string()),
                );
            };
            let path = match profile::profile_auth_path(&alias) {
                Ok(path) => path,
                Err(e) => return (alias, false, false, Some(e.to_string())),
            };

            let usage = match usage::fetch_usage_retried_unattended(&alias, &path, &current).await {
                Ok(usage) => usage,
                Err(e) => return (alias, false, false, Some(e.summary)),
            };

            if !auto_warmup || usage::usage_has_active_warmup_window(&usage, now) {
                return (alias, true, false, None);
            }

            if let Err(e) = warmup::warmup_account(&alias, &path).await {
                return (alias, true, false, Some(format!("warmup failed: {e}")));
            }

            if let Err(e) = usage::fetch_usage_retried_unattended(&alias, &path, &current).await {
                tracing::warn!("[{alias}] post-warmup cache refresh failed: {}", e.summary);
            }
            (alias, true, true, None)
        });
    }

    let mut summary = CacheRefreshSummary::default();
    while let Some(res) = tasks.join_next().await {
        let (alias, refreshed, warmed, err) = match res {
            Ok(value) => value,
            Err(e) => {
                summary.failed += 1;
                tracing::warn!("Cache refresh worker failed: {e}");
                continue;
            }
        };
        if refreshed {
            summary.refreshed += 1;
        }
        if warmed {
            summary.warmed += 1;
        }
        if let Some(err) = err {
            summary.failed += 1;
            tracing::warn!("[{alias}] cache refresh failed: {err}");
        }
    }

    Ok(summary)
}
