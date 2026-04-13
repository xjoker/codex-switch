mod auth;
mod cache;
mod cli;
mod color;
mod config;
mod daemon;
mod error;
mod jwt;
mod login;
mod output;
mod process;
mod profile;
mod tui;
mod update;
mod usage;
mod warmup;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Commands};
use output::{
    MessageMode, ProgressReporter, account_to_json, print_error, print_json,
    usage_to_json, user_println,
};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Priority: --debug flag > RUST_LOG env > config.toml daemon.log_level > default "error"
    let filter = if cli.debug {
        EnvFilter::new("codex_switch=debug")
    } else if std::env::var_os("RUST_LOG").is_some() {
        EnvFilter::from_default_env()
    } else if matches!(&cli.command, Commands::Daemon(_)) {
        let level = config::daemon_log_level();
        EnvFilter::new(format!("codex_switch={level}"))
    } else {
        EnvFilter::new("codex_switch=error")
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    let use_json = cli.json || cli.json_pretty;
    let message_mode = if matches!(&cli.command, Commands::Tui) {
        MessageMode::Silent
    } else if use_json {
        MessageMode::Stderr
    } else {
        MessageMode::Stdout
    };

    config::init();
    config::set_cli_proxy(cli.proxy.clone());
    color::init(cli.color);
    output::set_json_pretty(cli.json_pretty);
    output::set_message_mode(message_mode);

    let result = dispatch(cli.command, use_json).await;

    if let Err(e) = result {
        if use_json {
            print_error(&e.to_string());
        } else {
            eprintln!("{}", color::error(&format!("Error: {e}")));
        }
        std::process::exit(1);
    }
}

async fn dispatch(cmd: Commands, json: bool) -> Result<()> {
    // Startup auth change detection — skip for commands that manage auth themselves
    let auth_check = if !json {
        let should_check = !matches!(
            &cmd,
            Commands::Login { .. }
                | Commands::Import { .. }
                | Commands::SelfUpdate { .. }
                | Commands::Open
                | Commands::Launch { .. }
        );
        if should_check {
            check_auth_change()
        } else {
            AuthCheckResult::NoChange
        }
    } else {
        AuthCheckResult::NoChange
    };
    let auth_handled = !matches!(auth_check, AuthCheckResult::NoChange);

    match cmd {
        Commands::Use { alias, force } => use_cmd(alias.as_deref(), force, json).await?,
        Commands::List { force } => list_cmd(force, json, auth_handled).await?,
        Commands::Rename { old, new } => rename_cmd(&old, &new, json)?,
        Commands::Delete { alias } => delete_cmd(&alias, json)?,
        Commands::Login { alias, device } => login_cmd(alias.as_deref(), device, json).await?,
        Commands::Import { path, alias } => import_cmd(&path, alias.as_deref(), json).await?,
        Commands::SelfUpdate {
            check,
            version,
            dev,
            stable,
        } => {
            self_update_cmd(check, version.as_deref(), dev, stable, json).await?
        }
        Commands::Warmup { alias } => warmup_cmd(alias.as_deref(), json).await?,
        Commands::Launch { alias, args } => launch_cmd(alias.as_deref(), args, json).await?,
        Commands::Tui => tui::run_tui().await?,
        Commands::Open => open_cmd()?,
        Commands::Daemon(sub) => daemon::dispatch(sub).await?,
    }

    // If startup check actually synced the profile, re-sync after command execution
    // to capture any token refreshes that happened during the command.
    if matches!(auth_check, AuthCheckResult::Synced) {
        let current = profile::read_current();
        if !current.is_empty()
            && auth::codex_auth_path()
                .ok()
                .as_ref()
                .and_then(|p| profile::find_matching_profile(p))
                .is_none()
        {
            let _ = profile::update_profile_from_live(&current);
        }
    }

    Ok(())
}

// ── startup auth change detection ────────────────────────

#[derive(Debug)]
enum AuthCheckResult {
    NoChange,
    Detected,  // change detected but not synced (non-interactive or user declined)
    Synced,    // change detected and user accepted the sync
}

