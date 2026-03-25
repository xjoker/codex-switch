use std::io::{self, Write as IoWrite};
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::auth::{
    backup_auth, codex_auth_path, current_file, profiles_dir, read_auth, write_auth,
};
use crate::error::CsError;
use crate::jwt::parse_account_info;

pub fn profile_auth_path(alias: &str) -> PathBuf {
    profiles_dir().join(alias).join("auth.json")
}

pub fn list_profiles() -> Result<Vec<String>> {
    let dir = profiles_dir();
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut names: Vec<String> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    Ok(names)
}

pub fn read_current() -> String {
    std::fs::read_to_string(current_file())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn write_current(alias: &str) -> Result<()> {
    let path = current_file();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, alias)?;
    Ok(())
}

pub fn find_matching_profile(auth_path: &Path) -> Option<String> {
    let hash = crate::auth::sha256_file(auth_path)?;
    let profiles = list_profiles().ok()?;
    profiles.into_iter().find(|alias| {
        let p = profile_auth_path(alias);
        crate::auth::sha256_file(&p)
            .map(|h| h == hash)
            .unwrap_or(false)
    })
}

// ── Deduplication ─────────────────────────────────────────

#[derive(Debug)]
pub struct AccountIdentity {
    pub account_id: Option<String>,
    pub email: Option<String>,
}

pub fn extract_identity(auth: &serde_json::Value) -> AccountIdentity {
    let info = parse_account_info(auth);
    AccountIdentity {
        account_id: info.account_id,
        email: info.email.map(|e| e.to_lowercase()),
    }
}

