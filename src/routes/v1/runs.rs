#![forbid(unsafe_code)]

//! `/v1/runs` — enqueue and observe runs, jobs and log chunks.
//!
//! A run is only accepted for a **fully supported** plan at an **immutable
//! commit SHA**. Everything else is refused with the plan's own reasons, so a
//! caller never discovers an unsupported construct halfway through execution.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::guards::{require_scopes, SCOPE_RUNS_READ, SCOPE_RUNS_WRITE};
use crate::auth::Actor;
use crate::domain::now_rfc3339;
use crate::domain::plans::{compile, scan_workflow, JobSupport, PlanRequest, PlannedJob, Profile};
use crate::domain::runs::{
    accept_log_chunk, advance, AppendLog, Job, LifecycleEvent, LifecycleState, LogChunk, NewRun,
    Run,
};
use crate::error::ApiError;
use crate::state::{AppState, ServerEvent};

#[must_use]
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/runs", get(list_runs).post(create_run))
        .route("/runs/{run_id}", get(get_run))
        .route("/runs/{run_id}/jobs", get(list_jobs))
        .route("/runs/{run_id}/jobs/{job_id}/events", post(job_event))
        .route("/runs/{run_id}/logs", get(list_logs).post(append_log))
}

#[derive(Debug, Serialize)]
pub struct RunView {
    #[serde(flatten)]
    pub run: Run,
    pub jobs: Vec<Job>,
}

#[derive(Debug, Deserialize)]
pub struct ListRuns {
    #[serde(default = "default_limit")]
    pub limit: usize,
}

const fn default_limit() -> usize {
    50
}

/// The organisation a run belongs to comes from the token, never the body, so
/// a caller cannot enqueue work into a tenant it does not hold.
fn tenant_of(actor: &Actor) -> Result<Uuid, ApiError> {
    actor.org_id.ok_or(ApiError::Forbidden)
}

/// `POST /v1/runs`
async fn create_run(
    State(state): State<AppState>,
    actor: Actor,
    Json(input): Json<NewRun>,
) -> Result<(StatusCode, Json<RunView>), ApiError> {
    require_scopes(&actor, &[SCOPE_RUNS_WRITE])?;
    let org_id = tenant_of(&actor)?;

    let request = PlanRequest {
        repository: input.repository,
        revision: input.revision,
        workflow_path: input.workflow_path,
        workflow_yaml: input.workflow_yaml,
    };
    crate::domain::plans::validate_request(&request)?;
    let evidence = scan_workflow(&request.workflow_yaml)?;
    let plan = compile(&request, evidence)?;

    if !plan.fully_supported() {
        let blocked: Vec<String> = plan
            .unsupported()
            .iter()
            .map(|job| format!("{}: {}", job.name, describe(&job.support)))
            .collect();
        return Err(ApiError::bad_request(format!(
            "plan is not independently executable — {}",
            blocked.join("; ")
        )));
    }

    let now = now_rfc3339();
    let run = Run {
        id: Uuid::now_v7(),
        org_id,
        repository: request.repository.clone(),
        revision: request.revision.clone(),
        workflow_path: request.workflow_path.clone(),
        state: LifecycleState::Queued,
        created_at: now.clone(),
        updated_at: now.clone(),
    };

    // Jobs are created in the plan's deterministic topological order, so the
    // sequence numbers are stable for the same plan on every submission.
    let jobs: Vec<Job> = plan
        .order
        .iter()
        .enumerate()
        .filter_map(|(index, name)| {
            let planned = plan.jobs.iter().find(|job| &job.name == name)?;
            Some(Job {
                id: Uuid::now_v7(),
                run_id: run.id,
                name: planned.name.clone(),
                profile: profile_of(planned)?,
                needs: planned.needs.clone(),
                state: LifecycleState::Queued,
                sequence: u32::try_from(index).unwrap_or(u32::MAX),
                created_at: now.clone(),
                updated_at: now.clone(),
            })
        })
        .collect();

    state.store.insert_run(run.clone(), jobs.clone()).await;
    state
        .publish(ServerEvent::RunUpdated {
            run_id: run.id,
            org_id,
            state: run.state.as_str().to_owned(),
        })
        .await;

    Ok((StatusCode::CREATED, Json(RunView { run, jobs })))
}

