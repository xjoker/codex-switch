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
}