fn check_auth_change() -> AuthCheckResult {
    use std::io::{self, IsTerminal};

    let change = profile::detect_auth_change();
    if matches!(change, profile::AuthChange::NoChange) {
        return AuthCheckResult::NoChange;
    }

    // Non-interactive stdin — don't prompt, don't silently mutate state
    if !io::stdin().is_terminal() {
        match &change {
            profile::AuthChange::NewAccount => {
                let info = auth::codex_auth_path().map(|p| auth::read_account_info(&p)).unwrap_or_default();
                let label = info.email.as_deref().unwrap_or("unknown");
                user_println(&format!(
                    "Detected new account ({label}) in auth.json (use `codex-switch list` interactively to save)."
                ));
            }
            profile::AuthChange::TokensUpdated { alias } => {
                user_println(&format!(
                    "auth.json credentials changed for profile '{alias}' (use `codex-switch list` interactively to update)."
                ));
            }
            profile::AuthChange::NoChange => unreachable!(),
        }
        return AuthCheckResult::Detected;
    }

    let mut synced = false;

    match change {
        profile::AuthChange::NewAccount => {
            let info = auth::codex_auth_path().map(|p| auth::read_account_info(&p)).unwrap_or_default();
            let label = info.email.as_deref().unwrap_or("unknown");
            user_println(&format!(
                "Detected new account ({label}) in auth.json — not in any saved profile."
            ));
            if confirm("Save as a new profile? [Y/n] ") {
                match profile::cmd_save(None) {
                    Ok(action) => {
                        user_println(&format!(
                            "Profile {}: {}",
                            action.action(),
                            action.alias()
                        ));
                        synced = true;
                    }
                    Err(e) => eprintln!("{}", color::error(&format!("Failed to save: {e}"))),
                }
            }
        }
        profile::AuthChange::TokensUpdated { alias } => {
            let info = auth::codex_auth_path().map(|p| auth::read_account_info(&p)).unwrap_or_default();
            let label = info.email.as_deref().unwrap_or("unknown");
            user_println(&format!(
                "auth.json credentials changed for account '{alias}' ({label})."
            ));
            if confirm(&format!("Update profile '{alias}'? [Y/n] ")) {
                match profile::update_profile_from_live(&alias) {
                    Ok(()) => {
                        user_println(&format!("Profile '{alias}' updated."));
                        synced = true;
                    }
                    Err(e) => eprintln!("{}", color::error(&format!("Failed to update: {e}"))),
                }
            }
        }
        profile::AuthChange::NoChange => unreachable!(),
    }

    if synced {
        AuthCheckResult::Synced
    } else {
        AuthCheckResult::Detected
    }
}

/// Prompt the user for Y/n confirmation. Returns false on EOF or explicit "n"/"no".
fn confirm(prompt: &str) -> bool {
    use std::io::{self, Write as _};

    eprint!("{}", color::dim(prompt));
    io::stderr().flush().ok();
    let mut input = String::new();
    match io::stdin().read_line(&mut input) {
        Ok(0) => false, // EOF
        Ok(_) => !matches!(input.trim().to_lowercase().as_str(), "n" | "no"),
        Err(_) => false,
    }
}

// ── use ──────────────────────────────────────────────────

async fn use_cmd(alias: Option<&str>, force: bool, json: bool) -> Result<()> {
    if !force {
        let procs = process::detect_codex_processes();
        if !procs.is_empty() {
            for proc in &procs {
                tracing::debug!(
                    pid = proc.pid,
                    name = %proc.name,
                    "Blocking switch because Codex process is running"
                );
            }
            let pids: Vec<String> = procs.iter().map(|p| p.pid.to_string()).collect();
            user_println(&format!(
                "Warning: {} codex process(es) detected (PID: {})",
                procs.len(),
                pids.join(", ")
            ));
            user_println("Switching accounts while Codex is running may cause issues.");
            user_println("Use --force to switch anyway, or stop Codex first.");
            anyhow::bail!("Codex process(es) running, use --force to override");
        }
    }

    match alias {
        Some(a) => {
            profile::cmd_use(a)?;
            cache::set_last_used(a)?;
            if json {
                print_json(&output::JsonOk {
                    ok: true,
                    alias: a.to_string(),
                    action: "switched".into(),
                });
            }
        }
        None => best_cmd(json).await?,
    }
    Ok(())
}

// ── list (all profiles + usage, concurrent) ──────────────

