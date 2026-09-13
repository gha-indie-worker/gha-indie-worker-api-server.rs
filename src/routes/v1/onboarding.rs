#![forbid(unsafe_code)]

//! `/v1/onboarding/{org|user}/advance` — drive the two onboarding machines.
//!
//! The routes are thin on purpose: they load the current state, call
//! [`crate::domain::onboarding`], and persist the result. Every rule about
//! which step may follow which lives in that pure module, so the HTTP layer
//! cannot drift from it and neither can a future NATS or CLI caller.

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::guards::{require_org_role, require_scopes, SCOPE_ORGS_WRITE};
use crate::auth::Actor;
use crate::domain::onboarding::{
    advance_org, advance_user, OrgOnboardingEvent, OrgOnboardingState, UserOnboardingEvent,
    UserOnboardingState,
};
use crate::domain::orgs::OrgRole;
use crate::error::ApiError;
use crate::state::AppState;

use super::users::ensure_user;

#[must_use]
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/onboarding/org/advance", post(advance_org_route))
        .route("/onboarding/user/advance", post(advance_user_route))
}

#[derive(Debug, Deserialize)]
pub struct OrgAdvance {
    pub org_id: Uuid,
    #[serde(flatten)]
    pub event: OrgOnboardingEvent,
    /// Supplied with `verify_domain`; the organisation records it so the next
    /// step has evidence rather than a claim.
    #[serde(default)]
    pub domain: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OrgAdvanced {
    pub org_id: Uuid,
    pub from: OrgOnboardingState,
    pub to: OrgOnboardingState,
    pub next_event: Option<&'static str>,
}

/// `POST /v1/onboarding/org/advance`
async fn advance_org_route(
    State(state): State<AppState>,
    actor: Actor,
    Json(input): Json<OrgAdvance>,
) -> Result<Json<OrgAdvanced>, ApiError> {
    require_scopes(&actor, &[SCOPE_ORGS_WRITE])?;
    let user = ensure_user(&state, &actor).await?;
    let membership = state.store.membership(input.org_id, user.id).await;
    let role = require_org_role(&actor, input.org_id, membership, OrgRole::Admin)?;

    let org = state
        .store
        .org(input.org_id)
        .await
        .ok_or(ApiError::NotFound)?;
    // Billing is an owner/billing surface even when the caller is an admin.
    if matches!(input.event, OrgOnboardingEvent::LinkBilling) && !role.can_manage_billing() {
        return Err(ApiError::Forbidden);
    }

    // A domain supplied with this request counts as evidence for this
    // transition; otherwise only an already-recorded domain does.
    let supplied_domain = match input.domain.as_deref() {
        None => None,
        Some(domain) => Some(
            crate::domain::orgs::normalize_domain(domain)
                .map_err(|error| ApiError::bad_request(error.to_string()))?,
        ),
    };
    let has_domain = org.verified_domain.is_some() || supplied_domain.is_some();

    let from = state.store.org_onboarding(input.org_id).await;
    let to = advance_org(from, input.event, has_domain)?;

    if let Some(domain) = supplied_domain {
        state.store.set_org_domain(input.org_id, domain).await;
    }
    if let OrgOnboardingEvent::AllocateSeats { seats } = input.event {
        state.store.set_org_seats(input.org_id, seats).await;
    }
    state.store.set_org_onboarding(input.org_id, to).await;

    Ok(Json(OrgAdvanced {
        org_id: input.org_id,
        from,
        to,
        next_event: to.next_event(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct UserAdvance {
    #[serde(flatten)]
    pub event: UserOnboardingEvent,
}

#[derive(Debug, Serialize)]
pub struct UserAdvanced {
    pub user_id: Uuid,
    pub from: UserOnboardingState,
    pub to: UserOnboardingState,
    pub next_event: Option<&'static str>,
}

/// `POST /v1/onboarding/user/advance`
///
/// Scoped to the caller: there is no `user_id` in the payload, so one account
/// can never advance another's onboarding.
async fn advance_user_route(
    State(state): State<AppState>,
    actor: Actor,
    Json(input): Json<UserAdvance>,
) -> Result<Json<UserAdvanced>, ApiError> {
    let user = ensure_user(&state, &actor).await?;
    let from = state.store.user_onboarding(user.id).await;
    let to = advance_user(from, input.event)?;
    state.store.set_user_onboarding(user.id, to).await;
    Ok(Json(UserAdvanced {
        user_id: user.id,
        from,
        to,
        next_event: to.next_event(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_org_advance_request_carries_its_event_inline() {
        let request: OrgAdvance = serde_json::from_str(
            r#"{"org_id":"00000000-0000-0000-0000-000000000001","event":"allocate_seats","seats":5}"#,
        )
        .expect("deserialise");
        assert_eq!(request.org_id, Uuid::from_u128(1));
        assert_eq!(
            request.event,
            OrgOnboardingEvent::AllocateSeats { seats: 5 }
        );
        assert_eq!(request.domain, None);
    }

    #[test]
    fn a_domain_may_travel_with_the_verify_step() {
        let request: OrgAdvance = serde_json::from_str(
            r#"{"org_id":"00000000-0000-0000-0000-000000000001","event":"verify_domain","domain":"indiebuild.dev"}"#,
        )
        .expect("deserialise");
        assert_eq!(request.event, OrgOnboardingEvent::VerifyDomain);
        assert_eq!(request.domain.as_deref(), Some("indiebuild.dev"));
    }

    #[test]
    fn a_user_advance_request_names_only_an_event() {
        let request: UserAdvance =
            serde_json::from_str(r#"{"event":"verify_email"}"#).expect("deserialise");
        assert_eq!(request.event, UserOnboardingEvent::VerifyEmail);
        // There is no user_id field to forge.
        assert!(serde_json::from_str::<UserAdvance>(r#"{"event":"nope"}"#).is_err());
    }

    #[test]
    fn the_response_tells_a_client_what_may_come_next() {
        let advanced = OrgAdvanced {
            org_id: Uuid::nil(),
            from: OrgOnboardingState::Created,
            to: OrgOnboardingState::VerifiedDomain,
            next_event: OrgOnboardingState::VerifiedDomain.next_event(),
        };
        let encoded = serde_json::to_string(&advanced).expect("serialise");
        assert!(encoded.contains("\"next_event\":\"allocate_seats\""));
    }
}
