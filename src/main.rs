mod auth;
mod cache;
mod cli;
mod color;
mod commands;
mod config;
mod daemon;
mod error;
mod http_retry;
mod jwt;
mod launch;
mod logging;
mod login;
mod output;
mod profile;
mod provider;
mod signals;
mod tui;
mod update;
mod usage;
mod warmup;
mod workspace;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands, extract_launch_passthrough, merge_launch_args};
use output::{MessageMode, print_error, should_report_error, user_println};
use tracing_subscriber::EnvFilter;

/// The post-command profile re-sync (see `dispatch`) is best-effort: most failures
/// (profile deleted mid-command, unreadable auth.json, IO errors) are expected and stay
/// silent, same as before. The one case that must not be silent is
/// `ensure_live_not_older` refusing to overwrite a profile with older/unstamped live
/// credentials — that guard is protecting a single-use refresh token from being
/// destroyed, and its own error message already carries both timestamps and the
/// actionable next step, so surfacing it is just choosing to show information already
/// computed.
fn is_resync_freshness_guard_rejection(error: &anyhow::Error) -> bool {
    error.downcast_ref::<profile::StaleLiveAuth>().is_some()
}

/// Build the confirmation prompt for syncing live `auth.json` credentials back into a
/// profile, showing both timestamps so the user can tell a normal "codex just logged in"
/// sync from a direction that looks wrong before hitting enter (default remains Yes).
fn format_resync_confirm_prompt(
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

/// Best-effort read of the `last_refresh` field from an auth.json at `path`.
fn read_last_refresh(path: Result<std::path::PathBuf>) -> Option<String> {
    let val = auth::read_auth(&path.ok()?).ok()?;
    val.get("last_refresh")?.as_str().map(str::to_string)
}

#[tokio::main]
async fn main() {
    let raw: Vec<String> = std::env::args().collect();
    let (clap_argv, launch_passthrough) = extract_launch_passthrough(&raw);
    let cli = Cli::parse_from(&clap_argv);
    let use_json = cli.json || cli.json_pretty;
    let message_mode = if matches!(&cli.command, Commands::Tui) {
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
    if let Some(file_writer) = file_writer {
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

#[cfg(test)]
mod error_reporting_tests {
    use super::should_report_error;
    use crate::output::OutputAlreadyReported;

    #[test]
    fn already_reported_errors_are_not_printed_or_logged_again() {
        assert!(!should_report_error(&OutputAlreadyReported.into()));
        assert!(should_report_error(&anyhow::anyhow!("new failure")));
    }
}

#[cfg(test)]
mod resync_reporting_tests {
    use super::{format_resync_confirm_prompt, is_resync_freshness_guard_rejection};

    #[test]
    fn freshness_guard_rejection_for_older_live_is_reported() {
        let error = anyhow::Error::from(crate::profile::StaleLiveAuth {
            alias: "acme".to_string(),
            live: "2026-01-01T00:00:00Z".to_string(),
            profile: "2026-02-01T00:00:00Z".to_string(),
        });
        assert!(is_resync_freshness_guard_rejection(&error));
    }

    #[test]
    fn freshness_guard_rejection_survives_added_context() {
        // The re-sync path may wrap the refusal before it reaches the reporter;
        // downcast has to see through that, which a message-prefix match would not.
        let error = anyhow::Error::from(crate::profile::StaleLiveAuth {
            alias: "acme".to_string(),
            live: "no last_refresh".to_string(),
            profile: "2026-02-01T00:00:00Z".to_string(),
        })
        .context("syncing profile 'acme' after the command");
        assert!(is_resync_freshness_guard_rejection(&error));
    }

    #[test]
    fn a_message_that_merely_looks_like_the_guard_is_not_treated_as_one() {
        // Before this was typed, any error whose text began with "live auth.json"
        // was reported as the guard firing.
        let lookalike = anyhow::anyhow!("live auth.json could not be read: permission denied");
        assert!(!is_resync_freshness_guard_rejection(&lookalike));
    }

    #[test]
    fn unrelated_resync_errors_stay_silent() {
        let identity_mismatch =
            anyhow::anyhow!("authenticated account does not match profile 'acme'");
        assert!(!is_resync_freshness_guard_rejection(&identity_mismatch));

        let missing_profile = anyhow::anyhow!("profile 'acme' does not exist");
        assert!(!is_resync_freshness_guard_rejection(&missing_profile));
    }

    #[test]
    fn confirm_prompt_shows_direction_and_both_timestamps() {
        let prompt = format_resync_confirm_prompt(
            "acme",
            Some("2026-07-20T00:00:00Z"),
            Some("2026-07-10T00:00:00Z"),
        );
        assert!(prompt.contains("acme"));
        assert!(prompt.contains("live"));
        assert!(prompt.contains("2026-07-20T00:00:00Z"));
        assert!(prompt.contains("profile"));
        assert!(prompt.contains("2026-07-10T00:00:00Z"));
        assert!(prompt.contains("[Y/n]"));
    }

    #[test]
    fn confirm_prompt_falls_back_to_unknown_when_timestamp_missing() {
        let prompt = format_resync_confirm_prompt("acme", None, None);
        assert!(prompt.contains("unknown"));
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
        Commands::Provider(sub) => commands::provider_cmd(sub, json)?,
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
            && is_resync_freshness_guard_rejection(&e)
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
            let prompt =
                format_resync_confirm_prompt(&alias, live_ts.as_deref(), profile_ts.as_deref());
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
