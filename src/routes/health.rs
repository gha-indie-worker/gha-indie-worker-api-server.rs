#![forbid(unsafe_code)]

//! Liveness, readiness and metrics.
//!
//! * `/healthz` — **liveness**. Answers as long as the process can serve. It
//!   never touches a dependency, because a liveness probe that fails on a
//!   database blip restarts a healthy process.
//! * `/readyz` — **readiness**, and deliberately fail-closed: the canonical
//!   database pool must answer, and a *configured* shared-auth must be
//!   reachable. Not ready means "take me out of the load balancer", which is
//!   exactly right when this process cannot authenticate or persist.
//! * `/metrics` — a text placeholder in the Prometheus exposition format. The
//!   real counters come from `ores-middleware`'s metrics stage; this endpoint
//!   exists so scraping is wired before the counters land.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tokio::sync::RwLock;

use crate::state::AppState;

/// How long a shared-auth probe result is trusted. Readiness is polled every
/// few seconds; probing the auth authority that often would make this service
/// a load generator against it.
pub const PROBE_TTL: Duration = Duration::from_secs(10);
/// A probe that takes longer than this counts as unreachable.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Caches the last shared-auth probe. Cloning shares the cache.
#[derive(Clone, Debug, Default)]
pub struct ReadinessCache {
    inner: Arc<RwLock<Option<(Instant, bool)>>>,
}

impl ReadinessCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    async fn cached(&self) -> Option<bool> {
        self.inner
            .read()
            .await
            .as_ref()
            .filter(|(at, _)| at.elapsed() < PROBE_TTL)
            .map(|(_, ok)| *ok)
    }

    async fn store(&self, ok: bool) {
        *self.inner.write().await = Some((Instant::now(), ok));
    }
}

#[derive(Debug, Serialize)]
pub struct Health {
    pub status: &'static str,
    pub service: &'static str,
    pub version: &'static str,
    pub protocol: &'static str,
    pub uptime_seconds: u64,
}

#[derive(Debug, Serialize)]
pub struct Readiness {
    pub status: &'static str,
    pub service: &'static str,
    pub version: &'static str,
    pub database_ready: bool,
    pub shared_auth_ready: bool,
    /// Present only when not ready, so a healthy body stays small.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

pub const SERVICE: &str = "gha-indie-worker-api-server";
pub const PROTOCOL: &str = "giw.v1";

#[must_use]
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
}

/// Liveness. Never consults a dependency.
async fn healthz(State(state): State<AppState>) -> Json<Health> {
    Json(Health {
        status: "ok",
        service: SERVICE,
        version: env!("CARGO_PKG_VERSION"),
        protocol: PROTOCOL,
        uptime_seconds: state.started_at.elapsed().as_secs(),
    })
}

/// Readiness. Fail-closed on every configured dependency.
async fn readyz(State(state): State<AppState>) -> Response {
    let (database_ready, database_reason) = database_readiness(&state).await;
    let (shared_auth_ready, auth_reason) = shared_auth_readiness(&state).await;

    if database_ready && shared_auth_ready {
        return Json(Readiness {
            status: "ready",
            service: SERVICE,
            version: env!("CARGO_PKG_VERSION"),
            database_ready,
            shared_auth_ready,
            reason: None,
        })
        .into_response();
    }

    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(Readiness {
            status: "not_ready",
            service: SERVICE,
            version: env!("CARGO_PKG_VERSION"),
            database_ready,
            shared_auth_ready,
            reason: database_reason.or(auth_reason),
        }),
    )
        .into_response()
}

/// The canonical pool must exist and answer. A service that cannot persist a
/// run must not be handed traffic.
#[cfg(feature = "db")]
async fn database_readiness(state: &AppState) -> (bool, Option<&'static str>) {
    if !state.db.canonical_configured() {
        return (false, Some("canonical database pool is not configured"));
    }
    match crate::transport::db::probe(state.db.canonical.as_ref()).await {
        Ok(()) => (true, None),
        Err(error) => {
            tracing::error!(%error, "canonical database readiness probe failed");
            (false, Some("canonical database pool did not answer"))
        }
    }
}

