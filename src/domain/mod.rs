#![forbid(unsafe_code)]

//! The functional core.
//!
//! Every module here is pure: values in, values out, typed errors, explicit
//! state transitions, no I/O and no `async`. Effects (HTTP, SeaORM, NATS,
//! WebSocket) live in `crate::routes`, `crate::transport` and `crate::store`
//! and call into these functions. That boundary is what makes the state
//! machines testable without a database or a network.

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

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// RFC 3339 timestamp for wire payloads. Falls back to the Unix epoch rather
/// than panicking, because a formatting failure must never take a request down.
#[must_use]
pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// A bounded, portable identifier: the shape we accept for slugs, worker names,
/// profile names and job names. Rejecting everything else keeps path traversal,
/// header injection and SQL identifier games structurally impossible.
#[must_use]
pub fn is_portable_identifier(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Lowercase 40-character hexadecimal commit SHA. Branches and tags are
/// deliberately not accepted anywhere in the run pipeline.
#[must_use]
pub fn is_commit_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// `owner/repo`, both segments portable, no traversal, no host component.
#[must_use]
pub fn is_repository_slug(value: &str) -> bool {
    let mut parts = value.split('/');
    let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    is_portable_identifier(owner, 64) && is_portable_identifier(repo, 128)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_reject_separators_and_whitespace() {
        assert!(is_portable_identifier("rust-verify", 64));
        assert!(is_portable_identifier("gha_indie.worker", 64));
        assert!(!is_portable_identifier("", 64));
        assert!(!is_portable_identifier("has space", 64));
        assert!(!is_portable_identifier("has/slash", 64));
        assert!(!is_portable_identifier("../escape", 64));
        assert!(!is_portable_identifier("toolong", 3));
    }

    #[test]
    fn only_full_lowercase_shas_are_commit_shas() {
        assert!(is_commit_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_commit_sha("main"));
        assert!(!is_commit_sha("0123456789abcdef"));
        assert!(!is_commit_sha("z123456789abcdef0123456789abcdef01234567"));
    }

    #[test]
    fn repository_slugs_are_exactly_two_portable_segments() {
        assert!(is_repository_slug("gha-indie-worker/gha-clone-server.rs"));
        assert!(!is_repository_slug("gha-indie-worker"));
        assert!(!is_repository_slug("a/b/c"));
        assert!(!is_repository_slug("../../etc/passwd"));
    }

    #[test]
    fn timestamps_are_rfc3339() {
        let value = now_rfc3339();
        assert!(value.len() >= 20, "{value}");
        assert!(value.ends_with('Z') || value.ends_with("+00:00"), "{value}");
    }
}
