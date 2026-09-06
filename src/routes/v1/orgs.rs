#![forbid(unsafe_code)]

//! `/v1/orgs` — B2B organisations, members and invitations.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::auth::guards::{require_org_role, require_scopes, SCOPE_ORGS_READ, SCOPE_ORGS_WRITE};
use crate::auth::Actor;
use crate::domain::now_rfc3339;
use crate::domain::orgs::{
    advance_invitation, claim_seat, normalize_email, Invitation, InvitationState, NewInvitation,
    NewOrg, Org, OrgError, OrgMember, OrgRole,
};
use crate::error::ApiError;
use crate::state::AppState;

use super::users::ensure_user;

/// How long an invitation stays claimable.
pub const INVITATION_TTL_HOURS: i64 = 72;

#[must_use]
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/orgs", post(create_org))
        .route("/orgs/{org_id}", get(get_org))
        .route(
            "/orgs/{org_id}/invitations",
            get(list_invitations).post(create_invitation),
        )
        .route("/orgs/{org_id}/members", get(list_members).post(add_member))
}

#[derive(Debug, Serialize)]
pub struct OrgView {
    #[serde(flatten)]
    pub org: Org,
    pub onboarding: crate::domain::onboarding::OrgOnboardingState,
    pub occupied_seats: u32,
}

/// `POST /v1/orgs` — create an organisation with the caller as its owner.
async fn create_org(
    State(state): State<AppState>,
    actor: Actor,
    Json(input): Json<NewOrg>,
) -> Result<(StatusCode, Json<OrgView>), ApiError> {
    require_scopes(&actor, &[SCOPE_ORGS_WRITE])?;
    let user = ensure_user(&state, &actor).await?;

    let created_at = now_rfc3339();
    let org = Org::create(Uuid::now_v7(), input, created_at.clone()).map_err(map_org_error)?;
    if state.store.slug_taken(&org.slug).await {
        return Err(ApiError::conflict("organisation slug is already taken"));
    }

    let owner = OrgMember {
        org_id: org.id,
        user_id: user.id,
        role: OrgRole::Owner,
        joined_at: created_at,
    };
    state.store.insert_org(org.clone(), owner).await;

    Ok((
        StatusCode::CREATED,
        Json(OrgView {
            onboarding: state.store.org_onboarding(org.id).await,
            occupied_seats: state.store.occupied_seats(org.id).await,
            org,
        }),
    ))
}

/// `GET /v1/orgs/{org_id}`
async fn get_org(
    State(state): State<AppState>,
    actor: Actor,
    Path(org_id): Path<Uuid>,
) -> Result<Json<OrgView>, ApiError> {
    require_scopes(&actor, &[SCOPE_ORGS_READ])?;
    let user = ensure_user(&state, &actor).await?;
    let membership = state.store.membership(org_id, user.id).await;
    require_org_role(&actor, org_id, membership, OrgRole::Billing)?;

    let org = state.store.org(org_id).await.ok_or(ApiError::NotFound)?;
    Ok(Json(OrgView {
        onboarding: state.store.org_onboarding(org_id).await,
        occupied_seats: state.store.occupied_seats(org_id).await,
        org,
    }))
}

#[derive(Debug, Serialize)]
pub struct CreatedInvitation {
    #[serde(flatten)]
    pub invitation: Invitation,
    /// Returned exactly once. Only its SHA-256 digest is stored, so this value
    /// cannot be recovered from the API or from a database dump.
    pub token: String,
}

/// `POST /v1/orgs/{org_id}/invitations`
async fn create_invitation(
    State(state): State<AppState>,
    actor: Actor,
    Path(org_id): Path<Uuid>,
    Json(input): Json<NewInvitation>,
) -> Result<(StatusCode, Json<CreatedInvitation>), ApiError> {
    require_scopes(&actor, &[SCOPE_ORGS_WRITE])?;
    let user = ensure_user(&state, &actor).await?;
    let membership = state.store.membership(org_id, user.id).await;
    let role = require_org_role(&actor, org_id, membership, OrgRole::Admin)?;
    if !role.can_invite() {
        return Err(ApiError::Forbidden);
    }

    let org = state.store.org(org_id).await.ok_or(ApiError::NotFound)?;
    let email = normalize_email(&input.email).map_err(map_org_error)?;
    let invited_role = OrgRole::parse(&input.role).map_err(map_org_error)?;
    // Only an owner may mint another owner: an admin cannot promote past itself.
    if invited_role.is_owner() && !role.is_owner() {
        return Err(ApiError::Forbidden);
    }

    // Seats are claimed at invitation time so an organisation cannot oversubscribe
    // by sending more invitations than it bought.
    let occupied = state.store.occupied_seats(org_id).await;
    let pending = pending_invitations(&state, org_id).await;
    claim_seat(org.seats, occupied.saturating_add(pending)).map_err(map_org_error)?;

    let token = Uuid::new_v4().simple().to_string();
    let invitation = Invitation {
        id: Uuid::now_v7(),
        org_id,
        email,
        role: invited_role,
        state: InvitationState::Pending,
        created_at: now_rfc3339(),
        expires_at: expires_at(),
        token_digest: digest_of(&token),
    };
    state.store.insert_invitation(invitation.clone()).await;

    Ok((
        StatusCode::CREATED,
        Json(CreatedInvitation { invitation, token }),
    ))
}

