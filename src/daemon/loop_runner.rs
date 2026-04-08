use anyhow::Result;

use crate::{auth, cache, config, profile, usage};

/// Main daemon event loop: periodically checks usage and switches account when needed.
pub async fn run_daemon_loop() -> Result<()> {
    let cfg = config::get();
    let poll_secs = cfg.daemon.poll_interval_secs;
    let token_secs = cfg.daemon.token_check_interval_secs;

    let mut poll_interval = tokio::time::interval(std::time::Duration::from_secs(poll_secs));
    let mut token_interval = tokio::time::interval(std::time::Duration::from_secs(token_secs));
    let mut consecutive_failures: u32 = 0;

    tracing::info!(
        "Daemon loop started: poll={}s, token_check={}s, threshold={}%",
        poll_secs,
        token_secs,
        cfg.daemon.switch_threshold,
    );

    loop {
        tokio::select! {
            _ = poll_interval.tick() => {
                match check_and_switch().await {
                    Ok(switched) => {
                        consecutive_failures = 0;
                        if switched {
                            tracing::info!("Account switch completed");
                        }
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        let backoff_secs = poll_secs * 2u64.pow(consecutive_failures.min(4));
                        tracing::error!(
                            "Monitor cycle failed ({consecutive_failures}x): {e}, backing off {backoff_secs}s"
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                    }
                }
            }
            _ = token_interval.tick() => {
                usage::refresh_expiring_tokens().await;
            }
            _ = shutdown_signal() => {
                tracing::info!("Received shutdown signal, exiting daemon loop");
                break;
            }
        }
    }
    Ok(())
}

/// Check current account usage and switch to a better candidate if threshold exceeded.
///
/// Returns `true` if a switch was performed.
async fn check_and_switch() -> Result<bool> {
    let profiles = profile::list_profiles()?;
    if profiles.len() < 2 {
        return Ok(false);
    }

    let current = profile::read_current();
    if current.is_empty() {
        return Ok(false);
    }

    let cfg = config::get();
    let safety_7d = cfg.use_cfg.safety_margin_7d;
    let threshold = cfg.daemon.switch_threshold;
    let now = auth::now_unix_secs();

    // 1. Force-fetch current account's usage (bypass cache)
    let current_path = profile::profile_auth_path(&current)?;
    let current_usage = usage::fetch_usage_retried_force(&current, &current_path, &current)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.detail))?;

    // 2. Check if current account exceeds threshold
    let current_used = current_usage
        .primary
        .as_ref()
        .and_then(|w| w.used_percent)
        .unwrap_or(0.0);

    if current_used < threshold {
        tracing::debug!(
            "Current account '{}' at {:.1}%, below threshold {:.1}%",
            current,
            current_used,
            threshold,
        );
        return Ok(false);
    }

    tracing::info!(
        "Current account '{}' at {:.1}%, above threshold {:.1}% -- searching for better candidate",
        current,
        current_used,
        threshold,
    );

    // 3. Score current account using unified algorithm
    let current_info = profile::profile_auth_path(&current)
        .map(|p| auth::read_account_info(&p))
        .unwrap_or_default();
    let current_candidate = usage::Candidate::from_usage(
        current.clone(),
        &current_usage,
        current_info.is_team(),
        current_info.is_free(),
        cache::get_last_used(&current),
        now,
    );
    let current_score = usage::score_unified(&current_candidate, safety_7d);

    // 4. Score all other candidates and find the best
    let mut best: Option<(String, f64)> = None;

    for alias in &profiles {
        if alias == &current {
            continue;
        }
        let path = match profile::profile_auth_path(alias) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let u = match usage::fetch_usage_retried(alias, &path, &current).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("[{alias}] fetch failed: {}", e.summary);
                continue;
            }
        };

        let info = auth::read_account_info(&path);
        let candidate = usage::Candidate::from_usage(
            alias.clone(),
            &u,
            info.is_team(),
            info.is_free(),
            cache::get_last_used(alias),
            now,
        );

        if !usage::is_candidate_eligible(&candidate, safety_7d) {
            continue;
        }

        let s = usage::score_unified(&candidate, safety_7d);

        if s > current_score
            && best.as_ref().is_none_or(|(_, bs)| s > *bs) {
            best = Some((alias.clone(), s));
        }
    }

    // 5. Switch if a better candidate was found
    if let Some((best_alias, best_score)) = best {
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
        return Ok(true);
    }

    tracing::debug!("No better candidate found");
    Ok(false)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => {},
            _ = sigint.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.expect("Ctrl+C handler");
    }
}
