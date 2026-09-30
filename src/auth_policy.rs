//! Read-only evaluation of Codex authentication requirements relevant to the
//! file-backed ChatGPT credentials managed by codex-switch.

use anyhow::{Context, Result};
use base64::Engine as _;
use std::io::Read;
use std::path::{Path, PathBuf};

pub(crate) const FEDERATED_IDENTITY_ENV: &[&str] =
    &["OPENAI_FEDERATION_RULE_ID", "OPENAI_IDENTITY_TOKEN_FILE"];
const MAX_POLICY_BYTES: usize = 256 * 1024;
const DEFAULT_CHATGPT_BASE_URLS: &[&str] =
    &["https://chatgpt.com", "https://chatgpt.com/backend-api"];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AuthPolicy {
    pub allowed_login_methods: Option<Vec<String>>,
    pub allowed_login_methods_source: Option<String>,
    pub allowed_chatgpt_workspaces: Option<Vec<String>>,
    pub allowed_chatgpt_workspaces_source: Option<String>,
    pub cli_auth_credentials_store: Option<String>,
    pub cli_auth_credentials_store_source: Option<String>,
    pub chatgpt_base_url: Option<String>,
    pub chatgpt_base_url_source: Option<String>,
    pub forced_login_method: Option<String>,
    pub forced_login_method_source: Option<String>,
    pub forced_chatgpt_workspace_ids: Vec<String>,
    pub forced_chatgpt_workspace_ids_source: Option<String>,
}

impl AuthPolicy {
    pub(crate) fn validate_file_oauth(&self, operation: &str) -> Result<()> {
        if let Some(methods) = &self.allowed_login_methods
            && !methods.iter().any(|method| method == "chatgpt")
        {
            anyhow::bail!(
                "cannot {operation}: {} does not allow ChatGPT login",
                self.allowed_login_methods_source
                    .as_deref()
                    .unwrap_or("Codex managed policy")
            );
        }
        if self
            .effective_workspace_allowlist()
            .is_some_and(|ids| ids.is_empty())
        {
            anyhow::bail!(
                "cannot {operation}: {} allows no ChatGPT workspaces",
                self.allowed_chatgpt_workspaces_source
                    .as_deref()
                    .or(self.forced_chatgpt_workspace_ids_source.as_deref())
                    .unwrap_or("Codex managed policy")
            );
        }
        if let Some(method) = &self.forced_login_method
            && method != "chatgpt"
        {
            let reason = if method == "api" {
                "requires API key login"
            } else {
                "requires a login method unsupported by codex-switch"
            };
            anyhow::bail!(
                "cannot {operation}: {} {reason}; codex-switch uses ChatGPT OAuth",
                self.forced_login_method_source
                    .as_deref()
                    .unwrap_or("Codex managed policy")
            );
        }
        if let Some(store) = &self.cli_auth_credentials_store
            && store != "file"
        {
            anyhow::bail!(
                "cannot {operation}: {} requires cli_auth_credentials_store = \"{store}\"; codex-switch requires cli_auth_credentials_store = \"file\"",
                self.cli_auth_credentials_store_source
                    .as_deref()
                    .unwrap_or("Codex managed policy")
            );
        }
        if let Some(base_url) = &self.chatgpt_base_url
            && !DEFAULT_CHATGPT_BASE_URLS
                .iter()
                .any(|default| base_url.trim_end_matches('/') == *default)
        {
            anyhow::bail!(
                "cannot {operation}: {} sets chatgpt_base_url to a non-default endpoint that codex-switch does not support",
                self.chatgpt_base_url_source
                    .as_deref()
                    .unwrap_or("Codex managed policy")
            );
        }
        Ok(())
    }

    pub(crate) fn effective_workspace_allowlist(&self) -> Option<Vec<String>> {
        match (
            &self.allowed_chatgpt_workspaces,
            self.forced_chatgpt_workspace_ids.is_empty(),
        ) {
            (None, true) => None,
            (Some(allowed), true) => Some(allowed.clone()),
            (None, false) => Some(self.forced_chatgpt_workspace_ids.clone()),
            (Some(allowed), false) => Some(
                allowed
                    .iter()
                    .filter(|id| self.forced_chatgpt_workspace_ids.contains(id))
                    .cloned()
                    .collect(),
            ),
        }
    }