/// `GET /v1/runs`
async fn list_runs(
    State(state): State<AppState>,
    actor: Actor,
    Query(query): Query<ListRuns>,
) -> Result<Json<Vec<Run>>, ApiError> {
    require_scopes(&actor, &[SCOPE_RUNS_READ])?;
    let org_id = tenant_of(&actor)?;
    Ok(Json(
        state.store.runs_of(org_id, query.limit.clamp(1, 200)).await,
    ))
}

/// `GET /v1/runs/{run_id}`
async fn get_run(
    State(state): State<AppState>,
    actor: Actor,
    Path(run_id): Path<Uuid>,
) -> Result<Json<RunView>, ApiError> {
    require_scopes(&actor, &[SCOPE_RUNS_READ])?;
    let run = owned_run(&state, &actor, run_id).await?;
    let jobs = state.store.jobs_of(run_id).await;
    Ok(Json(RunView { run, jobs }))
}

/// `GET /v1/runs/{run_id}/jobs`
async fn list_jobs(
    State(state): State<AppState>,
    actor: Actor,
    Path(run_id): Path<Uuid>,
) -> Result<Json<Vec<Job>>, ApiError> {
    require_scopes(&actor, &[SCOPE_RUNS_READ])?;
    owned_run(&state, &actor, run_id).await?;
    Ok(Json(state.store.jobs_of(run_id).await))
}

#[derive(Debug, Deserialize)]
pub struct JobEvent {
    #[serde(flatten)]
    pub event: LifecycleEvent,
}

