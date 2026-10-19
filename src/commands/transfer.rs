use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use zeroize::Zeroizing;

use super::render::confirm_default_no;
use crate::output::{print_json, user_println};
use crate::transfer::plan::{self, ExportOptions, Strategy};
use crate::transfer::{self, PLAINTEXT_WARNING, SINGLE_USE_WARNING};
use crate::{auth, color, profile};

fn warn(msg: &str) {
    eprintln!("{}", color::warn(&format!("Warning: {msg}")));
}

/// One line from stdin; only the trailing newline is removed so a passphrase
/// may contain spaces.
fn passphrase_from_stdin() -> Result<Zeroizing<String>> {
    let mut raw = Zeroizing::new(String::new());
    std::io::stdin()
        .read_to_string(&mut raw)
        .context("reading passphrase from stdin")?;
    let line = raw.lines().next().unwrap_or_default().to_string();
    Ok(Zeroizing::new(line))
}

fn read_passphrase(stdin_mode: bool, confirm: bool) -> Result<Zeroizing<String>> {
    let pass = if stdin_mode {
        passphrase_from_stdin()?
    } else if std::io::stdin().is_terminal() {
        let first = Zeroizing::new(
            rpassword::prompt_password("Bundle passphrase: ").context("reading passphrase")?,
        );
        if confirm {
            let second = Zeroizing::new(
                rpassword::prompt_password("Repeat passphrase: ").context("reading passphrase")?,
            );
            if *first != *second {
                anyhow::bail!("passphrases do not match");
            }
        }
        first
    } else {
        anyhow::bail!(
            "no interactive terminal for a hidden prompt; pass the passphrase on stdin with --passphrase-stdin"
        );
    };
    if pass.is_empty() {
        anyhow::bail!("passphrase cannot be empty");
    }
    Ok(pass)
}

#[derive(Serialize)]
struct JsonExport {
    ok: bool,
    path: String,
    encrypted: bool,
    profiles: Vec<String>,
    providers: Vec<String>,
    config: bool,
    source_current: Option<String>,
    warning: &'static str,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn export_cmd(
    file: Option<&Path>,
    plaintext: bool,
    yes: bool,
    force: bool,
    passphrase_stdin: bool,
    no_providers: bool,
    no_config: bool,
    json: bool,
) -> Result<()> {
    let path: PathBuf = file
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(transfer::default_export_name()));
    if !force && path.exists() {
        anyhow::bail!(
            "{} already exists; choose another path or pass --force",
            path.display()
        );
    }
    let (payload, summary) = plan::collect_export(ExportOptions {
        no_providers,
        no_config,
    })?;

    let passphrase = if plaintext {
        if passphrase_stdin {
            anyhow::bail!("--plaintext and --passphrase-stdin cannot be combined");
        }
        if !yes {
            if json || !std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "a plaintext export contains live tokens and API keys; rerun with --plaintext --yes to confirm"
                );
            }
            warn(PLAINTEXT_WARNING);
            if !confirm_default_no("Write an unencrypted bundle? [y/N] ") {
                return Err(crate::error::CsError::Aborted.into());
            }
        }
        None
    } else {
        Some(read_passphrase(passphrase_stdin, true)?)
    };

    let bytes = Zeroizing::new(transfer::encode_bundle(
        &payload,
        passphrase.as_ref().map(|p| p.as_str()),
    )?);
    transfer::write_private_file(&path, &bytes, force)?;

    warn(SINGLE_USE_WARNING);
    if plaintext {
        warn(PLAINTEXT_WARNING);
    }
    if json {
        print_json(&JsonExport {
            ok: true,
            path: path.display().to_string(),
            encrypted: !plaintext,
            profiles: summary.profiles,
            providers: summary.providers,
            config: summary.config,
            source_current: payload.source_current,
            warning: SINGLE_USE_WARNING,
        });
    } else {
        println!(
            "{}",
            color::success(&format!(
                "Exported {} profile(s), {} provider(s){} to {} ({})",
                summary.profiles.len(),
                summary.providers.len(),
                if summary.config {
                    " and config.toml"
                } else {
                    ""
                },
                path.display(),
                if plaintext {
                    "UNENCRYPTED"
                } else {
                    "encrypted"
                }
            ))
        );
        println!(
            "Import it on the other machine with `codex-switch import {}`.",
            path.display()
        );
    }
    Ok(())
}

pub(crate) struct ImportBundleArgs<'a> {
    pub path: &'a Path,
    pub strategy: Strategy,
    pub dry_run: bool,
    pub yes: bool,
    pub activate: Option<Option<String>>,
    pub passphrase_stdin: bool,
    pub replace_live: bool,
    pub json: bool,
}

#[derive(Serialize)]
struct JsonImportBundle<'a> {
    ok: bool,
    dry_run: bool,
    strategy: &'static str,
    encrypted: bool,
    items: &'a [plan::PlanItem],
    notices: &'a [String],
    source_current: Option<&'a str>,
    source_current_target: Option<&'a str>,
    backup_dir: Option<String>,
    activated: Option<String>,
    warning: &'static str,
}

fn print_plan(plan: &plan::ImportPlan) {
    println!(
        "{:<44} {:<16} {:<10} {:<20} REASON",
        "KEY", "KIND", "ACTION", "TARGET"
    );
    for item in &plan.items {
        let mut reason = item.reason.clone();
        if let Some(url) = &item.base_url {
            reason.push_str(&format!("; base_url {url}"));
        }
        if let Some(n) = item.codex_config_overrides.filter(|n| *n > 0) {
            reason.push_str(&format!(
                "; {n} codex_config override(s) passed to Codex on launch"
            ));
        }
        println!(
            "{:<44} {:<16} {:<10} {:<20} {}",
            item.key,
            item.kind,
            item.action.as_str(),
            item.target.as_deref().unwrap_or("-"),
            reason
        );
        if item.replaces_live && item.action.writes() {
            println!("{:<44} also replaces the live auth.json", "");
        }
    }
    for notice in &plan.notices {
        warn(notice);
    }
}

