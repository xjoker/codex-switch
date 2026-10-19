//! Whole-store export bundles (`codex-switch export`) and their import
//! (`codex-switch import <bundle>`).
//!
//! A bundle is one UTF-8 JSON document. Its payload carries files keyed by a
//! logical path relative to the codex-switch home, never an absolute path, so
//! it can be applied on another OS. Encrypted bundles authenticate the whole
//! header as AES-GCM associated data.

pub(crate) mod apply;
pub(crate) mod crypto;
pub(crate) mod plan;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::error::CsError;

pub(crate) const FORMAT: &str = "codex-switch-export";
pub(crate) const SUPPORTED_SCHEMA: u32 = 1;
pub(crate) const EXTENSION: &str = "csbundle";
/// Bundles are a few KB per account; anything this large is not one.
const MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) const SINGLE_USE_WARNING: &str = "Exported credentials stay valid on the source machine too. OpenAI refresh tokens are single-use: if both machines keep using the same account, one of them will fail with `refresh_token_reused` and need `codex-switch login <alias>`. Use each account on one machine only.";
pub(crate) const PLAINTEXT_WARNING: &str = "This bundle is NOT encrypted. It contains live refresh tokens and API keys: anyone who has the file can use these accounts. Delete it as soon as the import is done.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Platform {
    pub os: String,
    pub arch: String,
}

/// The on-disk document. Field order is fixed: the AAD is serialised from it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bundle {
    pub format: String,
    pub schema_version: u32,
    pub app_version: String,
    pub platform: Platform,
    pub created_at: String,
    pub encryption: Option<crypto::Encryption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ciphertext: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Payload>,
}

#[derive(Serialize)]
struct HeaderAad<'a> {
    format: &'a str,
    schema_version: u32,
    app_version: &'a str,
    platform: &'a Platform,
    created_at: &'a str,
    encryption: &'a Option<crypto::Encryption>,
}

impl Bundle {
    fn aad(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&HeaderAad {
            format: &self.format,
            schema_version: self.schema_version,
            app_version: &self.app_version,
            platform: &self.platform,
            created_at: &self.created_at,
            encryption: &self.encryption,
        })?)
    }

    pub(crate) fn is_encrypted(&self) -> bool {
        self.encryption.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntryKind {
    ProfileAuth,
    Provider,
    ProviderModels,
    AppConfig,
}

impl EntryKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EntryKind::ProfileAuth => "profile_auth",
            EntryKind::Provider => "provider",
            EntryKind::ProviderModels => "provider_models",
            EntryKind::AppConfig => "app_config",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    pub key: String,
    pub kind: EntryKind,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Payload {
    pub source_current: Option<String>,
    pub entries: Vec<Entry>,
}

/// A validated logical key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LogicalKey {
    ProfileAuth(String),
    Provider(String),
    ProviderModels(String),
    AppConfig,
}

impl LogicalKey {
    pub(crate) fn kind(&self) -> EntryKind {
        match self {
            LogicalKey::ProfileAuth(_) => EntryKind::ProfileAuth,
            LogicalKey::Provider(_) => EntryKind::Provider,
            LogicalKey::ProviderModels(_) => EntryKind::ProviderModels,
            LogicalKey::AppConfig => EntryKind::AppConfig,
        }
    }

    pub(crate) fn render(&self) -> String {
        match self {
            LogicalKey::ProfileAuth(a) => format!("profiles/{a}/auth.json"),
            LogicalKey::Provider(a) => format!("providers/{a}/provider.toml"),
            LogicalKey::ProviderModels(a) => format!("providers/{a}/models.json"),
            LogicalKey::AppConfig => "config.toml".to_string(),
        }
    }

    /// Path under `app_home`, rebuilt component by component.
    pub(crate) fn path_in(&self, app_home: &Path) -> PathBuf {
        match self {
            LogicalKey::ProfileAuth(a) => app_home.join("profiles").join(a).join("auth.json"),
            LogicalKey::Provider(a) => app_home.join("providers").join(a).join("provider.toml"),
            LogicalKey::ProviderModels(a) => app_home.join("providers").join(a).join("models.json"),
            LogicalKey::AppConfig => app_home.join("config.toml"),
        }
    }
}

