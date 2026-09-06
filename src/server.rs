#![forbid(unsafe_code)]

//! Router assembly and process startup.

use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Router;
use tokio::sync::watch;

use crate::error::ApiError;
use crate::routes;
use crate::state::{AppState, StartupError};

/// Build the full router. Public so `tests/http.rs` can drive it with
/// `tower::ServiceExt::oneshot` without binding a port.
#[must_use]
pub fn router(state: AppState) -> Router {
    let max_body = state.config.max_body_bytes;
    Router::new()
        .merge(routes::health::router())
        .nest("/v1", routes::v1::router())
        .fallback(not_found)
        .with_state(state)
        .layer(DefaultBodyLimit::max(max_body))
}

/// Unknown paths get the same problem+json shape as every other error, so a
/// client has exactly one response format to parse.
async fn not_found() -> impl IntoResponse {
    ApiError::NotFound
}

/// Build the router with the middleware stack installed.
///
/// # Errors
/// Returns [`StartupError::Middleware`] when `ORES_MIDDLEWARE_*` configuration
/// is invalid — a half-configured stack must not bind a listener.
pub fn app(state: &AppState) -> Result<Router, StartupError> {
    let router = router(state.clone());
    crate::middleware::install(router, state).map_err(StartupError::Middleware)
}

/// Run the process: bind HTTP, start the optional TCP listener, serve until a
/// signal, then drain.
///
/// # Errors
/// Returns a startup failure, or the listener's I/O error.
pub async fn run(state: AppState) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let app = app(&state)?;

    // One drain signal, shared by every carrier, so HTTP and TCP stop
    // accepting at the same moment rather than one outliving the other.
    let (drain_tx, drain_rx) = watch::channel(false);

    #[cfg(feature = "tcp-transport")]
    let tcp = crate::transport::tcp::spawn(state.clone(), drain_rx.clone());
    #[cfg(not(feature = "tcp-transport"))]
    let tcp: Option<tokio::task::JoinHandle<()>> = {
        drop(drain_rx);
        None
    };

    #[cfg(feature = "nats-transport")]
    start_nats_intake(&state).await;

    tracing::info!(
        environment = state.config.env.as_str(),
        database = state.db.canonical_configured(),
        shared_auth = state.shared_auth_configured(),
        jwt = state.jwt.is_configured(),
        nats = state.nats_configured(),
        tcp = state.config.tcp_bind.is_some(),
        "starting gha-indie-worker-api-server"
    );

    let outcome = crate::transport::http::serve(
        &state.config.bind,
        app,
        state.config.shutdown_grace,
        drain_tx,
    )
    .await?;

    if let Some(handle) = tcp {
        // The TCP listener sees the same drain signal, so this is a short wait.
        if let Err(error) = handle.await {
            tracing::warn!(%error, "tcp transport task did not join cleanly");
        }
    }

    tracing::info!(outcome = ?outcome, "gha-indie-worker-api-server stopped");
    Ok(())
}

/// Subscribe to the JetStream-shaped run-request intake when it is enabled.
#[cfg(feature = "nats-transport")]
async fn start_nats_intake(state: &AppState) {
    if !state.config.nats_intake {
        return;
    }
    let Some(publisher) = state.nats.as_ref() else {
        tracing::warn!("nats intake is enabled but no broker is connected");
        return;
    };
    let handler = std::sync::Arc::new(|value: serde_json::Value| {
        // Intake requests are recorded, not executed: enqueuing a run must go
        // through the same plan validation the HTTP route performs, and that
        // path lands with the durable delivery store.
        tracing::info!(
            kind = value
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown"),
            "received a run request on the nats intake"
        );
    });
    if let Err(error) = publisher.run_intake(handler).await {
        tracing::warn!(%error, "nats intake could not be started");
    }
}

/// The status code an unmatched route produces. Published here so
/// `tests/http.rs` asserts against one constant.
pub const NOT_FOUND: StatusCode = StatusCode::NOT_FOUND;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ApiConfig;

    #[tokio::test]
    async fn the_router_builds_from_a_default_configuration() {
        let config = ApiConfig::from_lookup(|_| None).expect("defaults parse");
        let state = AppState::build(config).await.expect("state builds");
        let _router = router(state);
    }
}