async fn list_cmd(force: bool, json: bool, auth_already_handled: bool) -> Result<()> {
    if !auth_already_handled {
        profile::auto_track_current();
    }

    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        if json {
            print_json(&output::JsonUsageResult { profiles: vec![] });
        } else {
            println!("{}", color::dim("(no saved profiles)"));
        }
        return Ok(());
    }

    let current = profile::read_current();

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        config::get().network.max_concurrent,
    ));

    struct ListRow {
        name: String,
        is_current: bool,
        info: jwt::AccountInfo,
        usage_result: Option<std::result::Result<usage::UsageInfo, usage::UsageError>>,
    }

    let mut rows: Vec<ListRow> = profiles
        .into_iter()
        .filter_map(|name| {
            let path = match profile::profile_auth_path(&name) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("[{name}] failed to resolve profile path: {e}");
                    return None;
                }
            };
            let info = auth::read_account_info(&path);
            let usage_result = if force {
                None
            } else {
                cache::get(&name).map(Ok)
            };
            Some(ListRow {
                is_current: name == current,
                name,
                info,
                usage_result,
            })
        })
        .collect();

    let refresh_count = rows.iter().filter(|row| row.usage_result.is_none()).count();
    let mut progress = if json {
        None
    } else {
        Some(ProgressReporter::new("Refreshing usage", refresh_count))
    };

    let mut tasks = tokio::task::JoinSet::new();
    for (idx, row) in rows.iter().enumerate() {
        if row.usage_result.is_some() {
            continue;
        }

        let alias = row.name.clone();
        let current = current.clone();
        let sem = semaphore.clone();
        tasks.spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                return (
                    idx,
                    Err(usage::UsageError {
                        summary: "limiter closed".into(),
                        detail: "usage limiter closed".into(),
                    }),
                );
            };
            let path = match profile::profile_auth_path(&alias) {
                Ok(p) => p,
                Err(e) => {
                    return (
                        idx,
                        Err(usage::UsageError {
                            summary: format!("path error: {e}"),
                            detail: format!("failed to resolve profile path: {e}"),
                        }),
                    );
                }
            };
            let usage_result = if force {
                usage::fetch_usage_retried_force(&alias, &path, &current).await
            } else {
                usage::fetch_usage_retried(&alias, &path, &current).await
            };
            (idx, usage_result)
        });
    }

    let mut completed = 0usize;
    while let Some(task) = tasks.join_next().await {
        let (idx, usage_result) = task.map_err(|e| anyhow::anyhow!("usage worker failed: {e}"))?;
        rows[idx].usage_result = Some(usage_result);
        completed += 1;
        if let Some(progress) = progress.as_mut() {
            progress.advance(completed);
        }
    }

    if let Some(progress) = progress.as_mut() {
        progress.finish();
    }

    let mut json_items = vec![];

    for row in rows {
        let usage_result = row.usage_result.unwrap_or_else(|| {
            Err(usage::UsageError {
                summary: "unknown".into(),
                detail: "usage result missing".into(),
            })
        });
        if json {
            let ju = match &usage_result {
                Ok(u) => usage_to_json(Ok(u)),
                Err(e) => usage_to_json(Err(&e.detail)),
            };
            json_items.push(output::JsonProfileWithUsage {
                alias: row.name,
                is_current: row.is_current,
                account: account_to_json(&row.info),
                usage: ju,
            });
        } else {
            let mark = if row.is_current {
                color::active("*")
            } else {
                " ".to_string()
            };
            let alias_str = if row.is_current {
                color::bold(&row.name)
            } else {
                row.name.clone()
            };
            print!("{mark} {alias_str}");
            if let Some(email) = &row.info.email {
                print!("  {}", color::dim(email));
            }
            if row.info.plan_type.is_some() {
                print!(
                    "  {}",
                    color::plan(&row.info.plan_label(), row.info.plan_type.as_deref())
                );
            }
            println!();
            match usage_result {
                Ok(u) => print_usage_line(&u),
                Err(e) => println!(
                    "  {} {}",
                    color::error("!!"),
                    color::error(&e.summary)
                ),
            }
            println!(); // blank line between accounts
        }
    }

    if json {
        print_json(&output::JsonUsageResult {
            profiles: json_items,
        });
    }

    // Opportunistically refresh tokens about to expire (background, bounded)
    usage::refresh_expiring_tokens().await;

    Ok(())
}

// ── delete ───────────────────────────────────────────────

// ── rename ───────────────────────────────────────────────

fn rename_cmd(old: &str, new: &str, json: bool) -> Result<()> {
    profile::rename_profile(old, new)?;
    if json {
        print_json(&output::JsonOk {
            ok: true,
            alias: new.to_string(),
            action: "renamed".into(),
        });
    }
    Ok(())
}

fn delete_cmd(alias: &str, json: bool) -> Result<()> {
    profile::cmd_delete(alias)?;
    if json {
        print_json(&output::JsonOk {
            ok: true,
            alias: alias.to_string(),
            action: "deleted".into(),
        });
    }
    Ok(())
}

// ── login / reauth ────────────────────────────────────────

