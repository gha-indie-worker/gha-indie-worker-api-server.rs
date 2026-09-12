#![forbid(unsafe_code)]

//! `POST /v1/webhooks/github` — GitHub's `workflow_run` continuity hook.
//!
//! This route authenticates **itself**: GitHub cannot present a bearer, so the
//! request carries an HMAC over the raw body instead. That is why it is the one
//! `/v1` route without an [`crate::auth::Actor`] extractor.
//!
//! Order of checks, all before anything is written:
//!
//! 1. verification is configured at all (otherwise 503, not "accepted");
//! 2. `X-Hub-Signature-256` matches the raw body, compared in constant time;
//! 3. `X-GitHub-Delivery` is a UUID;
//! 4. the delivery has not already been claimed inside the retention window.
//!
//! The claim is inserted **after** the payload is understood, so a transient
//! downstream failure stays retryable with the same delivery ID.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::domain::webhooks::{parse_delivery_id, verify_signature, WebhookError};
use crate::domain::{is_commit_sha, is_repository_slug};
use crate::error::ApiError;
use crate::state::AppState;

pub const SIGNATURE_HEADER: &str = "x-hub-signature-256";
pub const DELIVERY_HEADER: &str = "x-github-delivery";
pub const EVENT_HEADER: &str = "x-github-event";

#[must_use]
pub fn router() -> Router<AppState> {
    Router::new().route("/webhooks/github", post(github))
}

