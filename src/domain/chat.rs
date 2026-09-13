#![forbid(unsafe_code)]

//! ores-chat surfaces and the actor context injected into the proxied request.
//!
//! The api-server never terminates a chat conversation; it reverse-proxies
//! `/v1/chat/*` to `ORES_CHAT_API_BASE` and attaches the *verified* actor as
//! headers. The pure part — which surface a path names, which headers an actor
//! produces, and which upstream paths are reachable — lives here so the effect
//! layer in `routes::v1::chat` stays a thin `reqwest` call.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::auth::VerifiedActor;

/// The three chat surfaces this fleet runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChatSurface {
    /// Anonymous marketing/sales chat on `app.` and the public site.
    VisitorSales,
    /// Logged-in customer support.
    CustomerSupport,
    /// Staff-only internal support. Requires an explicit scope.
    Internal,
}

impl ChatSurface {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VisitorSales => "visitor-sales",
            Self::CustomerSupport => "customer-support",
            Self::Internal => "internal",
        }
    }

    /// Whether an authenticated actor is required to reach this surface.
    #[must_use]
    pub const fn requires_actor(self) -> bool {
        match self {
            Self::VisitorSales => false,
            Self::CustomerSupport | Self::Internal => true,
        }
    }

    /// The scope an actor must hold, if any.
    #[must_use]
    pub const fn required_scope(self) -> Option<&'static str> {
        match self {
            Self::VisitorSales | Self::CustomerSupport => None,
            Self::Internal => Some("chat:internal"),
        }
    }

    /// # Errors
    /// Returns [`ChatError::UnknownSurface`] for any other segment.
    pub fn parse(value: &str) -> Result<Self, ChatError> {
        match value {
            "visitor-sales" => Ok(Self::VisitorSales),
            "customer-support" => Ok(Self::CustomerSupport),
            "internal" => Ok(Self::Internal),
            _ => Err(ChatError::UnknownSurface),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum ChatError {
    #[error("unknown chat surface")]
    UnknownSurface,
    #[error("chat upstream is not configured")]
    NotConfigured,
    #[error("chat upstream path is not allowed")]
    ForbiddenPath,
    #[error("chat upstream is unavailable")]
    Unavailable,
}

/// Header names carrying the verified actor to the chat upstream. These are set
/// by this server only; any inbound copy from the client is dropped first.
pub const HEADER_SUBJECT: &str = "x-giw-actor-subject";
pub const HEADER_ORG: &str = "x-giw-actor-org";
pub const HEADER_ROLES: &str = "x-giw-actor-roles";
pub const HEADER_SCOPES: &str = "x-giw-actor-scopes";
pub const HEADER_SOURCE: &str = "x-giw-actor-source";
pub const HEADER_SURFACE: &str = "x-giw-chat-surface";

/// Every header this server injects. `routes::v1::chat` strips each of these
/// from the inbound request before adding its own, so a client cannot forge an
/// actor by sending the header itself.
pub const INJECTED_HEADERS: [&str; 6] = [
    HEADER_SUBJECT,
    HEADER_ORG,
    HEADER_ROLES,
    HEADER_SCOPES,
    HEADER_SOURCE,
    HEADER_SURFACE,
];

/// Build the actor headers for one proxied request. Pure: the effect layer only
/// has to write these onto the outbound builder.
#[must_use]
pub fn actor_headers(
    actor: Option<&VerifiedActor>,
    surface: ChatSurface,
) -> Vec<(&'static str, String)> {
    let mut headers = vec![(HEADER_SURFACE, surface.as_str().to_owned())];
    if let Some(actor) = actor {
        headers.push((HEADER_SUBJECT, actor.subject.clone()));
        headers.push((HEADER_SOURCE, actor.source.as_str().to_owned()));
        if let Some(org) = actor.org_id {
            headers.push((HEADER_ORG, org.to_string()));
        }
        if !actor.roles.is_empty() {
            headers.push((HEADER_ROLES, join_sorted(&actor.roles)));
        }
        if !actor.scopes.is_empty() {
            headers.push((HEADER_SCOPES, join_sorted(&actor.scopes)));
        }
    }
    headers
}

fn join_sorted(values: &std::collections::BTreeSet<String>) -> String {
    values
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The upstream path suffix a proxied request may reach.
///
/// Only a bounded set of segments is forwarded; there is no traversal, no query
/// smuggling through the path, and no absolute URL.
///
/// # Errors
/// Returns [`ChatError::ForbiddenPath`] for anything else.
pub fn upstream_path(surface: ChatSurface, rest: &str) -> Result<String, ChatError> {
    const ALLOWED: [&str; 4] = ["conversations", "messages", "events", "handoff"];
    let rest = rest.trim_matches('/');
    if rest.is_empty() {
        return Ok(format!("v1/chat/{}", surface.as_str()));
    }
    if rest.contains("..") || rest.contains('\\') || rest.len() > 128 {
        return Err(ChatError::ForbiddenPath);
    }
    let mut segments = rest.split('/');
    let head = segments.next().unwrap_or_default();
    if !ALLOWED.contains(&head) {
        return Err(ChatError::ForbiddenPath);
    }
    for segment in segments {
        if segment.is_empty() || !super::is_portable_identifier(segment, 64) {
            return Err(ChatError::ForbiddenPath);
        }
    }
    Ok(format!("v1/chat/{}/{rest}", surface.as_str()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use uuid::Uuid;

    use super::*;
    use crate::auth::AuthSource;

    fn actor() -> VerifiedActor {
        VerifiedActor {
            subject: "sub-123".to_owned(),
            org_id: Some(Uuid::nil()),
            roles: BTreeSet::from(["admin".to_owned(), "member".to_owned()]),
            scopes: BTreeSet::from(["runs:read".to_owned(), "chat:internal".to_owned()]),
            source: AuthSource::SharedAuth,
            email: None,
        }
    }

    #[test]
    fn surfaces_round_trip_and_declare_their_requirements() {
        for surface in [
            ChatSurface::VisitorSales,
            ChatSurface::CustomerSupport,
            ChatSurface::Internal,
        ] {
            assert_eq!(ChatSurface::parse(surface.as_str()), Ok(surface));
        }
        assert_eq!(ChatSurface::parse("admin"), Err(ChatError::UnknownSurface));

        assert!(!ChatSurface::VisitorSales.requires_actor());
        assert!(ChatSurface::CustomerSupport.requires_actor());
        assert_eq!(
            ChatSurface::Internal.required_scope(),
            Some("chat:internal")
        );
        assert_eq!(ChatSurface::CustomerSupport.required_scope(), None);
    }

    #[test]
    fn an_anonymous_visitor_gets_only_the_surface_header() {
        let headers = actor_headers(None, ChatSurface::VisitorSales);
        assert_eq!(headers, vec![(HEADER_SURFACE, "visitor-sales".to_owned())]);
    }

    #[test]
    fn an_authenticated_actor_is_projected_deterministically() {
        let headers = actor_headers(Some(&actor()), ChatSurface::Internal);
        assert!(headers.contains(&(HEADER_SUBJECT, "sub-123".to_owned())));
        assert!(headers.contains(&(HEADER_SOURCE, "shared-auth".to_owned())));
        assert!(headers.contains(&(HEADER_ROLES, "admin member".to_owned())));
        assert!(headers.contains(&(HEADER_SCOPES, "chat:internal runs:read".to_owned())));
        // Sorted sets make the projection byte-identical across requests.
        assert_eq!(
            actor_headers(Some(&actor()), ChatSurface::Internal),
            headers
        );
    }

    #[test]
    fn every_injected_header_is_listed_for_stripping() {
        let headers = actor_headers(Some(&actor()), ChatSurface::Internal);
        for (name, _) in headers {
            assert!(INJECTED_HEADERS.contains(&name), "{name} is not stripped");
        }
    }

    #[test]
    fn upstream_paths_are_allow_listed_and_traversal_free() {
        assert_eq!(
            upstream_path(ChatSurface::CustomerSupport, ""),
            Ok("v1/chat/customer-support".to_owned())
        );
        assert_eq!(
            upstream_path(ChatSurface::Internal, "/conversations/abc-1/"),
            Ok("v1/chat/internal/conversations/abc-1".to_owned())
        );
        for bad in [
            "../../admin",
            "conversations/../secrets",
            "secrets",
            "conversations\\x",
            "conversations/with space",
        ] {
            assert_eq!(
                upstream_path(ChatSurface::Internal, bad),
                Err(ChatError::ForbiddenPath),
                "{bad}"
            );
        }
    }
}
