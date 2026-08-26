#![forbid(unsafe_code)]

use crate::error::ApiError;

pub fn require_bearer(header: Option<&str>) -> Result<&str, ApiError> {
    header
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
        .ok_or(ApiError::Unauthenticated)
}

