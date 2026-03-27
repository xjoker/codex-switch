use std::io::{self, Write as IoWrite};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::auth::{
    app_home, backup_auth, codex_auth_path, current_file, profiles_dir, read_auth, write_auth,
};
use crate::error::CsError;
use crate::jwt::parse_account_info;
use crate::output::{user_print, user_println};

const MAX_ALIAS_LEN: usize = 64;

pub fn profile_auth_path(alias: &str) -> PathBuf {
    profiles_dir().join(alias).join("auth.json")
}

pub fn validate_alias(alias: &str) -> Result<()> {
    if alias.is_empty() {
        anyhow::bail!("alias cannot be empty");
    }
    if alias == "." || alias == ".." {
        anyhow::bail!("alias cannot be '.' or '..'");
    }
    if alias.len() > MAX_ALIAS_LEN {
        anyhow::bail!("alias must be at most {MAX_ALIAS_LEN} characters");
    }
    if !alias
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        anyhow::bail!("alias may only contain ASCII letters, digits, '_', '-', '.'");
    }
    Ok(())
}

pub fn list_profiles() -> Result<Vec<String>> {
    let dir = profiles_dir();
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading profiles directory {}", dir.display()))?
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

fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("creating directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    Ok(())
}

fn ensure_profile_parent(path: &Path) -> Result<()> {
    ensure_private_dir(&app_home())?;
    ensure_private_dir(&profiles_dir())?;
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    Ok(())
}

fn write_current(alias: &str) -> Result<()> {
    let path = current_file();
    if let Some(p) = path.parent() {
        ensure_private_dir(p)?;
    }
    std::fs::write(&path, alias)
        .with_context(|| format!("writing current profile marker {}", path.display()))?;
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

        if let (Some(a), Some(b)) = (&identity.account_id, &existing.account_id)
            && a == b
        {
            return Some(alias);
        }

        if email_match.is_none()
            && let (Some(a), Some(b)) = (&identity.email, &existing.email)
            && a == b
        {
            email_match = Some(alias);
        }
    }

    email_match
}

pub fn alias_from_email(email: &str) -> String {
    let base = email.split('@').next().unwrap_or(email);
    let alias = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .chars()
        .take(MAX_ALIAS_LEN)
        .collect::<String>();
    if alias.is_empty() {
        "account".to_string()
    } else {
        alias
    }
}

// ── Return types ──────────────────────────────────────────

pub enum SaveAction {
    Created(String),
    Updated(String),
}

impl SaveAction {
    pub fn alias(&self) -> &str {
        match self {
            SaveAction::Created(alias) | SaveAction::Updated(alias) => alias,
        }
    }

    pub fn action(&self) -> &'static str {
        match self {
            SaveAction::Created(_) => "created",
            SaveAction::Updated(_) => "updated",
        }
    }
}

#[derive(Debug)]
pub struct ImportSuccess {
    pub source: PathBuf,
    pub alias: String,
    pub action: &'static str,
    pub account: crate::jwt::AccountInfo,
    pub usage: crate::usage::UsageInfo,
}

#[derive(Debug)]
pub struct ImportFailure {
    pub source: PathBuf,
    pub stage: &'static str,
    pub error: String,
}

#[derive(Debug, Default)]
pub struct ImportReport {
    pub imported: Vec<ImportSuccess>,
    pub skipped: Vec<ImportFailure>,
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
    if find_profile_by_identity(&identity).is_none()
        && let Ok(SaveAction::Created(a)) = cmd_save(None)
    {
        user_println(&format!("Auto-saved current account as profile: {a}"));
        return true;
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
                ensure_profile_parent(&dst)?;
                write_auth(&dst, &val)?;
                write_current(existing_alias)?;
                user_println(&format!("Updated profile: {existing_alias}"));
                return Ok(SaveAction::Updated(existing_alias.clone()));
            }
            identity
                .email
                .as_deref()
                .map(alias_from_email)
                .unwrap_or_else(|| "account".to_string())
        }
    };

    if alias.is_some()
        && let Some(existing_alias) = existing
    {
        let dst = profile_auth_path(&existing_alias);
        ensure_profile_parent(&dst)?;
        write_auth(&dst, &val)?;
        write_current(&existing_alias)?;
        if existing_alias != resolved_alias {
            user_println(&format!(
                "Duplicate account detected -- updated existing profile: {existing_alias} (not creating {resolved_alias})"
            ));
        } else {
            user_println(&format!("Updated profile: {existing_alias}"));
        }
        return Ok(SaveAction::Updated(existing_alias));
    }

    // New profile
    validate_alias(&resolved_alias)?;
    let dst = profile_auth_path(&resolved_alias);
    if dst.exists() {
        let unique = make_unique_alias(&resolved_alias);
        validate_alias(&unique)?;
        let unique_path = profile_auth_path(&unique);
        ensure_profile_parent(&unique_path)?;
        write_auth(&unique_path, &val)?;
        write_current(&unique)?;
        user_println(&format!(
            "Saved profile: {unique} (alias '{resolved_alias}' already taken)"
        ));
        return Ok(SaveAction::Created(unique));
    }

    ensure_profile_parent(&dst)?;
    write_auth(&dst, &val)?;
    write_current(&resolved_alias)?;
    user_println(&format!("Saved profile: {resolved_alias}"));
    Ok(SaveAction::Created(resolved_alias))
}

