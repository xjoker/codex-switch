use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::error::CsError;

const MAX_BACKUPS: usize = 3;

pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Upstream Codex version this release is contract-aligned with.
pub(crate) use crate::codex_compat::ALIGNED_CODEX_VERSION;

const CODEX_VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
static CODEX_CLI_VERSION: OnceLock<String> = OnceLock::new();

/// Use the same bounded local Codex version probe for request query parameters
/// and the HTTP User-Agent. A malformed, missing, or unresponsive CLI falls
/// back to the release's upstream contract version. This transport fallback
/// is not used by launch compatibility checks or the doctor report.
pub(crate) fn codex_cli_version() -> &'static str {
    CODEX_CLI_VERSION
        .get_or_init(detect_codex_cli_version)
        .as_str()
}

fn detect_codex_cli_version() -> String {
    let Some(path) = crate::launch::command_on_path("codex") else {
        return ALIGNED_CODEX_VERSION.to_string();
    };
    crate::codex_compat::probe_executable_with_timeout(&path, CODEX_VERSION_PROBE_TIMEOUT)
        .version
        .map(|version| format!("{}.{}.{}", version.major, version.minor, version.patch))
        .unwrap_or_else(|| ALIGNED_CODEX_VERSION.to_string())
}

/// User-Agent in the upstream shape: `codex_cli_rs/<version> (<os>; <arch>)`.
pub(crate) fn codex_user_agent() -> String {
    format!(
        "codex_cli_rs/{} ({}; {})",
        codex_cli_version(),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}
pub(crate) const ISSUER: &str = "https://auth.openai.com";
const DEFAULT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

pub(crate) fn token_url() -> String {
    std::env::var("CS_TOKEN_URL").unwrap_or_else(|_| DEFAULT_TOKEN_URL.to_string())
}

/// Serializes tests that redirect endpoint URLs (`CS_TOKEN_URL`, and the
/// warmup equivalents) at a mock server. Environment variables are
/// process-global, so a per-module lock only serializes that module and lets
/// tests in a sibling module retarget the variable mid-request; both modules
/// must take this one. Mirrors `profile::TEST_ENV_LOCK`, which does the same
/// for the `HOME` / `CODEX_HOME` group.
#[cfg(test)]
pub(crate) static URL_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// User Codex home (`$CODEX_HOME`, or `~/.codex`). Provider launches retain
/// this shared home and select a private native config profile.
pub(crate) fn user_codex_home() -> Result<PathBuf> {
    codex_home_from_values(std::env::var_os("CODEX_HOME"), dirs::home_dir())
}

/// ~/.codex/auth.json (or $CODEX_HOME/auth.json)
pub fn codex_auth_path() -> Result<PathBuf> {
    let codex_home = user_codex_home()?;
    validate_cli_auth_credentials_store(&codex_home)?;
    Ok(codex_home.join("auth.json"))
}

/// Resolve the filesystem location without consulting managed policy. Callers
/// may use this only after performing the operation-specific policy check.
pub(crate) fn codex_auth_path_unchecked() -> Result<PathBuf> {
    let codex_home = user_codex_home()?;
    Ok(codex_home.join("auth.json"))
}

pub(crate) fn ensure_file_credentials_store() -> Result<()> {
    ensure_chatgpt_backend_supported("use ChatGPT OAuth")
}

pub(crate) fn ensure_chatgpt_backend_supported(operation: &str) -> Result<()> {
    let codex_home = user_codex_home()?;
    crate::auth_policy::ensure_file_oauth_environment(operation)?;
    crate::auth_policy::load_auth_policy(&codex_home)?.validate_file_oauth(operation)
}

fn codex_home_from_values(
    configured_home: Option<OsString>,
    user_home: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(home) = configured_home.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(&home);
        if path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            anyhow::bail!(
                "CODEX_HOME contains '..' component which is not allowed: {}",
                path.display()
            );
        }
        return Ok(path);
    }

    let home = user_home.ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    Ok(home.join(".codex"))
}

fn validate_cli_auth_credentials_store(codex_home: &Path) -> Result<()> {
    crate::auth_policy::ensure_file_oauth_environment("access live ChatGPT credentials")?;
    crate::auth_policy::load_auth_policy(codex_home)?.validate_file_oauth("use ChatGPT OAuth")
}

#[cfg(test)]
fn validate_managed_auth_config(config: &toml::Value, account_id: Option<&str>) -> Result<()> {
    let text = toml::to_string(config)?;
    let policy = crate::auth_policy::resolve_from_texts(None, Some(&text), None, None, None)?;
    policy.validate_file_oauth("use ChatGPT OAuth")?;
    policy.validate_workspace(account_id)
}

/// Effective workspace ids permitted by Codex managed policy. Invalid or
/// unreadable policy is an error so the OAuth authorize page is never opened
/// with a broader workspace selection than the policy allows.
pub(crate) fn configured_forced_workspace_ids() -> Result<Vec<String>> {
    let codex_home = codex_home_from_values(std::env::var_os("CODEX_HOME"), dirs::home_dir())?;
    let policy = crate::auth_policy::load_auth_policy(&codex_home)?;
    Ok(policy.workspace_allowlist())
}

pub(crate) fn validate_managed_chatgpt_account(id_token: &str) -> Result<()> {
    let codex_home = codex_home_from_values(std::env::var_os("CODEX_HOME"), dirs::home_dir())?;
    let policy = crate::auth_policy::load_auth_policy(&codex_home)?;
    let auth = serde_json::json!({"tokens": {"id_token": id_token}});
    let account_id = crate::jwt::parse_account_info(&auth).account_id;
    policy.validate_workspace(account_id.as_deref())
}

/// Enforce the managed ChatGPT workspace policy for a complete auth value.
/// Keep this at credential-write boundaries: JWT claims are only a routing
/// hint until a caller has otherwise authenticated the credentials.
pub(crate) fn validate_managed_auth_value(auth: &serde_json::Value) -> Result<()> {
    let codex_home = codex_home_from_values(std::env::var_os("CODEX_HOME"), dirs::home_dir())?;
    let policy = crate::auth_policy::load_auth_policy(&codex_home)?;
    let account_id = crate::jwt::parse_account_info(auth).account_id;
    policy.validate_workspace(account_id.as_deref())
}

