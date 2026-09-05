//! Path 4: durable NATS request plane (API intake).
//!
//! Decode and validate the product request envelope without opening a
//! broker connection. Credentials are never CLI flags; they stay in the
//! environment or secret store that supplies `GHA_INDIE_WORKER_NATS_URL`.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const REQUEST_SUBJECT: &str = "dd.remote.web_api.gha-indie-worker.request";
pub const STATUS_SUBJECT: &str = "dd.remote.web_api.gha-indie-worker.status";
pub const CONTRACT: &str = "gha-indie-worker/web-api/v1";
pub const AUDIENCE: &str = "gha-indie-worker-api";
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionMode {
    DirectReadOnlyDatabase,
    StatelessHttp,
    StatefulMtlsTcp,
    Nats,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Read,
    Write,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum NatsError {
    #[error("NATS URL is required when mode is nats")]
    MissingUrl,
    #[error("invalid NATS URL")]
    InvalidUrl,
    #[error("unsupported request subject")]
    InvalidSubject,
    #[error("unsupported web/API contract")]
    Contract,
    #[error("wrong API audience")]
    Audience,
    #[error("invalid {0}")]
    InvalidIdentifier(&'static str),
    #[error("invalid resource")]
    InvalidResource,
    #[error("NATS mode requires a dedupe key")]
    MissingDedupeKey,
    #[error("request exceeds the byte limit")]
    RequestTooLarge,
    #[error("serialization failed")]
    Serialization,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEnvelope {
    pub contract: String,
    pub request_id: String,
    pub tenant_id: String,
    pub subject: String,
    pub audience: String,
    pub operation: Operation,
    pub resource: String,
    pub payload: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishRequest {
    pub subject: &'static str,
    pub payload: Vec<u8>,
}

pub fn require_nats_url<'a>(
    mode: InteractionMode,
    url: Option<&'a str>,
) -> Result<Option<&'a str>, NatsError> {
    let trimmed = url.map(str::trim).filter(|value| !value.is_empty());
    match (mode, trimmed) {
        (InteractionMode::Nats, None) => Err(NatsError::MissingUrl),
        (_, Some(url)) if valid_nats_url(url) => Ok(Some(url)),
        (_, Some(_)) => Err(NatsError::InvalidUrl),
        (_, None) => Ok(None),
    }
}

/// Build the request subject and serialized envelope without touching a broker.
pub fn publish_request(
    url: Option<&str>,
    mode: InteractionMode,
    envelope: &RequestEnvelope,
) -> Result<PublishRequest, NatsError> {
    if mode == InteractionMode::Nats {
        require_nats_url(mode, url)?;
    }
    envelope.validate_for(mode)?;
    let payload = serde_json::to_vec(envelope).map_err(|_| NatsError::Serialization)?;
    if payload.len() > MAX_REQUEST_BYTES {
        return Err(NatsError::RequestTooLarge);
    }
    Ok(PublishRequest {
        subject: REQUEST_SUBJECT,
        payload,
    })
}

/// Accept a published envelope on the org request subject. Pure: no broker.
pub fn accept_request(
    url: Option<&str>,
    mode: InteractionMode,
    subject: &str,
    payload: &[u8],
) -> Result<RequestEnvelope, NatsError> {
    if mode == InteractionMode::Nats {
        require_nats_url(mode, url)?;
    }
    if subject != REQUEST_SUBJECT {
        return Err(NatsError::InvalidSubject);
    }
    if payload.is_empty() || payload.len() > MAX_REQUEST_BYTES {
        return Err(NatsError::RequestTooLarge);
    }
    let envelope: RequestEnvelope =
        serde_json::from_slice(payload).map_err(|_| NatsError::Serialization)?;
    envelope.validate_for(mode)?;
    Ok(envelope)
}

impl RequestEnvelope {
    pub fn validate_for(&self, mode: InteractionMode) -> Result<(), NatsError> {
        if self.contract != CONTRACT {
            return Err(NatsError::Contract);
        }
        validate_identifier("request_id", &self.request_id, 128)?;
        validate_identifier("tenant_id", &self.tenant_id, 128)?;
        validate_identifier("subject", &self.subject, 255)?;
        if self.audience != AUDIENCE {
            return Err(NatsError::Audience);
        }
        validate_resource(&self.resource)?;
        if mode == InteractionMode::Nats {
            validate_dedupe_key(self.dedupe_key.as_deref())?;
        }
        Ok(())
    }
}

fn valid_nats_url(value: &str) -> bool {
    let Some(rest) = value
        .strip_prefix("nats://")
        .or_else(|| value.strip_prefix("tls://"))
    else {
        return false;
    };
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return false;
    }
    let host = rest.rsplit_once('@').map(|(_, host)| host).unwrap_or(rest);
    !host.is_empty()
        && (host.contains('.')
            || host.starts_with("127.0.0.1")
            || host.starts_with("localhost")
            || host.starts_with('['))
}

fn validate_identifier(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), NatsError> {
    if value.is_empty()
        || value.len() > maximum
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(NatsError::InvalidIdentifier(field));
    }
    Ok(())
}