fn build_auth_from_tokens(tokens: &login::LoginTokens) -> (serde_json::Value, jwt::AccountInfo) {
    let temp = serde_json::json!({
        "tokens": { "id_token": tokens.id_token, "access_token": tokens.access_token,
                    "refresh_token": tokens.refresh_token, "account_id": "" }
    });
    let info = jwt::parse_account_info(&temp);
    let account_id = info.account_id.as_deref().unwrap_or("").to_string();
    (login::build_auth_json(tokens, &account_id), info)
}

async fn login_cmd(alias: Option<&str>, device: bool, json: bool) -> Result<()> {
    if let Some(a) = alias {
        profile::validate_alias(a)?;
    }

    if let Some(a) = alias {
        let dst = profile::profile_auth_path(a)?;
        if dst.exists() {
            return reauth_profile(a, device, json).await;
        }
    }

    let tokens = if device {
        login::run_device_code_auth().await?
    } else {
        login::run_device_auth().await?
    };
    let (auth_val, _info) = build_auth_from_tokens(&tokens);

    match profile::save_auth_value(auth_val, alias)? {
        profile::SaveAction::Created(a) => {
            if !json {
                println!(
                    "{}",
                    color::success(&format!("[ok] Logged in -- saved as new profile: {a}"))
                );
            }
            if json {
                print_json(&output::JsonOk {
                    ok: true,
                    alias: a,
                    action: "created".into(),
                });
            }
        }
        profile::SaveAction::Updated(a) => {
            if !json {
                println!(
                    "{}",
                    color::success(&format!("[ok] Logged in -- updated existing profile: {a}"))
                );
            }
            if json {
                print_json(&output::JsonOk {
                    ok: true,
                    alias: a,
                    action: "updated".into(),
                });
            }
        }
    }
    Ok(())
}

async fn reauth_profile(alias: &str, device: bool, json: bool) -> Result<()> {
    let dst = profile::profile_auth_path(alias)?;
    let old_info = auth::read_account_info(&dst);

    if !json {
        println!(
            "Re-authorizing profile '{}' ({})...",
            color::bold(alias),
            old_info.email.as_deref().unwrap_or("unknown email")
        );
    }

    let tokens = if device {
        login::run_device_code_auth().await?
    } else {
        login::run_device_auth().await?
    };
    let (auth_val, new_info) = build_auth_from_tokens(&tokens);
    auth::write_auth(&dst, &auth_val)?;

    if profile::read_current() == alias {
        let live = auth::codex_auth_path()?;
        auth::backup_auth(&live)?;
        auth::write_auth(&live, &auth_val)?;
    }

    if json {
        print_json(&output::JsonOk {
            ok: true,
            alias: alias.to_string(),
            action: "reauthed".into(),
        });
    } else {
        println!(
            "{}",
            color::success(&format!(
                "[ok] Profile '{}' re-authorized (account: {})",
                alias,
                new_info.email.as_deref().unwrap_or("unknown")
            ))
        );
    }
    Ok(())
}

// ── best (internal, called by `use` with no alias) ────────

fn score_profile_candidates(
    fetched: Vec<(String, usage::UsageInfo)>,
    now: i64,
    safety_7d: f64,
    team_priority: bool,
) -> Vec<(usage::Candidate, usage::UsageInfo, f64)> {
    let pool_size = fetched.len();

    let mut candidates: Vec<(usage::Candidate, usage::UsageInfo)> = fetched
        .into_iter()
        .map(|(alias, u)| {
            let info = profile::profile_auth_path(&alias)
                .map(|p| auth::read_account_info(&p))
                .unwrap_or_default();
            let last_used = cache::get_last_used(&alias);
            let mut candidate = usage::Candidate::from_usage(
                alias,
                &u,
                info.is_team(),
                info.is_free(),
                last_used,
                now,
            );
            candidate.pool_size = pool_size;
            candidate.team_priority = team_priority;
            (candidate, u)
        })
        .collect();

    let pool_exhausted = candidates
        .iter()
        .filter(|(candidate, _)| candidate.effective_used_5h() >= 100.0)
        .count();
    for (candidate, _) in &mut candidates {
        candidate.pool_exhausted = pool_exhausted;
    }

    let mut scored: Vec<(usage::Candidate, usage::UsageInfo, f64)> = candidates
        .into_iter()
        .map(|(candidate, usage)| {
            let score = usage::score_unified(&candidate, safety_7d);
            (candidate, usage, score)
        })
        .collect();

    scored.sort_by(|a, b| {
        let eligible_a = usage::is_candidate_eligible(&a.0, safety_7d);
        let eligible_b = usage::is_candidate_eligible(&b.0, safety_7d);
        eligible_b
            .cmp(&eligible_a)
            .then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.0.last_used.cmp(&b.0.last_used))
            .then(a.0.alias.cmp(&b.0.alias))
    });

    scored
}