/// ~/.codex-switch/
pub fn app_home() -> Result<PathBuf> {
    // Keep application state relocatable without changing Codex's own home.
    if let Some(path) = std::env::var_os("CODEX_SWITCH_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }

    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
    Ok(home.join(".codex-switch"))
}

/// ~/.codex-switch/profiles/
pub fn profiles_dir() -> Result<PathBuf> {
    Ok(app_home()?.join("profiles"))
}

/// ~/.codex-switch/current
pub fn current_file() -> Result<PathBuf> {
    Ok(app_home()?.join("current"))
}

pub fn read_auth(path: &Path) -> Result<serde_json::Value> {
    if !path.exists() {
        return Err(CsError::NoAuthFile(path.display().to_string()).into());
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let val: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(val)
}

pub(crate) fn atomic_write_private(path: &Path, contents: &[u8]) -> Result<()> {
    #[cfg(windows)]
    {
        let shared_home = user_codex_home().ok();
        let owned_home = app_home().ok();
        atomic_write_private_inner(
            path,
            contents,
            shared_home.as_deref(),
            owned_home.as_deref(),
        )
    }
    #[cfg(not(windows))]
    atomic_write_private_inner(path, contents)
}

fn atomic_write_private_inner(
    path: &Path,
    contents: &[u8],
    #[cfg(windows)] shared_home: Option<&Path>,
    #[cfg(windows)] owned_home: Option<&Path>,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    #[cfg(windows)]
    let harden_parent =
        { should_harden_windows_parent(parent, shared_home, owned_home, parent.is_dir()) };
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating directory {}", parent.display()))?;
    #[cfg(windows)]
    if harden_parent {
        harden_windows_acl(parent, true)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", parent.display()))?;
    }

    #[cfg(windows)]
    let mut tmp = create_private_windows_temp(parent)
        .with_context(|| format!("creating protected temporary file in {}", parent.display()))?;
    #[cfg(not(windows))]
    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary file in {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", tmp.path().display()))?;
    }
    tmp.write_all(contents)
        .with_context(|| format!("writing temporary file for {}", path.display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("syncing temporary file for {}", path.display()))?;
    tmp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("atomically replacing {}", path.display()))?;
    #[cfg(windows)]
    harden_windows_acl(path, false)?;
    Ok(())
}

#[cfg(any(windows, test))]
fn windows_private_acl_sddl(current_user_sid: &str, directory: bool) -> String {
    let inheritance = if directory { "OICI" } else { "" };
    format!(
        "D:P(A;{inheritance};FA;;;{current_user_sid})\
         (A;{inheritance};FA;;;S-1-5-18)\
         (A;{inheritance};FA;;;S-1-5-32-544)"
    )
}

#[cfg(windows)]
fn should_harden_windows_parent(
    parent: &Path,
    shared_codex_home: Option<&Path>,
    owned_app_home: Option<&Path>,
    existed_before_write: bool,
) -> bool {
    if !existed_before_write || !parent.is_dir() {
        return true;
    }

    // Skip a shared Codex home only when all three existing paths resolve
    // successfully. Canonical paths avoid treating a sibling such as
    // `codex-switch-old` as a descendant of `codex-switch`; resolution errors
    // fail closed and retain directory hardening.
    let (Some(shared), Some(owned)) = (shared_codex_home, owned_app_home) else {
        return true;
    };
    let (Ok(parent_real), Ok(shared_real), Ok(owned_real)) = (
        parent.canonicalize(),
        shared.canonicalize(),
        owned.canonicalize(),
    ) else {
        return true;
    };

    // App-owned paths take precedence over the shared-home exception.
    if parent_real != shared_real {
        return true;
    }
    parent_real.starts_with(owned_real)
}

#[cfg(windows)]
fn harden_windows_acl(path: &Path, directory: bool) -> Result<()> {
    windows_acl_security_descriptor(path, directory, true).map(|_| ())
}

#[cfg(windows)]
fn windows_acl_security_descriptor(
    path: &Path,
    directory: bool,
    apply: bool,
) -> Result<*mut core::ffi::c_void> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// The process-token SID walk and the SDDL → security-descriptor
    /// conversion are identical on every call: cache both.  Only the
    /// `SetNamedSecurityInfoW` write must run per file, which is also the
    /// call a slow filesystem or antivirus makes expensive.
    struct AclParts {
        dir_sd: PSECURITY_DESCRIPTOR,
        file_sd: PSECURITY_DESCRIPTOR,
    }

    unsafe impl Send for AclParts {}
    unsafe impl Sync for AclParts {}

    static ACL_PARTS: std::sync::OnceLock<Result<AclParts, String>> = std::sync::OnceLock::new();

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: this wrapper is only constructed from a successful
            // OpenProcessToken call and owns that handle exactly once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    struct LocalAllocation(*mut core::ffi::c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            // SAFETY: both wrapped pointers come from Win32 APIs documented to
            // allocate with LocalAlloc and are released exactly once here.
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    fn last_error(path: &Path, api: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "{api} failed for {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )
    }

    let parts = ACL_PARTS.get_or_init(|| -> Result<AclParts, String> {
        (|| -> Result<AclParts> {
            let mut token = null_mut();
            // SAFETY: GetCurrentProcess returns a valid pseudo-handle, and
            // `token` points to writable storage for the owned token handle.
            if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
                return Err(last_error(path, "OpenProcessToken"));
            }
            let _token = OwnedHandle(token);

            let mut token_user_bytes = 0;
            // SAFETY: the null-buffer probe is the documented way to obtain
            // the TOKEN_USER size; no output buffer is dereferenced.
            let probe_ok = unsafe {
                GetTokenInformation(token, TokenUser, null_mut(), 0, &mut token_user_bytes)
            };
            let probe_error = std::io::Error::last_os_error();
            if probe_ok != 0
                || token_user_bytes == 0
                || probe_error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
            {
                return Err(anyhow::anyhow!(
                    "GetTokenInformation(TokenUser size) failed for {}: {probe_error}",
                    path.display()
                ));
            }

            let words = (token_user_bytes as usize).div_ceil(std::mem::size_of::<usize>());
            let mut token_user = vec![0usize; words];
            // SAFETY: the usize-backed buffer is suitably aligned for
            // TOKEN_USER and has the exact byte capacity from the size probe.
            if unsafe {
                GetTokenInformation(
                    token,
                    TokenUser,
                    token_user.as_mut_ptr().cast(),
                    token_user_bytes,
                    &mut token_user_bytes,
                )
            } == 0
            {
                return Err(last_error(path, "GetTokenInformation(TokenUser)"));
            }
            // SAFETY: GetTokenInformation initialized the aligned buffer as
            // TOKEN_USER, and the SID stays valid while `token_user` lives.
            let user_sid = unsafe { (*(token_user.as_ptr().cast::<TOKEN_USER>())).User.Sid };

            let mut string_sid = null_mut();
            // SAFETY: `user_sid` comes from the live TOKEN_USER buffer and the
            // API writes a LocalAlloc-owned, NUL-terminated UTF-16 pointer.
            if unsafe { ConvertSidToStringSidW(user_sid, &mut string_sid) } == 0 {
                return Err(last_error(path, "ConvertSidToStringSidW"));
            }
            let _string_sid = LocalAllocation(string_sid.cast());
            let mut sid_len = 0;
            // SAFETY: ConvertSidToStringSidW guarantees a NUL-terminated UTF-16
            // string, and `_string_sid` keeps that allocation alive.
            while unsafe { *string_sid.add(sid_len) } != 0 {
                sid_len += 1;
            }
            // SAFETY: `sid_len` was found within the API-provided allocation
            // and excludes the terminator.
            let current_user_sid =
                String::from_utf16(unsafe { std::slice::from_raw_parts(string_sid, sid_len) })
                    .with_context(|| {
                        format!(
                            "decoding ConvertSidToStringSidW output for {}",
                            path.display()
                        )
                    })?;

            let make_sd = |directory: bool| -> Result<PSECURITY_DESCRIPTOR> {
                let sddl = windows_private_acl_sddl(&current_user_sid, directory);
                let sddl_wide: Vec<u16> = std::ffi::OsStr::new(&sddl)
                    .encode_wide()
                    .chain(std::iter::once(0))
                    .collect();
                let mut sd: PSECURITY_DESCRIPTOR = null_mut();
                // SAFETY: `sddl_wide` is NUL-terminated and `sd` is writable;
                // the returned descriptor is intentionally leaked through
                // ACL_PARTS so every call reuses the same read-only memory.
                if unsafe {
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        sddl_wide.as_ptr(),
                        SDDL_REVISION_1,
                        &mut sd,
                        null_mut(),
                    )
                } == 0
                {
                    return Err(last_error(
                        path,
                        "ConvertStringSecurityDescriptorToSecurityDescriptorW",
                    ));
                }
                Ok(sd)
            };
            Ok(AclParts {
                dir_sd: make_sd(true)?,
                file_sd: make_sd(false)?,
            })
        })()
        .map_err(|error| format!("{error:#}"))
    });
    let security_descriptor = match parts {
        Ok(parts) => {
            if directory {
                parts.dir_sd
            } else {
                parts.file_sd
            }
        }
        Err(error) => return Err(anyhow::anyhow!("{error}")),
    };

    let mut dacl_present = 0;
    let mut dacl: *mut ACL = null_mut();
    let mut dacl_defaulted = 0;
    // SAFETY: `security_descriptor` is the cached, immutable descriptor; all
    // output pointers refer to initialized local variables.
    if unsafe {
        GetSecurityDescriptorDacl(
            security_descriptor,
            &mut dacl_present,
            &mut dacl,
            &mut dacl_defaulted,
        )
    } == 0
    {
        return Err(last_error(path, "GetSecurityDescriptorDacl"));
    }
    if dacl_present == 0 || dacl.is_null() {
        anyhow::bail!(
            "GetSecurityDescriptorDacl returned no DACL for {}",
            path.display()
        );
    }

    let path_wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // Writing a directory DACL makes Windows re-propagate inheritance through
    // the whole tree below it. `$CODEX_HOME` holds Codex sessions, worktrees,
    // and caches (tens of thousands of entries), where that took seconds on
    // every write. Skip the write when the exact protected DACL is already in
    // place; anything else, including an extra or missing ACE, is rewritten.
    if apply && windows_dacl_already_matches(&path_wide, dacl) {
        tracing::debug!(
            path = %path.display(),
            directory,
            "windows ACL already hardened"
        );
        return Ok(security_descriptor);
    }

    if !apply {
        return Ok(security_descriptor);
    }

    let acl_write_start = std::time::Instant::now();
    // SAFETY: the path is NUL-terminated, `dacl` points inside the live
    // security descriptor, and null owner/group/SACL pointers are required
    // because only the exact protected DACL is being replaced.
    let status = unsafe {
        SetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null(),
        )
    };
    let acl_ms = acl_write_start.elapsed().as_millis() as u64;
    if acl_ms >= 500 {
        tracing::warn!(
            path = %path.display(),
            directory,
            acl_ms,
            "windows ACL write is unusually slow; check OneDrive/AV on the profile directory"
        );
    }
    tracing::debug!(
        path = %path.display(),
        directory,
        acl_ms,
        "hardened windows ACL"
    );
    if status != ERROR_SUCCESS {
        return Err(anyhow::anyhow!(
            "SetNamedSecurityInfoW failed for {}: {}",
            path.display(),
            std::io::Error::from_raw_os_error(status as i32)
        ));
    }

    Ok(security_descriptor)
}

