mod auth;
mod cli;
#[allow(dead_code)]
mod color;
mod config;
mod error;
mod jwt;
mod login;
mod output;
mod profile;
mod tui;
mod usage;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};
use output::{account_to_json, format_reset_time, print_error, print_json, usage_to_json};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let use_json = cli.json;

    config::init();
    config::set_cli_proxy(cli.proxy.clone());
    color::init(cli.color);

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
        Commands::Use { alias } => use_cmd(alias.as_deref(), json).await?,
        Commands::List => list_cmd(json)?,
        Commands::Delete { alias } => delete_cmd(&alias, json)?,
        Commands::Status => status_cmd(json).await?,
        Commands::Login { alias } => login_cmd(alias.as_deref(), json).await?,
        Commands::Import { file, alias } => import_cmd(&file, &alias, json)?,
        Commands::Tui => tui::run_tui().await?,
        Commands::Open => open_cmd()?,
    }
    Ok(())
}

// ── use ──────────────────────────────────────────────────

async fn use_cmd(alias: Option<&str>, json: bool) -> Result<()> {
    match alias {
        Some(a) => {
            profile::cmd_use(a)?;
            if json {
                print_json(&output::JsonOk { ok: true, alias: a.to_string(), action: "switched".into() });
            }
        }
        None => best_cmd(json).await?,
    }
    Ok(())
}

// ── list ─────────────────────────────────────────────────

fn list_cmd(json: bool) -> Result<()> {
    profile::auto_track_current();

    if !json {
        return profile::cmd_list();
    }
    let profiles = profile::list_profiles()?;
    let current = profile::read_current();
    let items: Vec<output::JsonProfile> = profiles
        .iter()
        .map(|alias| {
            let path = profile::profile_auth_path(alias);
            let info = auth::read_account_info(&path);
            output::JsonProfile {
                alias: alias.clone(),
                is_current: alias == &current,
                account: account_to_json(&info),
            }
        })
        .collect();
    print_json(&output::JsonList {
        current: if current.is_empty() { None } else { Some(current) },
        count: items.len(),
        profiles: items,
    });
    Ok(())
}

// ── delete ───────────────────────────────────────────────

fn delete_cmd(alias: &str, json: bool) -> Result<()> {
    profile::cmd_delete(alias)?;
    if json {
        print_json(&output::JsonOk { ok: true, alias: alias.to_string(), action: "deleted".into() });
    }
    Ok(())
}

// ── status (all profiles + usage, concurrent) ────────────