/// Parse a logical key. Only the four documented shapes are accepted; anything
/// else (`..`, backslashes, drive letters, absolute paths, other file names)
/// is a hard error before anything is written.
pub(crate) fn parse_key(key: &str) -> Result<LogicalKey> {
    let bad = || CsError::InvalidBundle(format!("unsupported entry key '{key}'"));
    if key == "config.toml" {
        return Ok(LogicalKey::AppConfig);
    }
    if key.contains('\\') || key.contains(':') || key.starts_with('/') {
        return Err(bad().into());
    }
    let parts: Vec<&str> = key.split('/').collect();
    let [root, alias, file] = parts.as_slice() else {
        return Err(bad().into());
    };
    if crate::profile::validate_alias(alias).is_err() || alias.starts_with('.') {
        return Err(bad().into());
    }
    let alias = alias.to_string();
    match (*root, *file) {
        ("profiles", "auth.json") => Ok(LogicalKey::ProfileAuth(alias)),
        ("providers", "provider.toml") => Ok(LogicalKey::Provider(alias)),
        ("providers", "models.json") => Ok(LogicalKey::ProviderModels(alias)),
        _ => Err(bad().into()),
    }
}

fn payload_digest(payload: &Payload) -> Result<String> {
    let bytes = serde_json::to_vec(payload)?;
    Ok(hex::encode(sha2::Sha256::digest(&bytes)))
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn header(encryption: Option<crypto::Encryption>) -> Bundle {
    Bundle {
        format: FORMAT.to_string(),
        schema_version: SUPPORTED_SCHEMA,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        platform: Platform {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
        },
        created_at: now_rfc3339(),
        encryption,
        payload_sha256: None,
        ciphertext: None,
        payload: None,
    }
}

/// Serialise a payload into bundle bytes. `passphrase = None` writes plaintext.
pub(crate) fn encode_bundle(payload: &Payload, passphrase: Option<&str>) -> Result<Vec<u8>> {
    let bundle = match passphrase {
        Some(passphrase) => {
            let sealer = crypto::new_sealer(passphrase)?;
            let mut bundle = header(Some(sealer.encryption.clone()));
            let plain = zeroize::Zeroizing::new(serde_json::to_vec(payload)?);
            let ct = sealer.seal(&bundle.aad()?, &plain)?;
            bundle.ciphertext = Some(B64.encode(ct));
            bundle
        }
        None => {
            let mut bundle = header(None);
            bundle.payload_sha256 = Some(payload_digest(payload)?);
            bundle.payload = Some(payload.clone());
            bundle
        }
    };
    let mut out = serde_json::to_vec_pretty(&bundle)?;
    out.push(b'\n');
    Ok(out)
}

fn strip_bom(raw: &[u8]) -> &[u8] {
    raw.strip_prefix(b"\xEF\xBB\xBF".as_slice()).unwrap_or(raw)
}

/// Cheap content sniff used by `import` to choose the bundle path. Never
/// errors: anything unreadable is "not a bundle" and goes to the legacy path.
pub fn looks_like_bundle(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() > MAX_BUNDLE_BYTES {
        return false;
    }
    let Ok(raw) = std::fs::read(path) else {
        return false;
    };
    serde_json::from_slice::<serde_json::Value>(strip_bom(&raw))
        .ok()
        .and_then(|v| {
            v.get("format")
                .and_then(|f| f.as_str())
                .map(|f| f == FORMAT)
        })
        .unwrap_or(false)
}

/// Read and structurally validate a bundle file. Version problems are
/// reported here, before any passphrase is requested.
pub(crate) fn read_bundle(path: &Path) -> Result<Bundle> {
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.len() > MAX_BUNDLE_BYTES {
        return Err(CsError::NotABundle(format!("{} is too large", path.display())).into());
    }
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let raw = strip_bom(&raw);
    let probe: serde_json::Value = serde_json::from_slice(raw)
        .map_err(|_| CsError::NotABundle(format!("{} is not JSON", path.display())))?;
    if probe.get("format").and_then(|f| f.as_str()) != Some(FORMAT) {
        return Err(CsError::NotABundle(format!(
            "{} has no \"format\": \"{FORMAT}\"",
            path.display()
        ))
        .into());
    }
    let schema = probe
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| CsError::InvalidBundle("missing schema_version".into()))?;
    if schema > u64::from(SUPPORTED_SCHEMA) || schema == 0 {
        return Err(CsError::UnsupportedBundleVersion {
            app_version: probe
                .get("app_version")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            schema,
            supported: SUPPORTED_SCHEMA,
        }
        .into());
    }
    let bundle: Bundle = serde_json::from_slice(raw)
        .map_err(|e| CsError::InvalidBundle(format!("malformed bundle header: {e}")))?;
    match (&bundle.encryption, &bundle.ciphertext, &bundle.payload) {
        (Some(enc), Some(_), None) => crypto::check_bounds(enc)?,
        (None, None, Some(_)) if bundle.payload_sha256.is_some() => {}
        _ => {
            return Err(CsError::InvalidBundle(
                "bundle must carry either encryption+ciphertext or payload+payload_sha256".into(),
            )
            .into());
        }
    }
    Ok(bundle)
}