#[cfg(windows)]
fn create_private_windows_temp(parent: &Path) -> Result<tempfile::NamedTempFile<std::fs::File>> {
    use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
    };

    tempfile::Builder::new()
        .prefix(".codex-switch-auth-")
        .make_in(parent, |candidate| {
            let security_descriptor = windows_acl_security_descriptor(candidate, false, false)
                .map_err(|error| {
                    std::io::Error::other(format!("preparing protected ACL: {error:#}"))
                })?;
            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: security_descriptor,
                bInheritHandle: 0,
            };
            let candidate_wide: Vec<u16> = candidate
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            // SECURITY_ATTRIBUTES applies the exact protected DACL when the file is created.
            // Its descriptor points to the process-lifetime cached SD; the
            // attributes and NUL-terminated path remain live for this call.
            let handle = unsafe {
                CreateFileW(
                    candidate_wide.as_ptr(),
                    windows_sys::Win32::Foundation::GENERIC_READ
                        | windows_sys::Win32::Foundation::GENERIC_WRITE,
                    FILE_SHARE_DELETE | FILE_SHARE_READ,
                    &attributes,
                    CREATE_NEW,
                    FILE_ATTRIBUTE_NORMAL,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: CreateFileW returned a uniquely owned file handle which
            // is transferred to File and closed exactly once by its Drop.
            Ok(unsafe { std::fs::File::from_raw_handle(handle.cast()) })
        })
        .map_err(anyhow::Error::from)
}