/// The bounded subset of a `workflow_run` payload this service reads.
#[derive(Debug, Deserialize)]
pub struct WorkflowRunEvent {
    pub action: String,
    pub workflow_run: WorkflowRun,
    pub repository: Repository,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowRun {
    pub name: String,
    pub path: String,
    pub head_sha: String,
    #[serde(default)]
    pub conclusion: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Repository {
    pub full_name: String,
}

#[derive(Debug, Serialize)]
pub struct Accepted {
    pub accepted: bool,
    pub delivery: String,
    /// Why the delivery was accepted but not acted on, when that is the case.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignored_because: Option<&'static str>,
}

/// Conclusions that make a failed GitHub run eligible for the independent lane.
pub const FAILURE_CONCLUSIONS: [&str; 3] = ["failure", "timed_out", "cancelled"];

/// `POST /v1/webhooks/github`
async fn github(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Accepted>), ApiError> {
    let secret = state
        .config
        .webhooks
        .github_secret
        .as_deref()
        .ok_or(WebhookError::NotConfigured)?;

    verify_signature(secret.as_bytes(), &body, header(&headers, SIGNATURE_HEADER))?;
    let delivery = parse_delivery_id(header(&headers, DELIVERY_HEADER))?;

    // Only `workflow_run` is understood. Anything else is acknowledged so
    // GitHub stops retrying, and explicitly reported as ignored.
    if header(&headers, EVENT_HEADER) != Some("workflow_run") {
        return Ok(ignored(delivery, "event is not workflow_run"));
    }

    let event: WorkflowRunEvent = serde_json::from_slice(&body)
        .map_err(|_| ApiError::bad_request("payload is not a workflow_run event"))?;

    if let Some(reason) = ineligible(&event) {
        return Ok(ignored(delivery, reason));
    }

    // The claim lands only once the payload is understood and eligible, so a
    // transient failure before this point can be retried with the same id.
    let now_ms = u64::try_from(state.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    state.store.claim_delivery(delivery, now_ms).await?;

    tracing::info!(
        delivery = %delivery,
        repository = %event.repository.full_name,
        workflow = %event.workflow_run.path,
        "accepted a workflow_run continuity delivery"
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(Accepted {
            accepted: true,
            delivery: delivery.to_string(),
            ignored_because: None,
        }),
    ))
}

/// Every reason a well-formed delivery is still not actionable. Pure, so the
/// eligibility rules are tested without a signature or a server.
#[must_use]
pub fn ineligible(event: &WorkflowRunEvent) -> Option<&'static str> {
    if event.action != "completed" {
        return Some("workflow_run has not completed");
    }
    let conclusion = event.workflow_run.conclusion.as_deref().unwrap_or_default();
    if !FAILURE_CONCLUSIONS.contains(&conclusion) {
        return Some("conclusion is not eligible for the continuity lane");
    }
    if !is_repository_slug(&event.repository.full_name) {
        return Some("repository is not a plain owner/repo slug");
    }
    if !is_commit_sha(&event.workflow_run.head_sha) {
        return Some("head_sha is not a 40-hex commit SHA");
    }
    if crate::domain::plans::validate_workflow_path(&event.workflow_run.path).is_err() {
        return Some("workflow path is outside .github/workflows");
    }
    // Recursion guard: this service's own continuity workflow must never
    // re-trigger the continuity lane.
    if event
        .workflow_run
        .name
        .to_ascii_lowercase()
        .contains("continuity")
        || event.workflow_run.path.contains("gha-clone")
    {
        return Some("workflow is in the recursion-exclusion set");
    }
    None
}

fn ignored(delivery: uuid::Uuid, reason: &'static str) -> (StatusCode, Json<Accepted>) {
    (
        StatusCode::ACCEPTED,
        Json(Accepted {
            accepted: false,
            delivery: delivery.to_string(),
            ignored_because: Some(reason),
        }),
    )
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(action: &str, conclusion: &str) -> WorkflowRunEvent {
        WorkflowRunEvent {
            action: action.to_owned(),
            workflow_run: WorkflowRun {
                name: "ci".to_owned(),
                path: ".github/workflows/ci.yml".to_owned(),
                head_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
                conclusion: Some(conclusion.to_owned()),
            },
            repository: Repository {
                full_name: "gha-indie-worker/gha-indie-worker.rs".to_owned(),
            },
        }
    }

    #[test]
    fn a_completed_failure_on_an_exact_path_is_eligible() {
        assert_eq!(ineligible(&event("completed", "failure")), None);
        assert_eq!(ineligible(&event("completed", "timed_out")), None);
    }

    #[test]
    fn a_successful_or_in_progress_run_is_not_acted_on() {
        assert!(ineligible(&event("completed", "success")).is_some());
        assert!(ineligible(&event("requested", "failure")).is_some());
        let mut missing = event("completed", "failure");
        missing.workflow_run.conclusion = None;
        assert!(ineligible(&missing).is_some());
    }

    #[test]
    fn a_mutable_revision_or_stray_path_is_refused() {
        let mut branch = event("completed", "failure");
        branch.workflow_run.head_sha = "main".to_owned();
        assert_eq!(
            ineligible(&branch),
            Some("head_sha is not a 40-hex commit SHA")
        );

        let mut traversal = event("completed", "failure");
        traversal.workflow_run.path = ".github/workflows/../../evil.yml".to_owned();
        assert_eq!(
            ineligible(&traversal),
            Some("workflow path is outside .github/workflows")
        );

        let mut host = event("completed", "failure");
        host.repository.full_name = "https://evil.example/a/b".to_owned();
        assert_eq!(
            ineligible(&host),
            Some("repository is not a plain owner/repo slug")
        );
    }

    #[test]
    fn the_recursion_exclusion_set_stops_a_feedback_loop() {
        let mut named = event("completed", "failure");
        named.workflow_run.name = "GHA Continuity".to_owned();
        assert_eq!(
            ineligible(&named),
            Some("workflow is in the recursion-exclusion set")
        );

        let mut pathed = event("completed", "failure");
        pathed.workflow_run.path = ".github/workflows/gha-clone-meta.yml".to_owned();
        assert_eq!(
            ineligible(&pathed),
            Some("workflow is in the recursion-exclusion set")
        );
    }

    #[test]
    fn an_ignored_delivery_is_acknowledged_with_its_reason() {
        let (status, Json(body)) = ignored(uuid::Uuid::nil(), "event is not workflow_run");
        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(!body.accepted);
        assert_eq!(body.ignored_because, Some("event is not workflow_run"));
    }

    #[test]
    fn a_signature_failure_never_becomes_a_500() {
        let error: ApiError = WebhookError::SignatureMismatch.into();
        assert_eq!(error.status(), StatusCode::UNAUTHORIZED);
        let error: ApiError = WebhookError::NotConfigured.into();
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
