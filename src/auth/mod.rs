#![forbid(unsafe_code)]

//! Dual authentication: one verified actor from two independent authorities.
//!
//! shared-auth federates Supabase Auth and Neon Auth, so a caller may present
//! either a **shared-auth bearer** (introspected over the wire against
//! `SHARED_AUTH_BASE`, with an independent service credential) **or** a
//! **Supabase/Neon JWT** (verified locally against a cached JWKS or the
//! project's HS256 secret). Both collapse to one [`VerifiedActor`], so nothing
//! downstream has to know which authority spoke.
//!
//! Order matters: shared-auth is tried first when configured, because it is the
//! authority that can revoke. A JWT is only accepted when shared-auth either is
//! not configured or does not recognise the token.

pub mod guards;
pub mod jwt;
#[cfg(feature = "shared-auth")]
pub mod shared_auth;

use std::collections::BTreeSet;

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::http::HeaderMap;
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::AppState;

/// Which authority verified this actor.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthSource {
    SharedAuth,
    SupabaseJwt,
    NeonJwt,
}

impl AuthSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SharedAuth => "shared-auth",
            Self::SupabaseJwt => "supabase-jwt",
            Self::NeonJwt => "neon-jwt",
        }
    }
}

/// The single authenticated identity every handler sees.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct VerifiedActor {
    pub subject: String,
    pub org_id: Option<Uuid>,
    pub roles: BTreeSet<String>,
    pub scopes: BTreeSet<String>,
    pub source: AuthSource,
    pub email: Option<String>,
}

impl VerifiedActor {
    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }

    #[must_use]
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.contains(role)
    }

    /// # Errors
    /// Returns [`ApiError::Forbidden`] when the scope is absent.
    pub fn require_scope(&self, scope: &str) -> Result<(), ApiError> {
        if self.has_scope(scope) {
            Ok(())
        } else {
            Err(ApiError::Forbidden)
        }
    }

    /// # Errors
    /// Returns [`ApiError::Forbidden`] when the role is absent.
    pub fn require_role(&self, role: &str) -> Result<(), ApiError> {
        if self.has_role(role) {
            Ok(())
        } else {
            Err(ApiError::Forbidden)
        }
    }

    /// The token's own organisation claim must match the organisation in the
    /// path. A token scoped to one tenant can never address another.
    ///
    /// # Errors
    /// Returns [`ApiError::Forbidden`] on a mismatch or a missing claim.
    pub fn require_org(&self, org_id: Uuid) -> Result<(), ApiError> {
        match self.org_id {
            Some(claimed) if claimed == org_id => Ok(()),
            Some(_) | None => Err(ApiError::Forbidden),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum AuthError {
    #[error("no authentication authority is configured")]
    NotConfigured,
    #[error("missing or malformed Authorization header")]
    MissingBearer,
    #[error("token was not accepted by any configured authority")]
    Rejected,
    #[error("an authentication authority is unreachable")]
    AuthorityUnavailable,
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::AuthorityUnavailable | AuthError::NotConfigured => {
                Self::Unavailable("authentication authority")
            }
            AuthError::MissingBearer | AuthError::Rejected => Self::Unauthenticated,
        }
    }
}

/// Largest bearer token we will even look at.
pub const MAX_BEARER_BYTES: usize = 16 * 1024;

/// Extract a bounded bearer token from an `Authorization` header value.
///
/// # Errors
/// Returns [`AuthError::MissingBearer`] when the header is absent, is not a
/// `Bearer` credential, is empty, or is implausibly long.
pub fn bearer_from(header: Option<&str>) -> Result<&str, AuthError> {
    header
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty() && token.len() <= MAX_BEARER_BYTES)
        .ok_or(AuthError::MissingBearer)
}

/// Read the bearer token out of a header map.
///
/// # Errors
/// See [`bearer_from`].
pub fn bearer_of(headers: &HeaderMap) -> Result<&str, AuthError> {
    bearer_from(
        headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    )
}