/// Whether the object at `path_wide` already carries a protected DACL whose
/// ACEs are byte-identical, in order, to `desired`. Any read failure answers
/// `false`, so the caller falls back to writing the DACL.
#[cfg(windows)]
fn windows_dacl_already_matches(
    path_wide: &[u16],
    desired: *const windows_sys::Win32::Security::ACL,
) -> bool {
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorControl,
        PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    };

    /// Raw bytes of every ACE in `acl`, or `None` when one cannot be read.
    fn ace_bytes(acl: *const ACL) -> Option<Vec<Vec<u8>>> {
        // SAFETY: `acl` is a live ACL; its header is read-only here.
        let count = unsafe { (*acl).AceCount } as u32;
        let mut aces = Vec::with_capacity(count as usize);
        for index in 0..count {
            let mut ace = null_mut();
            // SAFETY: `index` is below AceCount and `ace` is writable storage.
            if unsafe { GetAce(acl, index, &mut ace) } == 0 || ace.is_null() {
                return None;
            }
            // SAFETY: GetAce returned a pointer to an ACE inside `acl`, which
            // starts with an ACE_HEADER whose AceSize covers the whole ACE.
            let size = unsafe { (*ace.cast::<ACE_HEADER>()).AceSize } as usize;
            aces.push(unsafe { std::slice::from_raw_parts(ace.cast::<u8>(), size) }.to_vec());
        }
        Some(aces)
    }

    let mut current: *mut ACL = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: `path_wide` is NUL-terminated; only the DACL is requested and
    // the returned descriptor is freed below.
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut current,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return false;
    }
    let matches = (|| {
        if current.is_null() {
            return false;
        }
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: `descriptor` is the live descriptor returned above.
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
            || control & SE_DACL_PROTECTED == 0
        {
            return false;
        }
        match (ace_bytes(current), ace_bytes(desired)) {
            (Some(current), Some(desired)) => current == desired,
            _ => false,
        }
    })();
    // SAFETY: GetNamedSecurityInfoW allocated `descriptor` with LocalAlloc;
    // `current` points into it and is not used after this point.
    unsafe {
        LocalFree(descriptor);
    }
    matches
}

#[cfg(windows)]
pub(crate) fn harden_windows_private_directory(path: &Path) -> Result<()> {
    harden_windows_acl(path, true)
}

#[cfg(windows)]
pub(crate) fn harden_windows_private_file(path: &Path) -> Result<()> {
    harden_windows_acl(path, false)
}

pub fn write_auth(path: &Path, val: &serde_json::Value) -> Result<()> {
    let raw = serde_json::to_string_pretty(val)?;
    atomic_write_private(path, raw.as_bytes())
}

pub fn sha256_file(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let digest = Sha256::digest(&data);
    Some(hex::encode(digest))
}

pub fn backup_auth(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let contents =
        std::fs::read(path).with_context(|| format!("reading backup source {}", path.display()))?;
    let bak = allocate_backup_path(path)?;
    atomic_write_private(&bak, &contents)
        .with_context(|| format!("backing up {} -> {}", path.display(), bak.display()))?;
    cleanup_old_backups(path);
    Ok(())
}

/// A backup path no earlier backup already occupies.
///
/// Nanoseconds rather than seconds: two switches inside one second are ordinary
/// (`use` followed by `launch`, or any script), and a second-resolution name
/// made the later backup overwrite the earlier one — quietly retaining fewer
/// real recovery points than `MAX_BACKUPS` promises.
///
/// The wider stamp still sorts correctly in `cleanup_old_backups` against
/// legacy seconds names, because the leading ten digits of a nanosecond stamp
/// are that same second, so the shorter name compares as the earlier one.
fn allocate_backup_path(path: &Path) -> Result<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    for collision in 0..1000u16 {
        let candidate = if collision == 0 {
            path.with_extension(format!("json.bak.{nanos}"))
        } else {
            path.with_extension(format!("json.bak.{nanos}-{collision}"))
        };
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "could not allocate a unique backup path for {}",
        path.display()
    )
}

pub fn update_tokens(
    path: &Path,
    id_token: &str,
    access_token: &str,
    refresh_token: &str,
) -> Result<()> {
    let mut val = read_auth(path)?;
    apply_tokens(&mut val, id_token, access_token, refresh_token)
        .with_context(|| format!("updating tokens in {}", path.display()))?;
    validate_managed_auth_value(&val)?;
    write_auth(path, &val)
}

pub fn apply_tokens(
    val: &mut serde_json::Value,
    id_token: &str,
    access_token: &str,
    refresh_token: &str,
) -> Result<()> {
    let tokens = val
        .get_mut("tokens")
        .and_then(|t| t.as_object_mut())
        .ok_or_else(|| anyhow::anyhow!("auth.json missing tokens object"))?;

    tokens.insert("id_token".into(), serde_json::json!(id_token));
    tokens.insert("access_token".into(), serde_json::json!(access_token));
    tokens.insert("refresh_token".into(), serde_json::json!(refresh_token));
    // Codex refreshes proactively when last_refresh is older than 8 days;
    // stamping it here keeps our refreshes recognized (matches upstream).
    if let Some(obj) = val.as_object_mut() {
        obj.insert(
            "last_refresh".into(),
            serde_json::json!(crate::output::format_iso8601(now_unix_secs())),
        );
    }
    Ok(())
}

/// Extract (access_token, refresh_token) from an auth.json Value.
pub fn extract_tokens(val: &serde_json::Value) -> (Option<String>, Option<String>) {
    let at = val
        .pointer("/tokens/access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let rt = val
        .pointer("/tokens/refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    (at, rt)
}

pub fn extract_id_token(val: &serde_json::Value) -> Option<String> {
    val.pointer("/tokens/id_token")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Current unix timestamp in seconds.
pub fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read auth.json and parse AccountInfo in one step (returns default on error).
pub fn read_account_info(path: &Path) -> crate::jwt::AccountInfo {
    read_auth(path)
        .map(|v| {
            let mut info = crate::jwt::parse_account_info(&v);
            crate::cache::apply_workspace_name(&mut info);
            info
        })
        .unwrap_or_default()
}

pub fn validate_auth_value(val: &serde_json::Value) -> Result<crate::jwt::AccountInfo> {
    let tokens = val
        .get("tokens")
        .and_then(|t| t.as_object())
        .ok_or_else(|| anyhow::anyhow!("auth.json missing tokens object"))?;

    let id_token = tokens
        .get("id_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("tokens.id_token is required"))?;

    let has_access = tokens
        .get("access_token")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());
    let has_refresh = tokens
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty());

    if !has_access && !has_refresh {
        return Err(anyhow::anyhow!(
            "tokens.access_token or tokens.refresh_token is required"
        ));
    }

    let payload = id_token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("tokens.id_token is not a valid JWT"))?;
    let decoded = {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| anyhow::anyhow!("tokens.id_token payload is not valid base64url"))?
    };
    let _: serde_json::Value = serde_json::from_slice(&decoded)
        .map_err(|_| anyhow::anyhow!("tokens.id_token payload is not valid JSON"))?;

    let info = crate::jwt::parse_account_info(val);
    if info.account_id.as_deref().is_none_or(str::is_empty) {
        return Err(anyhow::anyhow!(
            "id_token does not contain a usable account_id"
        ));
    }

    Ok(info)
}

