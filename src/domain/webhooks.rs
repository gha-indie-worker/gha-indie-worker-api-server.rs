#![forbid(unsafe_code)]

//! GitHub webhook admission: constant-time HMAC verification plus bounded
//! delivery-ID de-duplication.
//!
//! The signature is compared with [`subtle::ConstantTimeEq`] over raw bytes, so
//! neither the comparison time nor an early return leaks how much of the digest
//! matched. The delivery ledger is bounded by *both* a TTL and a maximum entry
//! count, so a flood of unique delivery IDs cannot grow the process.

use std::collections::VecDeque;
use std::fmt::Write as _;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

const SIGNATURE_PREFIX: &str = "sha256=";
const DIGEST_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum WebhookError {
    #[error("webhook verification is not configured")]
    NotConfigured,
    #[error("missing X-Hub-Signature-256")]
    MissingSignature,
    #[error("malformed X-Hub-Signature-256")]
    MalformedSignature,
    #[error("signature mismatch")]
    SignatureMismatch,
    #[error("missing X-GitHub-Delivery")]
    MissingDeliveryId,
    #[error("malformed X-GitHub-Delivery")]
    MalformedDeliveryId,
    #[error("delivery already processed")]
    DuplicateDelivery,
}

/// Compute the GitHub `sha256=<hex>` signature for a body.
#[must_use]
pub fn sign(secret: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC-SHA256 accepts any key length");
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let mut out = String::with_capacity(SIGNATURE_PREFIX.len() + DIGEST_BYTES * 2);
    out.push_str(SIGNATURE_PREFIX);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Verify `X-Hub-Signature-256` against the raw request body.
///
/// # Errors
/// Returns [`WebhookError`] when the header is absent, malformed, or does not
/// match. The variant is for logs only: [`crate::error::ApiError`] collapses all
/// of them to a bare 401 so a caller learns nothing about which check failed.
pub fn verify_signature(
    secret: &[u8],
    body: &[u8],
    header: Option<&str>,
) -> Result<(), WebhookError> {
    let header = header.ok_or(WebhookError::MissingSignature)?;
    let hex = header
        .strip_prefix(SIGNATURE_PREFIX)
        .ok_or(WebhookError::MalformedSignature)?;
    let supplied = decode_hex(hex).ok_or(WebhookError::MalformedSignature)?;
    if supplied.len() != DIGEST_BYTES {
        return Err(WebhookError::MalformedSignature);
    }

    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC-SHA256 accepts any key length");
    mac.update(body);
    let expected = mac.finalize().into_bytes();

    if bool::from(expected.as_slice().ct_eq(supplied.as_slice())) {
        Ok(())
    } else {
        Err(WebhookError::SignatureMismatch)
    }
}

/// Fixed-length lowercase/uppercase hex decode. Returns `None` on any non-hex
/// byte or an odd length, rather than silently dropping input.
#[must_use]
pub fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() & 1 == 1 {
        return None;
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(value.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        out.push(u8::try_from(high * 16 + low).ok()?);
    }
    Some(out)
}

/// # Errors
/// Returns [`WebhookError`] when the delivery header is absent or is not a UUID.
pub fn parse_delivery_id(header: Option<&str>) -> Result<Uuid, WebhookError> {
    let header = header.ok_or(WebhookError::MissingDeliveryId)?;
    Uuid::parse_str(header.trim()).map_err(|_| WebhookError::MalformedDeliveryId)
}

/// Bounded delivery-ID ledger.
///
/// In-process, exactly as `gha-clone-server` documents: it makes at-most-once
/// independent dispatch true for a single replica. Horizontal scaling requires
/// a shared durable claim first — the `webhook_deliveries` table in
/// `docs/orm-core-entities.md` is where that lands.
#[derive(Debug)]
pub struct DeliveryLedger {
    seen: VecDeque<(Uuid, u64)>,
    ttl_ms: u64,
    capacity: usize,
}

impl DeliveryLedger {
    #[must_use]
    pub fn new(ttl_ms: u64, capacity: usize) -> Self {
        Self {
            seen: VecDeque::new(),
            ttl_ms,
            capacity: capacity.max(1),
        }
    }

    /// Claim a delivery. The claim is inserted only when it is new, so a
    /// transient downstream failure remains retryable with the same delivery ID.
    ///
    /// # Errors
    /// Returns [`WebhookError::DuplicateDelivery`] when this ID is already
    /// claimed inside the retention window.
    pub fn claim(&mut self, delivery: Uuid, now_ms: u64) -> Result<(), WebhookError> {
        self.expire(now_ms);
        if self.seen.iter().any(|(seen, _)| *seen == delivery) {
            return Err(WebhookError::DuplicateDelivery);
        }
        while self.seen.len() >= self.capacity {
            self.seen.pop_front();
        }
        self.seen.push_back((delivery, now_ms));
        Ok(())
    }

    fn expire(&mut self, now_ms: u64) {
        while self
            .seen
            .front()
            .is_some_and(|(_, at)| now_ms.saturating_sub(*at) >= self.ttl_ms)
        {
            self.seen.pop_front();
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
    const BODY: &[u8] = br#"{"action":"completed"}"#;

    #[test]
    fn a_signature_produced_here_verifies_here() {
        let signature = sign(SECRET, BODY);
        assert!(signature.starts_with("sha256="));
        assert_eq!(signature.len(), "sha256=".len() + 64);
        assert_eq!(verify_signature(SECRET, BODY, Some(&signature)), Ok(()));
    }

    #[test]
    fn signature_verification_is_sensitive_to_body_and_key() {
        let signature = sign(SECRET, BODY);
        assert_eq!(
            verify_signature(SECRET, b"{}", Some(&signature)),
            Err(WebhookError::SignatureMismatch)
        );
        assert_eq!(
            verify_signature(b"another-secret-value-that-is-long", BODY, Some(&signature)),
            Err(WebhookError::SignatureMismatch)
        );
    }

    #[test]
    fn a_signature_with_the_right_length_but_wrong_bytes_is_a_mismatch_not_a_malformation() {
        let mut digest = sign(SECRET, BODY);
        // Flip the last hex nibble, keeping the length and alphabet valid.
        let last = digest.pop().expect("non-empty");
        digest.push(if last == '0' { '1' } else { '0' });
        assert_eq!(
            verify_signature(SECRET, BODY, Some(&digest)),
            Err(WebhookError::SignatureMismatch)
        );
    }

    #[test]
    fn malformed_headers_never_reach_the_comparison() {
        assert_eq!(
            verify_signature(SECRET, BODY, None),
            Err(WebhookError::MissingSignature)
        );
        for header in [
            "",
            "sha1=deadbeef",
            "sha256=",
            "sha256=zz",
            "sha256=abc",
            "sha256=00",
        ] {
            assert!(
                matches!(
                    verify_signature(SECRET, BODY, Some(header)),
                    Err(WebhookError::MalformedSignature)
                ),
                "{header}"
            );
        }
    }

    #[test]
    fn hex_decoding_is_total_and_rejects_partial_input() {
        assert_eq!(decode_hex("00ff"), Some(vec![0x00, 0xff]));
        assert_eq!(decode_hex("00FF"), Some(vec![0x00, 0xff]));
        assert_eq!(decode_hex(""), Some(Vec::new()));
        assert_eq!(decode_hex("f"), None);
        assert_eq!(decode_hex("0g"), None);
        assert_eq!(decode_hex("0 "), None);
    }

    #[test]
    fn delivery_ids_must_be_uuids() {
        let id = Uuid::new_v4();
        assert_eq!(parse_delivery_id(Some(&id.to_string())), Ok(id));
        assert_eq!(
            parse_delivery_id(None),
            Err(WebhookError::MissingDeliveryId)
        );
        assert_eq!(
            parse_delivery_id(Some("not-a-uuid")),
            Err(WebhookError::MalformedDeliveryId)
        );
    }

    #[test]
    fn the_ledger_claims_once_and_expires_by_ttl() {
        let mut ledger = DeliveryLedger::new(1_000, 16);
        let id = Uuid::new_v4();
        assert_eq!(ledger.claim(id, 0), Ok(()));
        assert_eq!(ledger.claim(id, 500), Err(WebhookError::DuplicateDelivery));
        // Past the TTL the entry is gone and the same delivery may be retried.
        assert_eq!(ledger.claim(id, 1_000), Ok(()));
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn the_ledger_is_bounded_by_entry_count() {
        let mut ledger = DeliveryLedger::new(u64::MAX, 4);
        for _ in 0..64 {
            assert_eq!(ledger.claim(Uuid::new_v4(), 0), Ok(()));
        }
        assert_eq!(ledger.len(), 4);
        assert!(!ledger.is_empty());
    }
}
