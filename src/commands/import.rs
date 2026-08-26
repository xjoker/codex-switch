use super::render::print_usage_line;
use crate::output::{self, ProgressReporter, account_to_json, print_json, usage_to_json};
use crate::{auth, cache, color, profile, usage};
use anyhow::Result;

/// Validation failed, but the auth server had already rotated the credentials
/// and they were rescued into a profile.
const STAGE_TOKEN_ROTATED: &str = "token_rotated";
/// Same rotation, but nothing could be written — the account is lost unless the
/// user acts.
const STAGE_TOKEN_ROTATION_LOST: &str = "token_rotation_lost";

/// Whether a failure also consumed the account's single-use `refresh_token`.
///
/// These entries look like any other line in a directory report, yet they mean
/// the source file is now worthless and a profile may have appeared — so they
/// get their own marker instead of blending into the skip list.
fn rotated_credentials(stage: &str) -> bool {
    stage == STAGE_TOKEN_ROTATED || stage == STAGE_TOKEN_ROTATION_LOST
}

/// Save credentials after the auth server rotated them and a later import step
/// failed.
///
/// They go to the profile store rather than back to the source file: it is the
/// tool's own storage, so it stays writable when the imported dump is not (auth
/// dumps are routinely copied in read-only), and it is where a successful
/// import would have put them. Validation never completed, so the rescue always
/// creates a unique profile rather than overwriting an existing identity. The
/// source file keeps the consumed token either way — that is unavoidable once
/// the server has rotated it — so the message has to steer the user away from
/// re-importing it.
fn rescue_rotated_credentials(
    source: &std::path::Path,
    val: serde_json::Value,
    alias: Option<&str>,
    suggested_alias: Option<&str>,
    cause: &anyhow::Error,
) -> profile::ImportFailure {
    match profile::save_recovered_import_auth_value(val, alias, suggested_alias) {
        Ok(profile::RecoveredImportAction::Profile(action)) => profile::ImportFailure {
            source: source.to_path_buf(),
            stage: STAGE_TOKEN_ROTATED,
            error: format!(
                "import failed after credential rotation ({cause}), so the usable credentials \
                 were {} as profile '{}'. {} now holds a dead refresh token — use the profile \
                 instead of importing that file again.",
                action.action(),
                action.alias(),
                source.display()
            ),
        },
        Ok(profile::RecoveredImportAction::Quarantined { path, reason }) => {
            profile::ImportFailure {
                source: source.to_path_buf(),
                stage: STAGE_TOKEN_ROTATED,
                error: format!(
                    "import failed after credential rotation ({cause}). Identity/policy \
                     validation or profile persistence also failed ({reason}), so the only usable \
                     credential copy was quarantined at {} and was not made into an activatable \
                     profile. Keep that file private and sign in again before deleting it. {} now \
                     holds a dead refresh token.",
                    path.display(),
                    source.display()
                ),
            }
        }
        Err(save_error) => profile::ImportFailure {
            source: source.to_path_buf(),
            stage: STAGE_TOKEN_ROTATION_LOST,
            error: format!(
                "import failed after the auth server rotated this account's credentials \
                 ({cause}), and {}",
                unsaveable_rotation_reason(&save_error)
            ),
        },
    }
}

fn unsaveable_rotation_reason(save_error: &anyhow::Error) -> String {
    format!(
        "saving them failed ({save_error:#}). The previous refresh token is already invalidated, \
         so this account has to sign in again."
    )
}

// ── import ───────────────────────────────────────────────