/// Build a shared reqwest client with standard user-agent and proxy support.
pub fn build_http_client() -> Result<reqwest::Client> {
    let proxy_url = crate::config::resolve_proxy();
    build_http_client_with_proxy(proxy_url.as_deref())
}

pub fn build_http_client_with_proxy(proxy_url: Option<&str>) -> Result<reqwest::Client> {
    build_http_client_with_proxy_and_redirect_policy(
        proxy_url,
        reqwest::redirect::Policy::default(),
    )
}

/// Build a shared client with the normal proxy and custom CA behavior while
/// allowing credential-bearing callers to choose their redirect policy.
pub(crate) fn build_http_client_with_redirect_policy(
    redirect_policy: reqwest::redirect::Policy,
) -> Result<reqwest::Client> {
    let proxy_url = crate::config::resolve_proxy();
    build_http_client_with_proxy_and_redirect_policy(proxy_url.as_deref(), redirect_policy)
}

pub(crate) fn build_http_client_with_proxy_and_redirect_policy(
    proxy_url: Option<&str>,
    redirect_policy: reqwest::redirect::Policy,
) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(codex_user_agent())
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(60))
        .redirect(redirect_policy);

    if let Some(url) = proxy_url {
        let sanitized_url = sanitize_proxy_url(url);
        tracing::debug!("Using proxy: {sanitized_url}");
        let mut proxy = reqwest::Proxy::all(url)
            .map_err(|e| anyhow::anyhow!("invalid proxy URL '{sanitized_url}': {e}"))?;
        if let Some(no_proxy) = crate::config::resolve_no_proxy() {
            tracing::debug!("No-proxy list: {no_proxy}");
            proxy = proxy.no_proxy(reqwest::NoProxy::from_string(&no_proxy));
        }
        builder = builder.proxy(proxy);
    }

    if let Some(path) = custom_ca_path_from_values(
        std::env::var_os("CODEX_CA_CERTIFICATE"),
        std::env::var_os("SSL_CERT_FILE"),
    ) {
        let pem = std::fs::read(&path)
            .with_context(|| format!("reading custom CA bundle {}", path.display()))?;
        let certificates = reqwest::Certificate::from_pem_bundle(&pem)
            .with_context(|| format!("parsing custom CA bundle {}", path.display()))?;
        if certificates.is_empty() {
            anyhow::bail!(
                "custom CA bundle {} contains no certificates",
                path.display()
            );
        }
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }

    Ok(builder.build()?)
}

fn custom_ca_path_from_values(
    codex_ca: Option<OsString>,
    ssl_cert_file: Option<OsString>,
) -> Option<PathBuf> {
    codex_ca
        .filter(|value| !value.is_empty())
        .or_else(|| ssl_cert_file.filter(|value| !value.is_empty()))
        .map(PathBuf::from)
}

fn sanitize_proxy_url(url: &str) -> String {
    let Some(scheme_sep) = url.find("://") else {
        return url.to_string();
    };
    let authority_start = scheme_sep + 3;
    let authority_end = url[authority_start..]
        .find(['/', '?', '#'])
        .map(|idx| authority_start + idx)
        .unwrap_or(url.len());
    let authority = &url[authority_start..authority_end];
    let Some(userinfo_end) = authority.rfind('@') else {
        return url.to_string();
    };
    let at_pos = authority_start + userinfo_end;

    let mut sanitized = String::with_capacity(url.len());
    sanitized.push_str(&url[..authority_start]);
    sanitized.push_str("***:***");
    sanitized.push_str(&url[at_pos..]);
    sanitized
}

/// An intercepting proxy re-signs traffic with its own CA, and rustls reports
/// that as a bare "UnknownIssuer" with no indication of what to do. The OS trust
/// store is consulted first, so reaching here means the CA is not installed
/// there either and has to be supplied explicitly.
fn tls_trust_hint(message: &str) -> Option<&'static str> {
    if message.contains("UnknownIssuer") || message.contains("invalid peer certificate") {
        return Some(
            "\n  hint: the server's certificate was not signed by a CA this machine trusts. \
             An intercepting proxy (Proxyman, Charles, a corporate MITM) re-signs traffic with \
             its own CA — add that CA to the system trust store, or export it as PEM and point \
             CODEX_CA_CERTIFICATE at the file.",
        );
    }
    None
}

/// Format a reqwest error with the full source chain for diagnostics.
pub fn format_reqwest_error(context: &str, err: &reqwest::Error) -> anyhow::Error {
    let mut msg = format!("{context}: {err}");
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        msg.push_str(&format!("\n  caused by: {cause}"));
        source = std::error::Error::source(cause);
    }
    if let Some(hint) = tls_trust_hint(&msg) {
        msg.push_str(hint);
    }
    anyhow::anyhow!("{msg}")
}

/// Format an authentication-request failure without endpoint details or the
/// reqwest source chain. URLs can contain userinfo and query credentials, and
/// intermediaries may include request details in lower-level error messages.
pub(crate) fn format_auth_reqwest_error(context: &str, err: reqwest::Error) -> anyhow::Error {
    let mut msg = format!("{context}: {}", err.without_url());
    if let Some(hint) = tls_trust_hint(&msg) {
        msg.push_str(hint);
    }
    anyhow::anyhow!("{msg}")
}

