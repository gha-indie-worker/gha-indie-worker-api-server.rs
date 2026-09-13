#![forbid(unsafe_code)]

//! `/v1` — the JSON API surface.
//!
//! Each module owns one resource and does exactly three things: parse the
//! request, call a pure function in [`crate::domain`], and persist through
//! [`crate::store::Store`]. Business rules never live here.

pub mod capabilities;
pub mod chat;
pub mod embeddings;
pub mod onboarding;
pub mod orgs;
pub mod plans;
pub mod runs;
pub mod sync;
pub mod users;
pub mod webhooks;
pub mod workers;

use axum::routing::get;
use axum::Router;

use crate::state::AppState;

#[must_use]
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/capabilities", get(capabilities::capabilities))
        .route("/ws", get(crate::transport::ws::handler))
        .merge(orgs::router())
        .merge(users::router())
        .merge(onboarding::router())
        .merge(plans::router())
        .merge(runs::router())
        .merge(workers::router())
        .merge(webhooks::router())
        .merge(chat::router())
        .merge(embeddings::router())
        .merge(sync::router())
}