/// Find an existing profile matching the given identity (account_id > email).
pub fn find_profile_by_identity(identity: &AccountIdentity) -> Option<String> {
    let profiles = list_profiles().ok()?;
    let mut email_match: Option<String> = None;

    for alias in profiles {
        let path = profile_auth_path(&alias);
        let val = match read_auth(&path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let existing = extract_identity(&val);

        if let (Some(a), Some(b)) = (&identity.account_id, &existing.account_id) {
            if a == b {
                return Some(alias);
            }
        }

        if email_match.is_none() {
            if let (Some(a), Some(b)) = (&identity.email, &existing.email) {
                if a == b {
                    email_match = Some(alias);
                }
            }
        }
    }

    email_match
}

pub fn alias_from_email(email: &str) -> String {
    let base = email.split('@').next().unwrap_or(email);
    base.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

// ── Return types ──────────────────────────────────────────

pub enum SaveAction {
    Created(String),
    Updated(String),
}

// ── Auto-track ────────────────────────────────────────────

/// If the live auth.json belongs to an untracked account, auto-save it.
/// Returns true if a new profile was created.
pub fn auto_track_current() -> bool {
    let src = codex_auth_path();
    if !src.exists() {
        return false;
    }
    let val = match read_auth(&src) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let identity = extract_identity(&val);
    if find_profile_by_identity(&identity).is_none() {
        if let Ok(SaveAction::Created(a)) = cmd_save(None) {
            println!("Auto-saved current account as profile: {a}");
            return true;
        }
    }
    false
}

// ── Command implementations ───────────────────────────────

pub fn cmd_save(alias: Option<&str>) -> Result<SaveAction> {
    let src = codex_auth_path();
    if !src.exists() {
        return Err(CsError::NoAuthFile(src.display().to_string()).into());
    }

    let val = read_auth(&src)?;
    let identity = extract_identity(&val);

    let existing = find_profile_by_identity(&identity);

    let resolved_alias = match alias {
        Some(a) => a.to_string(),
        None => {
            if let Some(ref existing_alias) = existing {
                let dst = profile_auth_path(existing_alias);
                write_auth(&dst, &val)?;
                write_current(existing_alias)?;
                println!("Updated profile: {existing_alias}");
                return Ok(SaveAction::Updated(existing_alias.clone()));
            }
            identity
                .email
                .as_deref()
                .map(alias_from_email)
                .unwrap_or_else(|| "account".to_string())
        }
    };

    if alias.is_some() {
        if let Some(existing_alias) = existing {
            let dst = profile_auth_path(&existing_alias);
            write_auth(&dst, &val)?;
            write_current(&existing_alias)?;
            if existing_alias != resolved_alias {
                println!("Duplicate account detected — updated existing profile: {existing_alias} (not creating {resolved_alias})");
            } else {
                println!("Updated profile: {existing_alias}");
            }
            return Ok(SaveAction::Updated(existing_alias));
        }
    }

    // New profile
    let dst = profile_auth_path(&resolved_alias);
    if dst.exists() {
        let unique = make_unique_alias(&resolved_alias);
        write_auth(&profile_auth_path(&unique), &val)?;
        write_current(&unique)?;
        println!("Saved profile: {unique} (alias '{resolved_alias}' already taken)");
        return Ok(SaveAction::Created(unique));
    }

    write_auth(&dst, &val)?;
    write_current(&resolved_alias)?;
    println!("Saved profile: {resolved_alias}");
    Ok(SaveAction::Created(resolved_alias))
}

fn make_unique_alias(base: &str) -> String {
    let mut n = 2;
    loop {
        let candidate = format!("{base}_{n}");
        if !profile_auth_path(&candidate).exists() {
            return candidate;
        }
        n += 1;
    }
}

pub fn cmd_use(alias: &str) -> Result<()> {
    let src = profile_auth_path(alias);
    if !src.exists() {
        return Err(CsError::NotFound(alias.to_string()).into());
    }

    let dst = codex_auth_path();

    if dst.exists() && find_matching_profile(&dst).is_none() {
        print!("Current auth.json does not belong to any saved profile — switching will overwrite it. Continue? [y/N] ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
            return Err(CsError::Aborted.into());
        }
    }

    backup_auth(&dst)?;
    let val = read_auth(&src)?;
    write_auth(&dst, &val)?;
    write_current(alias)?;

    println!("Switched to profile: {alias}");
    Ok(())
}

pub fn switch_profile(alias: &str) -> Result<()> {
    let src = profile_auth_path(alias);
    if !src.exists() {
        return Err(CsError::NotFound(alias.to_string()).into());
    }
    let dst = codex_auth_path();
    backup_auth(&dst)?;
    let val = read_auth(&src)?;
    write_auth(&dst, &val)?;
    write_current(alias)?;
    Ok(())
}

pub fn cmd_delete(alias: &str) -> Result<()> {
    let dir = profiles_dir().join(alias);
    if !dir.exists() {
        return Err(CsError::NotFound(alias.to_string()).into());
    }
    std::fs::remove_dir_all(&dir)?;

    if read_current() == alias {
        let _ = std::fs::write(current_file(), "");
    }

    println!("Deleted profile: {alias}");
    Ok(())
}

pub fn cmd_list() -> Result<()> {
    let profiles = list_profiles()?;
    if profiles.is_empty() {
        println!("(no saved profiles)");
        return Ok(());
    }
    let current = read_current();
    for alias in &profiles {
        let mark = if alias == &current { "*" } else { " " };
        println!("{mark} {alias}");
    }
    Ok(())
}

pub fn cmd_import(file: &str, alias: &str) -> Result<SaveAction> {
    let src = std::path::PathBuf::from(file);
    if !src.exists() {
        return Err(CsError::NoAuthFile(src.display().to_string()).into());
    }
    let val = read_auth(&src)?;
    let identity = extract_identity(&val);

    if let Some(existing) = find_profile_by_identity(&identity) {
        let dst = profile_auth_path(&existing);
        write_auth(&dst, &val)?;
        println!("Duplicate account detected — updated profile: {existing}");
        return Ok(SaveAction::Updated(existing));
    }

    let final_alias = if profile_auth_path(alias).exists() {
        let unique = make_unique_alias(alias);
        println!("Alias '{alias}' already taken — using '{unique}' instead");
        unique
    } else {
        alias.to_string()
    };
    write_auth(&profile_auth_path(&final_alias), &val)?;
    println!("Imported {file} → profile: {final_alias}");
    Ok(SaveAction::Created(final_alias))
}

pub fn rename_profile(old_alias: &str, new_alias: &str) -> Result<()> {
    let old_dir = profiles_dir().join(old_alias);
    if !old_dir.exists() {
        return Err(CsError::NotFound(old_alias.to_string()).into());
    }
    let new_dir = profiles_dir().join(new_alias);
    if new_dir.exists() {
        anyhow::bail!("profile '{new_alias}' already exists");
    }
    std::fs::rename(&old_dir, &new_dir)?;
    if read_current() == old_alias {
        write_current(new_alias)?;
    }
    println!("Renamed profile: {old_alias} → {new_alias}");
    Ok(())
}

pub fn save_auth_value(val: serde_json::Value, hint_alias: Option<&str>) -> Result<SaveAction> {
    let identity = extract_identity(&val);

    if let Some(existing) = find_profile_by_identity(&identity) {
        let dst = profile_auth_path(&existing);
        write_auth(&dst, &val)?;
        write_current(&existing)?;
        return Ok(SaveAction::Updated(existing));
    }

    let alias = hint_alias
        .map(|s| s.to_string())
        .or_else(|| identity.email.as_deref().map(alias_from_email))
        .unwrap_or_else(|| "account".to_string());

    let alias = if profile_auth_path(&alias).exists() {
        make_unique_alias(&alias)
    } else {
        alias
    };

    let auth_dst = codex_auth_path();
    write_auth(&auth_dst, &val)?;

    write_auth(&profile_auth_path(&alias), &val)?;
    write_current(&alias)?;
    Ok(SaveAction::Created(alias))
}
