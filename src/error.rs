#![forbid(unsafe_code)]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("unauthenticated")]
    Unauthenticated,
    #[error("forbidden")]
    Forbidden,
    #[error("invalid configuration: {0}")]
    InvalidConfiguration(&'static str),
    #[error("response serialization failed")]
    Serialization,
    #[error("configuration resolution failed: {0}")]
    ConfigurationResolution(String),
}
