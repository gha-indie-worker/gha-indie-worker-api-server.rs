#![forbid(unsafe_code)]

//! `/v1/chat/*` — a reverse proxy to `ORES_CHAT_API_BASE`.
//!
//! This server does not host conversations; it forwards them with the *verified*
//! actor attached as headers. Three things make that safe:
//!
//! * every actor header this service injects is **stripped from the inbound
//!   request first**, so a client cannot forge an identity by sending one;
//! * the upstream path is allow-listed by [`crate::domain::chat::upstream_path`],
//!   so a caller cannot reach an arbitrary upstream route through the proxy;
//! * the surface decides whether an actor is required at all, so the public
//!   sales widget works anonymously while support and internal do not.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;

use crate::auth::MaybeActor;
use crate::domain::chat::{actor_headers, upstream_path, ChatError, ChatSurface, INJECTED_HEADERS};
use crate::error::ApiError;
use crate::state::AppState;

/// Response headers worth passing back. Everything else (hop-by-hop headers,
/// upstream cookies, upstream auth) is dropped.
const FORWARDED_RESPONSE_HEADERS: [&str; 2] = ["content-type", "cache-control"];

/// Request headers worth forwarding, on top of the injected actor headers.
const FORWARDED_REQUEST_HEADERS: [&str; 3] = ["content-type", "accept", "accept-language"];

#[must_use]
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/chat/{surface}", any(proxy_root))
        .route("/chat/{surface}/{*rest}", any(proxy))
}

async fn proxy_root(
    state: State<AppState>,
    actor: MaybeActor,
    Path(surface): Path<String>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    forward(state, actor, surface, String::new(), method, headers, body).await
}

async fn proxy(
    state: State<AppState>,
    actor: MaybeActor,
    Path((surface, rest)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    forward(state, actor, surface, rest, method, headers, body).await
}

async fn forward(
    State(state): State<AppState>,
    MaybeActor(actor): MaybeActor,
    surface: String,
    rest: String,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let surface = ChatSurface::parse(&surface).map_err(map_chat_error)?;
    let base = state
        .config
        .chat
        .api_base
        .as_deref()
        .ok_or(ChatError::NotConfigured)
        .map_err(map_chat_error)?;

    if surface.requires_actor() && actor.is_none() {
        return Err(ApiError::Unauthenticated);
    }
    if let (Some(scope), Some(actor)) = (surface.required_scope(), actor.as_ref()) {
        actor.require_scope(scope)?;
    }

    let path = upstream_path(surface, &rest).map_err(map_chat_error)?;
    let url = format!("{}/{path}", base.trim_end_matches('/'));

    let mut request = state
        .http
        .request(method, &url)
        .timeout(state.config.chat.timeout);

    // Forward only the headers the upstream needs, and never one this service
    // injects — a client-supplied actor header must not survive the proxy.
    for name in FORWARDED_REQUEST_HEADERS {
        if INJECTED_HEADERS.contains(&name) {
            continue;
        }
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            request = request.header(name, value);
        }
    }
    for (name, value) in actor_headers(actor.as_ref(), surface) {
        request = request.header(name, value);
    }
    if !body.is_empty() {
        request = request.body(body.to_vec());
    }

    let upstream = request.send().await.map_err(|error| {
        tracing::warn!(%error, surface = surface.as_str(), "chat upstream request failed");
        map_chat_error(ChatError::Unavailable)
    })?;

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response_headers = HeaderMap::new();
    for name in FORWARDED_RESPONSE_HEADERS {
        if let Some(value) = upstream.headers().get(name) {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_bytes(value.as_bytes()),
            ) {
                response_headers.insert(name, value);
            }
        }
    }
    let payload = upstream.bytes().await.map_err(|error| {
        tracing::warn!(%error, "chat upstream body could not be read");
        map_chat_error(ChatError::Unavailable)
    })?;

    Ok((status, response_headers, payload).into_response())
}

fn map_chat_error(error: ChatError) -> ApiError {
    match error {
        ChatError::UnknownSurface => ApiError::NotFound,
        ChatError::ForbiddenPath => ApiError::Forbidden,
        ChatError::NotConfigured | ChatError::Unavailable => ApiError::Unavailable("chat upstream"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_forwarded_request_header_is_one_this_service_injects() {
        for name in FORWARDED_REQUEST_HEADERS {
            assert!(
                !INJECTED_HEADERS.contains(&name),
                "{name} would let a client forge an actor"
            );
        }
    }

    #[test]
    fn no_forwarded_response_header_leaks_upstream_credentials() {
        for name in FORWARDED_RESPONSE_HEADERS {
            assert!(!name.contains("cookie"), "{name}");
            assert!(!name.contains("auth"), "{name}");
        }
    }

    #[test]
    fn chat_errors_map_to_the_status_a_client_can_act_on() {
        assert_eq!(
            map_chat_error(ChatError::UnknownSurface).status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            map_chat_error(ChatError::ForbiddenPath).status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            map_chat_error(ChatError::NotConfigured).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            map_chat_error(ChatError::Unavailable).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn the_upstream_url_is_built_from_the_allow_listed_path_only() {
        let base = "https://chat.example/";
        let path =
            upstream_path(ChatSurface::CustomerSupport, "conversations/abc").expect("allow-listed");
        let url = format!("{}/{path}", base.trim_end_matches('/'));
        assert_eq!(
            url,
            "https://chat.example/v1/chat/customer-support/conversations/abc"
        );
        assert!(upstream_path(ChatSurface::CustomerSupport, "../admin").is_err());
    }
}