fn make_unique_alias(base: &str) -> String {
    let mut n = 2;
    loop {
        let suffix = format!("_{n}");
        let prefix_len = MAX_ALIAS_LEN.saturating_sub(suffix.len());
        let prefix = base.chars().take(prefix_len).collect::<String>();
        let candidate = format!("{prefix}{suffix}");
        if !profile_auth_path(&candidate).exists() {
            return candidate;
        }
        n += 1;
    }
}

pub fn cmd_use(alias: &str) -> Result<()> {
    validate_alias(alias)?;
    let src = profile_auth_path(alias);
    if !src.exists() {
        return Err(CsError::NotFound(alias.to_string()).into());
    }

    let dst = codex_auth_path();

    if dst.exists() && find_matching_profile(&dst).is_none() {
        user_print(
            "Current auth.json does not belong to any saved profile -- switching will overwrite it. Continue? [y/N] ",
        );
        io::stdout().flush()?;
        io::stderr().flush()?;
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

    user_println(&format!("Switched to profile: {alias}"));
    Ok(())
}

pub fn switch_profile(alias: &str) -> Result<()> {
    validate_alias(alias)?;
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
    validate_alias(alias)?;
    let dir = profiles_dir().join(alias);
    if !dir.exists() {
        return Err(CsError::NotFound(alias.to_string()).into());
    }
    if read_current() == alias {
        return Err(CsError::ActiveProfileDelete(alias.to_string()).into());
    }
    std::fs::remove_dir_all(&dir)
        .with_context(|| format!("removing profile directory {}", dir.display()))?;
    user_println(&format!("Deleted profile: {alias}"));
    Ok(())
}

pub fn collect_import_files(path: &Path) -> Result<Vec<PathBuf>> {
    if !path.exists() {
        return Err(CsError::NoAuthFile(path.display().to_string()).into());
    }

    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }

    let mut files = vec![];
    collect_import_files_recursive(path, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_import_files_recursive(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if entry
            .file_type()
            .with_context(|| format!("reading file type of {}", path.display()))?
            .is_dir()
        {
            collect_import_files_recursive(&path, files)?;
            continue;
        }
        if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            files.push(path);
        }
    }
    Ok(())
}

pub fn save_imported_auth_value(
    val: serde_json::Value,
    hint_alias: Option<&str>,
) -> Result<SaveAction> {
    let identity = extract_identity(&val);

    if let Some(existing) = find_profile_by_identity(&identity) {
        let dst = profile_auth_path(&existing);
        ensure_profile_parent(&dst)?;
        write_auth(&dst, &val)?;
        return Ok(SaveAction::Updated(existing));
    }

    let alias = hint_alias
        .map(|s| s.to_string())
        .or_else(|| identity.email.as_deref().map(alias_from_email))
        .unwrap_or_else(|| "account".to_string());
    validate_alias(&alias)?;
    let alias = if profile_auth_path(&alias).exists() {
        make_unique_alias(&alias)
    } else {
        alias
    };
    validate_alias(&alias)?;

    let dst = profile_auth_path(&alias);
    ensure_profile_parent(&dst)?;
    write_auth(&dst, &val)?;
    Ok(SaveAction::Created(alias))
}

pub fn rename_profile(old_alias: &str, new_alias: &str) -> Result<()> {
    validate_alias(old_alias)?;
    validate_alias(new_alias)?;
    let old_dir = profiles_dir().join(old_alias);
    if !old_dir.exists() {
        return Err(CsError::NotFound(old_alias.to_string()).into());
    }
    let new_dir = profiles_dir().join(new_alias);
    if new_dir.exists() {
        anyhow::bail!("profile '{new_alias}' already exists");
    }
    std::fs::rename(&old_dir, &new_dir).with_context(|| {
        format!(
            "renaming profile {} -> {}",
            old_dir.display(),
            new_dir.display()
        )
    })?;
    if let Err(err) = crate::cache::rename(old_alias, new_alias) {
        tracing::warn!("Failed to rename cache entry {old_alias} -> {new_alias}: {err}");
    }
    if read_current() == old_alias {
        write_current(new_alias)?;
    }
    user_println(&format!("Renamed profile: {old_alias} -> {new_alias}"));
    Ok(())
}