    pub(crate) fn workspace_allowlist(&self) -> Vec<String> {
        self.effective_workspace_allowlist().unwrap_or_default()
    }

    pub(crate) fn validate_workspace(&self, account_id: Option<&str>) -> Result<()> {
        let Some(allowed) = self.effective_workspace_allowlist() else {
            return Ok(());
        };
        if allowed.is_empty() {
            anyhow::bail!(
                "{} leaves no allowed ChatGPT workspace",
                self.allowed_chatgpt_workspaces_source
                    .as_deref()
                    .or(self.forced_chatgpt_workspace_ids_source.as_deref())
                    .unwrap_or("Codex managed policy")
            );
        }
        let account_id = account_id.ok_or_else(|| {
            anyhow::anyhow!(
                "login token has no workspace id required by {}",
                self.allowed_chatgpt_workspaces_source
                    .as_deref()
                    .or(self.forced_chatgpt_workspace_ids_source.as_deref())
                    .unwrap_or("Codex managed policy")
            )
        })?;
        if !allowed.iter().any(|id| id == account_id) {
            anyhow::bail!(
                "workspace {account_id} is not allowed by {}",
                self.allowed_chatgpt_workspaces_source
                    .as_deref()
                    .or(self.forced_chatgpt_workspace_ids_source.as_deref())
                    .unwrap_or("Codex managed workspace policy")
            );
        }
        Ok(())
    }
}

/// Fail closed when Codex selects workload identity over stored OAuth. Presence
/// is significant even for an empty value; this function never edits the env.
pub(crate) fn check_federated_identity_presence<F>(operation: &str, mut is_present: F) -> Result<()>
where
    F: FnMut(&str) -> bool,
{
    if let Some(name) = FEDERATED_IDENTITY_ENV.iter().find(|name| is_present(name)) {
        anyhow::bail!(
            "cannot {operation}: {name} is present, so Codex selects federated identity instead of file-backed ChatGPT credentials; codex-switch will not unset authentication variables. Ask your administrator to configure a compatible authentication source"
        );
    }
    Ok(())
}

pub(crate) fn ensure_file_oauth_environment(operation: &str) -> Result<()> {
    check_federated_identity_presence(operation, |name| std::env::var_os(name).is_some())
}

#[cfg(any(test, not(windows)))]
pub(crate) fn system_requirements_path(program_data: Option<&str>, os: &str) -> Option<PathBuf> {
    if os == "windows" {
        return program_data.map(|root| PathBuf::from(root).join("OpenAI/Codex/requirements.toml"));
    }
    Some(PathBuf::from("/etc/codex/requirements.toml"))
}

pub(crate) fn managed_config_path(codex_home: &Path, os: &str) -> PathBuf {
    if os == "windows" {
        codex_home.join("managed_config.toml")
    } else {
        PathBuf::from("/etc/codex/managed_config.toml")
    }
}

