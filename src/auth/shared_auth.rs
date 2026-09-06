#![forbid(unsafe_code)]

//! shared-auth introspection.
//!
//! This is the *only* module that names `shared_auth_client`. It is compiled
//! behind the default-on `shared-auth` feature, so an upstream API change in
//! the pinned `shared-auth-clients` revision is contained here: the rest of the
//! server keeps compiling with `--no-default-features` plus the other features.
//!
//! The independent service credential is attached to the introspection request
//! only. It is never returned to a caller, never logged, and never forwarded.

use std::collections::BTreeSet;
use std::sync::Arc;

use shared_auth_client::{ClientError, Introspection, SharedAuthClient};
use uuid::Uuid;

use super::{scope_set, AuthSource, VerifiedActor};
use crate::config::SharedAuthConfig;

/// A configured shared-auth authority.
#[derive(Clone)]
pub struct SharedAuthAuthority {
    client: SharedAuthClient,
    audience: Arc<str>,
}

impl std::fmt::Debug for SharedAuthAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SharedAuthAuthority")
            .field("audience", &self.audience)
            .finish_non_exhaustive()
    }
}

impl SharedAuthAuthority {
    /// Build the authority eagerly, so a bad base URL is a startup failure
    /// rather than a surprise on the first authenticated request.
    ///
    /// # Errors
    /// Returns [`ClientError`] when the base URL is unparseable or is cleartext
    /// HTTP to a public host.
    pub fn new(config: &SharedAuthConfig) -> Result<Self, ClientError> {
        let client = SharedAuthClient::try_new(config.base.clone())?
            .with_service_credential(config.introspect_secret.clone());
        Ok(Self {
            client,
            audience: Arc::from(config.audience.as_str()),
        })
    }

    #[must_use]
    pub fn audience(&self) -> &str {
        &self.audience
    }

    /// Introspect a bearer against this product audience.
    ///
    /// `Ok(None)` means "this authority does not recognise the token" — the
    /// caller then falls through to the JWT path. `Err` means the authority
    /// itself failed and the failure is worth logging.
    ///
    /// # Errors
    /// Returns [`ClientError`] when the introspection call fails at the
    /// transport or protocol level.
    pub async fn introspect(&self, token: &str) -> Result<Option<VerifiedActor>, ClientError> {
        let introspection = self
            .client
            .introspect_for_audience(token, &self.audience)
            .await?;
        Ok(to_actor(&introspection))
    }

    /// Liveness probe used by `/readyz`. Deliberately uses the unauthenticated
    /// capabilities endpoint: readiness must not depend on a live user token.
    ///
    /// # Errors
    /// Returns [`ClientError`] when shared-auth cannot be reached.
    pub async fn probe(&self) -> Result<(), ClientError> {
        self.client.capabilities().await.map(|_| ())
    }
}

/// Project an [`Introspection`] onto the fleet's one actor type.
///
/// The organisation identifier is not a first-class field on the wire type, so
/// it is read from the flattened remainder (`org_id`, then `organization_id`),
/// falling back to the provider tenant. Anything that is not a UUID is dropped
/// rather than guessed.
#[must_use]
pub fn to_actor(introspection: &Introspection) -> Option<VerifiedActor> {
    if !introspection.active {
        return None;
    }
    let subject = introspection.sub.as_deref()?.trim();
    if subject.is_empty() || subject.len() > 255 {
        return None;
    }
    Some(VerifiedActor {
        subject: subject.to_owned(),
        org_id: org_id_of(introspection),
        roles: introspection.roles.iter().cloned().collect::<BTreeSet<_>>(),
        scopes: scope_set(introspection.scope.as_deref()),
        source: AuthSource::SharedAuth,
        email: introspection.email.clone(),
    })
}

fn org_id_of(introspection: &Introspection) -> Option<Uuid> {
    for key in ["org_id", "organization_id", "tenant_id"] {
        if let Some(value) = introspection.rest.get(key).and_then(|value| value.as_str()) {
            if let Ok(parsed) = Uuid::parse_str(value.trim()) {
                return Some(parsed);
            }
        }
    }
    introspection
        .provider_tenant
        .as_deref()
        .and_then(|value| Uuid::parse_str(value.trim()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn introspection(json: serde_json::Value) -> Introspection {
        serde_json::from_value(json).expect("introspection deserialises")
    }

    #[test]
    fn an_inactive_token_produces_no_actor() {
        assert_eq!(
            to_actor(&introspection(serde_json::json!({
                "active": false,
                "sub": "sub-1",
            }))),
            None
        );
    }

    #[test]
    fn a_token_without_a_usable_subject_produces_no_actor() {
        assert_eq!(
            to_actor(&introspection(serde_json::json!({ "active": true }))),
            None
        );
        assert_eq!(
            to_actor(&introspection(serde_json::json!({
                "active": true,
                "sub": "   ",
            }))),
            None
        );
    }

    #[test]
    fn an_active_token_maps_roles_scopes_and_the_organisation() {
        let actor = to_actor(&introspection(serde_json::json!({
            "active": true,
            "sub": "sub-1",
            "email": "a@example.com",
            "roles": ["owner", "member"],
            "scope": "runs:read runs:write",
            "org_id": "00000000-0000-0000-0000-000000000001",
        })))
        .expect("active tokens produce an actor");

        assert_eq!(actor.subject, "sub-1");
        assert_eq!(actor.source, AuthSource::SharedAuth);
        assert_eq!(
            actor.roles,
            BTreeSet::from(["member".to_owned(), "owner".to_owned()])
        );
        assert_eq!(
            actor.scopes,
            BTreeSet::from(["runs:read".to_owned(), "runs:write".to_owned()])
        );
        assert_eq!(actor.org_id, Some(Uuid::from_u128(1)));
        assert_eq!(actor.email.as_deref(), Some("a@example.com"));
    }

    #[test]
    fn the_provider_tenant_is_the_last_organisation_fallback() {
        let actor = to_actor(&introspection(serde_json::json!({
            "active": true,
            "sub": "sub-1",
            "provider_tenant": "00000000-0000-0000-0000-000000000009",
        })))
        .expect("active");
        assert_eq!(actor.org_id, Some(Uuid::from_u128(9)));
    }

    #[test]
    fn a_non_uuid_organisation_claim_is_dropped_rather_than_guessed() {
        let actor = to_actor(&introspection(serde_json::json!({
            "active": true,
            "sub": "sub-1",
            "org_id": "acme",
        })))
        .expect("active");
        assert_eq!(actor.org_id, None);
    }
}