async fn select_best_profile(json: bool) -> Result<(String, usage::UsageInfo, f64)> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        anyhow::bail!("no saved profiles");
    }

    let current = profile::read_current();
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        config::get().network.max_concurrent,
    ));

    let mut tasks = tokio::task::JoinSet::new();
    let mut fetched: Vec<(String, usage::UsageInfo)> = Vec::with_capacity(profiles.len());

    for alias in profiles {
        if let Some(cached) = cache::get(&alias) {
            fetched.push((alias, cached));
            continue;
        }

        let current = current.clone();
        let sem = semaphore.clone();
        tasks.spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                return None;
            };
            let path = match profile::profile_auth_path(&alias) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("[{alias}] failed to resolve profile path: {e}");
                    return None;
                }
            };
            match usage::fetch_usage_retried(&alias, &path, &current).await {
                Ok(u) => Some((alias, u)),
                Err(e) => {
                    tracing::warn!("[{alias}] usage fetch failed during auto-select: {e}");
                    None
                }
            }
        });
    }

    let mut progress = if json {
        None
    } else {
        Some(ProgressReporter::new("Testing accounts", tasks.len()))
    };

    let mut completed = 0usize;
    while let Some(task) = tasks.join_next().await {
        completed += 1;
        if let Some(progress) = progress.as_mut() {
            progress.advance(completed);
        }
        if let Some((alias, usage)) =
            task.map_err(|e| anyhow::anyhow!("usage worker failed: {e}"))?
        {
            fetched.push((alias, usage));
        }
    }

    if let Some(progress) = progress.as_mut() {
        progress.finish();
    }

    if fetched.is_empty() {
        anyhow::bail!("all usage queries failed");
    }

    let safety_7d = config::get().use_cfg.safety_margin_7d;
    let team_priority = config::get().use_cfg.team_priority;
    let now = auth::now_unix_secs();
    let scored = score_profile_candidates(fetched, now, safety_7d, team_priority);
    let (best_candidate, best_usage, best_score) = scored
        .into_iter()
        .next()
        .context("failed to select best profile")?;

    Ok((best_candidate.alias, best_usage, best_score))
}

async fn best_cmd(json: bool) -> Result<()> {
    let (best_alias, best_usage, best_score) = select_best_profile(json).await?;

    profile::switch_profile(&best_alias)?;
    cache::set_last_used(&best_alias)?;

    let path = profile::profile_auth_path(&best_alias)?;
    let info = auth::read_account_info(&path);

    if json {
        print_json(&output::JsonBest {
            switched_to: best_alias.clone(),
            account: account_to_json(&info),
            usage: usage_to_json(Ok(&best_usage)),
            score: best_score,
            mode: "unified".to_string(),
        });
    } else {
        println!(
            "{}",
            color::success(&format!("Switched to: {best_alias}"))
        );
        print_usage_line(&best_usage);
    }

    // Opportunistically refresh tokens about to expire (background, bounded)
    usage::refresh_expiring_tokens().await;

    Ok(())
}

async fn launch_cmd(alias: Option<&str>, args: Vec<String>, json: bool) -> Result<()> {
    let target_alias = match alias {
        Some(alias) => {
            let profiles = profile::list_profiles()?;
            if !profiles.iter().any(|profile| profile == alias) {
                anyhow::bail!("Profile '{}' not found", alias);
            }
            alias.to_string()
        }
        None => {
            let (alias, _, _) = select_best_profile(json).await?;
            alias
        }
    };

    match std::process::Command::new("codex")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(_) => {}
        Err(_) => anyhow::bail!("codex not found in PATH. Install: npm install -g @openai/codex"),
    }

    let codex_auth = auth::codex_auth_path()?;
    let backup = codex_auth.with_extension("json.bak");
    let had_original = codex_auth.exists();

    struct AuthGuard {
        codex_auth: PathBuf,
        backup: PathBuf,
        had_original: bool,
    }

    impl Drop for AuthGuard {
        fn drop(&mut self) {
            if self.had_original {
                // Use copy + remove instead of rename for cross-platform safety
                // (rename fails on Windows when the target file already exists)
                let _ = std::fs::copy(&self.backup, &self.codex_auth);
                let _ = std::fs::remove_file(&self.backup);
            } else {
                let _ = std::fs::remove_file(&self.codex_auth);
            }
        }
    }

    if had_original {
        std::fs::copy(&codex_auth, &backup)
            .with_context(|| format!("backing up {}", codex_auth.display()))?;
    }

    let guard = AuthGuard {
        codex_auth: codex_auth.clone(),
        backup: backup.clone(),
        had_original,
    };

    profile::stage_profile_auth(&target_alias)?;

    if !json {
        user_println(&format!("Launching codex with profile '{target_alias}'..."));
    }

    let status = std::process::Command::new("codex")
        .args(&args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context("Failed to start codex")?;

    let exit_code = status.code().unwrap_or(-1);

    drop(guard);

    if json {
        print_json(&serde_json::json!({
            "ok": status.success(),
            "alias": target_alias,
            "action": "launched",
            "exit_code": exit_code,
        }));
    } else {
        user_println("codex exited, restored auth to previous state");
    }

    // Propagate codex exit code
    if exit_code != 0 {
        std::process::exit(exit_code);
    }

    Ok(())
}