/// `GET /v1/orgs/{org_id}/invitations`
async fn list_invitations(
    State(state): State<AppState>,
    actor: Actor,
    Path(org_id): Path<Uuid>,
) -> Result<Json<Vec<Invitation>>, ApiError> {
    require_scopes(&actor, &[SCOPE_ORGS_READ])?;
    let user = ensure_user(&state, &actor).await?;
    let membership = state.store.membership(org_id, user.id).await;
    require_org_role(&actor, org_id, membership, OrgRole::Admin)?;
    Ok(Json(state.store.invitations(org_id).await))
}

/// `GET /v1/orgs/{org_id}/members`
async fn list_members(
    State(state): State<AppState>,
    actor: Actor,
    Path(org_id): Path<Uuid>,
) -> Result<Json<Vec<OrgMember>>, ApiError> {
    require_scopes(&actor, &[SCOPE_ORGS_READ])?;
    let user = ensure_user(&state, &actor).await?;
    let membership = state.store.membership(org_id, user.id).await;
    require_org_role(&actor, org_id, membership, OrgRole::Billing)?;
    Ok(Json(state.store.members(org_id).await))
}

#[derive(Debug, serde::Deserialize)]
pub struct AcceptMember {
    /// The single-use invitation token from the creation response.
    pub token: String,
}

/// `POST /v1/orgs/{org_id}/members` — accept an invitation.
///
/// The caller proves membership with the invitation token, so this route is the
/// one place where a non-member may address an organisation. The token is
/// compared by digest and the invitation is advanced through its state machine,
/// which makes a replay a typed conflict rather than a second seat.
async fn add_member(
    State(state): State<AppState>,
    actor: Actor,
    Path(org_id): Path<Uuid>,
    Json(input): Json<AcceptMember>,
) -> Result<(StatusCode, Json<OrgMember>), ApiError> {
    let user = ensure_user(&state, &actor).await?;
    let digest = digest_of(&input.token);

    let invitation = state
        .store
        .invitations(org_id)
        .await
        .into_iter()
        .find(|candidate| candidate.token_digest == digest)
        .ok_or(ApiError::NotFound)?;

    advance_invitation(
        invitation.state,
        crate::domain::orgs::InvitationEvent::Accept,
    )
    .map_err(|error| ApiError::conflict(error.to_string()))?;

    let org = state.store.org(org_id).await.ok_or(ApiError::NotFound)?;
    let occupied = state.store.occupied_seats(org_id).await;
    claim_seat(org.seats, occupied).map_err(map_org_error)?;

    let member = OrgMember {
        org_id,
        user_id: user.id,
        role: invitation.role,
        joined_at: now_rfc3339(),
    };
    state.store.insert_member(member.clone()).await;
    let mut accepted = invitation;
    accepted.state = InvitationState::Accepted;
    state.store.insert_invitation(accepted).await;

    Ok((StatusCode::CREATED, Json(member)))
}

async fn pending_invitations(state: &AppState, org_id: Uuid) -> u32 {
    let count = state
        .store
        .invitations(org_id)
        .await
        .into_iter()
        .filter(|invitation| invitation.state == InvitationState::Pending)
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// SHA-256 of an invitation token. The token itself is never stored.
#[must_use]
pub fn digest_of(token: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let mut digest = [0_u8; 32];
    digest.copy_from_slice(&hasher.finalize());
    digest
}

fn expires_at() -> String {
    let expiry = time::OffsetDateTime::now_utc() + time::Duration::hours(INVITATION_TTL_HOURS);
    expiry
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| now_rfc3339())
}

/// Domain errors become client errors, never 500s.
fn map_org_error(error: OrgError) -> ApiError {
    match error {
        OrgError::SeatsExhausted | OrgError::LastOwner => ApiError::conflict(error.to_string()),
        other => ApiError::bad_request(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_digests_are_stable_and_collision_resistant_enough_to_compare() {
        let token = "0123456789abcdef";
        assert_eq!(digest_of(token), digest_of(token));
        assert_ne!(digest_of(token), digest_of("0123456789abcdee"));
        assert_eq!(digest_of(token).len(), 32);
    }

    #[test]
    fn seat_exhaustion_is_a_conflict_and_bad_input_is_a_bad_request() {
        assert_eq!(
            map_org_error(OrgError::SeatsExhausted).status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            map_org_error(OrgError::LastOwner).status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            map_org_error(OrgError::InvalidEmail).status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            map_org_error(OrgError::UnknownRole).status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn invitations_expire_within_the_documented_window() {
        let expiry = expires_at();
        assert!(expiry.len() >= 20, "{expiry}");
        assert!(expiry > now_rfc3339(), "{expiry}");
    }
}
