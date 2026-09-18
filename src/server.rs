#![forbid(unsafe_code)]

use crate::config::ApiConfig;
use crate::routes;
use axum::{routing::get, Json, Router};
use std::error::Error;

pub fn router() -> Router {
    Router::new()
        .route("/healthz", get(|| async { Json(routes::health::body()) }))
        .route("/readyz", get(|| async { Json(routes::health::body()) }))
        .route("/v1/catalog", get(|| async { Json(routes::v1::catalog()) }))
}

pub async fn run(config: &ApiConfig) -> Result<(), Box<dyn Error>> {
    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    eprintln!(
        "gha-indie-worker-api-server listening on {}",
        listener.local_addr()?
    );
    axum::serve(listener, router()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::{header, Method, Request, StatusCode},
    };
    use tower::ServiceExt;

    async fn request(method: Method, path: &str) -> (StatusCode, String, Vec<u8>) {
        let response = router()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("router response");
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("response body")
            .to_vec();
        (status, content_type, body)
    }

    #[tokio::test]
    async fn health_and_ready_contracts_are_json_and_consistent() {
        for path in ["/healthz", "/readyz"] {
            let (status, content_type, body) = request(Method::GET, path).await;
            assert_eq!(status, StatusCode::OK);
            assert!(content_type.starts_with("application/json"));
            let payload: serde_json::Value = serde_json::from_slice(&body).expect("health JSON");
            assert_eq!(payload["ok"], true);
            assert_eq!(payload["service"], "gha-indie-worker-api-server");
        }
    }

    #[tokio::test]
    async fn catalog_contract_exposes_worker_lease_resource() {
        let (status, content_type, body) = request(Method::GET, "/v1/catalog").await;
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("application/json"));
        let payload: serde_json::Value = serde_json::from_slice(&body).expect("catalog JSON");
        assert_eq!(payload["resource"], "WorkerLease");
    }

    #[tokio::test]
    async fn router_fails_closed_for_unknown_routes_and_wrong_methods() {
        let (missing, _, _) = request(Method::GET, "/does-not-exist").await;
        let (wrong_method, _, _) = request(Method::POST, "/healthz").await;
        assert_eq!(missing, StatusCode::NOT_FOUND);
        assert_eq!(wrong_method, StatusCode::METHOD_NOT_ALLOWED);
    }
}
