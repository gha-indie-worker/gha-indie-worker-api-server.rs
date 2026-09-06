//! HTTP surface tests.
//!
//! These drive the real router with `tower::ServiceExt::oneshot`, so routing,
//! extractors, error rendering and the auth boundary are all exercised — but no
//! port is bound and no network call is made. Every dependency is unconfigured,
//! which is exactly the state a fresh checkout runs in.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gha_indie_worker_api_server::config::ApiConfig;
use gha_indie_worker_api_server::server;
use gha_indie_worker_api_server::state::AppState;
use http_body_util::BodyExt as _;
use serde_json::Value;
use tower::ServiceExt as _;

async fn app() -> axum::Router {
    let config = ApiConfig::from_lookup(|_| None).expect("defaults parse");
    let state = AppState::build(config).await.expect("state builds");
    server::router(state)
}

async fn get(path: &str) -> (StatusCode, Value) {
    send(
        Request::builder()
            .uri(path)
            .body(Body::empty())
            .expect("request"),
    )
    .await
}

async fn send(request: Request<Body>) -> (StatusCode, Value) {
    let response = app()
        .await
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test]
async fn healthz_reports_the_service_without_touching_a_dependency() {
    let (status, body) = get("/healthz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["service"], "gha-indie-worker-api-server");
    assert_eq!(body["protocol"], "giw.v1");
    assert!(body["uptime_seconds"].is_number());
}

#[tokio::test]
async fn readyz_is_fail_closed_without_a_database() {
    let (status, body) = get("/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], "not_ready");
    assert_eq!(body["database_ready"], false);
    assert!(
        body["reason"].is_string(),
        "a not-ready answer must say why: {body}"
    );
}

#[tokio::test]
async fn capabilities_is_reachable_without_a_bearer() {
    // A client needs this before it can choose an auth flow, so it must not
    // require one.
    let (status, body) = get("/v1/capabilities").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["service"], "gha-indie-worker-api-server");

    let profiles = body["profiles"].as_array().expect("profiles is an array");
    assert!(profiles.iter().any(|value| value == "rust-verify"));

    let scopes = body["scopes"].as_array().expect("scopes is an array");
    assert!(scopes.iter().any(|value| value == "runs:write"));

    let exclusions = body["exclusions"]
        .as_array()
        .expect("exclusions is an array");
    assert!(!exclusions.is_empty(), "refusals must be published");

    assert_eq!(body["embeddings"]["vector_slots"], 4100);
    assert_eq!(body["carriers"]["websocket_protocol"], "giw.ws.v1");
}

#[tokio::test]
async fn an_authenticated_route_refuses_a_request_with_no_bearer() {
    let (status, body) = get("/v1/users/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["title"], "unauthenticated");
    assert_eq!(body["status"], 401);
}

#[tokio::test]
async fn a_malformed_authorization_header_is_also_unauthorized() {
    for header in ["", "Basic abc", "bearer lower", "Bearer "] {
        let request = Request::builder()
            .uri("/v1/runs")
            .header("authorization", header)
            .body(Body::empty())
            .expect("request");
        let (status, _) = send(request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{header:?}");
    }
}

#[tokio::test]
async fn a_bearer_no_authority_recognises_is_refused_not_accepted() {
    // Nothing is configured, so there is no authority at all. The answer must
    // still be a refusal, never a pass-through.
    let request = Request::builder()
        .uri("/v1/users/me")
        .header("authorization", "Bearer some-token-that-nobody-issued")
        .body(Body::empty())
        .expect("request");
    let (status, _) = send(request).await;
    assert!(
        status == StatusCode::UNAUTHORIZED || status == StatusCode::SERVICE_UNAVAILABLE,
        "an unverifiable bearer must never be accepted (got {status})"
    );
}

#[tokio::test]
async fn an_unknown_path_returns_the_same_problem_shape_as_every_other_error() {
    let (status, body) = get("/v1/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["title"], "not_found");
    assert_eq!(body["status"], 404);
    assert!(body["type"]
        .as_str()
        .is_some_and(|value| value.starts_with("urn:gha-indie-worker:api:")));
}

#[tokio::test]
async fn metrics_is_prometheus_text_not_json() {
    let response = app()
        .await
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("the router answers");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(content_type.starts_with("text/plain"), "{content_type}");

    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body collects")
        .to_bytes();
    let body = String::from_utf8(bytes.to_vec()).expect("utf-8");
    assert!(body.contains("giw_api_up 1"));
    assert!(body.contains("giw_api_dependency_configured{dependency=\"database\"} 0"));
}

#[tokio::test]
async fn the_github_webhook_refuses_a_request_it_cannot_verify() {
    // No secret is configured, so verification is unavailable — and an
    // unverifiable delivery is never "accepted".
    let request = Request::builder()
        .method("POST")
        .uri("/v1/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", "workflow_run")
        .header("x-github-delivery", "00000000-0000-0000-0000-000000000001")
        .body(Body::from("{}"))
        .expect("request");
    let (status, _) = send(request).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn a_plan_request_without_a_bearer_never_reaches_the_planner() {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/plans")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"repository":"a/b","revision":"main","workflow_path":"x","workflow_yaml":""}"#,
        ))
        .expect("request");
    let (status, body) = send(request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The refusal must not leak whether the payload would have been valid.
    assert_eq!(body["title"], "unauthenticated");
}