fn cleanup_old_backups(path: &Path) {
    let parent = match path.parent() {
        Some(p) => p,
        None => return,
    };
    let stem = match path.file_name().and_then(|f| f.to_str()) {
        Some(s) => s,
        None => return,
    };
    let prefix = format!("{stem}.bak.");

    let mut backups: Vec<PathBuf> = std::fs::read_dir(parent)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|name| name.starts_with(&prefix))
                .unwrap_or(false)
        })
        .map(|e| e.path())
        .collect();

    if backups.len() <= MAX_BACKUPS {
        return;
    }

    backups.sort();
    let to_remove = backups.len() - MAX_BACKUPS;
    for old in &backups[..to_remove] {
        let _ = std::fs::remove_file(old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_agent_uses_the_same_aligned_version_as_model_requests() {
        let version = codex_cli_version();
        assert!(codex_user_agent().starts_with(&format!("codex_cli_rs/{version} ")));
        assert_eq!(ALIGNED_CODEX_VERSION, "0.159.2");
    }

    #[test]
    fn codex_version_probe_has_a_hard_deadline() {
        #[cfg(windows)]
        let command = {
            let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
            let path = PathBuf::from(root)
                .join("System32")
                .join(r"WindowsPowerShell\v1.0\powershell.exe");
            let mut command = Command::new(path);
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 5",
            ]);
            command
        };
        #[cfg(not(windows))]
        let command = {
            let mut command = Command::new("/bin/sleep");
            command.arg("5");
            command
        };
        let start = std::time::Instant::now();
        let error = crate::app_server::output_with_timeout(command, Duration::from_millis(100))
            .expect_err("a hanging version command must time out");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn auth_request_errors_do_not_expose_url_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let url = format!(
            "http://user-secret:password-secret@{address}/oauth/token?refresh_secret=query-secret"
        );
        let error = reqwest::Client::new()
            .get(url)
            .send()
            .await
            .expect_err("the local listener was closed before the request");

        let formatted =
            format_auth_reqwest_error("token refresh request failed", error).to_string();
        for secret in [
            "user-secret",
            "password-secret",
            "refresh_secret",
            "query-secret",
        ] {
            assert!(
                !formatted.contains(secret),
                "authentication diagnostics exposed {secret:?}: {formatted}"
            );
        }
        assert!(formatted.contains("token refresh request failed"));
    }

    fn assert_recent_rfc3339(value: &serde_json::Value) {
        let text = value.as_str().expect("last_refresh should be a string");
        let parsed = chrono::DateTime::parse_from_rfc3339(text).expect("RFC3339 last_refresh");
        let age = chrono::Utc::now().signed_duration_since(parsed);
        assert!(
            age.num_seconds().abs() < 60,
            "last_refresh not recent: {text}"
        );
    }

    #[test]
    fn test_apply_tokens_updates_last_refresh() {
        let mut val = json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "old-id",
                "access_token": "old-access",
                "refresh_token": "old-refresh",
                "account_id": "acct"
            },
            "last_refresh": "2020-01-01T00:00:00Z"
        });

        apply_tokens(&mut val, "new-id", "new-access", "new-refresh").unwrap();

        assert_eq!(val["tokens"]["access_token"], "new-access");
        assert_recent_rfc3339(&val["last_refresh"]);
    }

    #[test]
    fn test_update_tokens_updates_last_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth(
            &path,
            &json!({
                "tokens": { "id_token": "a", "access_token": "b", "refresh_token": "c" },
                "last_refresh": "2020-01-01T00:00:00Z"
            }),
        )
        .unwrap();

        update_tokens(&path, "new-id", "new-access", "new-refresh").unwrap();

        let val = read_auth(&path).unwrap();
        assert_eq!(val["tokens"]["refresh_token"], "new-refresh");
        assert_recent_rfc3339(&val["last_refresh"]);
    }

    #[test]
    fn test_user_agent_matches_upstream_shape() {
        let ua = codex_user_agent();
        let version = codex_cli_version();
        assert!(
            ua.starts_with(&format!("codex_cli_rs/{version} (")),
            "unexpected UA: {ua}"
        );
        assert!(ua.ends_with(')'));
    }

    #[test]
    fn test_sanitize_proxy_url_masks_userinfo() {
        let url = "http://user:pass@example.com:8080/path?q=1";

        assert_eq!(
            sanitize_proxy_url(url),
            "http://***:***@example.com:8080/path?q=1"
        );
    }

    #[test]
    fn test_sanitize_proxy_url_keeps_url_without_userinfo() {
        let url = "socks5://example.com:1080";

        assert_eq!(sanitize_proxy_url(url), url);
    }

    #[cfg(unix)]
    #[test]
    fn test_write_auth_sets_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        write_auth(&path, &json!({ "tokens": {} })).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    fn backup_names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .filter(|name| name.starts_with("auth.json.bak."))
            .collect();
        names.sort();
        names
    }

    /// Two switches inside one second are ordinary — `use` then `launch`, or
    /// any script. A second-resolution backup name made the later one overwrite
    /// the earlier, so the pre-switch credentials the user expected to be able
    /// to recover were gone and `MAX_BACKUPS` retained fewer real recovery
    /// points than it claims.
    #[test]
    fn two_backups_within_the_same_second_are_both_retained() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        write_auth(&path, &json!({ "tokens": { "refresh_token": "first" } })).unwrap();
        backup_auth(&path).unwrap();
        write_auth(&path, &json!({ "tokens": { "refresh_token": "second" } })).unwrap();
        backup_auth(&path).unwrap();

        let names = backup_names(dir.path());
        assert_eq!(
            names.len(),
            2,
            "the first backup must survive a second one taken in the same second: {names:?}"
        );
    }

    /// `cleanup_old_backups` orders by file name, and this release changes the
    /// timestamp from seconds to nanoseconds — so both widths can sit in one
    /// directory. Lexicographic order stays equal to age order here because a
    /// 10-digit seconds value is compared against the leading 10 digits of the
    /// 19-digit nanosecond value, which are that same second. This test pins
    /// that reasoning so a future format change cannot break it silently.
    #[test]
    fn cleanup_keeps_the_newest_backups_across_both_timestamp_widths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth(&path, &json!({ "tokens": {} })).unwrap();

        // Oldest first: a legacy seconds name, then three nanosecond names.
        for suffix in [
            "1785000000",
            "1785000001000000000",
            "1785000002000000000",
            "1785000003000000000",
        ] {
            std::fs::write(dir.path().join(format!("auth.json.bak.{suffix}")), b"x").unwrap();
        }

        cleanup_old_backups(&path);

        assert_eq!(
            backup_names(dir.path()),
            vec![
                "auth.json.bak.1785000001000000000",
                "auth.json.bak.1785000002000000000",
                "auth.json.bak.1785000003000000000",
            ],
            "the legacy seconds backup is the oldest and must be the one dropped"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_backup_auth_sets_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        write_auth(&path, &json!({ "tokens": {} })).unwrap();
        backup_auth(&path).unwrap();

        let backup = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .find(|candidate| candidate != &path)
            .expect("backup file should exist");

        let mode = std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn test_explicit_non_file_credentials_stores_are_rejected() {
        for mode in ["keyring", "auto", "ephemeral"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("config.toml"),
                format!("cli_auth_credentials_store = \"{mode}\"\n"),
            )
            .unwrap();

            let err = validate_cli_auth_credentials_store(dir.path()).unwrap_err();

            assert!(
                err.to_string()
                    .contains("cli_auth_credentials_store = \"file\"")
            );
        }
    }

    #[test]
    fn test_missing_credentials_store_defaults_to_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "model = \"gpt-5\"\n").unwrap();

        validate_cli_auth_credentials_store(dir.path()).unwrap();
    }

    #[test]
    fn test_explicit_file_credentials_store_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "cli_auth_credentials_store = \"file\"\n",
        )
        .unwrap();

        validate_cli_auth_credentials_store(dir.path()).unwrap();
    }

    #[test]
    fn test_empty_codex_home_falls_back_to_default_home() {
        let user_home = PathBuf::from("/test-user-home");

        let codex_home =
            codex_home_from_values(Some(std::ffi::OsString::from("")), Some(user_home.clone()))
                .unwrap();

        assert_eq!(codex_home, user_home.join(".codex"));
    }

    #[test]
    fn test_managed_auth_rejects_api_only_policy() {
        let config: toml::Value = toml::from_str("forced_login_method = \"api\"\n").unwrap();

        let err = validate_managed_auth_config(&config, Some("workspace-a")).unwrap_err();

        assert!(err.to_string().contains("requires API key login"));
    }

    #[test]
    fn test_managed_auth_enforces_workspace_list() {
        let config: toml::Value = toml::from_str(
            "forced_login_method = \"chatgpt\"\nforced_chatgpt_workspace_id = [\"workspace-a\", \"workspace-b\"]\n",
        )
        .unwrap();

        validate_managed_auth_config(&config, Some("workspace-b")).unwrap();
        let err = validate_managed_auth_config(&config, Some("workspace-c")).unwrap_err();

        assert!(err.to_string().contains("workspace-c"));
    }

    #[test]
    fn windows_acl_sddl_replaces_the_dacl_instead_of_only_removing_inheritance() {
        let sddl = windows_private_acl_sddl("S-1-5-21-1-2-3-1001", true);
        assert!(sddl.starts_with("D:P"));
        assert_eq!(sddl.matches("(A;").count(), 3);
        assert!(
            !sddl.contains("S-1-1-0"),
            "the exact DACL path must not preserve unknown explicit ACEs"
        );
    }

    #[test]
    fn test_custom_ca_prefers_codex_ca_and_ignores_empty_values() {
        let selected = custom_ca_path_from_values(
            Some(OsString::from("/certs/codex.pem")),
            Some(OsString::from("/certs/ssl.pem")),
        );
        assert_eq!(selected, Some(PathBuf::from("/certs/codex.pem")));

        let fallback = custom_ca_path_from_values(
            Some(OsString::from("")),
            Some(OsString::from("/certs/ssl.pem")),
        );
        assert_eq!(fallback, Some(PathBuf::from("/certs/ssl.pem")));
    }

    #[test]
    fn unknown_issuer_error_explains_how_to_trust_an_intercepting_proxy() {
        let msg = "Usage API request failed: error sending request\n  caused by: invalid peer certificate: UnknownIssuer";
        let hint = super::tls_trust_hint(msg).expect("UnknownIssuer must carry a hint");
        assert!(
            hint.contains("CODEX_CA_CERTIFICATE"),
            "the hint must name the variable that fixes it: {hint}"
        );
    }

    #[test]
    fn an_ordinary_connection_failure_gets_no_certificate_hint() {
        let msg = "Usage API request failed: error sending request\n  caused by: tcp connect error: Connection refused (os error 61)";
        assert!(
            super::tls_trust_hint(msg).is_none(),
            "a hint about certificates would misdirect a plain connection failure"
        );
    }

    #[test]
    fn windows_private_acl_sddl_is_exact_and_language_neutral() {
        let current_user = "S-1-5-21-1-2-3-1001";
        assert_eq!(
            super::windows_private_acl_sddl(current_user, false),
            "D:P(A;;FA;;;S-1-5-21-1-2-3-1001)\
             (A;;FA;;;S-1-5-18)\
             (A;;FA;;;S-1-5-32-544)"
        );
        assert_eq!(
            super::windows_private_acl_sddl(current_user, true),
            "D:P(A;OICI;FA;;;S-1-5-21-1-2-3-1001)\
             (A;OICI;FA;;;S-1-5-18)\
             (A;OICI;FA;;;S-1-5-32-544)"
        );
    }

    #[cfg(windows)]
    #[test]
    fn shared_codex_parent_acl_is_left_alone_but_owned_parent_is_hardened() {
        let root = tempfile::tempdir().unwrap();
        let shared = root.path().join(".codex");
        let owned = root.path().join("codex-switch");
        let sibling = root.path().join("codex-switch-old");
        std::fs::create_dir(&shared).unwrap();
        std::fs::create_dir(&sibling).unwrap();
        std::fs::create_dir_all(owned.join("profiles").join("one")).unwrap();

        assert!(!super::should_harden_windows_parent(
            &shared,
            Some(&shared),
            Some(&owned),
            true
        ));
        assert!(super::should_harden_windows_parent(
            &owned.join("profiles").join("one"),
            Some(&owned.join("profiles").join("one")),
            Some(&owned),
            true
        ));
        let owned_alias = owned
            .join("profiles")
            .join("..")
            .join("profiles")
            .join("one");
        assert!(super::should_harden_windows_parent(
            &owned_alias,
            Some(&owned_alias),
            Some(&owned),
            true
        ));
        let sibling_alias = owned.join("..").join("codex-switch-old");
        assert!(!super::should_harden_windows_parent(
            &sibling_alias,
            Some(&sibling_alias),
            Some(&owned),
            true
        ));
        assert!(super::should_harden_windows_parent(
            &shared,
            Some(&shared),
            Some(&shared),
            true
        ));
        assert!(super::should_harden_windows_parent(
            &root.path().join("missing"),
            Some(&root.path().join("missing")),
            Some(&owned),
            false
        ));
        assert!(super::should_harden_windows_parent(
            &shared,
            Some(&shared),
            Some(&root.path().join("missing-owned")),
            true
        ));
    }

    #[cfg(windows)]
    #[test]
    fn protected_temp_is_private_before_first_write_and_failed_create_writes_nothing() {
        use std::os::windows::ffi::OsStrExt;

        fn acl_bytes(path: &Path) -> Vec<Vec<u8>> {
            use std::os::windows::ffi::OsStrExt;

            use windows_sys::Win32::Foundation::LocalFree;
            use windows_sys::Win32::Security::Authorization::{
                GetNamedSecurityInfoW, SE_FILE_OBJECT,
            };
            use windows_sys::Win32::Security::{
                ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce,
            };

            let wide = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut descriptor = std::ptr::null_mut();
            assert_eq!(
                unsafe {
                    GetNamedSecurityInfoW(
                        wide.as_ptr(),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        &mut dacl,
                        std::ptr::null_mut(),
                        &mut descriptor,
                    )
                },
                0
            );
            let mut result = Vec::new();
            for index in 0..unsafe { (*dacl).AceCount } as u32 {
                let mut ace = std::ptr::null_mut();
                assert_ne!(unsafe { GetAce(dacl, index, &mut ace) }, 0);
                let size = unsafe { (*ace.cast::<ACE_HEADER>()).AceSize } as usize;
                result.push(unsafe { std::slice::from_raw_parts(ace.cast::<u8>(), size) }.to_vec());
            }
            unsafe { LocalFree(descriptor) };
            result
        }

        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(windows_system_tool("icacls.exe"))
            .arg(dir.path())
            .args(["/grant", "*S-1-1-0:(OI)(CI)RX"])
            .status()
            .unwrap();
        assert!(status.success(), "failed to seed an extra parent ACE");
        let parent_acl_before = acl_bytes(dir.path());
        let owned_home = dir.path().join("codex-switch");
        std::fs::create_dir(&owned_home).unwrap();
        let temp = super::create_private_windows_temp(dir.path()).unwrap();
        assert_eq!(
            acl_bytes(dir.path()),
            parent_acl_before,
            "temp creation must not rewrite the parent DACL"
        );
        let descriptor = super::windows_acl_security_descriptor(temp.path(), false, false).unwrap();
        let mut dacl = std::ptr::null_mut();
        let mut present = 0;
        let mut defaulted = 0;
        assert_ne!(
            unsafe {
                windows_sys::Win32::Security::GetSecurityDescriptorDacl(
                    descriptor,
                    &mut present,
                    &mut dacl,
                    &mut defaulted,
                )
            },
            0
        );
        let wide = temp
            .path()
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        assert!(super::windows_dacl_already_matches(&wide, dacl));
        assert_eq!(
            temp.as_file().metadata().unwrap().len(),
            0,
            "temp must be private before content is written"
        );

        let shared_path = dir.path().join("auth.json");
        super::atomic_write_private_inner(
            &shared_path,
            b"first-secret",
            Some(dir.path()),
            Some(&owned_home),
        )
        .unwrap();
        assert_eq!(
            acl_bytes(dir.path()),
            parent_acl_before,
            "writing shared auth must not rewrite the shared parent DACL"
        );
        let shared_wide = shared_path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        assert!(super::windows_dacl_already_matches(&shared_wide, dacl));
        super::atomic_write_private_inner(
            &shared_path,
            b"replacement-secret",
            Some(dir.path()),
            Some(&owned_home),
        )
        .unwrap();
        assert_eq!(std::fs::read(&shared_path).unwrap(), b"replacement-secret");

        let missing_parent = dir.path().join("does-not-exist");
        let failure = super::create_private_windows_temp(&missing_parent);
        assert!(failure.is_err());
        assert!(std::fs::read_dir(&missing_parent).is_err());
    }

    /// Absolute path of a Windows system tool. Other tests swap `PATH` for a
    /// fake `codex` directory while these run, so a bare name can vanish.
    #[cfg(windows)]
    fn windows_system_tool(relative: &str) -> PathBuf {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        PathBuf::from(root).join("System32").join(relative)
    }

    #[cfg(windows)]
    #[test]
    fn hardened_windows_dacl_is_recognized_until_an_ace_is_added() {
        use std::os::windows::ffi::OsStrExt;

        let wide = |path: &Path| -> Vec<u16> {
            path.as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        };
        let desired = |path: &Path, directory: bool| {
            // Harden once, then read back the DACL the helper compares with.
            super::harden_windows_acl(path, directory).unwrap();
            let mut dacl = std::ptr::null_mut();
            let mut descriptor = std::ptr::null_mut();
            let status = unsafe {
                windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW(
                    wide(path).as_ptr(),
                    windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT,
                    windows_sys::Win32::Security::DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut descriptor,
                )
            };
            assert_eq!(status, 0);
            (dacl, descriptor)
        };

        let dir = tempfile::tempdir().unwrap();
        let (dacl, descriptor) = desired(dir.path(), true);
        assert!(
            super::windows_dacl_already_matches(&wide(dir.path()), dacl),
            "a directory hardened a moment ago must not be rewritten"
        );

        let status = std::process::Command::new(windows_system_tool("icacls.exe"))
            .arg(dir.path())
            .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
            .status()
            .unwrap();
        assert!(status.success(), "failed to seed an Everyone ACE");
        assert!(
            !super::windows_dacl_already_matches(&wide(dir.path()), dacl),
            "an extra ACE must force the DACL to be rewritten"
        );
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(descriptor);
        }
    }

    #[cfg(windows)]
    #[test]
    fn atomic_private_write_removes_unknown_explicit_windows_aces() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(windows_system_tool("icacls.exe"))
            .arg(dir.path())
            .args(["/grant", "*S-1-1-0:(OI)(CI)F"])
            .status()
            .unwrap();
        assert!(status.success(), "failed to seed an Everyone ACE");

        let path = dir.path().join("auth.json");
        super::atomic_write_private(&path, br#"{"refresh_token":"secret"}"#).unwrap();
        super::atomic_write_private(&path, br#"{"refresh_token":"replacement"}"#).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            br#"{"refresh_token":"replacement"}"#,
            "a protected temp handle must allow atomic replacement"
        );

        let inspect = r#"
$ErrorActionPreference = 'Stop'
foreach ($item in @($env:CS_ACL_DIR, $env:CS_ACL_FILE)) {
    $acl = if (Test-Path -LiteralPath $item -PathType Container) {
        [IO.Directory]::GetAccessControl($item)
    } else {
        [IO.File]::GetAccessControl($item)
    }
    Write-Output ('protected=' + $acl.AreAccessRulesProtected)
    foreach ($rule in $acl.Access) {
        Write-Output $rule.IdentityReference.Translate(
            [Security.Principal.SecurityIdentifier]
        ).Value
    }
}
"#;
        let output = std::process::Command::new(windows_system_tool(
            r"WindowsPowerShell\v1.0\powershell.exe",
        ))
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            inspect,
        ])
        .env("CS_ACL_DIR", dir.path())
        .env("CS_ACL_FILE", &path)
        .output()
        .unwrap();
        assert!(
            output.status.success(),
            "ACL inspection failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let acl = String::from_utf8(output.stdout).unwrap();
        assert_eq!(acl.matches("protected=True").count(), 2);
        assert!(
            !acl.lines().any(|line| line.trim() == "S-1-1-0"),
            "Everyone ACE survived exact DACL replacement:\n{acl}"
        );
    }
}
