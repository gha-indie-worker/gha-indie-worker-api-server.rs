#![forbid(unsafe_code)]

//! opto-sync boundary: causal envelopes pushed and pulled over `/v1/sync/*`.
//!
//! The server is not the authority on client state; it is a causally ordered
//! log. Each envelope carries a Lamport clock and an actor, and merging is a
//! pure, commutative, idempotent fold — so a client that retries a push, or
//! pushes out of order, converges to the same log.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Largest envelope payload accepted in one push.
pub const MAX_ENVELOPE_BYTES: usize = 64 * 1024;
/// Largest number of envelopes in one push, and the pull page size.
pub const MAX_ENVELOPES_PER_PUSH: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CausalEnvelope {
    pub id: Uuid,
    /// The authenticated actor's subject at push time. Servers never trust a
    /// client-supplied actor: the route overwrites this field.
    pub actor: String,
    /// Per-actor Lamport clock. Monotonic within one actor, unordered across.
    pub lamport: u64,
    pub kind: String,
    pub payload: serde_json::Value,
    pub at: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PushRequest {
    pub envelopes: Vec<CausalEnvelope>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PushResponse {
    pub accepted: usize,
    pub duplicates: usize,
    /// The merged per-actor clock after this push.
    pub clock: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PullQuery {
    /// Return envelopes strictly after this Lamport value for each actor.
    #[serde(default)]
    pub since: u64,
    #[serde(default = "default_pull_limit")]
    pub limit: usize,
}

const fn default_pull_limit() -> usize {
    MAX_ENVELOPES_PER_PUSH
}

#[derive(Clone, Debug, Serialize)]
pub struct PullResponse {
    pub envelopes: Vec<CausalEnvelope>,
    pub clock: BTreeMap<String, u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum SyncError {
    #[error("a push may carry between 1 and 256 envelopes")]
    InvalidBatch,
    #[error("an envelope payload exceeds 65536 bytes")]
    EnvelopeTooLarge,
    #[error("envelope kind must be 1-64 characters of [A-Za-z0-9._-]")]
    InvalidKind,
}

/// Validate a push batch.
///
/// # Errors
/// Returns [`SyncError`] when the batch is empty/oversized, an envelope is too
/// large, or a `kind` is not a bounded portable identifier.
pub fn validate_push(envelopes: &[CausalEnvelope]) -> Result<(), SyncError> {
    if envelopes.is_empty() || envelopes.len() > MAX_ENVELOPES_PER_PUSH {
        return Err(SyncError::InvalidBatch);
    }
    for envelope in envelopes {
        if !super::is_portable_identifier(&envelope.kind, 64) {
            return Err(SyncError::InvalidKind);
        }
        let encoded =
            serde_json::to_vec(&envelope.payload).map_err(|_| SyncError::EnvelopeTooLarge)?;
        if encoded.len() > MAX_ENVELOPE_BYTES {
            return Err(SyncError::EnvelopeTooLarge);
        }
    }
    Ok(())
}

/// Merge a push into an existing log. Returns `(accepted, duplicates)`.
///
/// Idempotent by envelope id and commutative in arrival order: the resulting
/// log is sorted by `(lamport, actor, id)`, so two replicas that receive the
/// same envelopes in different orders hold byte-identical logs.
pub fn merge(log: &mut Vec<CausalEnvelope>, incoming: Vec<CausalEnvelope>) -> (usize, usize) {
    let mut accepted = 0;
    let mut duplicates = 0;
    for envelope in incoming {
        if log.iter().any(|existing| existing.id == envelope.id) {
            duplicates += 1;
            continue;
        }
        log.push(envelope);
        accepted += 1;
    }
    log.sort_by(|a, b| {
        a.lamport
            .cmp(&b.lamport)
            .then_with(|| a.actor.cmp(&b.actor))
            .then_with(|| a.id.cmp(&b.id))
    });
    (accepted, duplicates)
}

/// The per-actor high-water mark of a log.
#[must_use]
pub fn clock(log: &[CausalEnvelope]) -> BTreeMap<String, u64> {
    let mut clock = BTreeMap::new();
    for envelope in log {
        let entry = clock.entry(envelope.actor.clone()).or_insert(0);
        *entry = (*entry).max(envelope.lamport);
    }
    clock
}

/// Everything strictly after `since`, bounded by `limit`.
#[must_use]
pub fn since(log: &[CausalEnvelope], since: u64, limit: usize) -> Vec<CausalEnvelope> {
    log.iter()
        .filter(|envelope| envelope.lamport > since)
        .take(limit.clamp(1, MAX_ENVELOPES_PER_PUSH))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(id: u128, actor: &str, lamport: u64) -> CausalEnvelope {
        CausalEnvelope {
            id: Uuid::from_u128(id),
            actor: actor.to_owned(),
            lamport,
            kind: "run.updated".to_owned(),
            payload: serde_json::json!({ "n": lamport }),
            at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn pushes_are_bounded_and_kinds_are_validated() {
        assert_eq!(validate_push(&[]), Err(SyncError::InvalidBatch));
        assert_eq!(validate_push(&[envelope(1, "a", 1)]), Ok(()));

        let too_many: Vec<CausalEnvelope> = (0..=MAX_ENVELOPES_PER_PUSH as u128)
            .map(|index| envelope(index, "a", 1))
            .collect();
        assert_eq!(validate_push(&too_many), Err(SyncError::InvalidBatch));

        let mut bad = envelope(1, "a", 1);
        bad.kind = "run updated".to_owned();
        assert_eq!(validate_push(&[bad]), Err(SyncError::InvalidKind));
    }

    #[test]
    fn merging_is_idempotent() {
        let mut log = Vec::new();
        let batch = vec![envelope(1, "a", 1), envelope(2, "a", 2)];
        assert_eq!(merge(&mut log, batch.clone()), (2, 0));
        assert_eq!(merge(&mut log, batch), (0, 2));
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn merging_is_commutative_in_arrival_order() {
        let forward = {
            let mut log = Vec::new();
            merge(&mut log, vec![envelope(1, "a", 1)]);
            merge(&mut log, vec![envelope(2, "b", 2), envelope(3, "a", 2)]);
            log
        };
        let reverse = {
            let mut log = Vec::new();
            merge(&mut log, vec![envelope(3, "a", 2), envelope(2, "b", 2)]);
            merge(&mut log, vec![envelope(1, "a", 1)]);
            log
        };
        assert_eq!(forward, reverse);
        assert_eq!(forward[0].lamport, 1);
        // Equal Lamport values break the tie by actor, then id.
        assert_eq!(forward[1].actor, "a");
        assert_eq!(forward[2].actor, "b");
    }

    #[test]
    fn the_clock_is_the_per_actor_high_water_mark() {
        let mut log = Vec::new();
        merge(
            &mut log,
            vec![
                envelope(1, "a", 1),
                envelope(2, "a", 7),
                envelope(3, "b", 3),
            ],
        );
        let clock = clock(&log);
        assert_eq!(clock.get("a"), Some(&7));
        assert_eq!(clock.get("b"), Some(&3));
        assert_eq!(clock.get("c"), None);
    }

    #[test]
    fn pulls_are_exclusive_of_the_cursor_and_bounded() {
        let mut log = Vec::new();
        merge(
            &mut log,
            (1..=10_u128)
                .map(|index| envelope(index, "a", index as u64))
                .collect(),
        );
        let page = since(&log, 5, 100);
        assert_eq!(page.len(), 5);
        assert_eq!(page[0].lamport, 6);
        assert_eq!(since(&log, 0, 3).len(), 3);
        assert_eq!(since(&log, 10, 100).len(), 0);
        // A zero limit is clamped up rather than returning nothing forever.
        assert_eq!(since(&log, 0, 0).len(), 1);
    }
}