/// Split an OAuth-style space-delimited scope string into a set.
#[must_use]
pub fn scope_set(scope: Option<&str>) -> BTreeSet<String> {
    scope
        .map(|value| {
            value
                .split_ascii_whitespace()
                .map(str::to_owned)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default()
}

/// Verify a bearer against every configured authority, in order.
///
/// # Errors
/// Returns [`AuthError::NotConfigured`] when no authority is configured at all,
/// and [`AuthError::Rejected`] when every configured authority refused.
pub async fn verify(state: &AppState, token: &str) -> Result<VerifiedActor, AuthError> {
    let mut configured = false;

    #[cfg(feature = "shared-auth")]
    if let Some(authority) = state.shared_auth.as_ref() {
        configured = true;
        match authority.introspect(token).await {
            Ok(Some(actor)) => return Ok(actor),
            // An inactive or unknown token falls through to the JWT path, since
            // shared-auth federates but does not mint every accepted token.
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(error = %error, "shared-auth introspection failed");
            }
        }
    }

    if state.jwt.is_configured() {
        configured = true;
        match state.jwt.verify(token).await {
            Ok(actor) => return Ok(actor),
            Err(error) => {
                tracing::debug!(error = %error, "jwt verification refused a token");
            }
        }
    }

    if configured {
        Err(AuthError::Rejected)
    } else {
        Err(AuthError::NotConfigured)
    }
}

/// Required-authentication extractor. A handler taking `Actor` is, by
/// construction, unreachable without a verified caller.
#[derive(Clone, Debug)]
pub struct Actor(pub VerifiedActor);

impl std::ops::Deref for Actor {
    type Target = VerifiedActor;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl FromRequestParts<AppState> for Actor {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_of(&parts.headers)?;
        let actor = verify(state, token).await?;
        Ok(Self(actor))
    }
}

/// Optional-authentication extractor, for surfaces that serve anonymous
/// visitors (public chat) but still want the actor when one is present.
#[derive(Clone, Debug)]
pub struct MaybeActor(pub Option<VerifiedActor>);

impl FromRequestParts<AppState> for MaybeActor {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Ok(token) = bearer_of(&parts.headers) else {
            return Ok(Self(None));
        };
        Ok(Self(verify(state, token).await.ok()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_extraction_is_bounded_and_strict() {
        assert_eq!(bearer_from(Some("Bearer abc")), Ok("abc"));
        assert_eq!(bearer_from(Some("Bearer   abc  ")), Ok("abc"));
        assert_eq!(bearer_from(None), Err(AuthError::MissingBearer));
        assert_eq!(bearer_from(Some("")), Err(AuthError::MissingBearer));
        assert_eq!(bearer_from(Some("abc")), Err(AuthError::MissingBearer));
        assert_eq!(
            bearer_from(Some("bearer abc")),
            Err(AuthError::MissingBearer)
        );
        assert_eq!(
            bearer_from(Some("Basic abc")),
            Err(AuthError::MissingBearer)
        );
        assert_eq!(bearer_from(Some("Bearer  ")), Err(AuthError::MissingBearer));

        let long = format!("Bearer {}", "x".repeat(MAX_BEARER_BYTES + 1));
        assert_eq!(bearer_from(Some(&long)), Err(AuthError::MissingBearer));
    }

    #[test]
    fn scope_strings_become_sets() {
        assert_eq!(
            scope_set(Some("runs:read  runs:write runs:read")),
            BTreeSet::from(["runs:read".to_owned(), "runs:write".to_owned()])
        );
        assert!(scope_set(None).is_empty());
        assert!(scope_set(Some("   ")).is_empty());
    }

    fn actor() -> VerifiedActor {
        VerifiedActor {
            subject: "sub".to_owned(),
            org_id: Some(Uuid::from_u128(1)),
            roles: BTreeSet::from(["admin".to_owned()]),
            scopes: BTreeSet::from(["runs:read".to_owned()]),
            source: AuthSource::SharedAuth,
            email: None,
        }
    }

    #[test]
    fn guards_are_deny_by_default() {
        let actor = actor();
        assert!(actor.require_scope("runs:read").is_ok());
        assert!(matches!(
            actor.require_scope("runs:write"),
            Err(ApiError::Forbidden)
        ));
        assert!(actor.require_role("admin").is_ok());
        assert!(matches!(
            actor.require_role("owner"),
            Err(ApiError::Forbidden)
        ));
    }

    #[test]
    fn a_token_cannot_address_another_tenant() {
        let actor = actor();
        assert!(actor.require_org(Uuid::from_u128(1)).is_ok());
        assert!(matches!(
            actor.require_org(Uuid::from_u128(2)),
            Err(ApiError::Forbidden)
        ));

        let mut tenantless = actor;
        tenantless.org_id = None;
        assert!(matches!(
            tenantless.require_org(Uuid::from_u128(1)),
            Err(ApiError::Forbidden)
        ));
    }

    #[test]
    fn auth_sources_have_stable_wire_names() {
        assert_eq!(AuthSource::SharedAuth.as_str(), "shared-auth");
        assert_eq!(AuthSource::SupabaseJwt.as_str(), "supabase-jwt");
        assert_eq!(AuthSource::NeonJwt.as_str(), "neon-jwt");
        assert_eq!(
            serde_json::to_string(&AuthSource::SupabaseJwt).expect("serialise"),
            "\"supabase-jwt\""
        );
    }
}
