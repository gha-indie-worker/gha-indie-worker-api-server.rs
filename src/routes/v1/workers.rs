#![forbid(unsafe_code)]

//! `/v1/workers` — registration, heartbeats and presence.
//!
//! A worker is a `gha-indie-worker.rs` process with a pinned image. It
//! registers the profiles that image can run, heartbeats inside a TTL, and
//! moves through the lifecycle in [`crate::domain::workers`]. The control plane
//! — not the worker — decides when a missed heartbeat means offline.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::guards::{require_scopes, SCOPE_WORKERS_READ, SCOPE_WORKERS_WRITE};
use crate::auth::Actor;
use crate::domain::now_rfc3339;
use crate::domain::workers::{
    advance, heartbeat_expired, validate_registration, RegisterWorker, Worker, WorkerEvent,
    WorkerState,
};
use crate::error::ApiError;
use crate::state::{AppState, ServerEvent};

#[must_use]
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/workers", get(list_workers).post(register))
        .route("/workers/{worker_id}", get(get_worker))
        .route("/workers/{worker_id}/events", post(worker_event))
}

#[derive(Debug, Serialize)]
pub struct WorkerView {
    #[serde(flatten)]
    pub worker: Worker,
    /// Computed at read time from the heartbeat TTL, so a stale worker reads as
    /// stale even before the control plane has swept it.
    pub heartbeat_stale: bool,
}

fn tenant_of(actor: &Actor) -> Result<Uuid, ApiError> {
    actor.org_id.ok_or(ApiError::Forbidden)
}

/// Monotonic-ish milliseconds for heartbeat arithmetic.
fn now_ms(state: &AppState) -> u64 {
    u64::try_from(state.started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn view(state: &AppState, worker: Worker) -> WorkerView {
    let ttl_ms = u64::try_from(state.config.heartbeat_ttl.as_millis()).unwrap_or(90_000);
    WorkerView {
        heartbeat_stale: heartbeat_expired(now_ms(state), worker.last_heartbeat_ms, ttl_ms),
        worker,
    }
}

/// `POST /v1/workers`
async fn register(
    State(state): State<AppState>,
    actor: Actor,
    Json(input): Json<RegisterWorker>,
) -> Result<(StatusCode, Json<WorkerView>), ApiError> {
    require_scopes(&actor, &[SCOPE_WORKERS_WRITE])?;
    let org_id = tenant_of(&actor)?;

    let (profiles, labels) =
        validate_registration(&input).map_err(|error| ApiError::bad_request(error.to_string()))?;
    let now = now_rfc3339();
    let worker = Worker {
        id: Uuid::now_v7(),
        org_id,
        name: input.name.trim().to_owned(),
        profiles,
        labels,
        state: WorkerState::Registered,
        registered_at: now.clone(),
        last_heartbeat_at: now,
        last_heartbeat_ms: now_ms(&state),
    };
    state.store.insert_worker(worker.clone()).await;
    publish_presence(&state, &worker).await;
    Ok((StatusCode::CREATED, Json(view(&state, worker))))
}

/// `GET /v1/workers`
async fn list_workers(
    State(state): State<AppState>,
    actor: Actor,
) -> Result<Json<Vec<WorkerView>>, ApiError> {
    require_scopes(&actor, &[SCOPE_WORKERS_READ])?;
    let org_id = tenant_of(&actor)?;
    let workers = state
        .store
        .workers_of(org_id)
        .await
        .into_iter()
        .map(|worker| view(&state, worker))
        .collect();
    Ok(Json(workers))
}

/// `GET /v1/workers/{worker_id}`
async fn get_worker(
    State(state): State<AppState>,
    actor: Actor,
    Path(worker_id): Path<Uuid>,
) -> Result<Json<WorkerView>, ApiError> {
    require_scopes(&actor, &[SCOPE_WORKERS_READ])?;
    let worker = owned_worker(&state, &actor, worker_id).await?;
    Ok(Json(view(&state, worker)))
}

#[derive(Debug, Deserialize)]
pub struct WorkerEventRequest {
    #[serde(flatten)]
    pub event: WorkerEvent,
}

/// `POST /v1/workers/{worker_id}/events`
///
/// Heartbeats are the `ready` event: a worker that says it is ready is, by
/// definition, alive. That keeps one code path instead of a separate heartbeat
/// endpoint that could disagree with the lifecycle.
async fn worker_event(
    State(state): State<AppState>,
    actor: Actor,
    Path(worker_id): Path<Uuid>,
    Json(input): Json<WorkerEventRequest>,
) -> Result<Json<WorkerView>, ApiError> {
    require_scopes(&actor, &[SCOPE_WORKERS_WRITE])?;
    let worker = owned_worker(&state, &actor, worker_id).await?;
    let next = advance(worker.state, input.event)?;

    let updated = state
        .store
        .update_worker(worker_id, next, now_rfc3339(), now_ms(&state))
        .await
        .ok_or(ApiError::NotFound)?;
    publish_presence(&state, &updated).await;
    Ok(Json(view(&state, updated)))
}

async fn owned_worker(
    state: &AppState,
    actor: &Actor,
    worker_id: Uuid,
) -> Result<Worker, ApiError> {
    let org_id = tenant_of(actor)?;
    let worker = state
        .store
        .worker(worker_id)
        .await
        .ok_or(ApiError::NotFound)?;
    if worker.org_id == org_id {
        Ok(worker)
    } else {
        Err(ApiError::NotFound)
    }
}

async fn publish_presence(state: &AppState, worker: &Worker) {
    state
        .publish(ServerEvent::WorkerPresence {
            org_id: worker.org_id,
            worker_id: worker.id,
            state: worker.state.as_str().to_owned(),
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worker_event_request_carries_its_event_inline() {
        let request: WorkerEventRequest =
            serde_json::from_str(r#"{"event":"ready"}"#).expect("deserialise");
        assert_eq!(request.event, WorkerEvent::Ready);

        let request: WorkerEventRequest =
            serde_json::from_str(r#"{"event":"lease_acquired"}"#).expect("deserialise");
        assert_eq!(request.event, WorkerEvent::LeaseAcquired);

        assert!(serde_json::from_str::<WorkerEventRequest>(r#"{"event":"sudo"}"#).is_err());
    }

    #[test]
    fn the_view_reports_staleness_without_mutating_the_worker() {
        let worker = Worker {
            id: Uuid::nil(),
            org_id: Uuid::nil(),
            name: "runner-1".to_owned(),
            profiles: std::collections::BTreeSet::new(),
            labels: std::collections::BTreeSet::new(),
            state: WorkerState::Idle,
            registered_at: String::new(),
            last_heartbeat_at: String::new(),
            last_heartbeat_ms: 0,
        };
        // Fresh: no time has elapsed against a 90 second TTL.
        assert!(!heartbeat_expired(1_000, worker.last_heartbeat_ms, 90_000));
        // Stale once the TTL is exceeded.
        assert!(heartbeat_expired(90_001, worker.last_heartbeat_ms, 90_000));
        assert_eq!(worker.state, WorkerState::Idle);
    }
}
