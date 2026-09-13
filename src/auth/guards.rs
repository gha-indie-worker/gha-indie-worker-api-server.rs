#![forbid(unsafe_code)]

//! Scope and organisation-role guards.
//!
//! Every guard is deny-by-default and returns [`ApiError::Forbidden`] rather
//! than a hint about what was missing. The scope constants are the complete
//! vocabulary this API understands; `/v1/capabilities` publishes them, so a
//! client never has to guess a scope name.

use uuid::Uuid;

use super::VerifiedActor;
use crate::domain::orgs::OrgRole;
use crate::error::ApiError;

pub const SCOPE_ORGS_READ: &str = "orgs:read";
pub const SCOPE_ORGS_WRITE: &str = "orgs:write";
pub const SCOPE_RUNS_READ: &str = "runs:read";
pub const SCOPE_RUNS_WRITE: &str = "runs:write";
pub const SCOPE_WORKERS_READ: &str = "workers:read";
pub const SCOPE_WORKERS_WRITE: &str = "workers:write";
pub const SCOPE_PLANS_WRITE: &str = "plans:write";
pub const SCOPE_EMBEDDINGS_READ: &str = "embeddings:read";
pub const SCOPE_EMBEDDINGS_WRITE: &str = "embeddings:write";
pub const SCOPE_SYNC: &str = "sync:rw";
pub const SCOPE_CHAT_INTERNAL: &str = "chat:internal";

/// Every scope this API recognises, published by `/v1/capabilities`.
pub const ALL_SCOPES: [&str; 11] = [
    SCOPE_ORGS_READ,
    SCOPE_ORGS_WRITE,
    SCOPE_RUNS_READ,
    SCOPE_RUNS_WRITE,
    SCOPE_WORKERS_READ,
    SCOPE_WORKERS_WRITE,
    SCOPE_PLANS_WRITE,
    SCOPE_EMBEDDINGS_READ,
    SCOPE_EMBEDDINGS_WRITE,
    SCOPE_SYNC,
    SCOPE_CHAT_INTERNAL,
];

/// Require every scope in `required`. An empty requirement passes: routes that
/// need no scope simply do not call this.
///
/// # Errors
/// Returns [`ApiError::Forbidden`] when any scope is absent.
pub fn require_scopes(actor: &VerifiedActor, required: &[&str]) -> Result<(), ApiError> {
    if required.iter().all(|scope| actor.has_scope(scope)) {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// Require *any* of `required`. Used where two scopes are equivalent for a
/// read (for example an org read reachable by an org or a run scope).
///
/// # Errors
/// Returns [`ApiError::Forbidden`] when none of the scopes is present.
pub fn require_any_scope(actor: &VerifiedActor, required: &[&str]) -> Result<(), ApiError> {
    if required.iter().any(|scope| actor.has_scope(scope)) {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// Require that the actor is a member of `org_id` at or above `minimum`.
///
/// The membership role is supplied by the caller (it comes from the store, not
/// from the token) so this stays a pure function; the token's own organisation
/// claim is checked separately by [`VerifiedActor::require_org`].
///
/// # Errors
/// Returns [`ApiError::Forbidden`] when the actor's token addresses another
/// organisation, when there is no membership, or when the role is too weak.
pub fn require_org_role(
    actor: &VerifiedActor,
    org_id: Uuid,
    membership: Option<OrgRole>,
    minimum: OrgRole,
) -> Result<OrgRole, ApiError> {
    actor.require_org(org_id)?;
    let role = membership.ok_or(ApiError::Forbidden)?;
    if role.at_least(minimum) {
        Ok(role)
    } else {
        Err(ApiError::Forbidden)
    }
}

/// Billing surfaces are reachable by owners and the dedicated billing role, and
/// by nobody else — including admins.
///
/// # Errors
/// Returns [`ApiError::Forbidden`] when the actor may not manage billing.
pub fn require_billing(
    actor: &VerifiedActor,
    org_id: Uuid,
    membership: Option<OrgRole>,
) -> Result<OrgRole, ApiError> {
    actor.require_org(org_id)?;
    let role = membership.ok_or(ApiError::Forbidden)?;
    if role.can_manage_billing() {
        Ok(role)
    } else {
        Err(ApiError::Forbidden)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::auth::AuthSource;

    fn actor(scopes: &[&str], org: Option<Uuid>) -> VerifiedActor {
        VerifiedActor {
            subject: "sub".to_owned(),
            org_id: org,
            roles: BTreeSet::new(),
            scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            source: AuthSource::SharedAuth,
            email: None,
        }
    }

    #[test]
    fn the_published_scope_vocabulary_is_unique() {
        let unique: BTreeSet<&str> = ALL_SCOPES.into_iter().collect();
        assert_eq!(unique.len(), ALL_SCOPES.len());
    }

    #[test]
    fn every_required_scope_must_be_present() {
        let actor = actor(&[SCOPE_RUNS_READ], None);
        assert!(require_scopes(&actor, &[SCOPE_RUNS_READ]).is_ok());
        assert!(require_scopes(&actor, &[]).is_ok());
        assert!(matches!(
            require_scopes(&actor, &[SCOPE_RUNS_READ, SCOPE_RUNS_WRITE]),
            Err(ApiError::Forbidden)
        ));
    }

    #[test]
    fn any_scope_passes_with_one_match() {
        let actor = actor(&[SCOPE_RUNS_READ], None);
        assert!(require_any_scope(&actor, &[SCOPE_ORGS_READ, SCOPE_RUNS_READ]).is_ok());
        assert!(matches!(
            require_any_scope(&actor, &[SCOPE_ORGS_READ]),
            Err(ApiError::Forbidden)
        ));
        assert!(matches!(
            require_any_scope(&actor, &[]),
            Err(ApiError::Forbidden)
        ));
    }

    #[test]
    fn org_role_guards_check_the_token_tenant_then_the_membership() {
        let org = Uuid::from_u128(1);
        let actor = actor(&[SCOPE_ORGS_WRITE], Some(org));

        assert_eq!(
            require_org_role(&actor, org, Some(OrgRole::Admin), OrgRole::Member),
            Ok(OrgRole::Admin)
        );
        assert!(matches!(
            require_org_role(&actor, org, Some(OrgRole::Member), OrgRole::Admin),
            Err(ApiError::Forbidden)
        ));
        assert!(matches!(
            require_org_role(&actor, org, None, OrgRole::Member),
            Err(ApiError::Forbidden)
        ));
        // A token for another tenant fails before membership is even consulted.
        assert!(matches!(
            require_org_role(
                &actor,
                Uuid::from_u128(2),
                Some(OrgRole::Owner),
                OrgRole::Member
            ),
            Err(ApiError::Forbidden)
        ));
    }

    #[test]
    fn billing_is_reachable_by_owners_and_billing_seats_only() {
        let org = Uuid::from_u128(1);
        let actor = actor(&[], Some(org));
        assert_eq!(
            require_billing(&actor, org, Some(OrgRole::Owner)),
            Ok(OrgRole::Owner)
        );
        assert_eq!(
            require_billing(&actor, org, Some(OrgRole::Billing)),
            Ok(OrgRole::Billing)
        );
        assert!(matches!(
            require_billing(&actor, org, Some(OrgRole::Admin)),
            Err(ApiError::Forbidden)
        ));
        assert!(matches!(
            require_billing(&actor, org, Some(OrgRole::Member)),
            Err(ApiError::Forbidden)
        ));
    }
}
