#![forbid(unsafe_code)]

//! `/v1/users/me` — the individual (B2C) surface.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};

use crate::auth::{Actor, VerifiedActor};
use crate::domain::now_rfc3339;
use crate::domain::users::{normalize_display_name, Me, Membership, UpdateMe, User};
use crate::error::ApiError;
use crate::state::AppState;
use uuid::Uuid;

#[must_use]
pub fn router() -> Router<AppState> {
    Router::new().route("/users/me", get(me).patch(update_me))
}

/// Resolve the caller's user record, creating it on first sight.
///
/// Registration is implicit: an actor the auth authority already verified is,
/// by definition, a real user. Making them call a separate `POST /users` first
/// would only add a state where a verified caller has no record.
///
/// # Errors
/// Returns [`ApiError::BadRequest`] when the authority supplied an email this
/// service will not store.
pub async fn ensure_user(state: &AppState, actor: &VerifiedActor) -> Result<User, ApiError> {
    if let Some(existing) = state.store.user_by_subject(&actor.subject).await {
        return Ok(existing);
    }
    let user = User::create(
        Uuid::now_v7(),
        actor.subject.clone(),
        actor.email.as_deref(),
        now_rfc3339(),
    )
    .map_err(|error| ApiError::bad_request(error.to_string()))?;
    state.store.insert_user(user.clone()).await;
    Ok(user)
}

/// Build the `/v1/users/me` projection.
async fn projection(state: &AppState, actor: &VerifiedActor, user: User) -> Me {
    let memberships = state
        .store
        .memberships_of(user.id)
        .await
        .into_iter()
        .map(|(org, role)| Membership {
            org_id: org.id,
            org_slug: org.slug,
            role,
        })
        .collect();
    Me {
        id: user.id,
        subject: user.subject,
        email: user.email,
        display_name: user.display_name,
        onboarding: state.store.user_onboarding(user.id).await,
        auth_source: actor.source.as_str().to_owned(),
        scopes: actor.scopes.iter().cloned().collect(),
        memberships,
    }
}

/// `GET /v1/users/me`
async fn me(State(state): State<AppState>, actor: Actor) -> Result<Json<Me>, ApiError> {
    let user = ensure_user(&state, &actor).await?;
    Ok(Json(projection(&state, &actor, user).await))
}

/// `PATCH /v1/users/me`
///
/// Only the display name is writable. Email and subject belong to the auth
/// authority; letting this service edit them would put two systems in charge of
/// one identity.
async fn update_me(
    State(state): State<AppState>,
    actor: Actor,
    Json(input): Json<UpdateMe>,
) -> Result<Json<Me>, ApiError> {
    let user = ensure_user(&state, &actor).await?;
    if let Some(display_name) = input.display_name.as_deref() {
        let display_name = normalize_display_name(display_name)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        state.store.set_display_name(user.id, display_name).await;
    }
    let user = state
        .store
        .user_by_subject(&actor.subject)
        .await
        .unwrap_or(user);
    Ok(Json(projection(&state, &actor, user).await))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::auth::AuthSource;
    use crate::config::ApiConfig;
    use crate::domain::onboarding::UserOnboardingState;

    fn actor() -> VerifiedActor {
        VerifiedActor {
            subject: "sub-1".to_owned(),
            org_id: None,
            roles: BTreeSet::new(),
            scopes: BTreeSet::from(["runs:read".to_owned()]),
            source: AuthSource::SupabaseJwt,
            email: Some("Alex@Example.COM".to_owned()),
        }
    }

    async fn state() -> AppState {
        let config = ApiConfig::from_lookup(|_| None).expect("defaults parse");
        AppState::build(config).await.expect("state builds")
    }

    #[tokio::test]
    async fn a_verified_caller_is_registered_on_first_sight() {
        let state = state().await;
        let actor = actor();

        let first = ensure_user(&state, &actor).await.expect("registers");
        assert_eq!(first.subject, "sub-1");
        assert_eq!(first.email.as_deref(), Some("alex@example.com"));
        assert_eq!(first.onboarding, UserOnboardingState::Signup);

        // Idempotent: the same subject resolves to the same record.
        let second = ensure_user(&state, &actor).await.expect("resolves");
        assert_eq!(first.id, second.id);
    }

    #[tokio::test]
    async fn the_projection_carries_the_auth_source_and_no_token() {
        let state = state().await;
        let actor = actor();
        let user = ensure_user(&state, &actor).await.expect("registers");
        let me = projection(&state, &actor, user).await;

        assert_eq!(me.auth_source, "supabase-jwt");
        assert_eq!(me.scopes, vec!["runs:read".to_owned()]);
        assert!(me.memberships.is_empty());

        let encoded = serde_json::to_string(&me).expect("serialise");
        assert!(!encoded.contains("token"));
        assert!(encoded.contains("supabase-jwt"));
    }

    #[tokio::test]
    async fn an_unusable_email_from_the_authority_is_a_bad_request_not_a_panic() {
        let state = state().await;
        let mut actor = actor();
        actor.email = Some("not-an-email".to_owned());
        let error = ensure_user(&state, &actor)
            .await
            .expect_err("an unusable email is refused");
        assert!(matches!(error, ApiError::BadRequest(_)));
    }
}
