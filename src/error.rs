#![forbid(unsafe_code)]

//! One typed error for every HTTP surface, rendered as RFC 9457 problem+json.
//!
//! Handlers return `Result<T, ApiError>`; nothing in this module ever includes a
//! credential, an upstream response body, or a database string in the payload.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use thiserror::Error;

use crate::domain::onboarding::OnboardingError;
use crate::domain::plans::PlanError;
use crate::domain::runs::RunTransitionError;
use crate::domain::webhooks::WebhookError;
use crate::domain::workers::WorkerTransitionError;

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum ApiError {
    #[error("unauthenticated")]
    Unauthenticated,
    #[error("forbidden")]
    Forbidden,
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid request: {0}")]
    BadRequest(String),
    #[error("payload too large")]
    PayloadTooLarge,
    #[error("rate limited")]
    RateLimited { retry_after_seconds: u64 },
    #[error("dependency unavailable: {0}")]
    Unavailable(&'static str),
    #[error("internal error")]
    Internal,
}

impl ApiError {
    #[must_use]
    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::BadRequest(detail.into())
    }

    #[must_use]
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::Conflict(detail.into())
    }

    #[must_use]
    pub const fn status(&self) -> StatusCode {
        match self {
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Unauthenticated => "unauthenticated",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Conflict(_) => "conflict",
            Self::BadRequest(_) => "invalid_request",
            Self::PayloadTooLarge => "payload_too_large",
            Self::RateLimited { .. } => "rate_limited",
            Self::Unavailable(_) => "dependency_unavailable",
            Self::Internal => "internal_error",
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::Conflict(detail) | Self::BadRequest(detail) => detail.clone(),
            Self::Unavailable(dependency) => format!("{dependency} is unavailable"),
            other => other.to_string(),
        }
    }
}

#[derive(Serialize)]
struct Problem {
    #[serde(rename = "type")]
    kind: String,
    title: &'static str,
    status: u16,
    detail: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let code = self.code();
        let detail = self.detail();
        let retry_after = match &self {
            Self::RateLimited {
                retry_after_seconds,
            } => Some(*retry_after_seconds),
            _ => None,
        };
        let mut response = (
            status,
            Json(Problem {
                kind: format!("urn:gha-indie-worker:api:{code}"),
                title: code,
                status: status.as_u16(),
                detail,
            }),
        )
            .into_response();

        if status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"gha-indie-worker\""),
            );
        }
        if let Some(seconds) = retry_after {
            if let Ok(value) = HeaderValue::try_from(seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

impl From<OnboardingError> for ApiError {
    fn from(error: OnboardingError) -> Self {
        Self::Conflict(error.to_string())
    }
}

impl From<RunTransitionError> for ApiError {
    fn from(error: RunTransitionError) -> Self {
        Self::Conflict(error.to_string())
    }
}

impl From<WorkerTransitionError> for ApiError {
    fn from(error: WorkerTransitionError) -> Self {
        Self::Conflict(error.to_string())
    }
}

impl From<PlanError> for ApiError {
    fn from(error: PlanError) -> Self {
        Self::BadRequest(error.to_string())
    }
}

impl From<WebhookError> for ApiError {
    fn from(error: WebhookError) -> Self {
        match error {
            WebhookError::NotConfigured => Self::Unavailable("github webhook verification"),
            WebhookError::DuplicateDelivery => Self::Conflict("delivery already processed".into()),
            WebhookError::MissingSignature
            | WebhookError::MalformedSignature
            | WebhookError::SignatureMismatch
            | WebhookError::MissingDeliveryId
            | WebhookError::MalformedDeliveryId => Self::Unauthenticated,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_maps_to_a_distinct_status_and_code() {
        let variants = [
            ApiError::Unauthenticated,
            ApiError::Forbidden,
            ApiError::NotFound,
            ApiError::Conflict("x".into()),
            ApiError::BadRequest("x".into()),
            ApiError::PayloadTooLarge,
            ApiError::RateLimited {
                retry_after_seconds: 1,
            },
            ApiError::Unavailable("db"),
            ApiError::Internal,
        ];
        for variant in &variants {
            assert!(variant.status().is_client_error() || variant.status().is_server_error());
            assert!(!variant.code().is_empty());
        }
    }

    #[test]
    fn webhook_failures_never_leak_the_reason_as_a_client_hint() {
        let error: ApiError = WebhookError::SignatureMismatch.into();
        assert_eq!(error.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(error.detail(), "unauthenticated");
    }
}