// ── import ───────────────────────────────────────────────

async fn import_cmd(path: &str, alias: Option<&str>, json: bool) -> Result<()> {
    let input = std::path::PathBuf::from(path);
    let files = profile::collect_import_files(&input)?;

    if input.is_dir() {
        if let Some(alias) = alias {
            anyhow::bail!(
                "alias '{alias}' can only be used when importing a single file, not a directory"
            );
        }
        if files.is_empty() {
            anyhow::bail!("no JSON files found under {}", input.display());
        }
    }

    if files.len() == 1 && input.is_file() {
        let imported = match import_one_file(&files[0], alias).await {
            Ok(imported) => imported,
            Err(failure) => anyhow::bail!("{}: {}", failure.stage, failure.error),
        };
        if json {
            print_json(&output::JsonOk {
                ok: true,
                alias: imported.alias,
                action: imported.action.to_string(),
            });
        } else {
            println!(
                "{}",
                color::success(&format!(
                    "Validated and {}: {} -> profile '{}'",
                    imported.action,
                    imported.source.display(),
                    imported.alias
                ))
            );
            print!("  ");
            print_usage_line(&imported.usage);
        }
        return Ok(());
    }

    let mut report = profile::ImportReport::default();
    let mut progress = if json {
        None
    } else {
        Some(ProgressReporter::new("Validating auth files", files.len()))
    };

    for (idx, file) in files.into_iter().enumerate() {
        match import_one_file(&file, None).await {
            Ok(success) => report.imported.push(success),
            Err(failure) => report.skipped.push(failure),
        }
        if let Some(progress) = progress.as_mut() {
            progress.advance(idx + 1);
        }
    }

    if let Some(progress) = progress.as_mut() {
        progress.finish();
    }

    if json {
        print_json(&output::JsonImportReport {
            imported: report
                .imported
                .iter()
                .map(|item| output::JsonImportEntry {
                    source: item.source.display().to_string(),
                    alias: item.alias.clone(),
                    action: item.action.to_string(),
                    account: account_to_json(&item.account),
                    usage: usage_to_json(Ok(&item.usage)),
                })
                .collect(),
            skipped: report
                .skipped
                .iter()
                .map(|item| output::JsonImportFailure {
                    source: item.source.display().to_string(),
                    stage: item.stage.to_string(),
                    error: item.error.clone(),
                })
                .collect(),
        });
    } else {
        println!(
            "{}",
            color::success(&format!(
                "Imported {} profile(s); skipped {} file(s)",
                report.imported.len(),
                report.skipped.len()
            ))
        );

        for item in &report.imported {
            println!(
                "  {} {} -> {} ({})",
                color::status_tag("OK"),
                item.source.display(),
                item.alias,
                item.action
            );
            print!("    ");
            print_usage_line(&item.usage);
        }

        for item in &report.skipped {
            println!(
                "  {} {} [{}] {}",
                color::status_tag("Skip"),
                item.source.display(),
                item.stage,
                item.error
            );
        }
    }
    Ok(())
}

async fn import_one_file(
    source: &std::path::Path,
    alias: Option<&str>,
) -> std::result::Result<profile::ImportSuccess, profile::ImportFailure> {
    let mut val = auth::read_auth(source).map_err(|e| profile::ImportFailure {
        source: source.to_path_buf(),
        stage: "file_format",
        error: e.to_string(),
    })?;

    auth::validate_auth_value(&val).map_err(|e| profile::ImportFailure {
        source: source.to_path_buf(),
        stage: "structure",
        error: e.to_string(),
    })?;

    let (usage, _) =
        usage::validate_import_auth(&mut val)
            .await
            .map_err(|e| profile::ImportFailure {
                source: source.to_path_buf(),
                stage: "usage_validation",
                error: e.to_string(),
            })?;

    let account = auth::validate_auth_value(&val).map_err(|e| profile::ImportFailure {
        source: source.to_path_buf(),
        stage: "structure",
        error: e.to_string(),
    })?;

    let action =
        profile::save_imported_auth_value(val, alias).map_err(|e| profile::ImportFailure {
            source: source.to_path_buf(),
            stage: "save",
            error: e.to_string(),
        })?;

    Ok(profile::ImportSuccess {
        source: source.to_path_buf(),
        alias: action.alias().to_string(),
        action: action.action(),
        account,
        usage,
    })
}

