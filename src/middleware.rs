#![forbid(unsafe_code)]

//! The `ores-middleware` installation.
//!
//! This module is the *only* place that names `ores_middleware`, and it is
//! behind the default-on `ores-mw` feature. The stack's order is fixed upstream
//! (proxy trust → TLS → ids → context → ip policy → **auth** → **rate limit** →
//! idempotency → handler → security headers / compression / metrics /
//! sync-observe); what this service supplies is the two integration points:
//!
//! * [`GiwAuthVerifier`] — shared-auth introspection or a Supabase/Neon JWT,
//!   both via [`crate::auth::verify`].
//! * [`crate::rate_limit::GiwRateLimiter`] — opaque principals over a bounded
//!   fixed window.
//!
//! The verifier is **fail-open at this layer on purpose**: the middleware
//! annotates a request with whatever identity it can establish, and the route's
//! [`crate::auth::Actor`] extractor is what refuses an unauthenticated caller
//! with 401. Putting the refusal in the extractor keeps `/healthz`, `/readyz`,
//! `/metrics`, `/v1/capabilities` and the GitHub webhook (which authenticates
//! itself with an HMAC) reachable without duplicating an allow-list here.
//!
//! Without the feature, [`install`] applies an equivalent local layer set —
//! request ids, sensitive-header redaction, timeout, tracing and compression —
//! so the service is never accidentally deployed without a middleware layer.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::http::{HeaderName, StatusCode};
use axum::Router;
use tower_http::compression::CompressionLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

pub const SERVICE_NAME: &str = "gha-indie-worker-api-server";

/// Layers this service always applies, feature or not.
#[must_use]
pub fn base_layers(router: Router, request_timeout: Duration) -> Router {
    let request_id = HeaderName::from_static("x-request-id");
    router
        .layer(CompressionLayer::new())
        .layer(SetSensitiveRequestHeadersLayer::new([
            axum::http::header::AUTHORIZATION,
            axum::http::header::COOKIE,
            HeaderName::from_static("x-hub-signature-256"),
        ]))
        .layer(PropagateRequestIdLayer::new(request_id.clone()))
        .layer(SetRequestIdLayer::new(request_id, MakeRequestUuid))
        .layer(TimeoutLayer::new(request_timeout))
        .layer(TraceLayer::new_for_http())
}

/// Install the middleware stack around `router`.
///
/// # Errors
/// Returns a human-readable message when `ORES_MIDDLEWARE_*` configuration is
/// invalid. The caller treats that as a startup failure: a service must not
/// bind a listener with a half-configured middleware stack.
pub fn install(router: Router, state: &AppState) -> Result<Router, String> {
    let router = base_layers(router, state.config.request_timeout);
    install_stack(router, state)
}

#[cfg(feature = "ores-mw")]
fn install_stack(router: Router, state: &AppState) -> Result<Router, String> {
    ores::install(router, state)
}

#[cfg(not(feature = "ores-mw"))]
fn install_stack(router: Router, _state: &AppState) -> Result<Router, String> {
    tracing::warn!(
        "ores-middleware is disabled; running with local layers only \
         (no ip policy, idempotency, or security-header stage)"
    );
    Ok(router)
}

#[cfg(feature = "ores-mw")]
mod ores {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

    use axum::Router;
    use ores_middleware::frameworks::axum::install_with_ores_logger;
    use ores_middleware::{
        stack_from_env, AuthDecision, AuthVerifier, IntegrationError, RequestContext,
        RequestMetadata, ResponseMetadata, SyncObserver,
    };

    use super::{claims_of, SERVICE_NAME};
    use crate::state::AppState;

    pub fn install(router: Router, state: &AppState) -> Result<Router, String> {
        let stack = stack_from_env(SERVICE_NAME)
            .map_err(|error| format!("ores-middleware configuration is invalid: {error}"))?
            .with_auth_verifier(Arc::new(GiwAuthVerifier {
                state: state.clone(),
            }))
            .with_rate_limiter(state.rate_limiter.clone())
            .with_sync_observer(Arc::new(GiwSyncObserver));

        Ok(install_with_ores_logger(
            router,
            Arc::new(stack),
            state.logger.clone(),
        ))
    }

