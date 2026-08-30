use crate::output::{format_local_datetime, print_json, user_println};
use crate::{auth, cache, color, config, profile, usage, warmup};
use anyhow::{Context, Result};

pub(crate) fn format_resync_confirm_prompt(
    alias: &str,
    live_last_refresh: Option<&str>,
    profile_last_refresh: Option<&str>,
) -> String {
    let live_ts = live_last_refresh.unwrap_or("unknown");
    let profile_ts = profile_last_refresh.unwrap_or("unknown");
    format!(
        "Update profile '{alias}' with live credentials? (live last_refresh={live_ts} -> profile last_refresh={profile_ts}) [Y/n] "
    )
}

pub(crate) async fn reset_card_cmd(alias: &str, yes: bool, json: bool) -> Result<()> {
    profile::validate_alias(alias)?;
    if json && !yes {
        anyhow::bail!("confirmation required; rerun with --yes to consume a reset card");
    }
    let path = profile::profile_auth_path(alias)?;
    if !path.exists() {
        anyhow::bail!("profile '{alias}' not found");
    }

    let usage = usage::fetch_usage_retried_force(alias, &path, &profile::read_current())
        .await
        .map_err(|e| anyhow::anyhow!("{alias}: {}", e.detail))?;
    let credit = match usage::earliest_reset_credit(&usage.reset_credits).cloned() {
        Some(credit) => credit,
        None => usage::fetch_earliest_reset_credit(alias, &path).await?,
    };
    if !yes {
        let expires = credit
            .expires_at
            .as_deref()
            .map(format_local_datetime)
            .unwrap_or_else(|| "no expiry".to_string());
        if !confirm_reset_card(alias, &expires) {
            anyhow::bail!("aborted");
        }
    }

    let result = match usage::consume_reset_credit_by_id(alias, &path, &credit.id).await {
        Ok(result) => result,
        Err(error) if error.outcome_unknown_after_request() => {
            if let Err(err) = cache::invalidate(alias) {
                tracing::warn!("Failed to invalidate usage cache for {alias}: {err}");
            }
            anyhow::bail!(error.user_facing_unknown_message(alias));
        }
        Err(error) => return Err(error.into()),
    };
    if let Err(err) = cache::invalidate(alias) {
        tracing::warn!("Failed to invalidate usage cache for {alias}: {err}");
    }
    if json {
        print_json(&serde_json::json!({
            "ok": true,
            "alias": alias,
            "action": "reset-card-consumed",
            "credit_id": result.credit.id,
            "expires_at": result.credit.expires_at,
            "code": result.code,
            "windows_reset": result.windows_reset,
            "redeemed_at": result.redeemed_at,
        }));
    } else {
        println!(
            "{}",
            color::success(&format!(
                "[ok] Consumed reset card for {alias} (was expiring at {})",
                result
                    .credit
                    .expires_at
                    .as_deref()
                    .map(format_local_datetime)
                    .unwrap_or_else(|| "no expiry".to_string())
            ))
        );
        if let Some(windows_reset) = result.windows_reset {
            println!("  windows reset: {windows_reset}");
        }
    }
    Ok(())
}

fn confirm_reset_card(alias: &str, expires: &str) -> bool {
    use std::io::{self, Write as _};

    eprint!(
        "{}",
        color::dim(&format!(
            "Use earliest reset card for '{alias}' (expires {expires})? [y/N] "
        ))
    );
    io::stderr().flush().ok();
    let mut input = String::new();
    match io::stdin().read_line(&mut input) {
        Ok(0) => false,
        Ok(_) => matches!(input.trim().to_lowercase().as_str(), "y" | "yes"),
        Err(_) => false,
    }
}

// ── open ─────────────────────────────────────────────────

pub(crate) fn open_cmd() -> Result<()> {
    let dir = auth::app_home()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating directory {}", dir.display()))?;
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&dir).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer.exe")
        .arg(dir.as_os_str())
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(&dir).spawn();
    match result {
        Ok(_) => println!("Opened: {}", dir.display()),
        Err(e) => println!(
            "{}",
            color::error(&format!(
                "Could not open file manager: {e}\nPath: {}",
                dir.display()
            ))
        ),
    }
    Ok(())
}