fn live_is_untracked() -> Result<bool> {
    let live = auth::codex_auth_path()?;
    Ok(live.exists() && profile::find_matching_profile(&live).is_none())
}

pub(crate) fn import_bundle_cmd(args: ImportBundleArgs<'_>) -> Result<()> {
    auth::ensure_file_credentials_store()?;
    let bundle = transfer::read_bundle(args.path)?;
    let encrypted = bundle.is_encrypted();
    let passphrase = if encrypted {
        Some(read_passphrase(args.passphrase_stdin, false)?)
    } else {
        warn(PLAINTEXT_WARNING);
        None
    };
    let payload = transfer::open_payload(&bundle, passphrase.as_ref().map(|p| p.as_str()))?;
    drop(passphrase);
    let preview = plan::classify(&payload, args.strategy, args.replace_live)?;

    if args.dry_run {
        if let Some(w) = transfer::apply::unfinished_import_warning() {
            warn(&w);
        }
        if args.json {
            print_json(&JsonImportBundle {
                ok: true,
                dry_run: true,
                strategy: args.strategy.as_str(),
                encrypted,
                items: &preview.items,
                notices: &preview.notices,
                source_current: preview.source_current.as_deref(),
                source_current_target: preview.source_current_target.as_deref(),
                backup_dir: None,
                activated: None,
                warning: SINGLE_USE_WARNING,
            });
        } else {
            print_plan(&preview);
            println!("Dry run: nothing was written.");
        }
        warn(SINGLE_USE_WARNING);
        return Ok(());
    }

    transfer::apply::ensure_no_unfinished_import()?;
    let can_prompt = !args.json && std::io::stdin().is_terminal() && !args.passphrase_stdin;
    // A plaintext bundle carries no authentication: whoever can write the
    // file chooses its accounts, endpoints and Codex overrides. Importing one
    // is confirmed exactly like writing one.
    let unauthenticated = !encrypted && preview.has_writes();
    if (preview.needs_confirmation() || unauthenticated) && !args.yes {
        if !can_prompt {
            if unauthenticated {
                anyhow::bail!(
                    "this bundle is NOT encrypted, so nothing proves who wrote it; review it with --dry-run and rerun with --yes"
                );
            }
            anyhow::bail!(
                "this import would overwrite or merge existing data, replace the live auth.json, or add Codex overrides; review it with --dry-run and rerun with --yes"
            );
        }
        print_plan(&preview);
        if !confirm_default_no("Apply these changes? [y/N] ") {
            return Err(crate::error::CsError::Aborted.into());
        }
    }
    // --activate replaces the live auth.json; like `use`, it must not discard
    // a login that belongs to no saved profile without asking.
    if args.activate.is_some() && !can_prompt && live_is_untracked()? {
        anyhow::bail!(
            "the live auth.json belongs to no saved profile and --activate would overwrite it; import without --activate, then run `codex-switch use <alias>` interactively"
        );
    }

    let result = {
        let _transaction = profile::lock_auth_transaction()?;
        let locked = plan::classify(&payload, args.strategy, args.replace_live)?;
        if locked.decisions() != preview.decisions() {
            anyhow::bail!(
                "the local store changed while the import was being prepared; rerun the import"
            );
        }
        let applied = transfer::apply::apply(&locked)?;
        (locked, applied)
    };
    let (final_plan, applied) = result;

    let activated = match &args.activate {
        None => None,
        Some(requested) => {
            let alias = requested
                .clone()
                .or_else(|| final_plan.source_current_target.clone())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "--activate has no alias: the bundle records no current account"
                    )
                })?;
            profile::validate_alias(&alias)?;
            if !profile::profile_auth_path(&alias)?.exists() {
                anyhow::bail!("cannot activate '{alias}': no such profile after import");
            }
            profile::cmd_use(&alias, can_prompt).with_context(|| {
                format!(
                    "the import itself finished{}, but --activate {alias} did not",
                    applied
                        .backup_dir
                        .as_ref()
                        .map(|d| format!(" (backup {})", d.display()))
                        .unwrap_or_default()
                )
            })?;
            Some(alias)
        }
    };

    if args.json {
        print_json(&JsonImportBundle {
            ok: true,
            dry_run: false,
            strategy: args.strategy.as_str(),
            encrypted,
            items: &final_plan.items,
            notices: &final_plan.notices,
            source_current: final_plan.source_current.as_deref(),
            source_current_target: final_plan.source_current_target.as_deref(),
            backup_dir: applied.backup_dir.as_ref().map(|p| p.display().to_string()),
            activated: activated.clone(),
            warning: SINGLE_USE_WARNING,
        });
    } else {
        print_plan(&final_plan);
        let written = final_plan
            .items
            .iter()
            .filter(|i| i.action.writes())
            .count();
        println!(
            "{}",
            color::success(&format!("Import finished: {written} item(s) written."))
        );
        if let Some(dir) = &applied.backup_dir {
            println!(
                "Backup and journal (contains secrets, keep private): {}",
                dir.display()
            );
        }
        match (&activated, &final_plan.source_current_target) {
            (Some(_), _) => {}
            (None, Some(target)) => println!(
                "Source machine's current account: {} → run `codex-switch use {target}`",
                final_plan.source_current.as_deref().unwrap_or(target)
            ),
            _ => {}
        }
        if !encrypted {
            println!(
                "Delete {} now that it has been imported.",
                args.path.display()
            );
        }
    }
    warn(SINGLE_USE_WARNING);
    Ok(())
}