// ── self-update ──────────────────────────────────────────

async fn self_update_cmd(
    check: bool,
    version: Option<&str>,
    dev: bool,
    stable: bool,
    json: bool,
) -> Result<()> {
    // Resolve the effective channel:
    // --dev → dev, --stable → stable, otherwise auto-detect from current version.
    let use_dev = if dev {
        true
    } else if stable || version.is_some() {
        false
    } else {
        update::is_dev_version(update::current_version())
    };

    if check {
        let current_version = update::current_version().to_string();
        let result = if use_dev {
            update::check_for_dev_update().await?
        } else {
            update::check_for_update(true).await?
        };

        if json {
            let (latest_version, update_available, install_source) = match &result {
                Some(info) => (
                    info.latest_version.clone(),
                    true,
                    info.install_source.as_str().to_string(),
                ),
                None => (
                    current_version.clone(),
                    false,
                    update::detect_install_source().as_str().to_string(),
                ),
            };
            print_json(&output::JsonSelfUpdate {
                ok: true,
                current_version,
                latest_version,
                update_available,
                updated: false,
                install_source,
                action: "checked".into(),
            });
            return Ok(());
        }

        let channel_label = if use_dev { " (dev)" } else { "" };
        match result {
            Some(info) => {
                let hint = if use_dev {
                    if info.install_source == update::InstallSource::Homebrew
                        && !update::is_dev_version(&info.current_version)
                    {
                        "brew uninstall codex-switch && codex-switch self-update --dev"
                    } else if dev {
                        // Explicit --dev flag: include it in the hint.
                        "codex-switch self-update --dev"
                    } else {
                        // Already on dev (auto-detected): plain self-update stays in dev.
                        "codex-switch self-update"
                    }
                } else if stable {
                    "codex-switch self-update --stable"
                } else {
                    info.install_source.upgrade_hint()
                };
                println!(
                    "{}",
                    color::warn(&format!(
                        "New version available{channel_label}: v{} (current v{}). Run `{hint}`.",
                        info.latest_version, info.current_version,
                    ))
                );
            }
            None => {
                println!(
                    "{}",
                    color::success(&format!(
                        "Already up to date{channel_label}: v{}",
                        update::current_version()
                    ))
                );
            }
        }
        return Ok(());
    }

    let show_progress = !json && update::should_show_download_progress();
    let result = if use_dev {
        update::self_update_dev(show_progress).await?
    } else {
        update::self_update(version, show_progress).await?
    };

    if json {
        print_json(&output::JsonSelfUpdate {
            ok: true,
            current_version: result.current_version.clone(),
            latest_version: result.latest_version.clone(),
            update_available: result.updated,
            updated: result.updated,
            install_source: result.install_source.as_str().to_string(),
            action: if result.updated {
                "updated".into()
            } else {
                "up_to_date".into()
            },
        });
        return Ok(());
    }

    if result.updated {
        let channel_label = if use_dev { " (dev)" } else { "" };
        println!(
            "{}",
            color::success(&format!(
                "Updated codex-switch{channel_label}: v{} -> v{}",
                result.current_version, result.latest_version
            ))
        );
        if dev && !update::is_dev_version(&result.current_version) {
            user_println(&color::dim(
                "Switched to dev channel. Run `codex-switch self-update --stable` to return.",
            ));
        } else if stable && update::is_dev_version(&result.current_version) {
            user_println(&color::dim(
                "Switched back to stable channel.",
            ));
        }
    } else {
        println!(
            "{}",
            color::success(&format!("Already up to date: v{}", result.current_version))
        );
    }

    Ok(())
}

// ── open ─────────────────────────────────────────────────

fn open_cmd() -> Result<()> {
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

// ── text output helpers ───────────────────────────────────

fn term_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
}