// ── warmup ────────────────────────────────────────────────

pub(crate) async fn warmup_cmd(alias: Option<&str>, json: bool) -> Result<()> {
    let aliases: Vec<String> = match alias {
        Some(a) => {
            let path = profile::profile_auth_path(a)?;
            if !path.exists() {
                anyhow::bail!("profile '{}' not found", a);
            }
            vec![a.to_string()]
        }
        None => profile::list_profiles()?,
    };

    if aliases.is_empty() {
        if json {
            print_json(&serde_json::json!({"results": []}));
        } else {
            user_println("(no saved profiles)");
        }
        return Ok(());
    }

    let mut results: Vec<serde_json::Value> = Vec::with_capacity(aliases.len());

    // Filter out accounts whose usage data proves an active rate-limit window.
    // A window that appears "just started" (elapsed < 5 min) likely means the previous warmup
    // ping didn't consume real quota — allow the user to retry.
    let now = auth::now_unix_secs();
    let mut to_warmup = Vec::new();
    for alias in &aliases {
        let already_active = cache::get(alias)
            .as_ref()
            .is_some_and(|u| usage::usage_has_active_warmup_window(u, now));
        if already_active {
            if json {
                results.push(serde_json::json!({"alias": alias, "ok": true, "skipped": true}));
            } else {
                user_println(&format!(
                    "  {} {}",
                    color::dim(alias),
                    color::dim("already active, skipped")
                ));
            }
        } else {
            to_warmup.push(alias.clone());
        }
    }

    if to_warmup.is_empty() {
        if json {
            results.sort_by(|a, b| {
                a["alias"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["alias"].as_str().unwrap_or(""))
            });
            print_json(&serde_json::json!({"ok": true, "results": results}));
        }
        return Ok(());
    }

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        config::get().network.max_concurrent,
    ));

    let mut had_error = false;
    let mut tasks = tokio::task::JoinSet::new();
    for alias in to_warmup {
        let path = match profile::profile_auth_path(&alias) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("[{alias}] failed to resolve profile path: {e}");
                if json {
                    results.push(
                        serde_json::json!({"alias": alias, "ok": false, "error": e.to_string()}),
                    );
                }
                had_error = true;
                continue;
            }
        };
        let sem = semaphore.clone();
        tasks.spawn(async move {
            let Ok(_permit) = sem.acquire().await else {
                return (alias, Err(anyhow::anyhow!("semaphore closed")));
            };
            let result = warmup::warmup_account(&alias, &path).await;
            (alias, result)
        });
    }

    while let Some(res) = tasks.join_next().await {
        let (alias, result) = res.context("warmup task panicked")?;
        match &result {
            Ok(()) => {
                if json {
                    results.push(serde_json::json!({"alias": alias, "ok": true}));
                } else {
                    user_println(&format!(
                        "  {} {}",
                        color::success(&alias),
                        color::dim("warmed up")
                    ));
                }
            }
            Err(e) => {
                let detail = format!("{e:#}");
                tracing::error!(alias = %alias, error = %detail, "warmup failed");
                if json {
                    results.push(serde_json::json!({"alias": alias, "ok": false, "error": detail}));
                } else {
                    user_println(&format!("  {} failed: {}", color::error(&alias), detail));
                }
                had_error = true;
            }
        }
    }

    if json {
        results.sort_by(|a, b| {
            a["alias"]
                .as_str()
                .unwrap_or("")
                .cmp(b["alias"].as_str().unwrap_or(""))
        });
        // Embed overall status in JSON so callers get a single valid object.
        // Use std::process::exit to signal failure without a second JSON error line.
        print_json(&serde_json::json!({"ok": !had_error, "results": results}));
        if had_error {
            std::process::exit(1);
        }
    } else if had_error {
        anyhow::bail!("one or more warmup operations failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::format_resync_confirm_prompt;

    #[test]
    fn resync_prompt_shows_direction_and_missing_timestamps() {
        let prompt = format_resync_confirm_prompt("acme", Some("2026-07-20T00:00:00Z"), None);

        assert_eq!(
            prompt,
            "Update profile 'acme' with live credentials? (live last_refresh=2026-07-20T00:00:00Z -> profile last_refresh=unknown) [Y/n] "
        );
    }
}
