pub mod image;
pub mod layout;
pub mod migration;
pub mod plan;
pub mod preferences;
pub mod updates;
#[cfg(windows)]
pub mod windows;

use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Operation cancelled; the last durable checkpoint can be resumed.")]
    Cancelled,
}
pub type Result<T> = std::result::Result<T, Error>;
pub fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