/// Without the `db` feature there is no pool to be ready, and readiness is
/// fail-closed, so this process is never handed traffic.
#[cfg(not(feature = "db"))]
#[allow(clippy::unused_async)]
async fn database_readiness(_state: &AppState) -> (bool, Option<&'static str>) {
    (false, Some("the `db` feature is disabled"))
}

/// shared-auth is only *required* when it is configured. An unconfigured
/// authority is a deployment choice; a configured-but-unreachable one is an
/// outage, and readiness says so.
async fn shared_auth_readiness(state: &AppState) -> (bool, Option<&'static str>) {
    if !state.shared_auth_configured() {
        return (true, None);
    }
    if let Some(cached) = state.readiness.cached().await {
        return (
            cached,
            (!cached).then_some("shared-auth did not answer the last probe"),
        );
    }
    let ok = probe_shared_auth(state).await;
    state.readiness.store(ok).await;
    (ok, (!ok).then_some("shared-auth did not answer"))
}

#[cfg(feature = "shared-auth")]
async fn probe_shared_auth(state: &AppState) -> bool {
    let Some(authority) = state.shared_auth.as_ref() else {
        return true;
    };
    match tokio::time::timeout(PROBE_TIMEOUT, authority.probe()).await {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            tracing::error!(%error, "shared-auth readiness probe failed");
            false
        }
        Err(_) => {
            tracing::error!("shared-auth readiness probe timed out");
            false
        }
    }
}

#[cfg(not(feature = "shared-auth"))]
#[allow(clippy::unused_async)]
async fn probe_shared_auth(_state: &AppState) -> bool {
    true
}

/// Prometheus text exposition. Placeholder counters only; the middleware's
/// metrics stage is the source of truth once it is exporting.
async fn metrics(State(state): State<AppState>) -> Response {
    let body = format!(
        "# HELP giw_api_up 1 when the process is serving.\n\
         # TYPE giw_api_up gauge\n\
         giw_api_up 1\n\
         # HELP giw_api_uptime_seconds Seconds since the process started serving.\n\
         # TYPE giw_api_uptime_seconds counter\n\
         giw_api_uptime_seconds {uptime}\n\
         # HELP giw_api_dependency_configured 1 when a dependency is configured.\n\
         # TYPE giw_api_dependency_configured gauge\n\
         giw_api_dependency_configured{{dependency=\"database\"}} {database}\n\
         giw_api_dependency_configured{{dependency=\"shared_auth\"}} {shared_auth}\n\
         giw_api_dependency_configured{{dependency=\"nats\"}} {nats}\n",
        uptime = state.started_at.elapsed().as_secs(),
        database = u8::from(state.db.canonical_configured()),
        shared_auth = u8::from(state.shared_auth_configured()),
        nats = u8::from(state.nats_configured()),
    );
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_fresh_cache_has_no_verdict() {
        let cache = ReadinessCache::new();
        assert_eq!(cache.cached().await, None);
    }

    #[tokio::test]
    async fn a_stored_verdict_is_returned_inside_the_ttl() {
        let cache = ReadinessCache::new();
        cache.store(false).await;
        assert_eq!(cache.cached().await, Some(false));
        cache.store(true).await;
        assert_eq!(cache.cached().await, Some(true));
    }

    #[test]
    fn a_ready_body_omits_the_reason_field() {
        let body = serde_json::to_string(&Readiness {
            status: "ready",
            service: SERVICE,
            version: "0.1.0",
            database_ready: true,
            shared_auth_ready: true,
            reason: None,
        })
        .expect("serialise");
        assert!(!body.contains("reason"));
        assert!(body.contains("\"database_ready\":true"));
    }

    #[test]
    fn a_not_ready_body_names_one_reason() {
        let body = serde_json::to_string(&Readiness {
            status: "not_ready",
            service: SERVICE,
            version: "0.1.0",
            database_ready: false,
            shared_auth_ready: true,
            reason: Some("canonical database pool is not configured"),
        })
        .expect("serialise");
        assert!(body.contains("canonical database pool is not configured"));
    }
}
