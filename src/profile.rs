use std::fs::{File, OpenOptions};
use std::io::{self, Write as IoWrite};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use fs4::{FileExt, TryLockError};

use crate::auth::{
    app_home, atomic_write_private, backup_auth, codex_auth_path, current_file, profiles_dir,
    read_auth, write_auth,
};
use crate::error::CsError;
use crate::jwt::parse_account_info;
use crate::output::{user_print, user_println};

const MAX_ALIAS_LEN: usize = 64;

pub fn profile_auth_path(alias: &str) -> Result<PathBuf> {
    Ok(profiles_dir()?.join(alias).join("auth.json"))
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
    let dir = profiles_dir()?;
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
    current_file()
        .and_then(|p| std::fs::read_to_string(p).map_err(Into::into))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("creating directory {}", path.display()))?;
    #[cfg(windows)]
    crate::auth::harden_windows_private_directory(path)
        .with_context(|| format!("securing directory {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    Ok(())
}

fn ensure_profile_parent(path: &Path) -> Result<()> {
    ensure_private_dir(&app_home()?)?;
    ensure_private_dir(&profiles_dir()?)?;
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    Ok(())
}

fn deleted_profiles_dir() -> Result<PathBuf> {
    Ok(app_home()?.join("deleted-profiles"))
}

fn auth_lock_path() -> Result<PathBuf> {
    Ok(app_home()?.join("auth.lock"))
}

fn launch_lock_path() -> Result<PathBuf> {
    Ok(app_home()?.join("launch.lock"))
}

/// Maximum time to wait for an auth-related lock. A timeout is reported rather
/// than replacing the inode because an OS lock is the only reliable liveness signal.
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(200);

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn lock_live_auth() -> Result<File> {
    let path = auth_lock_path()?;
    acquire_file_lock(&path, LOCK_WAIT_TIMEOUT, "auth")
}

/// Serialize the short launch staging window without holding the auth write
/// lock while Codex starts and reads the staged credentials.
pub fn lock_launch_session() -> Result<File> {
    let path = launch_lock_path()?;
    acquire_file_lock(&path, LOCK_WAIT_TIMEOUT, "launch session")
}

/// Reject concurrent reset-card consumes for the same profile across processes.
pub fn lock_reset_card_consume(profile_path: &Path) -> Result<File> {
    let parent = profile_path.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "profile auth path has no parent: {}",
            profile_path.display()
        )
    })?;
    acquire_file_lock(
        &parent.join("reset-card-consume.lock"),
        Duration::ZERO,
        "reset card consume",
    )
}

struct AuthTransaction {
    _launch: File,
    _auth: File,
}

fn lock_auth_transaction() -> Result<AuthTransaction> {
    lock_auth_transaction_after_launch(|| {})
}

fn lock_auth_transaction_after_launch(after_launch: impl FnOnce()) -> Result<AuthTransaction> {
    // Every writer uses this order. Launch holds the first lock across its
    // stage/start/restore window and only takes the auth lock for each write.
    let launch = lock_launch_session()?;
    after_launch();
    let auth = lock_live_auth()?;
    Ok(AuthTransaction {
        _launch: launch,
        _auth: auth,
    })
}

