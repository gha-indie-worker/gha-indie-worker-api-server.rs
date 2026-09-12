#![forbid(unsafe_code)]

//! `POST /v1/plans` — compile a workflow to fixed profiles without executing it.
//!
//! Planning is always safe: it reads a document and reports a classification.
//! It is the only way to learn *why* a workflow is not independently executable
//! before submitting it to `POST /v1/runs`, which refuses anything less than a
//! fully supported plan.

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;

use crate::auth::guards::{require_scopes, SCOPE_PLANS_WRITE};
use crate::auth::Actor;
use crate::domain::plans::{compile, scan_workflow, Plan, PlanRequest, PlannedJob};
use crate::error::ApiError;
use crate::state::AppState;

#[must_use]
pub fn router() -> Router<AppState> {
    Router::new().route("/plans", post(create_plan))
}

#[derive(Debug, Serialize)]
pub struct PlanResponse {
    #[serde(flatten)]
    pub plan: Plan,
    /// True only when every job maps to an operator-reviewed profile.
    pub executable: bool,
    /// The jobs blocking execution, with the reason for each.
    pub blocked: Vec<PlannedJob>,
}

async fn create_plan(
    State(state): State<AppState>,
    actor: Actor,
    Json(request): Json<PlanRequest>,
) -> Result<Json<PlanResponse>, ApiError> {
    require_scopes(&actor, &[SCOPE_PLANS_WRITE])?;
    Ok(Json(plan(&request)?))
}

/// The pure half of the route, so it is testable without a server.
///
/// # Errors
/// Returns [`ApiError::BadRequest`] for any [`crate::domain::plans::PlanError`].
pub fn plan(request: &PlanRequest) -> Result<PlanResponse, ApiError> {
    crate::domain::plans::validate_request(request)?;
    let jobs = scan_workflow(&request.workflow_yaml)?;
    let plan = compile(request, jobs)?;
    let blocked = plan.unsupported().into_iter().cloned().collect();
    Ok(PlanResponse {
        executable: plan.fully_supported(),
        blocked,
        plan,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plans::{Exclusion, JobSupport, Profile};

    fn request(yaml: &str) -> PlanRequest {
        PlanRequest {
            repository: "gha-indie-worker/gha-indie-worker.rs".to_owned(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            workflow_path: ".github/workflows/ci.yml".to_owned(),
            workflow_yaml: yaml.to_owned(),
        }
    }

    #[test]
    fn a_supported_workflow_plans_as_executable() {
        let response = plan(&request(
            "jobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: cargo test\n",
        ))
        .expect("plans");
        assert!(response.executable);
        assert!(response.blocked.is_empty());
        assert_eq!(
            response.plan.jobs[0].support,
            JobSupport::Independent {
                profile: Profile::RustVerify
            }
        );
    }

    #[test]
    fn an_unsupported_workflow_plans_but_reports_every_blocker() {
        let response = plan(&request(
            "jobs:\n  test:\n    runs-on: ubuntu-latest\n    steps:\n      - run: cargo test\n  \
             release:\n    runs-on: macos-14\n    steps:\n      - run: flutter build ios\n",
        ))
        .expect("plans");
        assert!(!response.executable);
        assert_eq!(response.blocked.len(), 1);
        assert_eq!(response.blocked[0].name, "release");
    }

    #[test]
    fn a_workflow_with_no_recognised_evidence_is_reported_not_guessed() {
        let response = plan(&request(
            "jobs:\n  mystery:\n    runs-on: ubuntu-latest\n    steps:\n      - run: ./do-the-thing\n",
        ))
        .expect("plans");
        assert!(!response.executable);
        assert_eq!(
            response.blocked[0].support,
            JobSupport::Unsupported {
                reason: Exclusion::NoRecognisedEvidence
            }
        );
    }

    #[test]
    fn a_mutable_revision_is_refused_before_anything_is_parsed() {
        let mut bad = request("jobs:\n  test:\n    steps:\n      - run: cargo test\n");
        bad.revision = "main".to_owned();
        assert!(matches!(plan(&bad), Err(ApiError::BadRequest(_))));
    }

    #[test]
    fn a_workflow_without_jobs_is_a_bad_request() {
        assert!(matches!(
            plan(&request("name: ci\non:\n  push:\n")),
            Err(ApiError::BadRequest(_))
        ));
    }
}
