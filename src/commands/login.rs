use crate::output::{self, print_json};
use crate::{color, login, profile, workspace};
use anyhow::Result;

// ── login / reauth ────────────────────────────────────────

pub(crate) async fn login_cmd(alias: Option<&str>, device: bool, json: bool) -> Result<()> {
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
    let (auth_val, _info) = login::build_auth_from_tokens(&tokens);
    let workspace_auth = auth_val.clone();

    let action = profile::save_auth_value(auth_val, alias)?;
    if let Err(err) = workspace::refresh_for_auth(&workspace_auth).await {
        tracing::debug!("workspace metadata unavailable after login: {err}");
    }
    match action {
        profile::SaveAction::Created(a) => {
            tracing::info!(action = "login", alias = %a, outcome = "created", "profile login completed");
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
            tracing::info!(action = "login", alias = %a, outcome = "updated", "profile login completed");
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
    let old_info = crate::auth::read_account_info(&dst);

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
    let (auth_val, new_info) = login::build_auth_from_tokens(&tokens);
    profile::replace_profile_auth_and_live_if_current(alias, &auth_val)?;
    if let Err(err) = workspace::refresh_for_auth(&auth_val).await {
        tracing::debug!("workspace metadata unavailable after re-login: {err}");
    }
    tracing::info!(
        action = "login",
        alias,
        outcome = "reauthorized",
        "profile login completed"
    );

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
