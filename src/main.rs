mod auth;
mod cache;
mod cli;
#[allow(dead_code)]
mod color;
mod config;
mod error;
mod jwt;
mod login;
mod output;
mod process;
mod profile;
mod tui;
mod update;
mod usage;

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Commands};
use output::{
    MessageMode, ProgressReporter, account_to_json, format_reset_time, print_error, print_json,
    usage_to_json, user_println,
};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // --debug sets tracing to debug level; otherwise respect RUST_LOG env
    let filter = if cli.debug {
        EnvFilter::new("codex_switch=debug")
    } else {
        EnvFilter::from_default_env()
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
    match cmd {
        Commands::Use { alias, force } => use_cmd(alias.as_deref(), force, json).await?,
        Commands::List { force } => list_cmd(force, json).await?,
        Commands::Delete { alias } => delete_cmd(&alias, json)?,
        Commands::Login { alias, device } => login_cmd(alias.as_deref(), device, json).await?,
        Commands::Import { path, alias } => import_cmd(&path, alias.as_deref(), json).await?,
        Commands::SelfUpdate { check, version } => {
            self_update_cmd(check, version.as_deref(), json).await?
        }
        Commands::Tui => tui::run_tui().await?,
        Commands::Open => open_cmd()?,
    }
    Ok(())
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

async fn list_cmd(force: bool, json: bool) -> Result<()> {
    profile::auto_track_current();

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
        .map(|name| {
            let path = profile::profile_auth_path(&name);
            let info = auth::read_account_info(&path);
            let usage_result = if force {
                None
            } else {
                cache::get(&name).map(Ok)
            };
            ListRow {
                is_current: name == current,
                name,
                info,
                usage_result,
            }
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
                return (idx, Err(usage::UsageError {
                    summary: "limiter closed".into(),
                    detail: "usage limiter closed".into(),
                }));
            };
            let path = profile::profile_auth_path(&alias);
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
        let usage_result = row
            .usage_result
            .unwrap_or_else(|| Err(usage::UsageError {
                summary: "unknown".into(),
                detail: "usage result missing".into(),
            }));
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
                print!(" {}", color::dim(&format!("({email})")));
            }
            if row.info.plan_type.is_some() {
                print!(
                    "  {}",
                    color::plan(&row.info.plan_label(), row.info.plan_type.as_deref())
                );
            }
            println!();
            match usage_result {
                Ok(u) => {
                    let tag = if usage::is_available(&u) {
                        "OK"
                    } else {
                        "Limited"
                    };
                    print!("  {} ", color::status_tag(tag));
                    print_usage_line(&u);
                }
                Err(e) => println!("  {} {}", color::status_tag("Error"), color::error(&e.summary)),
            }
        }
    }

    if json {
        print_json(&output::JsonUsageResult {
            profiles: json_items,
        });
    }
    Ok(())
}

// ── delete ───────────────────────────────────────────────

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
        let dst = profile::profile_auth_path(a);
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
                    color::success(&format!("✓ Logged in — saved as new profile: {a}"))
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
                    color::success(&format!("✓ Logged in — updated existing profile: {a}"))
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
    let dst = profile::profile_auth_path(alias);
    let old_info = auth::read_account_info(&dst);

    if !json {
        println!(
            "Re-authorizing profile '{}' ({})…",
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
        let live = auth::codex_auth_path();
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
                "✓ Profile '{}' re-authorized (account: {})",
                alias,
                new_info.email.as_deref().unwrap_or("unknown")
            ))
        );
    }
    Ok(())
}

// ── best (internal, called by `use` with no alias) ────────