fn validate_resource(value: &str) -> Result<(), NatsError> {
    if value.is_empty()
        || value.len() > 256
        || !value.starts_with('/')
        || value.contains("..")
        || value.contains(['?', '#'])
        || value.chars().any(char::is_control)
    {
        return Err(NatsError::InvalidResource);
    }
    Ok(())
}

fn validate_dedupe_key(value: Option<&str>) -> Result<(), NatsError> {
    let value = value.ok_or(NatsError::MissingDedupeKey)?;
    if value.len() < 8 {
        return Err(NatsError::MissingDedupeKey);
    }
    validate_identifier("dedupe_key", value, 128)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn envelope(operation: Operation) -> RequestEnvelope {
        RequestEnvelope {
            contract: CONTRACT.into(),
            request_id: "request-01".into(),
            tenant_id: "tenant-01".into(),
            subject: "11111111-1111-4111-8111-111111111111".into(),
            audience: AUDIENCE.into(),
            operation,
            resource: "/builds".into(),
            payload: json!({"limit": 25}),
            dedupe_key: Some("tenant-01:request-01".into()),
        }
    }

    #[test]
    fn nats_mode_fails_closed_without_a_url() {
        assert_eq!(
            require_nats_url(InteractionMode::Nats, None),
            Err(NatsError::MissingUrl)
        );
        assert_eq!(
            require_nats_url(InteractionMode::Nats, Some("   ")),
            Err(NatsError::MissingUrl)
        );
        assert_eq!(
            require_nats_url(InteractionMode::StatelessHttp, None),
            Ok(None)
        );
        assert_eq!(
            accept_request(
                None,
                InteractionMode::Nats,
                REQUEST_SUBJECT,
                &serde_json::to_vec(&envelope(Operation::Write)).unwrap()
            ),
            Err(NatsError::MissingUrl)
        );
    }

    #[test]
    fn publish_and_accept_are_a_pure_envelope_round_trip() {
        let planned = publish_request(
            Some("nats://127.0.0.1:4222"),
            InteractionMode::Nats,
            &envelope(Operation::Write),
        )
        .expect("valid NATS envelope");

        assert_eq!(planned.subject, REQUEST_SUBJECT);
        assert_eq!(
            planned.subject,
            "dd.remote.web_api.gha-indie-worker.request"
        );

        let accepted = accept_request(
            Some("nats://127.0.0.1:4222"),
            InteractionMode::Nats,
            planned.subject,
            &planned.payload,
        )
        .expect("matching intake");
        assert_eq!(accepted, envelope(Operation::Write));
        assert!(!String::from_utf8_lossy(&planned.payload).contains("nats://"));
    }

    #[test]
    fn nats_envelope_requires_a_dedupe_key() {
        let mut missing = envelope(Operation::Write);
        missing.dedupe_key = None;
        assert_eq!(
            publish_request(
                Some("nats://127.0.0.1:4222"),
                InteractionMode::Nats,
                &missing
            ),
            Err(NatsError::MissingDedupeKey)
        );
    }

    #[test]
    fn credentials_never_appear_as_typed_nats_fields() {
        let planned = publish_request(
            Some("nats://worker:credential@nats.internal:4222"),
            InteractionMode::Nats,
            &envelope(Operation::Read),
        )
        .expect("URL userinfo is transport config, not envelope data");
        let decoded: RequestEnvelope =
            serde_json::from_slice(&planned.payload).expect("round-trip");
        let encoded = serde_json::to_value(&decoded).expect("json");
        assert!(encoded.get("password").is_none());
        assert!(encoded.get("token").is_none());
        assert!(encoded.get("nats_url").is_none());
        assert!(!format!("{decoded:?}").contains("credential"));
    }
}
