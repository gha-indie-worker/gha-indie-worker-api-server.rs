#![forbid(unsafe_code)]

use crate::config::ApiConfig;
use crate::routes;
use axum::{
    routing::get,
    Json, Router,
};
use std::error::Error;

pub fn router() -> Router {
    Router::new()
        .route("/healthz", get(|| async { Json(routes::health::body()) }))
        .route("/readyz", get(|| async { Json(routes::health::body()) }))
        .route("/v1/catalog", get(|| async { Json(routes::v1::catalog()) }))
}

pub async fn run(config: &ApiConfig) -> Result<(), Box<dyn Error>> {
    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    eprintln!("gha-indie-worker-api-server listening on {}", listener.local_addr()?);
    axum::serve(listener, router()).await?;
    Ok(())
}