fn read_limited(path: &Path) -> Result<Option<String>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading managed policy {}", path.display()));
        }
    };
    if metadata.len() > MAX_POLICY_BYTES as u64 {
        anyhow::bail!(
            "managed policy {} exceeds the {} byte safety limit",
            path.display(),
            MAX_POLICY_BYTES
        );
    }
    if !metadata.is_file() {
        anyhow::bail!("managed policy {} is not a regular file", path.display());
    }
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening managed policy {}", path.display()))?;
    if !file
        .metadata()
        .with_context(|| format!("checking managed policy {}", path.display()))?
        .is_file()
    {
        anyhow::bail!("managed policy {} is not a regular file", path.display());
    }
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(MAX_POLICY_BYTES));
    file.take((MAX_POLICY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading managed policy {}", path.display()))?;
    if bytes.len() > MAX_POLICY_BYTES {
        anyhow::bail!(
            "managed policy {} exceeds the {} byte safety limit",
            path.display(),
            MAX_POLICY_BYTES
        );
    }
    String::from_utf8(bytes)
        .map(Some)
        .with_context(|| format!("managed policy {} is not UTF-8", path.display()))
}

fn decode_mdm_toml(encoded: &str, source: &str) -> Result<toml::Value> {
    if encoded.len() > MAX_POLICY_BYTES * 2 {
        anyhow::bail!("{source} exceeds the managed policy size limit");
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .with_context(|| format!("decoding {source}"))?;
    if bytes.len() > MAX_POLICY_BYTES {
        anyhow::bail!("{source} exceeds the managed policy size limit");
    }
    let text = String::from_utf8(bytes).with_context(|| format!("{source} is not UTF-8"))?;
    toml::from_str(&text).with_context(|| format!("parsing {source}"))
}

fn string_array(value: &toml::Value, key: &str, source: &str) -> Result<Option<Vec<String>>> {
    let Some(value) = value.get(key) else {
        return Ok(None);
    };
    let array = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("{source}: {key} must be an array of strings"))?;
    let values = array
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("{source}: {key} must be an array of strings"))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(values))
}

fn optional_string(value: &toml::Value, key: &str, source: &str) -> Result<Option<String>> {
    let Some(value) = value.get(key) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(|s| Some(s.to_owned()))
        .ok_or_else(|| anyhow::anyhow!("{source}: {key} must be a string"))
}

fn merge_requirements(policy: &mut AuthPolicy, value: &toml::Value, source: &str) -> Result<()> {
    if let Some(methods) = string_array(value, "allowed_login_methods", source)? {
        if methods.is_empty() || methods.iter().any(|m| m != "chatgpt" && m != "api") {
            anyhow::bail!("{source}: allowed_login_methods must contain chatgpt and/or api");
        }
        policy.allowed_login_methods = Some(methods);
        policy.allowed_login_methods_source = Some(source.to_owned());
    }
    if let Some(workspaces) = string_array(value, "allowed_chatgpt_workspaces", source)? {
        policy.allowed_chatgpt_workspaces = Some(workspaces);
        policy.allowed_chatgpt_workspaces_source = Some(source.to_owned());
    }
    if let Some(store) = optional_string(value, "cli_auth_credentials_store", source)? {
        if !["file", "keyring", "auto", "ephemeral"].contains(&store.as_str()) {
            anyhow::bail!("{source}: cli_auth_credentials_store has an unsupported value");
        }
        policy.cli_auth_credentials_store = Some(store);
        policy.cli_auth_credentials_store_source = Some(source.to_owned());
    }
    if let Some(url) = optional_string(value, "chatgpt_base_url", source)? {
        policy.chatgpt_base_url = Some(url);
        policy.chatgpt_base_url_source = Some(source.to_owned());
    }
    Ok(())
}