/// `POST /v1/runs/{run_id}/jobs/{job_id}/events`
///
/// The only way a job's state changes. The transition is validated by the pure
/// state machine first, so a worker reporting an impossible transition gets a
/// 409 instead of corrupting the run.
async fn job_event(
    State(state): State<AppState>,
    actor: Actor,
    Path((run_id, job_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<JobEvent>,
) -> Result<Json<RunView>, ApiError> {
    require_scopes(&actor, &[SCOPE_RUNS_WRITE])?;
    let run = owned_run(&state, &actor, run_id).await?;
    let job = state.store.job(job_id).await.ok_or(ApiError::NotFound)?;
    if job.run_id != run.id {
        return Err(ApiError::NotFound);
    }

    let next = advance(job.state, input.event)?;
    let now = now_rfc3339();
    let run = state
        .store
        .apply_job_state(job_id, next, now)
        .await
        .ok_or(ApiError::NotFound)?;

    state
        .publish(ServerEvent::JobUpdated {
            run_id,
            job_id,
            state: next.as_str().to_owned(),
        })
        .await;
    state
        .publish(ServerEvent::RunUpdated {
            run_id,
            org_id: run.org_id,
            state: run.state.as_str().to_owned(),
        })
        .await;

    let jobs = state.store.jobs_of(run_id).await;
    Ok(Json(RunView { run, jobs }))
}

#[derive(Debug, Deserialize)]
pub struct ListLogs {
    pub job_id: Uuid,
    #[serde(default)]
    pub after: u64,
    #[serde(default = "default_log_limit")]
    pub limit: usize,
}

const fn default_log_limit() -> usize {
    200
}

/// `GET /v1/runs/{run_id}/logs?job_id=…&after=…`
async fn list_logs(
    State(state): State<AppState>,
    actor: Actor,
    Path(run_id): Path<Uuid>,
    Query(query): Query<ListLogs>,
) -> Result<Json<Vec<LogChunk>>, ApiError> {
    require_scopes(&actor, &[SCOPE_RUNS_READ])?;
    owned_run(&state, &actor, run_id).await?;
    Ok(Json(
        state
            .store
            .logs_of(query.job_id, query.after, query.limit.clamp(1, 1_000))
            .await,
    ))
}

/// `POST /v1/runs/{run_id}/logs`
async fn append_log(
    State(state): State<AppState>,
    actor: Actor,
    Path(run_id): Path<Uuid>,
    Json(input): Json<AppendLog>,
) -> Result<StatusCode, ApiError> {
    require_scopes(&actor, &[SCOPE_RUNS_WRITE])?;
    let run = owned_run(&state, &actor, run_id).await?;
    let job = state
        .store
        .job(input.job_id)
        .await
        .ok_or(ApiError::NotFound)?;
    if job.run_id != run.id {
        return Err(ApiError::NotFound);
    }

    let (last, retained) = state.store.last_log_sequence(input.job_id).await;
    accept_log_chunk(last, input.sequence, input.text.len(), retained)
        .map_err(|error| ApiError::conflict(error.to_string()))?;

    state
        .store
        .append_log(LogChunk {
            run_id,
            job_id: input.job_id,
            sequence: input.sequence,
            stream: input.stream,
            at: now_rfc3339(),
            text: input.text,
        })
        .await;
    state
        .publish(ServerEvent::LogAppended {
            run_id,
            job_id: input.job_id,
            sequence: input.sequence,
        })
        .await;
    Ok(StatusCode::ACCEPTED)
}

/// Load a run and prove the caller's tenant owns it. A run id from another
/// tenant is reported as **not found**, not as forbidden, so the API does not
/// confirm that an id exists.
async fn owned_run(state: &AppState, actor: &Actor, run_id: Uuid) -> Result<Run, ApiError> {
    let org_id = tenant_of(actor)?;
    let run = state.store.run(run_id).await.ok_or(ApiError::NotFound)?;
    if run.org_id == org_id {
        Ok(run)
    } else {
        Err(ApiError::NotFound)
    }
}

fn profile_of(planned: &PlannedJob) -> Option<Profile> {
    match planned.support {
        JobSupport::Independent { profile } => Some(profile),
        JobSupport::DelegatedToArc { .. } | JobSupport::Unsupported { .. } => None,
    }
}

fn describe(support: &JobSupport) -> String {
    match support {
        JobSupport::Independent { profile } => profile.as_str().to_owned(),
        JobSupport::DelegatedToArc { lane } => format!("delegated to {lane}"),
        JobSupport::Unsupported { reason } => reason.as_str().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plans::Exclusion;

    #[test]
    fn only_independent_jobs_carry_a_profile() {
        let independent = PlannedJob {
            name: "test".to_owned(),
            needs: Vec::new(),
            support: JobSupport::Independent {
                profile: Profile::RustVerify,
            },
        };
        assert_eq!(profile_of(&independent), Some(Profile::RustVerify));

        let delegated = PlannedJob {
            name: "ios".to_owned(),
            needs: Vec::new(),
            support: JobSupport::DelegatedToArc {
                lane: "github-hosted-native".to_owned(),
            },
        };
        assert_eq!(profile_of(&delegated), None);

        let unsupported = PlannedJob {
            name: "deploy".to_owned(),
            needs: Vec::new(),
            support: JobSupport::Unsupported {
                reason: Exclusion::SecretExpression,
            },
        };
        assert_eq!(profile_of(&unsupported), None);
    }

    #[test]
    fn blockers_are_described_in_terms_the_caller_can_act_on() {
        assert_eq!(
            describe(&JobSupport::Unsupported {
                reason: Exclusion::DynamicMatrix
            }),
            "dynamic_matrix"
        );
        assert_eq!(
            describe(&JobSupport::DelegatedToArc {
                lane: "github-hosted-native".to_owned()
            }),
            "delegated to github-hosted-native"
        );
        assert_eq!(
            describe(&JobSupport::Independent {
                profile: Profile::NodeVerify
            }),
            "node-verify"
        );
    }

    #[test]
    fn a_job_event_request_carries_its_event_inline() {
        let request: JobEvent = serde_json::from_str(r#"{"event":"start"}"#).expect("deserialise");
        assert_eq!(request.event, LifecycleEvent::Start);
        assert!(serde_json::from_str::<JobEvent>(r#"{"event":"delete"}"#).is_err());
    }

    #[test]
    fn log_queries_default_to_the_head_of_the_stream() {
        let query: ListLogs =
            serde_json::from_str(r#"{"job_id":"00000000-0000-0000-0000-000000000001"}"#)
                .expect("deserialise");
        assert_eq!(query.after, 0);
        assert_eq!(query.limit, 200);
    }
}
