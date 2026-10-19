use thiserror::Error;

#[derive(Debug, Error)]
pub enum CsError {
    #[error("profile '{0}' not found")]
    NotFound(String),

    #[error("no auth.json found at {0}")]
    NoAuthFile(String),

    #[error("operation aborted by user")]
    Aborted,

    #[error("cannot delete the active profile '{0}'")]
    ActiveProfileDelete(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("not a codex-switch export bundle: {0}")]
    NotABundle(String),

    #[error(
        "this bundle was created by codex-switch {app_version} (schema {schema}); this build reads \
         schema <= {supported}. Run `codex-switch self-update` on this machine."
    )]
    UnsupportedBundleVersion {
        app_version: String,
        schema: u64,
        supported: u32,
    },

    #[error("unsupported or corrupted encryption parameters: {0}")]
    UnsupportedEncryption(String),

    #[error("wrong passphrase (or the file header was modified)")]
    WrongPassphrase,

    #[error("bundle was modified or corrupted after it was created")]
    BundleTampered,

    #[error("invalid bundle: {0}")]
    InvalidBundle(String),
}

impl CsError {
    /// Stable machine-readable code for `--json` error output.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            CsError::NotABundle(_) => Some("not_a_bundle"),
            CsError::UnsupportedBundleVersion { .. } => Some("unsupported_bundle_version"),
            CsError::UnsupportedEncryption(_) => Some("unsupported_encryption"),
            CsError::WrongPassphrase => Some("wrong_passphrase"),
            CsError::BundleTampered => Some("bundle_tampered"),
            CsError::InvalidBundle(_) => Some("invalid_bundle"),
            _ => None,
        }
    }
}
