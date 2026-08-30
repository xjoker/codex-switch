use crate::cli::{Cli, Commands, extract_launch_passthrough, merge_launch_args};
use crate::output::{MessageMode, print_error, should_report_error, user_println};
use crate::{auth, color, commands, config, daemon, logging, output, profile, tui};
use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

/// Best-effort read of the `last_refresh` field from an auth.json at `path`.
fn read_last_refresh(path: Result<std::path::PathBuf>) -> Option<String> {
    let val = auth::read_auth(&path.ok()?).ok()?;
    val.get("last_refresh")?.as_str().map(str::to_string)
}

pub async fn run_cli() {
    let raw: Vec<String> = std::env::args().collect();
    let (clap_argv, launch_passthrough) = extract_launch_passthrough(&raw);
    let cli = Cli::parse_from(&clap_argv);
    let is_tui = matches!(&cli.command, Commands::Tui);
    let use_json = cli.json || cli.json_pretty;
    let message_mode = if is_tui {
        MessageMode::Silent
    } else if use_json {
        MessageMode::Stderr
    } else {
        MessageMode::Stdout
    };

    color::init(cli.color);
    output::set_json_pretty(cli.json_pretty);
    output::set_message_mode(message_mode);
    if let Err(e) = config::init() {
        if use_json {
            print_error(&e.to_string());
        } else {
            eprintln!("{}", color::error(&format!("Error: {e}")));
        }
        std::process::exit(1);
    }

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
    // Keep diagnostic logs even when the daemon detaches and discards stdio.
    // File logging failure must not prevent normal account switching.
    let file_writer = match logging::file_log_writer() {
        Ok(writer) => Some(writer),
        Err(error) => {
            eprintln!(
                "{}",
                color::warn(&format!("Warning: file logging is unavailable: {error}"))
            );
            None
        }
    };
    if is_tui {
        use tracing_subscriber::fmt::writer::MakeWriterExt;
        let tui_writer = logging::tui_log_writer();
        if let Some(file_writer) = file_writer {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(tui_writer.and(file_writer))
                .init();
        } else {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(tui_writer)
                .init();
        }
    } else if let Some(file_writer) = file_writer {
        use tracing_subscriber::fmt::writer::MakeWriterExt;
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::io::stderr.and(file_writer))
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    }
    for warning in config::startup_warnings() {
        eprintln!("{}", color::warn(&format!("Warning: {warning}")));
    }
    config::set_cli_proxy(cli.proxy.clone());

    let result = dispatch(cli.command, use_json, launch_passthrough).await;

    if let Err(e) = result {
        if should_report_error(&e) {
            tracing::error!(error = %format!("{e:#}"), "command failed");
            if use_json {
                print_error(&format!("{e:#}"));
            } else {
                eprintln!("{}", color::error(&format!("Error: {e:#}")));
            }
        }
        std::process::exit(1);
    }
}

async fn dispatch(
    cmd: Commands,
    json: bool,
    launch_passthrough: Option<Vec<String>>,
) -> Result<()> {
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
        Commands::Use {
            alias,
            consume_card,
        } => commands::use_cmd(alias.as_deref(), json, consume_card).await?,
        Commands::List { force } => commands::list_cmd(force, json, auth_handled).await?,
        Commands::ResetCard { alias, yes } => commands::reset_card_cmd(&alias, yes, json).await?,
        Commands::Rename { old, new } => commands::rename_cmd(&old, &new, json)?,
        Commands::Delete { alias, yes } => commands::delete_cmd(&alias, yes, json)?,
        Commands::Login { alias, device } => {
            commands::login_cmd(alias.as_deref(), device, json).await?
        }
        Commands::Import { path, alias } => {
            commands::import_cmd(&path, alias.as_deref(), json).await?
        }
        Commands::SelfUpdate {
            check,
            version,
            dev,
            stable,
        } => commands::self_update_cmd(check, version.as_deref(), dev, stable, json).await?,
        Commands::Warmup { alias } => commands::warmup_cmd(alias.as_deref(), json).await?,
        Commands::Launch {
            alias,
            consume_card,
            model,
            args,
        } => {
            let args = merge_launch_args(args, launch_passthrough);
            commands::launch_cmd(alias.as_deref(), args, json, consume_card, model.as_deref())
                .await?
        }
        Commands::Tui => tui::run_tui().await?,
        Commands::Open => commands::open_cmd()?,
        Commands::Provider(sub) => commands::provider_cmd(sub, json).await?,
        Commands::Daemon(sub) => daemon::dispatch(sub, json).await?,
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
            && let Err(e) = profile::update_profile_from_live(&current)
            && e.downcast_ref::<profile::StaleLiveAuth>().is_some()
        {
            eprintln!(
                "{}",
                color::warn(&format!("Warning: post-command profile sync skipped: {e}"))
            );
        }
    }

    Ok(())
}

// ── startup auth change detection ────────────────────────

#[derive(Debug)]
enum AuthCheckResult {
    NoChange,
    Detected, // change detected but not synced (non-interactive or user declined)
    Synced,   // change detected and user accepted the sync
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
                let info = auth::codex_auth_path()
                    .map(|p| auth::read_account_info(&p))
                    .unwrap_or_default();
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
            let info = auth::codex_auth_path()
                .map(|p| auth::read_account_info(&p))
                .unwrap_or_default();
            let label = info.email.as_deref().unwrap_or("unknown");
            user_println(&format!(
                "Detected new account ({label}) in auth.json — not in any saved profile."
            ));
            if commands::confirm("Save as a new profile? [Y/n] ") {
                match profile::cmd_save(None) {
                    Ok(action) => {
                        user_println(&format!("Profile {}: {}", action.action(), action.alias()));
                        synced = true;
                    }
                    Err(e) => eprintln!("{}", color::error(&format!("Failed to save: {e}"))),
                }
            }
        }
        profile::AuthChange::TokensUpdated { alias } => {
            let info = auth::codex_auth_path()
                .map(|p| auth::read_account_info(&p))
                .unwrap_or_default();
            let label = info.email.as_deref().unwrap_or("unknown");
            user_println(&format!(
                "auth.json credentials changed for account '{alias}' ({label})."
            ));
            let live_ts = read_last_refresh(auth::codex_auth_path());
            let profile_ts = read_last_refresh(profile::profile_auth_path(&alias));
            let prompt = commands::format_resync_confirm_prompt(
                &alias,
                live_ts.as_deref(),
                profile_ts.as_deref(),
            );
            if commands::confirm(&prompt) {
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