/// Decrypt (or checksum-verify) the payload and migrate it to the current
/// schema.
pub(crate) fn open_payload(bundle: &Bundle, passphrase: Option<&str>) -> Result<Payload> {
    let payload = match (&bundle.encryption, &bundle.ciphertext, &bundle.payload) {
        (Some(enc), Some(ct), None) => {
            let passphrase = passphrase.ok_or_else(|| anyhow::anyhow!("passphrase required"))?;
            let ct = B64.decode(ct).map_err(|_| CsError::BundleTampered)?;
            let plain = crypto::open(enc, passphrase, &bundle.aad()?, &ct)?;
            let value: serde_json::Value = serde_json::from_slice(&plain)
                .map_err(|e| CsError::InvalidBundle(format!("payload is not JSON: {e}")))?;
            migrate(value, bundle.schema_version)?
        }
        (None, None, Some(payload)) => {
            if Some(payload_digest(payload)?) != bundle.payload_sha256 {
                return Err(CsError::BundleTampered.into());
            }
            payload.clone()
        }
        _ => return Err(CsError::InvalidBundle("inconsistent bundle".into()).into()),
    };
    Ok(payload)
}

/// One step per schema version. Schema 1 is the first, so this is identity.
fn migrate(value: serde_json::Value, from: u32) -> Result<Payload> {
    match from {
        1 => serde_json::from_value(value)
            .map_err(|e| CsError::InvalidBundle(format!("malformed payload: {e}")).into()),
        other => Err(CsError::UnsupportedBundleVersion {
            app_version: "unknown".into(),
            schema: u64::from(other),
            supported: SUPPORTED_SCHEMA,
        }
        .into()),
    }
}

/// Write a user-chosen destination with 0600 / a file-only DACL, without
/// touching the parent directory's permissions (unlike `atomic_write_private`).
pub(crate) fn write_private_file(path: &Path, contents: &[u8], overwrite: bool) -> Result<()> {
    use std::io::Write;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    if !overwrite && path.exists() {
        anyhow::bail!(
            "{} already exists; choose another path or pass --force",
            path.display()
        );
    }
    let mut tmp = tempfile::NamedTempFile::new_in(&parent)
        .with_context(|| format!("creating temporary file in {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    crate::auth::harden_windows_private_file(tmp.path())?;
    tmp.write_all(contents)?;
    tmp.as_file().sync_all()?;
    if overwrite {
        tmp.persist(path).map_err(|e| e.error)
    } else {
        tmp.persist_noclobber(path).map_err(|e| e.error)
    }
    .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub(crate) fn default_export_name() -> String {
    format!(
        "codex-switch-{}.{EXTENSION}",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_validation() {
        assert_eq!(
            parse_key("profiles/work/auth.json").unwrap(),
            LogicalKey::ProfileAuth("work".into())
        );
        assert_eq!(parse_key("config.toml").unwrap(), LogicalKey::AppConfig);
        for bad in [
            "../x",
            "/etc/x",
            "profiles/a/../../x",
            "C:\\x",
            "profiles/a/other.json",
            "profiles/../auth.json",
            "profiles/a\\b/auth.json",
            "providers/a/auth.json",
            "profiles/a/b/auth.json",
            "",
        ] {
            assert!(parse_key(bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn plaintext_round_trip_detects_corruption() {
        let payload = Payload {
            source_current: Some("a".into()),
            entries: vec![Entry {
                key: "config.toml".into(),
                kind: EntryKind::AppConfig,
                text: "[cache]\nttl = 1\n".into(),
            }],
        };
        let bytes = encode_bundle(&payload, None).unwrap();
        let mut bundle: Bundle = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(open_payload(&bundle, None).unwrap(), payload);
        bundle.payload.as_mut().unwrap().entries[0].text.push('x');
        assert!(open_payload(&bundle, None).is_err());
    }
}