async fn status_cmd(json: bool) -> Result<()> {
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

    if !json {
        eprint!("{}", color::dim(&format!("Fetching usage for {} account(s)…", profiles.len())));
    }

    let tasks: Vec<_> = profiles
        .iter()
        .map(|name| {
            let name = name.clone();
            let current = current.clone();
            async move {
                let path = profile::profile_auth_path(&name);
                let info = auth::read_account_info(&path);
                let usage_result = usage::fetch_usage_retried(&name, &path, &current).await;
                (name.clone(), name == current, info, usage_result)
            }
        })
        .collect();

    let results = futures::future::join_all(tasks).await;

    if !json {
        // Clear the "Fetching..." line
        eprint!("\r{}\r", " ".repeat(60));
    }
    let mut json_items = vec![];

    for (name, is_current, info, usage_result) in results {
        if json {
            let ju = match &usage_result {
                Ok(u) => usage_to_json(Ok(u)),
                Err(e) => usage_to_json(Err(e.as_str())),
            };
            json_items.push(output::JsonProfileWithUsage {
                alias: name,
                is_current,
                account: account_to_json(&info),
                usage: ju,
            });
        } else {
            let mark = if is_current { color::active("*") } else { " ".to_string() };
            let alias_str = if is_current { color::bold(&name) } else { name.clone() };
            print!("{mark} {alias_str}");
            if let Some(email) = &info.email {
                print!(" {}", color::dim(&format!("({email})")));
            }
            if info.plan_type.is_some() {
                print!("  {}", color::plan(&info.plan_label(), info.plan_type.as_deref()));
            }
            println!();
            match usage_result {
                Ok(u) => {
                    let tag = if usage::is_available(&u) { "OK" } else { "Limited" };
                    print!("  {} ", color::status_tag(tag));
                    print_usage_line(&u);
                }
                Err(e) => println!("  {} fetch failed: {e}", color::status_tag("Error")),
            }
        }
    }

    if json {
        print_json(&output::JsonUsageResult { profiles: json_items });
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

async fn login_cmd(alias: Option<&str>, json: bool) -> Result<()> {
    if let Some(a) = alias {
        let dst = profile::profile_auth_path(a);
        if dst.exists() {
            return reauth_profile(a, json).await;
        }
    }

    let tokens = login::run_device_auth().await?;
    let (auth_val, _info) = build_auth_from_tokens(&tokens);

    match profile::save_auth_value(auth_val, alias)? {
        profile::SaveAction::Created(a) => {
            println!("{}", color::success(&format!("✓ Logged in — saved as new profile: {a}")));
            if json { print_json(&output::JsonOk { ok: true, alias: a, action: "created".into() }); }
        }
        profile::SaveAction::Updated(a) => {
            println!("{}", color::success(&format!("✓ Logged in — updated existing profile: {a}")));
            if json { print_json(&output::JsonOk { ok: true, alias: a, action: "updated".into() }); }
        }
    }
    Ok(())
}

async fn reauth_profile(alias: &str, json: bool) -> Result<()> {
    let dst = profile::profile_auth_path(alias);
    let old_info = auth::read_account_info(&dst);

    println!(
        "Re-authorizing profile '{}' ({})…",
        color::bold(alias),
        old_info.email.as_deref().unwrap_or("unknown email")
    );

    let tokens = login::run_device_auth().await?;
    let (auth_val, new_info) = build_auth_from_tokens(&tokens);
    auth::write_auth(&dst, &auth_val)?;

    if profile::read_current() == alias {
        let live = auth::codex_auth_path();
        auth::backup_auth(&live)?;
        auth::write_auth(&live, &auth_val)?;
    }

    if json {
        print_json(&output::JsonOk { ok: true, alias: alias.to_string(), action: "reauthed".into() });
    } else {
        println!("{}", color::success(&format!(
            "✓ Profile '{}' re-authorized (account: {})",
            alias,
            new_info.email.as_deref().unwrap_or("unknown")
        )));
    }
    Ok(())
}

// ── best (internal, called by `use` with no alias) ────────

async fn best_cmd(json: bool) -> Result<()> {
    let profiles = profile::list_profiles()?;
    if profiles.is_empty() {
        if json { print_error("no saved profiles"); }
        else { println!("{}", color::dim("(no saved profiles)")); }
        return Ok(());
    }

    if !json {
        eprint!("{}", color::dim(&format!("Querying usage for {} account(s)…", profiles.len())));
    }

    let current = profile::read_current();
    let tasks: Vec<_> = profiles
        .iter()
        .map(|alias| {
            let alias = alias.clone();
            let current = current.clone();
            async move {
                let path = profile::profile_auth_path(&alias);
                match usage::fetch_usage_retried(&alias, &path, &current).await {
                    Ok(u) => Some((alias, u)),
                    Err(_) => None,
                }
            }
        })
        .collect();

    let results: Vec<Option<(String, usage::UsageInfo)>> =
        futures::future::join_all(tasks).await;

    if !json {
        eprint!("\r{}\r", " ".repeat(60));
    }

    let mut scored: Vec<(String, usage::UsageInfo, f64)> = results
        .into_iter()
        .flatten()
        .map(|(alias, u)| { let s = usage::score(&u); (alias, u, s) })
        .collect();

    if scored.is_empty() {
        if json { print_error("all usage queries failed"); }
        else { println!("{}", color::error("All usage queries failed")); }
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

fn import_cmd(file: &str, alias: &str, json: bool) -> Result<()> {
    match profile::cmd_import(file, alias)? {
        profile::SaveAction::Created(a) => {
            if json { print_json(&output::JsonOk { ok: true, alias: a, action: "created".into() }); }
        }
        profile::SaveAction::Updated(a) => {
            if json { print_json(&output::JsonOk { ok: true, alias: a, action: "updated".into() }); }
        }
    }
    Ok(())
}

// ── open ─────────────────────────────────────────────────

fn open_cmd() -> Result<()> {
    let dir = auth::app_home();
    std::fs::create_dir_all(&dir)?;
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&dir).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer.exe").arg(dir.as_os_str()).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(&dir).spawn();
    match result {
        Ok(_) => println!("Opened: {}", dir.display()),
        Err(e) => println!("{}", color::error(&format!("Could not open file manager: {e}\nPath: {}", dir.display()))),
    }
    Ok(())
}

// ── text output helpers ───────────────────────────────────

fn print_usage_line(u: &usage::UsageInfo) {
    if let Some(w) = &u.primary {
        let pct = w.used_percent.unwrap_or(0.0);
        let pct_str = format!("{pct:.0}%");
        let reset = w.resets_at.map(format_reset_time).unwrap_or_else(|| "unknown".into());
        print!("5h {} used {}", color::usage_pct(&pct_str, pct), color::dim(&format!("(resets: {reset})")));
    }
    if let Some(w) = &u.secondary {
        let pct = w.used_percent.unwrap_or(0.0);
        let pct_str = format!("{pct:.0}%");
        let reset = w.resets_at.map(format_reset_time).unwrap_or_else(|| "unknown".into());
        print!("  7d {} used {}", color::usage_pct(&pct_str, pct), color::dim(&format!("(resets: {reset})")));
    }
    println!();
}