    /// Bridges the fleet middleware's auth hook onto this service's dual-auth
    /// verification. See the module docs for why this is fail-open.
    pub struct GiwAuthVerifier {
        state: AppState,
    }

    impl AuthVerifier for GiwAuthVerifier {
        fn verify<'a>(
            &'a self,
            request: &'a RequestMetadata,
        ) -> Pin<Box<dyn Future<Output = Result<AuthDecision, IntegrationError>> + Send + 'a>>
        {
            Box::pin(async move {
                let header = request.headers.get("authorization").map(String::as_str);
                let Ok(token) = crate::auth::bearer_from(header) else {
                    return Ok(AuthDecision::default());
                };
                match crate::auth::verify(&self.state, token).await {
                    Ok(actor) => Ok(AuthDecision {
                        user_id: Some(actor.subject.clone()),
                        tenant_id: actor.org_id.map(|org| org.to_string()),
                        claims: claims_of(&actor),
                    }),
                    Err(_) => Ok(AuthDecision::default()),
                }
            })
        }
    }

    /// opto-sync observation. Fail-open and always recorded: a sync sink that
    /// is down must never turn a served request into an error.
    pub struct GiwSyncObserver;

    impl SyncObserver for GiwSyncObserver {
        fn observe<'a>(
            &'a self,
            context: &'a RequestContext,
            request: &'a RequestMetadata,
            response: &'a ResponseMetadata,
        ) -> Pin<Box<dyn Future<Output = Result<(), IntegrationError>> + Send + 'a>> {
            Box::pin(async move {
                tracing::debug!(
                    request_id = %context.request_id,
                    trace_id = %context.trace_id,
                    method = %request.method,
                    path = %request.path,
                    status = response.status,
                    duration_ms = response.duration_ms,
                    "sync observation recorded"
                );
                Ok(())
            })
        }
    }
}

/// Project a verified actor into the middleware's flat claim map.
///
/// Only non-identifying, already-verified facts travel: never the token, never
/// the email, never a provider identifier.
#[must_use]
pub fn claims_of(actor: &crate::auth::VerifiedActor) -> BTreeMap<String, String> {
    let mut claims = BTreeMap::new();
    claims.insert("auth.source".to_owned(), actor.source.as_str().to_owned());
    if !actor.roles.is_empty() {
        claims.insert(
            "auth.roles".to_owned(),
            actor
                .roles
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    if !actor.scopes.is_empty() {
        claims.insert(
            "auth.scopes".to_owned(),
            actor
                .scopes
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    claims
}

/// Status code the local timeout layer produces, published by
/// `/v1/capabilities` so a client can tell a gateway timeout from ours.
pub const TIMEOUT_STATUS: StatusCode = StatusCode::REQUEST_TIMEOUT;

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use uuid::Uuid;

    use super::*;
    use crate::auth::{AuthSource, VerifiedActor};

    fn actor() -> VerifiedActor {
        VerifiedActor {
            subject: "sub-1".to_owned(),
            org_id: Some(Uuid::from_u128(1)),
            roles: BTreeSet::from(["admin".to_owned(), "member".to_owned()]),
            scopes: BTreeSet::from(["runs:read".to_owned()]),
            source: AuthSource::SharedAuth,
            email: Some("alex@example.com".to_owned()),
        }
    }

    #[test]
    fn claims_carry_only_verified_non_identifying_facts() {
        let claims = claims_of(&actor());
        assert_eq!(
            claims.get("auth.source").map(String::as_str),
            Some("shared-auth")
        );
        assert_eq!(
            claims.get("auth.roles").map(String::as_str),
            Some("admin member")
        );
        assert_eq!(
            claims.get("auth.scopes").map(String::as_str),
            Some("runs:read")
        );
        assert!(!claims.values().any(|value| value.contains("alex")));
        assert!(!claims.contains_key("auth.email"));
    }

    #[test]
    fn an_actor_without_roles_or_scopes_produces_only_a_source() {
        let mut bare = actor();
        bare.roles.clear();
        bare.scopes.clear();
        let claims = claims_of(&bare);
        assert_eq!(claims.len(), 1);
        assert!(claims.contains_key("auth.source"));
    }
}