pub(crate) async fn import_cmd(path: &str, alias: Option<&str>, json: bool) -> Result<()> {
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
        } else if imported.action == "unchanged" {
            println!(
                "{}",
                color::success(&format!(
                    "Already saved as profile '{}'; skipped {} to protect its single-use refresh \
                     token. Run `codex-switch login {}` to refresh that profile.",
                    imported.alias,
                    imported.source.display(),
                    imported.alias
                ))
            );
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

    let all_skipped = report.imported.is_empty();
    let credentials_lost = report
        .skipped
        .iter()
        .any(|item| item.stage == STAGE_TOKEN_ROTATION_LOST);
    if json {
        print_json(&output::JsonImportReport {
            ok: !all_skipped,
            credentials_lost,
            imported: report
                .imported
                .iter()
                .map(|item| output::JsonImportEntry {
                    source: item.source.display().to_string(),
                    alias: item.alias.clone(),
                    action: item.action.to_string(),
                    account: account_to_json(&item.account, item.usage.plan_type.as_deref()),
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
        if all_skipped {
            return Err(super::super::OutputAlreadyReported.into());
        }
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
            let line = format!(
                "  {} {} [{}] {}",
                color::status_tag(if rotated_credentials(item.stage) {
                    "Rotated"
                } else {
                    "Skip"
                }),
                item.source.display(),
                item.stage,
                item.error
            );
            if rotated_credentials(item.stage) {
                println!("{}", color::warn(&line));
            } else {
                println!("{line}");
            }
        }

        let rotated = report
            .skipped
            .iter()
            .filter(|item| rotated_credentials(item.stage))
            .count();
        if rotated > 0 {
            println!(
                "{}",
                color::warn(&format!(
                    "  {rotated} file(s) had their credentials rotated during validation; their \
                     refresh token is spent and importing those files again will fail."
                ))
            );
        }

        if all_skipped {
            anyhow::bail!(
                "no profiles imported; all {} files were skipped",
                report.skipped.len()
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

    let source_account = auth::validate_auth_value(&val).map_err(|e| profile::ImportFailure {
        source: source.to_path_buf(),
        stage: "structure",
        error: e.to_string(),
    })?;
    if let Some(alias) = alias {
        profile::validate_alias(alias).map_err(|e| profile::ImportFailure {
            source: source.to_path_buf(),
            stage: "alias",
            error: e.to_string(),
        })?;
    }
    auth::validate_managed_auth_value(&val).map_err(|e| profile::ImportFailure {
        source: source.to_path_buf(),
        stage: "managed_policy",
        error: e.to_string(),
    })?;
    let suggested_alias = source_account
        .email
        .as_deref()
        .map(profile::alias_from_email);

    // Refuse to duplicate an account that is already saved. `import` is
    // create-only, so validating (which rotates the single-use refresh token)
    // and then writing a second profile would race the two copies into
    // `refresh_token_reused`. This runs *before* validation so the source
    // token is never spent on a re-import. It only declines — it never
    // overwrites — so a conservative match cannot hand credentials to the
    // wrong profile.
    if let Some(existing) = profile::existing_import_target(source, &val) {
        let mut account = source_account;
        cache::apply_workspace_name(&mut account);
        let usage = cache::get(&existing).unwrap_or_default();
        return Ok(profile::ImportSuccess {
            source: source.to_path_buf(),
            alias: existing,
            action: "unchanged",
            account,
            usage,
        });
    }

    let usage::ImportValidation {
        refreshed,
        validated_account_id,
        result,
    } = usage::validate_import_auth(&mut val).await;
    let rotated = refreshed.is_some();
    let usage = match result {
        Ok(usage) => usage,
        // A rotation already happened inside the validation, so `val` now holds
        // the only credentials the auth server still accepts. They must be
        // written somewhere durable before this failure is reported.
        Err(error) if rotated => {
            return Err(rescue_rotated_credentials(
                source,
                val,
                alias,
                suggested_alias.as_deref(),
                &error,
            ));
        }
        Err(error) => {
            return Err(profile::ImportFailure {
                source: source.to_path_buf(),
                stage: "usage_validation",
                error: error.to_string(),
            });
        }
    };

    // This second structure check inspects the *refreshed* value, so a
    // malformed refresh reply fails it at a point where the source file's
    // token is already spent. `val` is then the only credential the auth
    // server still accepts and has to be rescued, exactly as above.
    let mut account = match auth::validate_auth_value(&val) {
        Ok(account) => account,
        Err(error) if rotated => {
            return Err(rescue_rotated_credentials(
                source,
                val,
                alias,
                suggested_alias.as_deref(),
                &error,
            ));
        }
        Err(error) => {
            return Err(profile::ImportFailure {
                source: source.to_path_buf(),
                stage: "structure",
                error: error.to_string(),
            });
        }
    };
    cache::apply_workspace_name(&mut account);

    let validated_account_id = validated_account_id.ok_or_else(|| profile::ImportFailure {
        source: source.to_path_buf(),
        stage: "usage_validation",
        error: "Usage API validation did not bind an account_id".to_string(),
    })?;
    let action = match profile::save_imported_auth_value(
        &val,
        alias,
        &validated_account_id,
        suggested_alias.as_deref(),
    ) {
        Ok(action) => action,
        // Policy may change while the network call is in flight, and storage
        // may fail after validation. In either case `val` is now the only copy
        // of a rotated credential, so try both profile and quarantine recovery
        // before declaring it lost.
        Err(error) if rotated => {
            return Err(rescue_rotated_credentials(
                source,
                val,
                alias,
                suggested_alias.as_deref(),
                &error,
            ));
        }
        Err(error) => {
            return Err(profile::ImportFailure {
                source: source.to_path_buf(),
                stage: "save",
                error: error.to_string(),
            });
        }
    };

    Ok(profile::ImportSuccess {
        source: source.to_path_buf(),
        alias: action.alias().to_string(),
        action: action.action(),
        account,
        usage,
    })
}