pub fn save_auth_value(val: serde_json::Value, hint_alias: Option<&str>) -> Result<SaveAction> {
    let identity = extract_identity(&val);

    if let Some(existing) = find_profile_by_identity(&identity) {
        let dst = profile_auth_path(&existing);
        ensure_profile_parent(&dst)?;
        write_auth(&dst, &val)?;
        write_current(&existing)?;
        return Ok(SaveAction::Updated(existing));
    }

    let alias = hint_alias
        .map(|s| s.to_string())
        .or_else(|| identity.email.as_deref().map(alias_from_email))
        .unwrap_or_else(|| "account".to_string());
    validate_alias(&alias)?;

    let alias = if profile_auth_path(&alias).exists() {
        make_unique_alias(&alias)
    } else {
        alias
    };
    validate_alias(&alias)?;

    let auth_dst = codex_auth_path();
    write_auth(&auth_dst, &val)?;

    let profile_dst = profile_auth_path(&alias);
    ensure_profile_parent(&profile_dst)?;
    write_auth(&profile_dst, &val)?;
    write_current(&alias)?;
    Ok(SaveAction::Created(alias))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::{LazyLock, Mutex, MutexGuard};

    use anyhow::Result;

    use super::{cmd_delete, cmd_use, rename_profile, switch_profile, validate_alias};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct TestEnv {
        _lock: MutexGuard<'static, ()>,
        _home: tempfile::TempDir,
        old_home: Option<OsString>,
        old_codex_home: Option<OsString>,
    }

    impl TestEnv {
        fn new() -> Self {
            let lock = ENV_LOCK.lock().unwrap();
            let home = tempfile::tempdir().unwrap();
            let codex_home = home.path().join(".codex");
            let old_home = std::env::var_os("HOME");
            let old_codex_home = std::env::var_os("CODEX_HOME");

            unsafe {
                std::env::set_var("HOME", home.path());
                std::env::set_var("CODEX_HOME", &codex_home);
            }

            Self {
                _lock: lock,
                _home: home,
                old_home,
                old_codex_home,
            }
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            unsafe {
                match &self.old_home {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
                match &self.old_codex_home {
                    Some(value) => std::env::set_var("CODEX_HOME", value),
                    None => std::env::remove_var("CODEX_HOME"),
                }
            }
        }
    }

    fn assert_invalid_alias(result: Result<()>, expected_message: &str) {
        let err = result.unwrap_err();
        assert_eq!(err.to_string(), expected_message);
    }

    #[test]
    fn validate_alias_accepts_expected_values() {
        assert!(validate_alias("alpha-123_.beta").is_ok());
        assert!(validate_alias(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn validate_alias_rejects_reserved_or_empty_values() {
        assert!(validate_alias("").is_err());
        assert!(validate_alias(".").is_err());
        assert!(validate_alias("..").is_err());
    }

    #[test]
    fn validate_alias_rejects_separators_and_non_ascii() {
        assert!(validate_alias("../escape").is_err());
        assert!(validate_alias("with/slash").is_err());
        assert!(validate_alias("\u{4E2D}\u{6587}").is_err());
        assert!(validate_alias(&"a".repeat(65)).is_err());
    }

    #[test]
    fn profile_commands_reject_invalid_alias_inputs() {
        let _env = TestEnv::new();

        for alias in ["../escape", "with/slash"] {
            assert_invalid_alias(
                cmd_use(alias),
                "alias may only contain ASCII letters, digits, '_', '-', '.'",
            );
            assert_invalid_alias(
                switch_profile(alias),
                "alias may only contain ASCII letters, digits, '_', '-', '.'",
            );
            assert_invalid_alias(
                cmd_delete(alias),
                "alias may only contain ASCII letters, digits, '_', '-', '.'",
            );
            assert_invalid_alias(
                rename_profile(alias, "valid-alias"),
                "alias may only contain ASCII letters, digits, '_', '-', '.'",
            );
        }

        assert_invalid_alias(cmd_use(""), "alias cannot be empty");
        assert_invalid_alias(switch_profile(""), "alias cannot be empty");
        assert_invalid_alias(cmd_delete(""), "alias cannot be empty");
        assert_invalid_alias(rename_profile("", "valid-alias"), "alias cannot be empty");
    }

    #[test]
    fn rename_profile_rejects_invalid_new_alias() {
        let _env = TestEnv::new();
        let old_dir = super::profiles_dir().join("valid-alias");
        std::fs::create_dir_all(&old_dir).unwrap();

        for alias in ["../escape", "with/slash"] {
            assert_invalid_alias(
                rename_profile("valid-alias", alias),
                "alias may only contain ASCII letters, digits, '_', '-', '.'",
            );
        }

        assert_invalid_alias(rename_profile("valid-alias", ""), "alias cannot be empty");
    }
}