fn merge_managed_config(policy: &mut AuthPolicy, value: &toml::Value, source: &str) -> Result<()> {
    if let Some(method) = optional_string(value, "forced_login_method", source)? {
        policy.forced_login_method = Some(method);
        policy.forced_login_method_source = Some(source.to_owned());
    }
    if let Some(value) = value.get("forced_chatgpt_workspace_id") {
        let ids = match value {
            toml::Value::String(id) => vec![id.clone()],
            toml::Value::Array(items) => items
                .iter()
                .map(|item| {
                    item.as_str().map(str::to_owned).ok_or_else(|| {
                        anyhow::anyhow!(
                            "{source}: forced_chatgpt_workspace_id must contain strings"
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            _ => anyhow::bail!(
                "{source}: forced_chatgpt_workspace_id must be a string or list of strings"
            ),
        };
        policy.forced_chatgpt_workspace_ids = ids
            .into_iter()
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .collect();
        policy.forced_chatgpt_workspace_ids_source = Some(source.to_owned());
    }
    if let Some(store) = optional_string(value, "cli_auth_credentials_store", source)? {
        policy.cli_auth_credentials_store = Some(store);
        policy.cli_auth_credentials_store_source = Some(source.to_owned());
    }
    if let Some(url) = optional_string(value, "chatgpt_base_url", source)? {
        policy.chatgpt_base_url = Some(url);
        policy.chatgpt_base_url_source = Some(source.to_owned());
    }
    // managed_config is an intentional policy table in config.toml. Don't
    // treat similarly named auth fields at the document root as managed.
    Ok(())
}

fn config_table<'a>(
    document: &'a toml::Value,
    key: &str,
    source: &str,
) -> Result<Option<&'a toml::Value>> {
    match document.get(key) {
        None => Ok(None),
        Some(value) if value.is_table() => Ok(Some(value)),
        Some(_) => anyhow::bail!("{source}: {key} must be a table"),
    }
}

pub(crate) fn resolve_from_texts(
    system: Option<&str>,
    user: Option<&str>,
    managed_file: Option<&str>,
    mdm_requirements: Option<&str>,
    mdm_config: Option<&str>,
) -> Result<AuthPolicy> {
    let mut policy = AuthPolicy::default();
    if let Some(text) = user {
        let document: toml::Value = toml::from_str(text).context("parsing Codex config.toml")?;
        merge_managed_config(&mut policy, &document, "user config")?;
        if let Some(managed) = config_table(&document, "managed_config", "Codex config.toml")? {
            merge_managed_config(&mut policy, managed, "managed_config")?;
        }
    }
    if let Some(text) = managed_file {
        let document: toml::Value = toml::from_str(text).context("parsing managed_config.toml")?;
        merge_managed_config(&mut policy, &document, "managed_config.toml")?;
    }
    if let Some(text) = mdm_config {
        let document = decode_mdm_toml(text, "MDM config_toml_base64")?;
        if let Some(managed) = config_table(&document, "managed_config", "MDM config")? {
            merge_managed_config(&mut policy, managed, "MDM managed_config")?;
        }
        merge_managed_config(&mut policy, &document, "MDM config")?;
    }
    // Requirements govern effective defaults, so user config cannot override
    // a forced credential store or backend URL.
    if let Some(text) = system {
        let value: toml::Value =
            toml::from_str(text).context("parsing system requirements.toml")?;
        merge_requirements(&mut policy, &value, "system requirements.toml")?;
    }
    if let Some(text) = mdm_requirements {
        let value = decode_mdm_toml(text, "MDM requirements_toml_base64")?;
        merge_requirements(&mut policy, &value, "MDM requirements")?;
    }
    Ok(policy)
}

pub(crate) fn load_auth_policy(codex_home: &Path) -> Result<AuthPolicy> {
    let (requirements, mdm_requirements, mdm_config) = read_platform_policy_sources()?;
    let config = read_limited(&codex_home.join("config.toml"))?;
    #[cfg(test)]
    let managed_path = codex_home.join("managed_config.toml");
    #[cfg(not(test))]
    let managed_path = managed_config_path(codex_home, std::env::consts::OS);
    let managed_file = read_limited(&managed_path)?;
    resolve_from_texts(
        requirements.as_deref(),
        config.as_deref(),
        managed_file.as_deref(),
        mdm_requirements.as_deref(),
        mdm_config.as_deref(),
    )
}

#[cfg(not(test))]
fn read_platform_policy_sources() -> Result<(Option<String>, Option<String>, Option<String>)> {
    #[cfg(windows)]
    let requirements_path =
        Some(windows_program_data_path()?.join("OpenAI/Codex/requirements.toml"));
    #[cfg(not(windows))]
    let requirements_path = system_requirements_path(None, std::env::consts::OS);
    let requirements = requirements_path
        .as_deref()
        .map(read_limited)
        .transpose()?
        .flatten();
    let (mdm_requirements, mdm_config) = read_mdm_policy()?;
    Ok((requirements, mdm_requirements, mdm_config))
}

#[cfg(windows)]
fn windows_program_data_path() -> Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::{ptr, slice};
    use windows_sys::Win32::Foundation::S_OK;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath};

    let mut path = ptr::null_mut();
    // SAFETY: the API writes an allocated, NUL-terminated UTF-16 path into
    // `path`; it is released with CoTaskMemFree on both success and failure.
    let status =
        unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, 0, ptr::null_mut(), &mut path) };
    if status != S_OK || path.is_null() {
        if !path.is_null() {
            unsafe { CoTaskMemFree(path.cast()) };
        }
        anyhow::bail!(
            "Windows could not resolve the ProgramData known folder (HRESULT {status:#x})"
        );
    }
    let mut length = 0usize;
    // SAFETY: SHGetKnownFolderPath guarantees a NUL-terminated allocation.
    unsafe {
        while *path.add(length) != 0 {
            length += 1;
        }
    }
    // SAFETY: `path` has `length` initialized UTF-16 code units.
    let value = unsafe { OsString::from_wide(slice::from_raw_parts(path, length)) };
    unsafe { CoTaskMemFree(path.cast()) };
    Ok(PathBuf::from(value))
}