/// Render a progress bar without outer brackets.
/// `=` for used portion, `-` for remaining, `|` for pace marker.
fn render_progress_bar(used_pct: f64, pace_pct: Option<f64>, bar_width: usize) -> String {
    let used_pos = ((used_pct / 100.0) * bar_width as f64)
        .round()
        .clamp(0.0, bar_width as f64) as usize;
    let pace_pos = pace_pct.map(|p| {
        ((p / 100.0) * bar_width as f64)
            .round()
            .clamp(0.0, (bar_width.saturating_sub(1)) as f64) as usize
    });

    let mut bar = String::with_capacity(bar_width);
    for i in 0..bar_width {
        if pace_pos == Some(i) {
            bar.push('|');
        } else if i < used_pos {
            bar.push('=');
        } else {
            bar.push('-');
        }
    }
    bar
}

/// Format relative reset time: "~2h17m" or "~4d18h"
fn format_reset_short_relative(w: &usage::WindowUsage) -> String {
    let Some(resets_at) = w.resets_at else {
        return "--".into();
    };
    let remaining_secs = (resets_at - crate::auth::now_unix_secs()).max(0) as u64;
    if remaining_secs == 0 {
        return "expired".into();
    }
    if remaining_secs < 3600 {
        format!("~{}m", remaining_secs / 60)
    } else if remaining_secs < 86400 {
        format!("~{}h{}m", remaining_secs / 3600, (remaining_secs % 3600) / 60)
    } else {
        format!("~{}d{}h", remaining_secs / 86400, (remaining_secs % 86400) / 3600)
    }
}

fn print_usage_line(u: &usage::UsageInfo) {
    let width = term_width();
    // Each line: "  5h  bar  XXX% left  ~Xh" ≈ bar_width + 30
    let bar_width = if width >= 80 {
        16
    } else if width >= 60 {
        10
    } else {
        6
    };

    if let Some(w) = &u.primary {
        let pct = w.used_percent.unwrap_or(0.0);
        let remaining_pct = (100.0 - pct).max(0.0);
        let pace = usage::visible_pace_percent(w, usage::WINDOW_5H_SECS);
        let bar = render_progress_bar(pct, pace, bar_width);
        let reset = format_reset_short_relative(w);
        println!(
            "  5h  {}  {}   {}",
            color::usage_pct(&bar, pct),
            color::usage_pct(&format!("{remaining_pct:>3.0}% left"), pct),
            color::dim(&reset),
        );
    }
    if let Some(w) = &u.secondary {
        let pct = w.used_percent.unwrap_or(0.0);
        let remaining_pct = (100.0 - pct).max(0.0);
        let pace = usage::visible_pace_percent(w, usage::WINDOW_7D_SECS);
        let bar = render_progress_bar(pct, pace, bar_width);
        let reset = format_reset_short_relative(w);
        println!(
            "  7d  {}  {}   {}",
            color::usage_pct(&bar, pct),
            color::usage_pct(&format!("{remaining_pct:>3.0}% left"), pct),
            color::dim(&reset),
        );
    }
    if let Some(balance) = u.credits_balance {
        let unlimited = u.unlimited_credits == Some(true);
        let text = if unlimited {
            "credits: unlimited".to_string()
        } else {
            format!("credits: ${balance:.2}")
        };
        println!("  {}", color::credits(&text, balance, unlimited));
    }
}

// ── warmup ────────────────────────────────────────────────

async fn warmup_cmd(alias: Option<&str>, json: bool) -> Result<()> {
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

    // Filter out accounts whose 5h window is genuinely active (has usage AND reset in future).
    // The usage API returns resets_at on every call, so resets_at > now alone is not enough;
    // we also require used_percent > 0 to confirm the window was actually activated.
    let now = auth::now_unix_secs();
    let mut to_warmup = Vec::new();
    for alias in &aliases {
        let already_active = cache::get(alias)
            .and_then(|u| u.primary)
            .is_some_and(|w| {
                w.resets_at.is_some_and(|t| t > now)
                    && w.used_percent.is_some_and(|p| p > 0.0)
            });
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
                a["alias"].as_str().unwrap_or("").cmp(b["alias"].as_str().unwrap_or(""))
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
                    results.push(serde_json::json!({"alias": alias, "ok": false, "error": e.to_string()}));
                }
                had_error = true;
                continue;
            }
        };
        let sem = semaphore.clone();
        tasks.spawn(async move {
            let _permit = sem.acquire().await;
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
                    user_println(&format!("  {} {}", color::success(&alias), color::dim("warmed up")));
                }
            }
            Err(e) => {
                if json {
                    results.push(serde_json::json!({"alias": alias, "ok": false, "error": e.to_string()}));
                } else {
                    user_println(&format!("  {} failed: {}", color::error(&alias), e));
                }
                had_error = true;
            }
        }
    }

    if json {
        results.sort_by(|a, b| {
            a["alias"].as_str().unwrap_or("").cmp(b["alias"].as_str().unwrap_or(""))
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