async fn best_cmd(json: bool) -> Result<()> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        if json {
            print_error("no saved profiles");
        } else {
            println!("{}", color::dim("(no saved profiles)"));
        }
        return Ok(());
    }

    let current = profile::read_current();
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(
        config::get().network.max_concurrent,
    ));

    let mut tasks = tokio::task::JoinSet::new();
    let mut scored: Vec<(String, usage::UsageInfo, f64)> = Vec::with_capacity(profiles.len());

    for alias in profiles {
        if let Some(cached) = cache::get(&alias) {
            let score = usage::score(&cached);
            scored.push((alias, cached, score));
            continue;
        }

        let current = current.clone();
        let sem = semaphore.clone();
        tasks.spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                return None;
            };
            let path = profile::profile_auth_path(&alias);
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
            let score = usage::score(&usage);
            scored.push((alias, usage, score));
        }
    }

    if let Some(progress) = progress.as_mut() {
        progress.finish();
    }

    if scored.is_empty() {
        if json {
            print_error("all usage queries failed");
        } else {
            println!("{}", color::error("All usage queries failed"));
        }
        return Ok(());
    }

    scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    let (best_alias, best_usage, best_score) = scored.remove(0);

    profile::switch_profile(&best_alias)?;

    let path = profile::profile_auth_path(&best_alias);
    let info = auth::read_account_info(&path);

    if json {
        print_json(&output::JsonBest {
            switched_to: best_alias.clone(),
            account: account_to_json(&info),
            usage: usage_to_json(Ok(&best_usage)),
            score: best_score,
        });
    } else {
        println!("{}", color::success(&format!("Switched to: {best_alias}")));
        print!("  ");
        print_usage_line(&best_usage);
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
                    "Validated and {}: {} → profile '{}'",
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
                "  {} {} → {} ({})",
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

async fn self_update_cmd(check: bool, version: Option<&str>, json: bool) -> Result<()> {
    if check {
        let current_version = update::current_version().to_string();
        let result = update::check_for_update(true).await?;

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

        match result {
            Some(info) => {
                println!(
                    "{}",
                    color::warn(&format!(
                        "New version available: v{} (current v{}). Run `{}`.",
                        info.latest_version,
                        info.current_version,
                        info.install_source.upgrade_hint()
                    ))
                );
            }
            None => {
                println!(
                    "{}",
                    color::success(&format!(
                        "Already up to date: v{}",
                        update::current_version()
                    ))
                );
            }
        }
        return Ok(());
    }

    let result =
        update::self_update(version, !json && update::should_show_download_progress()).await?;

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
        println!(
            "{}",
            color::success(&format!(
                "Updated codex-switch: v{} → v{}",
                result.current_version, result.latest_version
            ))
        );
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
    let dir = auth::app_home();
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

fn print_usage_line(u: &usage::UsageInfo) {
    if let Some(w) = &u.primary {
        let pct = w.used_percent.unwrap_or(0.0);
        let pct_str = format!("{pct:.0}%");
        let remaining = w.resets_at.map(|ts| ts - crate::auth::now_unix_secs()).unwrap_or(0);
        let reset = w
            .resets_at
            .map(format_reset_time)
            .unwrap_or_else(|| "unknown".into());
        let reset_colored = color::reset_time(&format!("(resets: {reset})"), remaining);
        print!("5h {} used {reset_colored}", color::usage_pct(&pct_str, pct));
    }
    if let Some(w) = &u.secondary {
        let pct = w.used_percent.unwrap_or(0.0);
        let pct_str = format!("{pct:.0}%");
        let remaining = w.resets_at.map(|ts| ts - crate::auth::now_unix_secs()).unwrap_or(0);
        let reset = w
            .resets_at
            .map(format_reset_time)
            .unwrap_or_else(|| "unknown".into());
        let reset_colored = color::reset_time(&format!("(resets: {reset})"), remaining);
        print!("  7d {} used {reset_colored}", color::usage_pct(&pct_str, pct));
    }
    if let Some(balance) = u.credits_balance {
        let unlimited = u.unlimited_credits == Some(true);
        let text = if unlimited {
            "credits: unlimited".to_string()
        } else {
            format!("credits: ${balance:.2}")
        };
        print!("  {}", color::credits(&text, balance, unlimited));
    }
    println!();
}
