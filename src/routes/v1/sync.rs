#![forbid(unsafe_code)]

//! `/v1/sync/*` — the opto-sync boundary.
//!
//! The server is a causally ordered log, not the authority on client state.
//! A client pushes envelopes and pulls everything after a cursor; merging is
//! idempotent and commutative, so a retry after a dropped connection converges
//! rather than duplicating.
//!
//! The actor on every pushed envelope is **overwritten** with the verified
//! subject, so a client cannot attribute an envelope to somebody else.

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::guards::{require_scopes, SCOPE_SYNC};
use crate::auth::Actor;
use crate::domain::sync::{validate_push, PullQuery, PullResponse, PushRequest, PushResponse};
use crate::error::ApiError;
use crate::state::AppState;

#[must_use]
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/sync/push", post(push))
        .route("/sync/pull", get(pull))
}

/// `POST /v1/sync/push`
async fn push(
    State(state): State<AppState>,
    actor: Actor,
    Json(mut request): Json<PushRequest>,
) -> Result<Json<PushResponse>, ApiError> {
    require_scopes(&actor, &[SCOPE_SYNC])?;
    validate_push(&request.envelopes).map_err(|error| ApiError::bad_request(error.to_string()))?;

    // The client does not get to choose who an envelope came from.
    for envelope in &mut request.envelopes {
        envelope.actor.clone_from(&actor.subject);
    }

    let (accepted, duplicates) = state.store.push_sync(request.envelopes).await;
    Ok(Json(PushResponse {
        accepted,
        duplicates,
        clock: state.store.sync_clock().await,
    }))
}

/// `GET /v1/sync/pull?since=…&limit=…`
async fn pull(
    State(state): State<AppState>,
    actor: Actor,
    Query(query): Query<PullQuery>,
) -> Result<Json<PullResponse>, ApiError> {
    require_scopes(&actor, &[SCOPE_SYNC])?;
    Ok(Json(PullResponse {
        envelopes: state.store.pull_sync(query.since, query.limit).await,
        clock: state.store.sync_clock().await,
    }))
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::domain::sync::{CausalEnvelope, MAX_ENVELOPES_PER_PUSH};

    fn envelope(actor: &str) -> CausalEnvelope {
        CausalEnvelope {
            id: Uuid::from_u128(1),
            actor: actor.to_owned(),
            lamport: 1,
            kind: "run.updated".to_owned(),
            payload: serde_json::Value::Null,
            at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn a_pull_cursor_defaults_to_the_start_of_the_log() {
        let query: PullQuery = serde_json::from_str("{}").expect("deserialise");
        assert_eq!(query.since, 0);
        assert_eq!(query.limit, MAX_ENVELOPES_PER_PUSH);
    }

    #[test]
    fn a_client_supplied_actor_is_overwritten_not_trusted() {
        let mut envelopes = vec![envelope("somebody-else")];
        let subject = "sub-1".to_owned();
        for envelope in &mut envelopes {
            envelope.actor.clone_from(&subject);
        }
        assert_eq!(envelopes[0].actor, "sub-1");
    }

    #[test]
    fn an_oversized_or_empty_push_is_refused_before_any_write() {
        assert!(validate_push(&[]).is_err());
        let too_many: Vec<CausalEnvelope> = (0..=MAX_ENVELOPES_PER_PUSH)
            .map(|_| envelope("sub-1"))
            .collect();
        assert!(validate_push(&too_many).is_err());
    }
}