fn acquire_file_lock(path: &Path, timeout: Duration, label: &str) -> Result<File> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }

    let file = open_lock_file(path)?;
    let deadline = Instant::now() + timeout;
    loop {
        match FileExt::try_lock(&file) {
            Ok(()) => {
                write_lock_holder(&file);
                return Ok(file);
            }
            Err(TryLockError::WouldBlock) => {
                #[cfg(test)]
                notify_test_lock_attempt(label);
                if Instant::now() >= deadline {
                    let holder =
                        read_lock_holder(path).unwrap_or_else(|| "unknown holder".to_string());
                    anyhow::bail!(
                        "{label} lock {} remained held for {:.3}s by {holder}; refusing to replace the live lock file",
                        path.display(),
                        timeout.as_secs_f64(),
                    );
                }
                std::thread::sleep(LOCK_POLL_INTERVAL);
            }
            Err(TryLockError::Error(e)) => {
                return Err(anyhow::Error::from(e))
                    .with_context(|| format!("locking {}", path.display()));
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_LOCK_ATTEMPT_NOTIFIER:
        std::cell::RefCell<Option<(String, std::sync::mpsc::Sender<()>)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn notify_on_test_lock_attempt(label: &str, sender: std::sync::mpsc::Sender<()>) {
    TEST_LOCK_ATTEMPT_NOTIFIER.with(|notifier| {
        *notifier.borrow_mut() = Some((label.to_string(), sender));
    });
}

#[cfg(test)]
fn notify_test_lock_attempt(label: &str) {
    TEST_LOCK_ATTEMPT_NOTIFIER.with(|notifier| {
        let should_notify = notifier
            .borrow()
            .as_ref()
            .is_some_and(|(target, _)| target == label);
        if should_notify && let Some((_, sender)) = notifier.borrow_mut().take() {
            let _ = sender.send(());
        }
    });
}

/// Open a stable lock inode. Permission/ownership errors are reported rather
/// than recovered by unlinking because another process may still hold it.
fn open_lock_file(path: &Path) -> Result<File> {
    try_open_lock_file(path).with_context(|| {
        format!(
            "opening auth lock {}; check the file and parent directory ownership",
            path.display()
        )
    })
}

fn try_open_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
}

/// Best-effort: write `pid epoch_secs` to the lock file for diagnostics.
/// Failure is non-fatal — the OS-level flock is the source of truth.
fn write_lock_holder(file: &File) {
    use std::io::Seek;
    let pid = std::process::id();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("{pid} {ts}\n");
    let _ = file.set_len(0);
    let mut f = file;
    let _ = f.seek(std::io::SeekFrom::Start(0));
    let _ = f.write_all(line.as_bytes());
}

fn read_lock_holder(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write_current(alias: &str) -> Result<()> {
    let path = current_file()?;
    atomic_write_private(&path, alias.as_bytes())
        .with_context(|| format!("writing current profile marker {}", path.display()))?;
    Ok(())
}

fn switch_live_auth(alias: &str) -> Result<()> {
    validate_alias(alias)?;
    let src = profile_auth_path(alias)?;
    if !src.exists() {
        return Err(CsError::NotFound(alias.to_string()).into());
    }

    let _transaction = lock_auth_transaction()?;
    let val = read_auth(&src)?;
    crate::auth::validate_managed_auth_value(&val)?;
    let dst = codex_auth_path()?;
    backup_auth(&dst)?;
    write_auth(&dst, &val)?;
    write_current(alias)?;
    Ok(())
}

/// Compare-and-swap a refresh rotation while holding the auth transaction.
/// A concurrent re-login supersedes the presented token and must win.
pub fn update_profile_tokens_if_refresh_matches(
    alias: &str,
    presented_refresh_token: &str,
    id_token: &str,
    access_token: &str,
    new_refresh_token: &str,
) -> Result<bool> {
    update_profile_tokens_if_refresh_matches_after_launch(
        alias,
        presented_refresh_token,
        id_token,
        access_token,
        new_refresh_token,
        || {},
    )
}

fn update_profile_tokens_if_refresh_matches_after_launch(
    alias: &str,
    presented_refresh_token: &str,
    id_token: &str,
    access_token: &str,
    new_refresh_token: &str,
    after_launch: impl FnOnce(),
) -> Result<bool> {
    validate_alias(alias)?;
    let profile_path = profile_auth_path(alias)?;
    let _transaction = lock_auth_transaction_after_launch(after_launch)?;
    let profile = read_auth(&profile_path)?;
    if refresh_token(&profile) != Some(presented_refresh_token) {
        return Ok(false);
    }
    let mut updated = profile;
    crate::auth::apply_tokens(&mut updated, id_token, access_token, new_refresh_token)?;
    crate::auth::validate_managed_auth_value(&updated)?;
    crate::auth::update_tokens(&profile_path, id_token, access_token, new_refresh_token)?;
    if read_current() == alias {
        let live = codex_auth_path()?;
        let live_auth = read_auth(&live)?;
        if refresh_token(&live_auth) == Some(presented_refresh_token) {
            crate::auth::update_tokens(&live, id_token, access_token, new_refresh_token)?;
        }
    }
    Ok(true)
}

/// Replace a saved profile and its live copy, when current, as one serialized
/// transaction. Used by CLI/TUI re-login paths.
pub fn replace_profile_auth_and_live_if_current(
    alias: &str,
    val: &serde_json::Value,
) -> Result<()> {
    validate_alias(alias)?;
    let profile_path = profile_auth_path(alias)?;
    let _transaction = lock_auth_transaction()?;
    crate::auth::validate_managed_auth_value(val)?;
    ensure_same_account_identity(alias, &read_auth(&profile_path)?, val)?;
    write_auth(&profile_path, val)?;
    if read_current() == alias {
        let live = codex_auth_path()?;
        backup_auth(&live)?;
        write_auth(&live, val)?;
    }
    Ok(())
}

pub fn find_matching_profile(auth_path: &Path) -> Option<String> {
    let hash = crate::auth::sha256_file(auth_path)?;
    let profiles = list_profiles().ok()?;
    profiles.into_iter().find(|alias| {
        profile_auth_path(alias)
            .ok()
            .and_then(|p| crate::auth::sha256_file(&p))
            .map(|h| h == hash)
            .unwrap_or(false)
    })
}

pub fn active_profile_from_live() -> Option<String> {
    let src = codex_auth_path().ok()?;
    if !src.exists() {
        return None;
    }

    if let Some(alias) = find_matching_profile(&src) {
        return Some(alias);
    }

    let val = read_auth(&src).ok()?;
    let identity = extract_identity(&val);
    find_profile_by_identity_exact(&identity)
}

pub fn sync_current_from_live() -> Option<String> {
    let _transaction = lock_auth_transaction().ok()?;
    let alias = active_profile_from_live()?;
    if read_current() != alias
        && let Err(e) = write_current(&alias)
    {
        tracing::debug!("sync_current_from_live: could not sync current pointer: {e}");
    }
    Some(alias)
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

fn ensure_same_account_identity(
    alias: &str,
    existing: &serde_json::Value,
    incoming: &serde_json::Value,
) -> Result<()> {
    let existing = extract_identity(existing);
    let incoming = extract_identity(incoming);
    let email_matches = matches!(
        (&existing.email, &incoming.email),
        (Some(existing), Some(incoming)) if existing == incoming
    );
    let account_matches = match (&existing.account_id, &incoming.account_id) {
        (Some(existing), Some(incoming)) => existing == incoming,
        _ => true,
    };
    if email_matches && account_matches {
        return Ok(());
    }
    anyhow::bail!("authenticated account does not match profile '{alias}'")
}

/// Find a profile with a strict match: both account_id AND email must be present and equal.
/// Used by `auto_track_current` to avoid silently syncing on ambiguous email-only matches.
pub fn find_profile_by_identity_exact(identity: &AccountIdentity) -> Option<String> {
    let (Some(target_id), Some(target_email)) = (&identity.account_id, &identity.email) else {
        return None; // identity itself is incomplete — no exact match possible
    };
    let profiles = list_profiles().ok()?;
    for alias in profiles {
        let path = match profile_auth_path(&alias) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let val = match read_auth(&path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let existing = extract_identity(&val);
        if let (Some(eid), Some(eemail)) = (&existing.account_id, &existing.email)
            && eid == target_id
            && eemail == target_email
        {
            return Some(alias);
        }
    }
    None
}

/// Profiles matching an identity, split by match strength so callers can tell
/// an unambiguous hit from "several workspaces share this email".
#[derive(Default)]
struct IdentityMatches {
    /// account_id AND email both equal — unambiguous by construction.
    exact: Option<String>,
    /// email equal while one side carries no account_id — possibly several.
    email_only: Vec<String>,
}

fn scan_profiles_by_identity(identity: &AccountIdentity) -> IdentityMatches {
    let mut matches = IdentityMatches::default();
    let Ok(profiles) = list_profiles() else {
        return matches;
    };

    for alias in profiles {
        let path = match profile_auth_path(&alias) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let val = match read_auth(&path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let existing = extract_identity(&val);

        // Match: account_id AND email both equal (same person, same workspace)
        if let (Some(a1), Some(a2)) = (&identity.account_id, &existing.account_id)
            && a1 == a2
            && let (Some(e1), Some(e2)) = (&identity.email, &existing.email)
            && e1 == e2
        {
            matches.exact = Some(alias);
            return matches;
        }

        // Fallback: email-only match (when account_id is missing on either side)
        if let (Some(a), Some(b)) = (&identity.email, &existing.email)
            && a == b
            && (identity.account_id.is_none() || existing.account_id.is_none())
        {
            matches.email_only.push(alias);
        }
    }

    matches
}

/// Find an existing profile matching the given identity (account_id+email > email-only).
pub fn find_profile_by_identity(identity: &AccountIdentity) -> Option<String> {
    let IdentityMatches { exact, email_only } = scan_profiles_by_identity(identity);
    exact.or_else(|| email_only.into_iter().next())
}

/// The saved profile that these to-be-imported credentials already belong to,
/// if any.
///
/// `import` is deliberately create-only (see [`save_imported_auth_value`]), so a
/// re-import of an already-saved account would otherwise write a *second*
/// profile for it. Because OpenAI refresh tokens are single-use, the two copies
/// then race: whichever refreshes first rotates the token and the other dies
/// with `refresh_token_reused`, forcing a full re-login. Callers use this to
/// skip such a re-import instead of duplicating the account.
///
/// Detection is intentionally conservative and read-only — it never writes and
/// never overwrites, so a false positive can only decline an import, never hand
/// an account to the wrong profile:
/// - byte-identical to a stored profile ([`find_matching_profile`]), or
/// - the exact same `account_id` **and** `email` as a stored profile
///   ([`find_profile_by_identity_exact`]). Requiring the email too means a
///   shared Team `account_id` alone never matches a different member.
pub fn existing_import_target(source: &Path, val: &serde_json::Value) -> Option<String> {
    find_matching_profile(source).or_else(|| find_profile_by_identity_exact(&extract_identity(val)))
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

#[derive(Debug)]
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
#[allow(dead_code)] // Binary-only import command consumes these fields.
pub(crate) enum RecoveredImportAction {
    Profile(SaveAction),
    Quarantined { path: PathBuf, reason: String },
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

// ── Startup auth change detection ─────────────────────────

#[derive(Debug)]
pub enum AuthChange {
    /// Live auth.json belongs to a completely new account.
    NewAccount,
    /// Live auth.json matches an existing profile's identity but tokens differ.
    TokensUpdated { alias: String },
    /// No actionable change.
    NoChange,
}

/// Compare live auth.json against all saved profiles.
/// - Exact SHA256 match → NoChange
/// - Identity match (email + account_id) but different content → TokensUpdated
/// - Email matches several profiles and no account_id to disambiguate → NoChange (warned)
/// - No identity match → NewAccount
pub fn detect_auth_change() -> AuthChange {
    let auth_path = match codex_auth_path() {
        Ok(p) => p,
        Err(_) => return AuthChange::NoChange,
    };
    if !auth_path.exists() {
        return AuthChange::NoChange;
    }
    let val = match read_auth(&auth_path) {
        Ok(v) => v,
        Err(_) => return AuthChange::NoChange,
    };

    // Exact file match — nothing changed
    if find_matching_profile(&auth_path).is_some() {
        return AuthChange::NoChange;
    }

    let identity = extract_identity(&val);
    if identity.email.is_none() && identity.account_id.is_none() {
        return AuthChange::NoChange;
    }

    // The read-back path writes live credentials into a profile, so an
    // email-only guess across workspaces would clobber the wrong account.
    let IdentityMatches { exact, email_only } = scan_profiles_by_identity(&identity);
    if let Some(alias) = exact {
        return AuthChange::TokensUpdated { alias };
    }
    match email_only.as_slice() {
        [] => AuthChange::NewAccount,
        [only] => AuthChange::TokensUpdated {
            alias: only.clone(),
        },
        ambiguous => {
            user_println(&format!(
                "auth.json carries no account id and its email matches {} profiles ({}) — \
                 refusing to guess which one to update. \
                 Run `codex-switch use <alias>` to restore a known profile, \
                 or `codex-switch save <alias>` to store these credentials explicitly.",
                ambiguous.len(),
                ambiguous.join(", ")
            ));
            AuthChange::NoChange
        }
    }
}

/// `last_refresh` as written by `auth::apply_tokens` and `login`: an RFC3339
/// string at the auth.json root. Absent or malformed values yield `None`.
fn parse_last_refresh(val: &serde_json::Value) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let raw = val.get("last_refresh")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(raw).ok()
}

fn refresh_token(val: &serde_json::Value) -> Option<&str> {
    val.get("tokens")?.get("refresh_token")?.as_str()
}

/// How an auth.json's `last_refresh` looks to the rollback guard, phrased for
/// the user: a refusal has to say what each side actually carried, otherwise
/// "cannot be ordered" is unactionable.
fn describe_last_refresh(val: &serde_json::Value) -> String {
    match (
        parse_last_refresh(val),
        val.get("last_refresh").and_then(|v| v.as_str()),
    ) {
        (Some(ts), _) => ts.to_string(),
        (None, Some(raw)) => format!("unparseable last_refresh '{raw}'"),
        (None, None) => "no last_refresh".to_string(),
    }
}

/// The incoming credentials would replace a `refresh_token` that cannot be
/// shown to be the dead one, so writing them risks destroying the account's
/// only working credential.
///
/// Typed rather than a bare message: callers decide whether to surface this to
/// the user, and matching on error text couples them to this wording — a
/// rewording would silently turn the check off instead of failing to compile.
#[derive(Debug)]
pub struct StaleLiveAuth {
    pub alias: String,
    /// The incoming copy's `last_refresh` state, as `describe_last_refresh` renders it.
    pub live: String,
    /// The stored profile's `last_refresh` state, same rendering.
    pub profile: String,
}

impl std::fmt::Display for StaleLiveAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to overwrite profile '{}': the incoming credentials carry a different \
             refresh_token and cannot be shown to be the newer of the two \
             (incoming: {}; profile: {}). Refresh tokens are single-use, so the older copy is \
             already revoked and overwriting would destroy the working one. Choose a side \
             explicitly: `codex-switch use {}` keeps the profile's credentials and pushes them \
             back into ~/.codex/auth.json, after which the two agree again; \
             `codex-switch delete {}` followed by a fresh save keeps the incoming ones.",
            self.alias, self.live, self.profile, self.alias, self.alias
        )
    }
}

impl std::error::Error for StaleLiveAuth {}

/// Refuse to replace a profile's `refresh_token` with one that cannot be proven newer.
///
/// OpenAI rotates `refresh_token` on every use: of two different tokens for the
/// same account, exactly one is still usable and the other is already dead.
/// Picking wrong is unrecoverable without a full re-login, so the guard demands
/// positive evidence before letting a rotation through.
///
/// `last_refresh` is only weak evidence — it is wall-clock, second-resolution,
/// and moves backwards on NTP corrections — so it is allowed to decide only when
/// both sides carry a parseable stamp and those stamps actually differ. Equal,
/// missing or malformed stamps are a conflict, not a default. The common case
/// never reaches the timestamps at all: an ordinary sync rotates `access_token`
/// while `refresh_token` stays put, and identical tokens cannot revoke anything.
fn ensure_live_not_older(
    alias: &str,
    profile: &serde_json::Value,
    incoming: &serde_json::Value,
) -> Result<()> {
    if refresh_token(profile) == refresh_token(incoming) {
        return Ok(());
    }
    if let (Some(incoming_ts), Some(profile_ts)) =
        (parse_last_refresh(incoming), parse_last_refresh(profile))
        && incoming_ts > profile_ts
    {
        return Ok(());
    }
    Err(StaleLiveAuth {
        alias: alias.to_string(),
        live: describe_last_refresh(incoming),
        profile: describe_last_refresh(profile),
    }
    .into())
}

/// The one door through which credentials reach an existing profile.
///
/// Every entry point that copies an already-minted auth.json into the profile
/// store (`cmd_save`, `update_profile_from_live`, and login replacement) goes
/// through here, so the two invariants cannot be bypassed by adding a caller:
/// the credentials must belong to this profile's account, and they must not
/// roll its single-use `refresh_token` backwards. Imports are create-only and
/// never select an existing profile.
///
/// A profile that does not exist yet has nothing to protect, so this doubles as
/// the create path; callers that require an existing profile check that first.
fn write_profile_credentials(alias: &str, incoming: &serde_json::Value) -> Result<()> {
    validate_alias(alias)?;
    crate::auth::validate_managed_auth_value(incoming)?;
    let dst = profile_auth_path(alias)?;
    if let Ok(existing) = read_auth(&dst) {
        ensure_same_account_identity(alias, &existing, incoming)?;
        ensure_live_not_older(alias, &existing, incoming)?;
    }
    ensure_profile_parent(&dst)?;
    write_auth(&dst, incoming)?;
    Ok(())
}

/// The profile these credentials provably belong to, if there is exactly one.
///
/// `Err` when several profiles share the email and nothing tells them apart:
/// same-email-different-workspace is the shape that has already destroyed the
/// wrong account's `refresh_token` in the wild, so the caller has to either
/// surface the choice to the user or fall back to creating a new profile —
/// never to picking a candidate.
fn resolve_identity_target(identity: &AccountIdentity) -> Result<Option<String>> {
    let IdentityMatches {
        exact,
        mut email_only,
    } = scan_profiles_by_identity(identity);
    if let Some(alias) = exact {
        return Ok(Some(alias));
    }
    match email_only.len() {
        0 => Ok(None),
        1 => Ok(email_only.pop()),
        _ => {
            email_only.sort();
            anyhow::bail!(
                "Ambiguous account: {} profiles share email '{}' with different workspaces ({}) -- refusing to guess which one to update.\nRun `codex-switch save <alias>` with one of the profiles above, or a new alias, to choose explicitly.",
                email_only.len(),
                identity.email.as_deref().unwrap_or("unknown"),
                email_only.join(", ")
            )
        }
    }
}

/// Where credentials should land when the user named an alias explicitly.
///
/// An alias the user typed outranks every identity heuristic: naming an
/// existing profile *is* the disambiguation that the ambiguity refusal asks
/// for, so it is obeyed verbatim. Only when the alias does not exist yet does
/// deduplication get a say, and then ambiguity is harmless — falling back to
/// creating the named profile overwrites nothing.
fn resolve_named_target(alias: &str, identity: &AccountIdentity) -> Result<Option<String>> {
    validate_alias(alias)?;
    if profile_auth_path(alias)?.exists() {
        return Ok(Some(alias.to_string()));
    }
    Ok(resolve_identity_target(identity).ok().flatten())
}

/// Copy the live auth.json into an existing profile's directory and mark it current.
/// The profile is written in canonical format. The live file is also normalized
/// (best-effort) to ensure SHA256 consistency; failure to normalize live is non-fatal.
pub fn update_profile_from_live(alias: &str) -> Result<()> {
    validate_alias(alias)?;
    let _transaction = lock_auth_transaction()?;
    let src = codex_auth_path()?;
    let val = read_auth(&src)?;
    // This is a read-back into a profile the caller already knows, not a save:
    // a missing profile is its own failure, distinct from the guards below.
    read_auth(&profile_auth_path(alias)?)?;
    write_profile_credentials(alias, &val)?;
    // Best-effort: normalize live file to match profile (same key ordering)
    if let Err(e) = write_auth(&src, &val) {
        tracing::debug!("Could not normalize live auth.json: {e}");
    }
    write_current(alias)?;
    Ok(())
}

// ── Auto-track ────────────────────────────────────────────

/// If the live auth.json belongs to an untracked account, auto-save it.
/// Returns true if a new profile was created.
pub fn auto_track_current() -> bool {
    let src = match codex_auth_path() {
        Ok(p) => p,
        Err(_) => return false,
    };
    if !src.exists() {
        return false;
    }
    let val = match read_auth(&src) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let identity = extract_identity(&val);

    if find_profile_by_identity_exact(&identity).is_some() {
        // Exact match (account_id + email) — safe to sync the current pointer.
        let _ = sync_current_from_live();
        return false;
    }
    // Email-only matches are ambiguous (same email, different workspace) —
    // fall through to cmd_save which will prompt the user if interactive.
    if find_profile_by_identity(&identity).is_some() {
        return false;
    }

    if let Ok(SaveAction::Created(a)) = cmd_save(None) {
        user_println(&format!("Auto-saved current account as profile: {a}"));
        return true;
    }
    false
}

// ── Command implementations ───────────────────────────────

pub fn cmd_save(alias: Option<&str>) -> Result<SaveAction> {
    let src = codex_auth_path()?;
    if !src.exists() {
        return Err(CsError::NoAuthFile(src.display().to_string()).into());
    }

    let _transaction = lock_auth_transaction()?;
    let val = read_auth(&src)?;
    // Best-effort: normalize live file to canonical formatting for SHA256 consistency
    if let Err(e) = write_auth(&src, &val) {
        tracing::debug!("Could not normalize live auth.json: {e}");
    }
    let identity = extract_identity(&val);

    let update_target = match alias {
        Some(a) => resolve_named_target(a, &identity)?,
        // A bare `save` has no user-supplied disambiguation, so an ambiguous
        // email is fatal here rather than a fallback to creating a profile.
        None => resolve_identity_target(&identity)?,
    };

    if let Some(target) = update_target {
        write_profile_credentials(&target, &val)?;
        write_current(&target)?;
        match alias {
            Some(named) if named != target => user_println(&format!(
                "Duplicate account detected -- updated existing profile: {target} (not creating {named})"
            )),
            _ => user_println(&format!("Updated profile: {target}")),
        }
        return Ok(SaveAction::Updated(target));
    }

    // New profile
    let resolved_alias = match alias {
        Some(a) => a.to_string(),
        None => identity
            .email
            .as_deref()
            .map(alias_from_email)
            .unwrap_or_else(|| "account".to_string()),
    };
    validate_alias(&resolved_alias)?;
    let dst = profile_auth_path(&resolved_alias)?;
    if dst.exists() {
        let unique = make_unique_alias(&resolved_alias)?;
        validate_alias(&unique)?;
        write_profile_credentials(&unique, &val)?;
        write_current(&unique)?;
        user_println(&format!(
            "Saved profile: {unique} (alias '{resolved_alias}' already taken)"
        ));
        return Ok(SaveAction::Created(unique));
    }

    write_profile_credentials(&resolved_alias, &val)?;
    write_current(&resolved_alias)?;
    user_println(&format!("Saved profile: {resolved_alias}"));
    Ok(SaveAction::Created(resolved_alias))
}

fn make_unique_alias(base: &str) -> Result<String> {
    const MAX_RETRIES: u32 = 1000;
    let mut n: u32 = 2;
    loop {
        let suffix = format!("_{n}");
        let prefix_len = MAX_ALIAS_LEN.saturating_sub(suffix.len());
        let prefix = base.chars().take(prefix_len).collect::<String>();
        let candidate = format!("{prefix}{suffix}");
        if !profile_auth_path(&candidate)?.exists() {
            return Ok(candidate);
        }
        n += 1;
        if n > MAX_RETRIES {
            anyhow::bail!(
                "could not generate a unique alias for '{base}' after {MAX_RETRIES} attempts"
            );
        }
    }
}

pub fn cmd_use(alias: &str, allow_prompt: bool) -> Result<()> {
    validate_alias(alias)?;
    let src = profile_auth_path(alias)?;
    if !src.exists() {
        return Err(CsError::NotFound(alias.to_string()).into());
    }

    let dst = codex_auth_path()?;

    if dst.exists() && find_matching_profile(&dst).is_none() {
        if !allow_prompt {
            anyhow::bail!(
                "current auth.json is not tracked; interactive confirmation is required before overwriting it"
            );
        }
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

    switch_live_auth(alias)?;
    user_println(&format!("Switched to profile: {alias}"));
    Ok(())
}

pub fn switch_profile(alias: &str) -> Result<()> {
    switch_live_auth(alias)
}

/// Write a profile's auth.json to the live codex auth path WITHOUT updating
/// the current-profile marker.  Used by `launch` for temporary switching.
/// Caller MUST hold the lock from `lock_live_auth()`.
pub fn stage_profile_auth(alias: &str) -> Result<()> {
    validate_alias(alias)?;
    let src = profile_auth_path(alias)?;
    if !src.exists() {
        return Err(CsError::NotFound(alias.to_string()).into());
    }
    let val = read_auth(&src)?;
    crate::auth::validate_managed_auth_value(&val)?;
    let dst = codex_auth_path()?;
    write_auth(&dst, &val)?;
    Ok(())
}

pub fn cmd_delete(alias: &str) -> Result<()> {
    validate_alias(alias)?;
    let _transaction = lock_auth_transaction()?;
    let dir = profiles_dir()?.join(alias);
    if !dir.exists() {
        return Err(CsError::NotFound(alias.to_string()).into());
    }
    if read_current() == alias {
        return Err(CsError::ActiveProfileDelete(alias.to_string()).into());
    }
    let deleted_dir = deleted_profiles_dir()?;
    ensure_private_dir(&deleted_dir)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    let archived = deleted_dir.join(format!("{alias}.backup-{timestamp}"));
    std::fs::rename(&dir, &archived).with_context(|| {
        format!(
            "archiving profile directory {} to {}",
            dir.display(),
            archived.display()
        )
    })?;
    user_println(&format!(
        "Deleted profile: {alias} (recoverable from {})",
        archived.display()
    ));
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
    val: &serde_json::Value,
    hint_alias: Option<&str>,
    validated_account_id: &str,
    suggested_alias: Option<&str>,
) -> Result<SaveAction> {
    let _transaction = lock_auth_transaction()?;
    let identity = extract_identity(val);
    let account_id = identity
        .account_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("imported auth must contain a non-empty account_id"))?;
    if account_id != validated_account_id {
        anyhow::bail!(
            "imported account_id '{account_id}' does not match Usage API validated account_id \
             '{validated_account_id}'"
        );
    }
    crate::auth::validate_managed_auth_value(val)?;

    // Usage API proves the bearer can access this workspace, but a Team
    // workspace id is shared by multiple users and the JWT is not
    // signature-verified here. It therefore cannot prove ownership of an
    // existing profile. Imports are create-only; collisions get a unique alias.
    create_import_profile(val, hint_alias, suggested_alias)
}

/// Preserve credentials rotated by the auth server after validation later
/// failed. Without a successful Usage API response they may never overwrite an
/// existing profile; a unique recovery profile is the only safe destination.
#[allow(dead_code)] // Called by the binary-only import command, not the library target.
pub(crate) fn save_recovered_import_auth_value(
    val: serde_json::Value,
    hint_alias: Option<&str>,
    suggested_alias: Option<&str>,
) -> Result<RecoveredImportAction> {
    let _transaction = lock_auth_transaction()?;
    let account_id = extract_identity(&val)
        .account_id
        .filter(|account_id| !account_id.is_empty());
    let validation = account_id
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("rotated credentials have no authenticated account_id"))
        .and_then(|_| crate::auth::validate_managed_auth_value(&val));
    let profile_result = validation.and_then(|_| {
        create_import_profile(&val, hint_alias, suggested_alias).map(RecoveredImportAction::Profile)
    });
    match profile_result {
        Ok(action) => Ok(action),
        Err(error) => {
            let path = quarantine_recovered_import(&val).with_context(|| {
                format!(
                    "profile recovery failed ({error:#}) and quarantining rotated credentials \
                     also failed"
                )
            })?;
            Ok(RecoveredImportAction::Quarantined {
                path,
                reason: error.to_string(),
            })
        }
    }
}

fn quarantine_recovered_import(val: &serde_json::Value) -> Result<PathBuf> {
    let recovery_dir = crate::auth::app_home()?.join("recovery");
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    let contents = serde_json::to_vec_pretty(val).context("serializing recovered credentials")?;
    for suffix in 0..1000u16 {
        let filename = if suffix == 0 {
            format!("rotated-import-{timestamp}.json")
        } else {
            format!("rotated-import-{timestamp}-{suffix}.json")
        };
        let path = recovery_dir.join(filename);
        if path.exists() {
            continue;
        }
        crate::auth::atomic_write_private(&path, &contents)?;
        return Ok(path);
    }
    anyhow::bail!("could not allocate a unique rotated-import recovery path")
}

fn create_import_profile(
    val: &serde_json::Value,
    hint_alias: Option<&str>,
    suggested_alias: Option<&str>,
) -> Result<SaveAction> {
    let identity = extract_identity(val);
    let alias = hint_alias
        .map(|s| s.to_string())
        .or_else(|| identity.email.as_deref().map(alias_from_email))
        .or_else(|| suggested_alias.map(str::to_string))
        .unwrap_or_else(|| "account".to_string());
    validate_alias(&alias)?;
    let alias = if profile_auth_path(&alias)?.exists() {
        make_unique_alias(&alias)?
    } else {
        alias
    };
    validate_alias(&alias)?;

    write_profile_credentials(&alias, val)?;
    Ok(SaveAction::Created(alias))
}

pub fn rename_profile(old_alias: &str, new_alias: &str) -> Result<()> {
    validate_alias(old_alias)?;
    validate_alias(new_alias)?;
    let old_dir = profiles_dir()?.join(old_alias);
    if !old_dir.exists() {
        return Err(CsError::NotFound(old_alias.to_string()).into());
    }
    let new_dir = profiles_dir()?.join(new_alias);
    if new_dir.exists() {
        anyhow::bail!("profile '{new_alias}' already exists");
    }
    let _transaction = lock_auth_transaction()?;
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
    let _transaction = lock_auth_transaction()?;
    crate::auth::validate_managed_auth_value(&val)?;
    let identity = extract_identity(&val);

    let existing = match hint_alias {
        Some(alias) => resolve_named_target(alias, &identity)?,
        None => resolve_identity_target(&identity)?,
    };

    if let Some(existing) = existing {
        let dst = profile_auth_path(&existing)?;
        // These credentials were just minted, so the freshness gate does not
        // apply: a legacy profile carries no stamp to order against, and
        // re-login is precisely how such a profile is recovered. Identity is
        // still checked — without it, `login <alias>` naming a profile that
        // holds a different workspace would overwrite that account's token.
        if dst.exists() {
            ensure_same_account_identity(&existing, &read_auth(&dst)?, &val)?;
        }
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

    let alias = if profile_auth_path(&alias)?.exists() {
        make_unique_alias(&alias)?
    } else {
        alias
    };
    validate_alias(&alias)?;

    let auth_dst = codex_auth_path()?;
    write_auth(&auth_dst, &val)?;

    let profile_dst = profile_auth_path(&alias)?;
    ensure_profile_parent(&profile_dst)?;
    write_auth(&profile_dst, &val)?;
    write_current(&alias)?;
    Ok(SaveAction::Created(alias))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::MutexGuard;
    use std::thread::JoinHandle;
    use std::time::Duration;

    use anyhow::Result;
    use fs4::FileExt;

    use super::{
        cmd_delete, cmd_save, cmd_use, lock_reset_card_consume, rename_profile, switch_profile,
        validate_alias,
    };

    #[test]
    fn reset_card_lock_rejects_a_concurrent_process_instead_of_queueing() {
        let home = tempfile::tempdir().unwrap();
        let auth_path = home.path().join("account/auth.json");
        let first = lock_reset_card_consume(&auth_path).unwrap();

        let second = lock_reset_card_consume(&auth_path).unwrap_err();
        assert!(second.to_string().contains("remained held"));

        drop(first);
        assert!(lock_reset_card_consume(&auth_path).is_ok());
    }

    struct TestEnv {
        _lock: MutexGuard<'static, ()>,
        _home: tempfile::TempDir,
        old_home: Option<OsString>,
        old_codex_home: Option<OsString>,
        old_app_home: Option<OsString>,
    }

    struct ThreadCleanup<G> {
        blocker: Option<G>,
        workers: Vec<JoinHandle<()>>,
    }

    impl<G> ThreadCleanup<G> {
        fn new(blocker: G) -> Self {
            Self {
                blocker: Some(blocker),
                workers: Vec::new(),
            }
        }

        fn push(&mut self, worker: JoinHandle<()>) {
            self.workers.push(worker);
        }

        fn release_blocker(&mut self) {
            self.blocker.take();
        }

        fn join_all(&mut self) {
            let mut first_panic = None;
            for worker in self.workers.drain(..) {
                if let Err(panic) = worker.join()
                    && first_panic.is_none()
                {
                    first_panic = Some(panic);
                }
            }
            if let Some(panic) = first_panic {
                std::panic::resume_unwind(panic);
            }
        }
    }

    impl<G> Drop for ThreadCleanup<G> {
        fn drop(&mut self) {
            self.blocker.take();
            for worker in self.workers.drain(..) {
                let _ = worker.join();
            }
        }
    }

    impl TestEnv {
        fn new() -> Self {
            let lock = super::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let home = tempfile::tempdir().unwrap();
            let codex_home = home.path().join(".codex");
            let app_home = home.path().join(".codex-switch");
            let old_home = std::env::var_os("HOME");
            let old_codex_home = std::env::var_os("CODEX_HOME");
            let old_app_home = std::env::var_os("CODEX_SWITCH_HOME");

            unsafe {
                std::env::set_var("HOME", home.path());
                std::env::set_var("CODEX_HOME", &codex_home);
                std::env::set_var("CODEX_SWITCH_HOME", &app_home);
            }

            Self {
                _lock: lock,
                _home: home,
                old_home,
                old_codex_home,
                old_app_home,
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
                match &self.old_app_home {
                    Some(value) => std::env::set_var("CODEX_SWITCH_HOME", value),
                    None => std::env::remove_var("CODEX_SWITCH_HOME"),
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
                cmd_use(alias, true),
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

        assert_invalid_alias(cmd_use("", true), "alias cannot be empty");
        assert_invalid_alias(switch_profile(""), "alias cannot be empty");
        assert_invalid_alias(cmd_delete(""), "alias cannot be empty");
        assert_invalid_alias(rename_profile("", "valid-alias"), "alias cannot be empty");
    }

    #[test]
    fn rename_profile_rejects_invalid_new_alias() {
        let _env = TestEnv::new();
        let old_dir = super::profiles_dir().unwrap().join("valid-alias");
        std::fs::create_dir_all(&old_dir).unwrap();

        for alias in ["../escape", "with/slash"] {
            assert_invalid_alias(
                rename_profile("valid-alias", alias),
                "alias may only contain ASCII letters, digits, '_', '-', '.'",
            );
        }

        assert_invalid_alias(rename_profile("valid-alias", ""), "alias cannot be empty");
    }

    #[test]
    fn switch_profile_waits_for_auth_lock() {
        let _env = TestEnv::new();

        let live = crate::auth::codex_auth_path().unwrap();
        let current =
            realistic_auth_json("current@example.com", "acct_current", "acc_old", "ref_old");
        crate::auth::write_auth(&live, &current).unwrap();

        let next = realistic_auth_json("next@example.com", "acct_next", "acc_new", "ref_new");
        let profile_path = super::profile_auth_path("next-profile").unwrap();
        super::ensure_profile_parent(&profile_path).unwrap();
        crate::auth::write_auth(&profile_path, &next).unwrap();

        let lock_path = super::auth_lock_path().unwrap();
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .unwrap();
        FileExt::lock(&lock_file).unwrap();

        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            super::notify_on_test_lock_attempt("auth", attempt_tx);
            let _ = done_tx.send(super::switch_profile("next-profile"));
        });
        let mut cleanup = ThreadCleanup::new(lock_file);
        cleanup.push(handle);

        attempt_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("switch did not reach auth lock attempt");
        assert!(
            matches!(
                done_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "switch should block while auth lock is held"
        );
        assert_eq!(
            crate::auth::read_auth(&live)
                .unwrap()
                .pointer("/tokens/access_token")
                .and_then(|v| v.as_str()),
            Some("acc_old")
        );

        cleanup.release_blocker();

        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("switch did not finish after auth lock release")
            .unwrap();
        cleanup.join_all();
        assert_eq!(
            crate::auth::read_auth(&live)
                .unwrap()
                .pointer("/tokens/access_token")
                .and_then(|v| v.as_str()),
            Some("acc_new")
        );
        assert_eq!(super::read_current(), "next-profile");
    }

    #[test]
    fn auth_lock_timeout_preserves_live_lock_inode() {
        let _env = TestEnv::new();
        let lock_path = super::auth_lock_path().unwrap();
        super::ensure_private_dir(lock_path.parent().unwrap()).unwrap();
        let holder = super::open_lock_file(&lock_path).unwrap();
        FileExt::lock(&holder).unwrap();
        super::write_lock_holder(&holder);

        let err =
            super::acquire_file_lock(&lock_path, Duration::from_millis(25), "auth").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("auth lock"), "{message}");
        assert!(
            message.contains(&lock_path.display().to_string()),
            "{message}"
        );

        let reopened = super::open_lock_file(&lock_path).unwrap();
        assert!(matches!(
            FileExt::try_lock(&reopened),
            Err(fs4::TryLockError::WouldBlock)
        ));
        FileExt::unlock(&holder).unwrap();
    }

    #[test]
    fn switch_profile_waits_for_launch_session_lease() {
        let _env = TestEnv::new();
        let next = realistic_auth_json("next@example.com", "acct_next", "acc_new", "ref_new");
        let profile_path = super::profile_auth_path("next-profile").unwrap();
        super::ensure_profile_parent(&profile_path).unwrap();
        crate::auth::write_auth(&profile_path, &next).unwrap();

        let lease = super::lock_launch_session().unwrap();
        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            super::notify_on_test_lock_attempt("launch session", attempt_tx);
            let _ = done_tx.send(super::switch_profile("next-profile"));
        });
        let mut cleanup = ThreadCleanup::new(lease);
        cleanup.push(handle);

        attempt_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("switch did not reach launch session lock attempt");
        assert!(
            matches!(
                done_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ),
            "switch must wait while the launch session lease is held"
        );

        cleanup.release_blocker();
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("switch did not finish after launch session lease release")
            .unwrap();
        cleanup.join_all();
    }

    #[test]
    fn refreshed_profile_and_live_auth_update_are_one_transaction() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "a-old", "a-ref");
        let bob = realistic_auth_json("bob@example.com", "acct_b", "b-old", "b-ref");
        let alice_path = super::profile_auth_path("alice").unwrap();
        let bob_path = super::profile_auth_path("bob").unwrap();
        super::ensure_profile_parent(&alice_path).unwrap();
        super::ensure_profile_parent(&bob_path).unwrap();
        crate::auth::write_auth(&alice_path, &alice).unwrap();
        crate::auth::write_auth(&bob_path, &bob).unwrap();
        super::switch_profile("alice").unwrap();

        let auth_gate = super::lock_live_auth().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let updater = std::thread::spawn(move || {
            let result = super::update_profile_tokens_if_refresh_matches_after_launch(
                "alice",
                "a-ref",
                "a-id-new",
                "a-new",
                "a-ref-new",
                || {
                    let _ = started_tx.send(());
                },
            );
            let _ = done_tx.send(result);
        });
        let mut cleanup = ThreadCleanup::new(auth_gate);
        cleanup.push(updater);
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let (switch_tx, switch_rx) = std::sync::mpsc::channel();
        let switcher = std::thread::spawn(move || {
            let _ = switch_tx.send(super::switch_profile("bob"));
        });
        cleanup.push(switcher);
        cleanup.release_blocker();
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .then_some(())
            .expect("refresh CAS should persist");
        switch_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("profile switch did not finish after refresh transaction")
            .unwrap();
        cleanup.join_all();

        assert_eq!(super::read_current(), "bob");
        let live = crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap();
        assert_eq!(
            live.pointer("/tokens/access_token")
                .and_then(|v| v.as_str()),
            Some("b-old")
        );
        let alice_updated = crate::auth::read_auth(&alice_path).unwrap();
        assert_eq!(
            alice_updated
                .pointer("/tokens/access_token")
                .and_then(|v| v.as_str()),
            Some("a-new")
        );
    }

    #[test]
    fn sync_current_from_live_matches_live_identity() {
        let _env = TestEnv::new();

        let alpha = realistic_auth_json("alpha@example.com", "acct_alpha", "acc_a", "ref_a");
        let alpha_path = super::profile_auth_path("alpha").unwrap();
        super::ensure_profile_parent(&alpha_path).unwrap();
        crate::auth::write_auth(&alpha_path, &alpha).unwrap();

        let beta = realistic_auth_json("beta@example.com", "acct_beta", "acc_b_old", "ref_b_old");
        let beta_path = super::profile_auth_path("beta").unwrap();
        super::ensure_profile_parent(&beta_path).unwrap();
        crate::auth::write_auth(&beta_path, &beta).unwrap();

        super::write_current("alpha").unwrap();
        let live = realistic_auth_json("beta@example.com", "acct_beta", "acc_b_new", "ref_b_new");
        crate::auth::write_auth(&crate::auth::codex_auth_path().unwrap(), &live).unwrap();

        assert_eq!(super::sync_current_from_live().as_deref(), Some("beta"));
        assert_eq!(super::read_current(), "beta");
    }

    // ── detect_auth_change tests ─────────────────────────────

    fn make_jwt(email: &str, account_id: &str) -> String {
        let claims = serde_json::json!({
            "email": email,
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "plus",
                "chatgpt_account_id": account_id,
                "chatgpt_user_id": format!("user_{account_id}"),
                "organizations": [],
            }
        });
        let json = serde_json::to_vec(&claims).unwrap();
        let encoded = {
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            URL_SAFE_NO_PAD.encode(json)
        };
        format!("x.{encoded}.y")
    }

    /// Build a realistic auth.json matching the format produced by `login::build_auth_json`.
    fn realistic_auth_json(
        email: &str,
        account_id: &str,
        access_token: &str,
        refresh_token: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": make_jwt(email, account_id),
                "access_token": access_token,
                "refresh_token": refresh_token,
                "account_id": account_id,
            },
            "last_refresh": "2026-04-07T00:00:00Z"
        })
    }

    // ── Basic branch coverage ────────────────────────────────

    #[test]
    fn detect_no_auth_file_returns_no_change() {
        let _env = TestEnv::new();
        assert!(matches!(
            super::detect_auth_change(),
            super::AuthChange::NoChange
        ));
    }

    #[test]
    fn detect_corrupt_auth_file_returns_no_change() {
        let env = TestEnv::new();
        let live = crate::auth::codex_auth_path().unwrap();
        let parent = live.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
        std::fs::write(&live, "{invalid json!!!").unwrap();
        assert!(matches!(
            super::detect_auth_change(),
            super::AuthChange::NoChange
        ));
        drop(env);
    }

    #[test]
    fn detect_exact_match_returns_no_change() {
        let _env = TestEnv::new();
        let val = realistic_auth_json("test@example.com", "acct_1", "acc_a", "ref_a");
        let live = crate::auth::codex_auth_path().unwrap();
        crate::auth::write_auth(&live, &val).unwrap();
        super::cmd_save(Some("test-profile")).unwrap();
        assert!(matches!(
            super::detect_auth_change(),
            super::AuthChange::NoChange
        ));
    }

    #[test]
    fn existing_import_target_matches_saved_account_and_ignores_others() {
        let env = TestEnv::new();
        // Save a profile for alice / acct_1.
        let val = realistic_auth_json("alice@example.com", "acct_1", "acc_a", "ref_a");
        let live = crate::auth::codex_auth_path().unwrap();
        crate::auth::write_auth(&live, &val).unwrap();
        super::cmd_save(Some("alice")).unwrap();

        // A fresh dump of the SAME account with rotated tokens (so the bytes,
        // and thus the file hash, differ) is still detected by account_id +
        // email — this is exactly the re-import that would otherwise duplicate
        // the account and spend its single-use refresh token.
        let reimport = realistic_auth_json("alice@example.com", "acct_1", "acc_new", "ref_new");
        let src = env._home.path().join("incoming.json");
        std::fs::write(&src, serde_json::to_vec(&reimport).unwrap()).unwrap();
        assert_eq!(
            super::existing_import_target(&src, &reimport).as_deref(),
            Some("alice"),
            "a re-import of a saved account must be detected so it can be skipped"
        );

        // A genuinely different account is not matched, so it still imports.
        let other = realistic_auth_json("bob@example.com", "acct_2", "acc_b", "ref_b");
        let other_src = env._home.path().join("other.json");
        std::fs::write(&other_src, serde_json::to_vec(&other).unwrap()).unwrap();
        assert_eq!(super::existing_import_target(&other_src, &other), None);
    }

    #[test]
    fn detect_new_account_when_no_profiles_exist() {
        let _env = TestEnv::new();
        let val = realistic_auth_json("new@example.com", "acct_new", "acc_x", "ref_x");
        let live = crate::auth::codex_auth_path().unwrap();
        crate::auth::write_auth(&live, &val).unwrap();
        assert!(matches!(
            super::detect_auth_change(),
            super::AuthChange::NewAccount
        ));
    }

    #[test]
    fn detect_new_account_when_different_identity() {
        let _env = TestEnv::new();
        let alice = realistic_auth_json("alice@example.com", "acct_alice", "acc_1", "ref_1");
        let live = crate::auth::codex_auth_path().unwrap();
        crate::auth::write_auth(&live, &alice).unwrap();
        super::cmd_save(Some("alice")).unwrap();
        // Different person
        let bob = realistic_auth_json("bob@example.com", "acct_bob", "acc_2", "ref_2");
        crate::auth::write_auth(&live, &bob).unwrap();
        assert!(matches!(
            super::detect_auth_change(),
            super::AuthChange::NewAccount
        ));
    }

    // ── Token update scenarios (real refresh patterns) ───────

    #[test]
    fn detect_tokens_updated_refresh_token_changed() {
        let _env = TestEnv::new();
        let val = realistic_auth_json("user@example.com", "acct_u", "acc_old", "ref_old");
        let live = crate::auth::codex_auth_path().unwrap();
        crate::auth::write_auth(&live, &val).unwrap();
        super::cmd_save(Some("user-profile")).unwrap();
        // Re-login: new refresh_token
        let updated = realistic_auth_json("user@example.com", "acct_u", "acc_old", "ref_new");
        crate::auth::write_auth(&live, &updated).unwrap();
        match super::detect_auth_change() {
            super::AuthChange::TokensUpdated { alias } => assert_eq!(alias, "user-profile"),
            other => panic!("expected TokensUpdated, got {other:?}"),
        }
    }

    #[test]
    fn detect_tokens_updated_only_access_token_changed() {
        let _env = TestEnv::new();
        // Simulates token refresh where only access_token rotates (refresh_token reused)
        let val = realistic_auth_json("user@example.com", "acct_u", "acc_old", "ref_same");
        let live = crate::auth::codex_auth_path().unwrap();
        crate::auth::write_auth(&live, &val).unwrap();
        super::cmd_save(Some("user-profile")).unwrap();
        let updated = realistic_auth_json("user@example.com", "acct_u", "acc_new", "ref_same");
        crate::auth::write_auth(&live, &updated).unwrap();
        match super::detect_auth_change() {
            super::AuthChange::TokensUpdated { alias } => assert_eq!(alias, "user-profile"),
            other => panic!("expected TokensUpdated, got {other:?}"),
        }
    }

    #[test]
    fn detect_tokens_updated_only_last_refresh_timestamp_changed() {
        let _env = TestEnv::new();
        // Simulates codex CLI updating only the last_refresh timestamp
        let val = realistic_auth_json("user@example.com", "acct_u", "acc_1", "ref_1");
        let live = crate::auth::codex_auth_path().unwrap();
        crate::auth::write_auth(&live, &val).unwrap();
        super::cmd_save(Some("ts-profile")).unwrap();
        // Same tokens, different timestamp
        let mut updated = realistic_auth_json("user@example.com", "acct_u", "acc_1", "ref_1");
        updated["last_refresh"] = serde_json::json!("2026-04-08T12:00:00Z");
        crate::auth::write_auth(&live, &updated).unwrap();
        match super::detect_auth_change() {
            super::AuthChange::TokensUpdated { alias } => assert_eq!(alias, "ts-profile"),
            other => panic!("expected TokensUpdated, got {other:?}"),
        }
    }

    // ── Identity matching edge cases ─────────────────────────

    #[test]
    fn detect_tokens_updated_email_case_insensitive() {
        let _env = TestEnv::new();
        let val = realistic_auth_json("User@Example.COM", "acct_u", "acc_1", "ref_1");
        let live = crate::auth::codex_auth_path().unwrap();
        crate::auth::write_auth(&live, &val).unwrap();
        super::cmd_save(Some("case-profile")).unwrap();
        // Same email different case, new token
        let updated = realistic_auth_json("user@example.com", "acct_u", "acc_2", "ref_2");
        crate::auth::write_auth(&live, &updated).unwrap();
        match super::detect_auth_change() {
            super::AuthChange::TokensUpdated { alias } => assert_eq!(alias, "case-profile"),
            other => panic!("expected TokensUpdated, got {other:?}"),
        }
    }

    #[test]
    fn detect_tokens_updated_email_only_fallback_when_account_id_missing() {
        let _env = TestEnv::new();
        // Profile saved with account_id
        let val = realistic_auth_json("fallback@example.com", "acct_fb", "acc_1", "ref_1");
        let live = crate::auth::codex_auth_path().unwrap();
        crate::auth::write_auth(&live, &val).unwrap();
        super::cmd_save(Some("fb-profile")).unwrap();
        // Live auth.json has no account_id in JWT claims (email-only match)
        let claims_no_id = serde_json::json!({
            "email": "fallback@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "plus",
                "organizations": [],
            }
        });
        let json_bytes = serde_json::to_vec(&claims_no_id).unwrap();
        let encoded = {
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            URL_SAFE_NO_PAD.encode(json_bytes)
        };
        let jwt_no_id = format!("x.{encoded}.y");
        // account_id is empty string — should be treated as None after fix
        let updated = serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": jwt_no_id,
                "access_token": "acc_new",
                "refresh_token": "ref_new",
                "account_id": "",
            },
            "last_refresh": "2026-04-08T00:00:00Z"
        });
        crate::auth::write_auth(&live, &updated).unwrap();
        match super::detect_auth_change() {
            super::AuthChange::TokensUpdated { alias } => assert_eq!(alias, "fb-profile"),
            other => panic!("expected TokensUpdated via email fallback, got {other:?}"),
        }
    }

    // ── update_profile_from_live ─────────────────────────────

    #[test]
    fn update_profile_from_live_syncs_content_and_preserves_others() {
        let _env = TestEnv::new();
        let live = crate::auth::codex_auth_path().unwrap();

        // Create two profiles
        let alice = realistic_auth_json("alice@example.com", "acct_a", "acc_a1", "ref_a1");
        crate::auth::write_auth(&live, &alice).unwrap();
        super::cmd_save(Some("alice")).unwrap();
        let bob = realistic_auth_json("bob@example.com", "acct_b", "acc_b1", "ref_b1");
        crate::auth::write_auth(&live, &bob).unwrap();
        super::cmd_save(Some("bob")).unwrap();

        // Update live with new alice tokens. The stamp has to move forward: a
        // rotated refresh_token may only overwrite the stored one when the live
        // copy can prove it is the newer of the two.
        let alice_updated = stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_a2",
            "ref_a2",
            Some("2026-04-09T00:00:00Z"),
        );
        crate::auth::write_auth(&live, &alice_updated).unwrap();
        super::update_profile_from_live("alice").unwrap();

        // Verify: alice's profile file content matches updated live
        let profile_val =
            crate::auth::read_auth(&super::profile_auth_path("alice").unwrap()).unwrap();
        assert_eq!(profile_val["tokens"]["access_token"], "acc_a2");
        assert_eq!(profile_val["tokens"]["refresh_token"], "ref_a2");
        assert_eq!(profile_val["OPENAI_API_KEY"], serde_json::Value::Null);

        // Verify: bob's profile was NOT modified
        let bob_val = crate::auth::read_auth(&super::profile_auth_path("bob").unwrap()).unwrap();
        assert_eq!(bob_val["tokens"]["access_token"], "acc_b1");

        // Verify: current marker updated
        assert_eq!(super::read_current(), "alice");
    }

    #[test]
    fn update_profile_from_live_rejects_different_account_identity() {
        let _env = TestEnv::new();
        let live = crate::auth::codex_auth_path().unwrap();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "acc_a1", "ref_a1");
        crate::auth::write_auth(&live, &alice).unwrap();
        super::cmd_save(Some("alice")).unwrap();

        let bob = realistic_auth_json("bob@example.com", "acct_b", "acc_b1", "ref_b1");
        crate::auth::write_auth(&live, &bob).unwrap();

        let result = super::update_profile_from_live("alice");
        assert!(result.is_err());
        let saved = crate::auth::read_auth(&super::profile_auth_path("alice").unwrap()).unwrap();
        assert_eq!(saved["tokens"]["access_token"], "acc_a1");
    }

    #[test]
    fn relogin_rejects_different_account_identity() {
        let _env = TestEnv::new();
        let live = crate::auth::codex_auth_path().unwrap();
        let alice = realistic_auth_json("alice@example.com", "acct_a", "acc_a1", "ref_a1");
        crate::auth::write_auth(&live, &alice).unwrap();
        super::cmd_save(Some("alice")).unwrap();

        let bob = realistic_auth_json("bob@example.com", "acct_b", "acc_b1", "ref_b1");
        let result = super::replace_profile_auth_and_live_if_current("alice", &bob);
        assert!(result.is_err());
        let saved = crate::auth::read_auth(&super::profile_auth_path("alice").unwrap()).unwrap();
        assert_eq!(saved["tokens"]["access_token"], "acc_a1");
    }

    #[test]
    fn relogin_allows_matching_legacy_email_without_account_id() {
        let _env = TestEnv::new();
        let live = crate::auth::codex_auth_path().unwrap();
        let old = realistic_auth_json("alice@example.com", "", "acc_a1", "ref_a1");
        crate::auth::write_auth(&live, &old).unwrap();
        super::cmd_save(Some("alice")).unwrap();

        let refreshed = realistic_auth_json("Alice@example.com", "", "acc_a2", "ref_a2");
        super::replace_profile_auth_and_live_if_current("alice", &refreshed).unwrap();
        let saved = crate::auth::read_auth(&super::profile_auth_path("alice").unwrap()).unwrap();
        assert_eq!(saved["tokens"]["access_token"], "acc_a2");
    }

    // ── Failure paths ────────────────────────────────────────

    #[test]
    fn update_profile_from_live_fails_when_no_auth_file() {
        let _env = TestEnv::new();
        // No live auth.json exists
        let result = super::update_profile_from_live("nonexistent");
        assert!(result.is_err());
    }

    // ── Rollback protection (one-time-rotation refresh tokens) ──

    /// `realistic_auth_json` with an explicit (or absent) `last_refresh` stamp.
    fn stamped_auth_json(
        email: &str,
        account_id: &str,
        access_token: &str,
        refresh_token: &str,
        last_refresh: Option<&str>,
    ) -> serde_json::Value {
        let mut val = realistic_auth_json(email, account_id, access_token, refresh_token);
        match last_refresh {
            Some(ts) => val["last_refresh"] = serde_json::json!(ts),
            None => {
                val.as_object_mut().unwrap().remove("last_refresh");
            }
        }
        val
    }

    fn seed_profile(alias: &str, val: &serde_json::Value) {
        let path = super::profile_auth_path(alias).unwrap();
        super::ensure_profile_parent(&path).unwrap();
        crate::auth::write_auth(&path, val).unwrap();
    }

    fn write_live(val: &serde_json::Value) {
        crate::auth::write_auth(&crate::auth::codex_auth_path().unwrap(), val).unwrap();
    }

    fn profile_refresh_token(alias: &str) -> String {
        crate::auth::read_auth(&super::profile_auth_path(alias).unwrap()).unwrap()["tokens"]
            ["refresh_token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn profile_access_token(alias: &str) -> String {
        crate::auth::read_auth(&super::profile_auth_path(alias).unwrap()).unwrap()["tokens"]
            ["access_token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// The rollback guard is typed so callers can recognise it without matching
    /// on wording; every entry point must reject through that same type.
    fn assert_rollback_refusal(err: &anyhow::Error) -> &super::StaleLiveAuth {
        err.downcast_ref::<super::StaleLiveAuth>()
            .unwrap_or_else(|| panic!("the refusal must stay downcastable, got: {err:#}"))
    }

    #[test]
    fn update_profile_from_live_rejects_live_older_than_profile() {
        let _env = TestEnv::new();
        // The profile already holds a rotated refresh token; live still holds the
        // dead predecessor. Copying live over the profile would destroy the only
        // usable credential for this account.
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_new",
                "ref_new",
                Some("2026-07-28T04:51:15Z"),
            ),
        );
        super::write_current("bob").unwrap();
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_dead",
            "ref_dead",
            Some("2026-07-20T00:00:00Z"),
        ));

        let err = super::update_profile_from_live("alice").unwrap_err();
        // Typed, so a caller deciding whether to show this to the user does not
        // have to match on the wording.
        let stale = err
            .downcast_ref::<super::StaleLiveAuth>()
            .unwrap_or_else(|| panic!("the refusal must stay downcastable, got: {err:#}"));
        assert_eq!(stale.alias, "alice");
        assert!(
            err.to_string().contains("older"),
            "error must explain the inverted direction, got: {err}"
        );
        assert_eq!(profile_refresh_token("alice"), "ref_new");
        assert_eq!(
            super::read_current(),
            "bob",
            "a rejected read-back must not repoint the current profile"
        );
    }

    #[test]
    fn update_profile_from_live_accepts_live_newer_than_profile() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_old",
                "ref_old",
                Some("2026-07-20T00:00:00Z"),
            ),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_new",
            Some("2026-07-28T04:51:15Z"),
        ));

        super::update_profile_from_live("alice").unwrap();
        assert_eq!(profile_refresh_token("alice"), "ref_new");
        assert_eq!(super::read_current(), "alice");
    }

    #[test]
    fn update_profile_from_live_allows_same_refresh_token_without_any_timestamp() {
        let _env = TestEnv::new();
        // Legacy profile without a stamp, and the refresh token did not rotate:
        // the write cannot revoke anything, so the ordinary sync must not be
        // blocked just because neither side can be ordered in time.
        seed_profile(
            "alice",
            &stamped_auth_json("alice@example.com", "acct_a", "acc_old", "ref_same", None),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_same",
            None,
        ));

        super::update_profile_from_live("alice").unwrap();
        assert_eq!(profile_access_token("alice"), "acc_new");
    }

    #[test]
    fn update_profile_from_live_refuses_rotated_token_when_profile_has_no_timestamp() {
        let _env = TestEnv::new();
        // A legacy profile carries no stamp, so nothing orders it against the
        // live copy. The refresh tokens differ, which means exactly one of them
        // is still valid — guessing would destroy the other.
        seed_profile(
            "alice",
            &stamped_auth_json("alice@example.com", "acct_a", "acc_old", "ref_old", None),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_new",
            Some("2026-07-20T00:00:00Z"),
        ));

        let err = super::update_profile_from_live("alice").unwrap_err();
        assert_rollback_refusal(&err);
        assert!(
            err.to_string().contains("no last_refresh"),
            "the message must say the profile carries no stamp, got: {err}"
        );
        assert_eq!(profile_refresh_token("alice"), "ref_old");
    }

    #[test]
    fn update_profile_from_live_refuses_rotated_token_when_profile_timestamp_is_unparseable() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_old",
                "ref_old",
                Some("not-a-timestamp"),
            ),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_new",
            Some("2026-07-20T00:00:00Z"),
        ));

        let err = super::update_profile_from_live("alice").unwrap_err();
        assert_rollback_refusal(&err);
        assert!(
            err.to_string().contains("not-a-timestamp"),
            "the message must echo the unusable stamp, got: {err}"
        );
        assert_eq!(profile_refresh_token("alice"), "ref_old");
    }

    #[test]
    fn update_profile_from_live_refuses_rotated_token_when_timestamps_are_equal() {
        let _env = TestEnv::new();
        // Equal wall-clock stamps (the field has second resolution) prove
        // nothing about which rotation happened first.
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_old",
                "ref_old",
                Some("2026-07-20T00:00:00Z"),
            ),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_new",
            Some("2026-07-20T00:00:00Z"),
        ));

        let err = super::update_profile_from_live("alice").unwrap_err();
        assert_rollback_refusal(&err);
        assert_eq!(profile_refresh_token("alice"), "ref_old");
    }

    #[test]
    fn update_profile_from_live_rejects_unstamped_live_against_stamped_profile() {
        let _env = TestEnv::new();
        // A stamped profile records a known refresh time; an unstamped live file
        // cannot prove it is at least as fresh, so the copy is refused.
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_new",
                "ref_new",
                Some("2026-07-28T04:51:15Z"),
            ),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_dead",
            "ref_dead",
            None,
        ));

        assert!(super::update_profile_from_live("alice").is_err());
        assert_eq!(profile_refresh_token("alice"), "ref_new");
    }

    // ── Read-back identity ambiguity (same email, several workspaces) ──

    fn jwt_without_account_id(email: &str) -> String {
        let claims = serde_json::json!({
            "email": email,
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "plus",
                "organizations": [],
            }
        });
        let json = serde_json::to_vec(&claims).unwrap();
        let encoded = {
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            URL_SAFE_NO_PAD.encode(json)
        };
        format!("x.{encoded}.y")
    }

    fn auth_json_without_account_id(
        email: &str,
        access_token: &str,
        refresh_token: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": jwt_without_account_id(email),
                "access_token": access_token,
                "refresh_token": refresh_token,
                "account_id": "",
            },
            "last_refresh": "2026-07-20T00:00:00Z"
        })
    }

    #[test]
    fn detect_auth_change_refuses_to_guess_between_same_email_workspaces() {
        let _env = TestEnv::new();
        seed_profile(
            "oai001",
            &realistic_auth_json("oai001@ozi.xyz", "acct_team", "acc_t", "ref_t"),
        );
        seed_profile(
            "oai001_20x",
            &realistic_auth_json("oai001@ozi.xyz", "acct_personal", "acc_p", "ref_p"),
        );
        // Live file carries no account_id — the email alone matches both profiles.
        write_live(&auth_json_without_account_id(
            "oai001@ozi.xyz",
            "acc_live",
            "ref_live",
        ));

        match super::detect_auth_change() {
            super::AuthChange::NoChange => {}
            other => panic!("ambiguous email match must not select a profile, got {other:?}"),
        }
    }

    #[test]
    fn detect_auth_change_picks_workspace_profile_by_account_id() {
        let _env = TestEnv::new();
        seed_profile(
            "oai001",
            &realistic_auth_json("oai001@ozi.xyz", "acct_team", "acc_t", "ref_t"),
        );
        seed_profile(
            "oai001_20x",
            &realistic_auth_json("oai001@ozi.xyz", "acct_personal", "acc_p", "ref_p"),
        );
        write_live(&realistic_auth_json(
            "oai001@ozi.xyz",
            "acct_personal",
            "acc_p2",
            "ref_p2",
        ));

        match super::detect_auth_change() {
            super::AuthChange::TokensUpdated { alias } => assert_eq!(alias, "oai001_20x"),
            other => panic!("expected TokensUpdated for oai001_20x, got {other:?}"),
        }
    }

    // ── cmd_save identity ambiguity (same email, several workspaces) ──

    #[test]
    fn cmd_save_ambiguous_email_refuses_to_guess_between_profiles() {
        let _env = TestEnv::new();
        seed_profile(
            "oai001",
            &realistic_auth_json("oai001@ozi.xyz", "acct_team", "acc_t", "ref_t"),
        );
        seed_profile(
            "oai001_20x",
            &realistic_auth_json("oai001@ozi.xyz", "acct_personal", "acc_p", "ref_p"),
        );
        // Live file carries no account_id — the email alone matches both profiles.
        write_live(&auth_json_without_account_id(
            "oai001@ozi.xyz",
            "acc_live",
            "ref_live",
        ));

        let err = cmd_save(None).expect_err("ambiguous email match must not silently save");
        let msg = err.to_string();
        assert!(
            msg.contains("oai001"),
            "message should list candidate: {msg}"
        );
        assert!(
            msg.contains("oai001_20x"),
            "message should list candidate: {msg}"
        );

        // Neither existing profile was silently overwritten.
        assert_eq!(profile_refresh_token("oai001"), "ref_t");
        assert_eq!(profile_refresh_token("oai001_20x"), "ref_p");
    }

    #[test]
    fn cmd_save_exact_match_updates_the_right_profile() {
        let _env = TestEnv::new();
        seed_profile(
            "oai001",
            &realistic_auth_json("oai001@ozi.xyz", "acct_team", "acc_t", "ref_t"),
        );
        seed_profile(
            "oai001_20x",
            &realistic_auth_json("oai001@ozi.xyz", "acct_personal", "acc_p", "ref_p"),
        );
        write_live(&stamped_auth_json(
            "oai001@ozi.xyz",
            "acct_personal",
            "acc_p2",
            "ref_p2",
            Some("2026-04-09T00:00:00Z"),
        ));

        match cmd_save(None) {
            Ok(super::SaveAction::Updated(alias)) => assert_eq!(alias, "oai001_20x"),
            other => panic!("expected exact match to update oai001_20x, got {other:?}"),
        }
        assert_eq!(profile_refresh_token("oai001_20x"), "ref_p2");
        assert_eq!(profile_refresh_token("oai001"), "ref_t");
    }

    #[test]
    fn cmd_save_single_email_only_candidate_updates_it() {
        let _env = TestEnv::new();
        seed_profile(
            "fallback",
            &auth_json_without_account_id("fallback@example.com", "acc_old", "ref_old"),
        );
        let mut live = auth_json_without_account_id("fallback@example.com", "acc_new", "ref_new");
        live["last_refresh"] = serde_json::json!("2026-07-25T00:00:00Z");
        write_live(&live);

        match cmd_save(None) {
            Ok(super::SaveAction::Updated(alias)) => assert_eq!(alias, "fallback"),
            other => panic!("expected single email-only candidate to update, got {other:?}"),
        }
        assert_eq!(profile_refresh_token("fallback"), "ref_new");
    }

    // ── An explicit alias is the user's own disambiguation ──

    /// Two profiles for one email is exactly the state the ambiguity refusal
    /// tells the user to resolve with `save <alias>`, so that alias has to be
    /// obeyed verbatim — redirecting it to the other twin would overwrite the
    /// single-use refresh token the user was trying to protect.
    fn seed_email_twins() {
        seed_profile(
            "oai001",
            &realistic_auth_json("oai001@ozi.xyz", "acct_team", "acc_t", "ref_t"),
        );
        seed_profile(
            "oai001_20x",
            &realistic_auth_json("oai001@ozi.xyz", "acct_personal", "acc_p", "ref_p"),
        );
    }

    #[test]
    fn cmd_save_with_explicit_alias_writes_to_that_profile_not_its_email_twin() {
        let _env = TestEnv::new();
        seed_email_twins();
        // No account_id on the live copy: the email alone matches both twins,
        // and "first candidate wins" would land on `oai001`.
        write_live(&auth_json_without_account_id(
            "oai001@ozi.xyz",
            "acc_live",
            "ref_live",
        ));

        match cmd_save(Some("oai001_20x")) {
            Ok(super::SaveAction::Updated(alias)) => assert_eq!(alias, "oai001_20x"),
            other => panic!("expected the named profile to be updated, got {other:?}"),
        }
        assert_eq!(profile_refresh_token("oai001_20x"), "ref_live");
        assert_eq!(
            profile_refresh_token("oai001"),
            "ref_t",
            "the twin the user did not name must keep its credentials"
        );
        assert_eq!(super::read_current(), "oai001_20x");
    }

    #[test]
    fn save_imported_auth_value_rejects_email_only_credentials_even_with_explicit_alias() {
        let _env = TestEnv::new();
        seed_email_twins();
        let imported = auth_json_without_account_id("oai001@ozi.xyz", "acc_imp", "ref_imp");

        let err =
            super::save_imported_auth_value(&imported, Some("oai001_20x"), "acct_import", None)
                .expect_err("unverified JWT email must not select an existing profile");
        assert!(err.to_string().contains("non-empty account_id"));
        assert_eq!(profile_refresh_token("oai001_20x"), "ref_p");
        assert_eq!(profile_refresh_token("oai001"), "ref_t");
    }

    #[test]
    fn save_imported_auth_value_does_not_overwrite_an_explicit_alias() {
        let _env = TestEnv::new();
        seed_email_twins();
        let imported = realistic_auth_json("oai001@ozi.xyz", "acct_attacker", "acc_imp", "ref_imp");
        let action =
            super::save_imported_auth_value(&imported, Some("oai001_20x"), "acct_attacker", None)
                .expect("an explicit alias collision should create a unique profile");
        match action {
            super::SaveAction::Created(alias) => assert_eq!(alias, "oai001_20x_2"),
            other => panic!("import must never update the named profile, got {other:?}"),
        }
        assert_eq!(profile_refresh_token("oai001_20x"), "ref_p");
        assert_eq!(profile_refresh_token("oai001_20x_2"), "ref_imp");
    }

    #[test]
    fn save_imported_auth_value_requires_usage_validated_account_id_match() {
        let _env = TestEnv::new();
        let imported = realistic_auth_json("alice@example.com", "acct_alice", "acc_imp", "ref_imp");
        let err = super::save_imported_auth_value(&imported, None, "acct_other", None)
            .expect_err("unverified JWT identity cannot replace validation evidence");
        assert!(err.to_string().contains("does not match Usage API"));
        assert!(super::list_profiles().unwrap().is_empty());
    }

    #[test]
    fn imported_credentials_never_overwrite_an_existing_profile_without_user_proof() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &realistic_auth_json(
                "alice@example.com",
                "acct_shared_workspace",
                "access_existing",
                "refresh_existing",
            ),
        );
        let imported = realistic_auth_json(
            "alice@example.com",
            "acct_shared_workspace",
            "access_imported",
            "refresh_imported",
        );

        let action = super::save_imported_auth_value(
            &imported,
            None,
            "acct_shared_workspace",
            Some("alice"),
        )
        .expect("a validated import should be preserved in a new profile");

        match action {
            super::SaveAction::Created(alias) => assert_eq!(alias, "alice_2"),
            other => panic!("import must create instead of overwriting, got {other:?}"),
        }
        assert_eq!(profile_refresh_token("alice"), "refresh_existing");
        assert_eq!(profile_refresh_token("alice_2"), "refresh_imported");
    }

    #[test]
    fn refresh_token_cas_does_not_overwrite_a_concurrent_relogin() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &realistic_auth_json("alice@example.com", "acct_a", "old_access", "refresh_new"),
        );
        let written = super::update_profile_tokens_if_refresh_matches(
            "alice",
            "refresh_old",
            "stale_id",
            "stale_access",
            "stale_refresh",
        )
        .unwrap();
        assert!(
            !written,
            "a re-login that replaced the presented token wins"
        );
        assert_eq!(profile_refresh_token("alice"), "refresh_new");

        let written = super::update_profile_tokens_if_refresh_matches(
            "alice",
            "refresh_new",
            "fresh_id",
            "fresh_access",
            "fresh_refresh",
        )
        .unwrap();
        assert!(written);
        assert_eq!(profile_refresh_token("alice"), "fresh_refresh");
    }

    #[test]
    fn switch_rejects_disallowed_managed_workspace_without_changing_live_auth() {
        let env = TestEnv::new();
        seed_profile(
            "blocked",
            &realistic_auth_json(
                "blocked@example.com",
                "workspace-blocked",
                "blocked_access",
                "blocked_refresh",
            ),
        );
        let original = realistic_auth_json(
            "allowed@example.com",
            "workspace-allowed",
            "live_access",
            "live_refresh",
        );
        write_live(&original);
        std::fs::create_dir_all(env._home.path().join(".codex")).unwrap();
        std::fs::write(
            env._home.path().join(".codex/config.toml"),
            "forced_chatgpt_workspace_id = \"workspace-allowed\"\n",
        )
        .unwrap();

        let err = super::switch_profile("blocked").expect_err("managed policy must fail closed");
        assert!(err.to_string().contains("not allowed"));
        assert_eq!(
            crate::auth::read_auth(&crate::auth::codex_auth_path().unwrap()).unwrap(),
            original
        );
    }

    #[test]
    fn save_rejects_disallowed_managed_workspace_before_creating_profile() {
        let env = TestEnv::new();
        let blocked = realistic_auth_json(
            "blocked@example.com",
            "workspace-blocked",
            "blocked_access",
            "blocked_refresh",
        );
        write_live(&blocked);
        std::fs::create_dir_all(env._home.path().join(".codex")).unwrap();
        std::fs::write(
            env._home.path().join(".codex/config.toml"),
            "forced_chatgpt_workspace_id = \"workspace-allowed\"\n",
        )
        .unwrap();

        let err = super::cmd_save(Some("blocked"))
            .expect_err("managed policy must guard new profile creation");
        assert!(err.to_string().contains("not allowed"));
        assert!(!super::profile_auth_path("blocked").unwrap().exists());
    }

    // ── Rollback protection on the save/import entry points ──

    /// A profile holding the rotated token, plus a live copy still holding its
    /// already-revoked predecessor.
    fn seed_profile_ahead_of_live() {
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_new",
                "ref_new",
                Some("2026-07-28T04:51:15Z"),
            ),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_dead",
            "ref_dead",
            Some("2026-07-20T00:00:00Z"),
        ));
    }

    #[test]
    fn cmd_save_refuses_to_roll_a_profile_back_to_a_revoked_token() {
        let _env = TestEnv::new();
        seed_profile_ahead_of_live();

        let named = cmd_save(Some("alice")).expect_err("an explicit alias must not skip the guard");
        assert_eq!(assert_rollback_refusal(&named).alias, "alice");
        assert_eq!(profile_refresh_token("alice"), "ref_new");

        let inferred = cmd_save(None).expect_err("the inferred target must not skip the guard");
        assert_rollback_refusal(&inferred);
        assert_eq!(profile_refresh_token("alice"), "ref_new");
    }

    #[test]
    fn save_imported_auth_value_preserves_a_stale_dump_without_overwriting() {
        let _env = TestEnv::new();
        seed_profile_ahead_of_live();
        // A stale auth.json dump on disk is the same hazard as a stale live file.
        let imported = stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_dead",
            "ref_dead",
            Some("2026-07-20T00:00:00Z"),
        );

        let action = super::save_imported_auth_value(&imported, None, "acct_a", None)
            .expect("import should preserve the dump in a unique profile");
        match action {
            super::SaveAction::Created(alias) => assert_eq!(alias, "alice_2"),
            other => panic!("import must not update the newer profile, got {other:?}"),
        }
        assert_eq!(profile_refresh_token("alice"), "ref_new");
        assert_eq!(profile_refresh_token("alice_2"), "ref_dead");
    }

    #[test]
    fn cmd_save_allows_resave_when_the_refresh_token_did_not_rotate() {
        let _env = TestEnv::new();
        // Neither side is stamped, so nothing can be ordered — but the refresh
        // token is identical, so the write cannot revoke anything.
        seed_profile(
            "alice",
            &stamped_auth_json("alice@example.com", "acct_a", "acc_old", "ref_same", None),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_same",
            None,
        ));

        match cmd_save(None) {
            Ok(super::SaveAction::Updated(alias)) => assert_eq!(alias, "alice"),
            other => panic!("an unrotated re-save must still go through, got {other:?}"),
        }
        assert_eq!(profile_access_token("alice"), "acc_new");
    }

    #[test]
    fn cmd_save_refuses_a_rotated_token_when_the_stamps_are_equal() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_old",
                "ref_old",
                Some("2026-07-20T00:00:00Z"),
            ),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_new",
            Some("2026-07-20T00:00:00Z"),
        ));

        let err = cmd_save(None).expect_err("equal stamps cannot order two different tokens");
        assert_rollback_refusal(&err);
        assert_eq!(profile_refresh_token("alice"), "ref_old");
    }

    #[test]
    fn cmd_save_refuses_a_rotated_token_when_the_profile_has_no_stamp() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &stamped_auth_json("alice@example.com", "acct_a", "acc_old", "ref_old", None),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_new",
            Some("2026-07-20T00:00:00Z"),
        ));

        let err = cmd_save(None).expect_err("an unstamped profile cannot be proven older");
        let msg = err.to_string();
        assert_rollback_refusal(&err);
        assert!(
            msg.contains("no last_refresh"),
            "the message must name the profile's state, got: {msg}"
        );
        assert!(
            msg.contains("codex-switch use alice"),
            "the message must offer a way out, got: {msg}"
        );
        assert_eq!(profile_refresh_token("alice"), "ref_old");
    }

    #[test]
    fn cmd_save_updates_the_profile_when_live_is_provably_newer() {
        let _env = TestEnv::new();
        seed_profile(
            "alice",
            &stamped_auth_json(
                "alice@example.com",
                "acct_a",
                "acc_old",
                "ref_old",
                Some("2026-07-20T00:00:00Z"),
            ),
        );
        write_live(&stamped_auth_json(
            "alice@example.com",
            "acct_a",
            "acc_new",
            "ref_new",
            Some("2026-07-28T04:51:15Z"),
        ));

        match cmd_save(None) {
            Ok(super::SaveAction::Updated(alias)) => assert_eq!(alias, "alice"),
            other => panic!("the normal forward sync must still work, got {other:?}"),
        }
        assert_eq!(profile_refresh_token("alice"), "ref_new");
        assert_eq!(super::read_current(), "alice");
    }

    #[test]
    fn detect_no_identity_in_jwt_returns_no_change() {
        let _env = TestEnv::new();
        // auth.json with no email in JWT, no account_id in claims,
        // and empty account_id in tokens (should be filtered to None)
        let empty_claims = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "plus",
                "organizations": [],
            }
        });
        let json_bytes = serde_json::to_vec(&empty_claims).unwrap();
        let encoded = {
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            URL_SAFE_NO_PAD.encode(json_bytes)
        };
        let jwt_empty = format!("x.{encoded}.y");
        let val = serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": jwt_empty,
                "access_token": "acc_x",
                "refresh_token": "ref_x",
                "account_id": "",
            },
            "last_refresh": "2026-04-07T00:00:00Z"
        });
        let live = crate::auth::codex_auth_path().unwrap();
        crate::auth::write_auth(&live, &val).unwrap();
        assert!(matches!(
            super::detect_auth_change(),
            super::AuthChange::NoChange
        ));
    }

    #[test]
    fn login_with_an_explicit_alias_never_writes_to_its_email_twin() {
        let _env = TestEnv::new();
        seed_email_twins();
        // Freshly minted credentials for the personal workspace. Resolving by
        // identity would land on `oai001_20x` and silently replace the working
        // token of a profile the user did not name.
        let minted = realistic_auth_json("oai001@ozi.xyz", "acct_personal", "acc_new", "ref_new");

        let err = super::save_auth_value(minted, Some("oai001"))
            .expect_err("a named profile holding another workspace must not be reassigned");
        assert!(
            format!("{err:#}").contains("oai001"),
            "the refusal must name the profile that was asked for: {err:#}"
        );
        assert_eq!(
            profile_refresh_token("oai001_20x"),
            "ref_p",
            "the twin the user did not name must keep its credentials"
        );
    }

    #[test]
    fn login_without_an_alias_refuses_to_pick_between_email_twins() {
        let _env = TestEnv::new();
        seed_email_twins();
        let minted = auth_json_without_account_id("oai001@ozi.xyz", "acc_new", "ref_new");

        let err = super::save_auth_value(minted, None)
            .expect_err("an ambiguous email must not resolve to the first candidate");
        assert!(format!("{err:#}").contains("oai001"), "{err:#}");
        assert_eq!(profile_refresh_token("oai001"), "ref_t");
        assert_eq!(profile_refresh_token("oai001_20x"), "ref_p");
    }

    #[test]
    fn login_replaces_an_unstamped_profile_because_the_credentials_are_new() {
        let _env = TestEnv::new();
        // A legacy profile with no last_refresh. The freshness gate would call
        // this unorderable and refuse — but re-login is exactly how a user
        // recovers such a profile, so it must not be blocked here.
        seed_profile(
            "legacy",
            &stamped_auth_json("legacy@example.com", "acct_l", "acc_old", "ref_old", None),
        );
        let minted = realistic_auth_json("legacy@example.com", "acct_l", "acc_fresh", "ref_fresh");

        match super::save_auth_value(minted, Some("legacy")) {
            Ok(super::SaveAction::Updated(alias)) => assert_eq!(alias, "legacy"),
            other => panic!("re-login must be able to replace a legacy profile, got {other:?}"),
        }
        assert_eq!(profile_refresh_token("legacy"), "ref_fresh");
    }
}
