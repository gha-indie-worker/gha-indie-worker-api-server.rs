#![forbid(unsafe_code)]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("unauthenticated")]
    Unauthenticated,
    #[error("forbidden")]
    Forbidden,
}