// Unit tests exercise policy semantics with injected TOML/base64 fixtures.
// They never read the developer machine's enterprise policy files or MDM.
#[cfg(test)]
fn read_platform_policy_sources() -> Result<(Option<String>, Option<String>, Option<String>)> {
    Ok((None, None, None))
}

#[cfg(all(target_os = "macos", not(test)))]
fn read_mdm_policy() -> Result<(Option<String>, Option<String>)> {
    // This uses the system managed-preference domain and accepts only values
    // marked forced by macOS. User-editable preferences are not MDM policy.
    macos_mdm::read()
}

#[cfg(all(not(target_os = "macos"), not(test)))]
fn read_mdm_policy() -> Result<(Option<String>, Option<String>)> {
    Ok((None, None))
}

#[cfg(all(target_os = "macos", not(test)))]
mod macos_mdm {
    use anyhow::{Context, Result};
    use std::ffi::{CStr, CString, c_char, c_void};
    type CFTypeRef = *const c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            alloc: CFTypeRef,
            c_str: *const c_char,
            encoding: u32,
        ) -> CFTypeRef;
        fn CFStringGetCString(
            string: CFTypeRef,
            buffer: *mut c_char,
            size: isize,
            encoding: u32,
        ) -> bool;
        fn CFStringGetLength(string: CFTypeRef) -> isize;
        fn CFPreferencesCopyAppValue(key: CFTypeRef, app_id: CFTypeRef) -> CFTypeRef;
        fn CFPreferencesAppValueIsForced(key: CFTypeRef, app_id: CFTypeRef) -> bool;
        fn CFGetTypeID(value: CFTypeRef) -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFRelease(value: CFTypeRef);
    }
    const UTF8: u32 = 0x08000100;
    const APP: &str = "com.openai.codex";

    unsafe fn cf_string(value: &str) -> Result<CFTypeRef> {
        let c = CString::new(value)?;
        // SAFETY: CoreFoundation copies the NUL-terminated input string.
        let result = unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) };
        if result.is_null() {
            anyhow::bail!("creating a CoreFoundation string failed");
        }
        Ok(result)
    }
    unsafe fn read_forced(key: &str, app: CFTypeRef) -> Result<Option<String>> {
        let key_ref = unsafe { cf_string(key)? };
        // SAFETY: key_ref and app are valid CFStringRefs for these APIs.
        let forced = unsafe { CFPreferencesAppValueIsForced(key_ref, app) };
        let value = unsafe { CFPreferencesCopyAppValue(key_ref, app) };
        unsafe {
            CFRelease(key_ref);
        }
        if value.is_null() || !forced {
            if !value.is_null() {
                unsafe { CFRelease(value) };
            }
            return Ok(None);
        }
        // SAFETY: CFGetTypeID accepts any CFTypeRef; never pass a non-string
        // property-list value to the CFString conversion API.
        if unsafe { CFGetTypeID(value) } != unsafe { CFStringGetTypeID() } {
            unsafe { CFRelease(value) };
            anyhow::bail!("forced MDM preference {key} must be a string");
        }
        // Keep the original UTF-16 length so an embedded NUL cannot make the
        // later C-string conversion silently accept only a prefix.
        let utf16_length = unsafe { CFStringGetLength(value) };
        if utf16_length < 0 {
            unsafe { CFRelease(value) };
            anyhow::bail!("forced MDM preference {key} has an invalid string length");
        }
        let mut bytes = vec![0i8; 512 * 1024];
        let ok =
            unsafe { CFStringGetCString(value, bytes.as_mut_ptr(), bytes.len() as isize, UTF8) };
        unsafe {
            CFRelease(value);
        }
        if !ok {
            anyhow::bail!("forced MDM preference {key} is not a bounded UTF-8 string");
        }
        // SAFETY: CFStringGetCString writes a NUL-terminated UTF-8 buffer on success.
        let c_string = unsafe { CStr::from_ptr(bytes.as_ptr()) };
        let text = std::str::from_utf8(c_string.to_bytes())
            .with_context(|| format!("forced MDM preference {key} is not valid UTF-8"))?;
        if text.encode_utf16().count() != utf16_length as usize {
            anyhow::bail!("forced MDM preference {key} contains an embedded NUL or was truncated");
        }
        Ok(Some(text.to_owned()))
    }
    pub(super) fn read() -> Result<(Option<String>, Option<String>)> {
        let app = unsafe { cf_string(APP)? };
        let result = (|| -> Result<(Option<String>, Option<String>)> {
            // Keep `?` inside this closure so every error path reaches the
            // CoreFoundation release below.
            unsafe {
                let requirements = read_forced("requirements_toml_base64", app)
                    .context("reading forced Codex MDM requirements")?;
                let config = read_forced("config_toml_base64", app)
                    .context("reading forced Codex MDM config")?;
                Ok((requirements, config))
            }
        })();
        unsafe {
            CFRelease(app);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn federated_environment_presence_rejects_file_oauth_even_when_empty() {
        let error = check_federated_identity_presence("switch to a ChatGPT profile", |name| {
            name == "OPENAI_IDENTITY_TOKEN_FILE"
        })
        .expect_err("a present federation variable selects a different auth source");
        let message = format!("{error:#}");
        assert!(message.contains("OPENAI_IDENTITY_TOKEN_FILE"), "{message}");
        assert!(message.contains("switch to a ChatGPT profile"), "{message}");
        assert!(message.contains("will not unset"), "{message}");
    }

    #[test]
    fn file_oauth_preflight_allows_unset_federation_environment() {
        check_federated_identity_presence("login", |_| false)
            .expect("ordinary file-backed OAuth remains available");
    }

    #[test]
    fn managed_config_path_matches_codex_platform_location() {
        let home = Path::new("/home/test/.codex");
        assert_eq!(
            managed_config_path(home, "windows"),
            home.join("managed_config.toml")
        );
        assert_eq!(
            managed_config_path(home, "linux"),
            PathBuf::from("/etc/codex/managed_config.toml")
        );
    }

    #[test]
    fn unix_requirements_path_matches_codex_platform_location() {
        assert_eq!(
            system_requirements_path(None, "linux"),
            Some(PathBuf::from("/etc/codex/requirements.toml"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_requirements_path_matches_codex_platform_location() {
        assert_eq!(
            system_requirements_path(Some(r"C:\ProgramData"), "windows"),
            Some(PathBuf::from(
                r"C:\ProgramData\OpenAI\Codex\requirements.toml"
            ))
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_requirements_root_ignores_overridden_programdata_environment() {
        let _lock = crate::profile::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let old = std::env::var_os("ProgramData");
        unsafe { std::env::set_var("ProgramData", r"Z:\attacker-controlled") };
        let result = windows_program_data_path().unwrap();
        unsafe {
            match old {
                Some(value) => std::env::set_var("ProgramData", value),
                None => std::env::remove_var("ProgramData"),
            }
        }
        assert!(!result.starts_with(r"Z:\attacker-controlled"));
    }

    #[test]
    fn requirement_sources_override_per_field_and_config_sources_follow_precedence() {
        let system = "allowed_login_methods = ['api']\nallowed_chatgpt_workspaces = ['s1']";
        let user =
            "[managed_config]\nforced_login_method = 'api'\nforced_chatgpt_workspace_id = 'u1'";
        let mdm_reqs = base64::engine::general_purpose::STANDARD
            .encode("allowed_login_methods = ['chatgpt']\nallowed_chatgpt_workspaces = ['m1']");
        let mdm_cfg = base64::engine::general_purpose::STANDARD
            .encode("forced_login_method = 'chatgpt'\nforced_chatgpt_workspace_id = 'm1'");
        let policy = resolve_from_texts(
            Some(system),
            Some(user),
            None,
            Some(&mdm_reqs),
            Some(&mdm_cfg),
        )
        .unwrap();
        assert_eq!(
            policy.allowed_login_methods.as_deref(),
            Some(["chatgpt".to_string()].as_slice())
        );
        assert_eq!(policy.workspace_allowlist(), vec!["m1"]);
        assert!(policy.validate_file_oauth("login").is_ok());
    }

    #[test]
    fn empty_workspace_requirement_blocks_chatgpt_login() {
        let policy = resolve_from_texts(
            Some("allowed_chatgpt_workspaces = []"),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(policy.validate_file_oauth("login").is_err());
    }

    #[test]
    fn non_default_forced_endpoint_is_rejected() {
        let policy = AuthPolicy {
            chatgpt_base_url: Some("https://corp.example/backend-api".into()),
            ..AuthPolicy::default()
        };
        assert!(
            policy
                .validate_file_oauth("launch")
                .unwrap_err()
                .to_string()
                .contains("does not support")
        );
    }

    #[test]
    fn supported_chatgpt_base_url_defaults_are_accepted() {
        for value in ["https://chatgpt.com", "https://chatgpt.com/backend-api/"] {
            let policy = AuthPolicy {
                chatgpt_base_url: Some(value.into()),
                ..AuthPolicy::default()
            };
            policy.validate_file_oauth("login").unwrap();
        }
    }

    #[test]
    fn disjoint_user_workspace_and_managed_allowlist_fails_closed() {
        let policy = resolve_from_texts(
            Some("allowed_chatgpt_workspaces = ['managed']"),
            Some("forced_chatgpt_workspace_id = 'user'"),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(policy.validate_file_oauth("login").is_err());
    }

    #[test]
    fn system_requirements_override_user_credential_store_setting() {
        let policy = resolve_from_texts(
            Some("cli_auth_credentials_store = 'keyring'"),
            Some("cli_auth_credentials_store = 'file'"),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            policy.cli_auth_credentials_store.as_deref(),
            Some("keyring")
        );
        assert!(
            policy
                .validate_file_oauth("login")
                .unwrap_err()
                .to_string()
                .contains("system requirements.toml")
        );
    }

    #[test]
    fn managed_config_sources_override_in_their_documented_order() {
        let mdm =
            base64::engine::general_purpose::STANDARD.encode("forced_login_method = 'chatgpt'");
        let policy = resolve_from_texts(
            None,
            Some("forced_login_method = 'api'"),
            Some("forced_login_method = 'api'"),
            None,
            Some(&mdm),
        )
        .unwrap();
        assert_eq!(policy.forced_login_method.as_deref(), Some("chatgpt"));
        assert!(policy.validate_file_oauth("login").is_ok());
    }

    #[test]
    fn oversized_policy_file_is_rejected_before_reading_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("requirements.toml");
        std::fs::write(&path, vec![b' '; MAX_POLICY_BYTES + 1]).unwrap();
        assert!(
            read_limited(&path)
                .unwrap_err()
                .to_string()
                .contains("safety limit")
        );
    }

    #[test]
    fn invalid_requirement_array_fails_closed() {
        assert!(
            resolve_from_texts(Some("allowed_login_methods = []"), None, None, None, None).is_err()
        );
        assert!(
            resolve_from_texts(
                Some("allowed_chatgpt_workspaces = ['ok', 1]"),
                None,
                None,
                None,
                None
            )
            .is_err()
        );
    }
}
