#![forbid(unsafe_code)]

//! B2B organisations: membership roles, seats and invitations.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use super::is_portable_identifier;

/// Membership role inside one organisation. The order of the variants is the
/// privilege lattice used by [`OrgRole::at_least`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrgRole {
    /// Read-only plus billing surfaces. Deliberately the weakest role so a
    /// finance seat can never reach run execution.
    Billing,
    Member,
    Admin,
    Owner,
}

impl OrgRole {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Billing => "billing",
            Self::Member => "member",
            Self::Admin => "admin",
            Self::Owner => "owner",
        }
    }

    /// # Errors
    /// Returns [`OrgError::UnknownRole`] for anything outside the four roles.
    pub fn parse(value: &str) -> Result<Self, OrgError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "owner" => Ok(Self::Owner),
            "admin" => Ok(Self::Admin),
            "member" => Ok(Self::Member),
            "billing" => Ok(Self::Billing),
            _ => Err(OrgError::UnknownRole),
        }
    }

    /// Privilege comparison on the lattice. `Billing` is not "less than
    /// member" for billing surfaces — those are checked with
    /// [`OrgRole::can_manage_billing`] instead.
    #[must_use]
    pub fn at_least(self, required: Self) -> bool {
        self >= required
    }

    #[must_use]
    pub const fn can_invite(self) -> bool {
        matches!(self, Self::Owner | Self::Admin)
    }

    #[must_use]
    pub const fn can_manage_billing(self) -> bool {
        matches!(self, Self::Owner | Self::Billing)
    }

    #[must_use]
    pub const fn can_run_workloads(self) -> bool {
        matches!(self, Self::Owner | Self::Admin | Self::Member)
    }

    /// Removing the last owner would strand the organisation, so it is illegal.
    #[must_use]
    pub const fn is_owner(self) -> bool {
        matches!(self, Self::Owner)
    }
}

impl fmt::Display for OrgRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum OrgError {
    #[error("unknown organisation role")]
    UnknownRole,
    #[error("organisation slug must be 1-64 characters of [a-z0-9._-]")]
    InvalidSlug,
    #[error("organisation name must be 1-200 characters")]
    InvalidName,
    #[error("verified domain must be a bounded DNS name")]
    InvalidDomain,
    #[error("invitation email must contain exactly one '@' and be at most 320 bytes")]
    InvalidEmail,
    #[error("seat count must be between 1 and 10000")]
    InvalidSeatCount,
    #[error("an organisation must keep at least one owner")]
    LastOwner,
    #[error("seats are exhausted")]
    SeatsExhausted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Org {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub verified_domain: Option<String>,
    pub seats: u32,
    pub created_at: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct NewOrg {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub verified_domain: Option<String>,
}

impl Org {
    /// Smart constructor: the only way to build an [`Org`] from untrusted input.
    ///
    /// # Errors
    /// Returns [`OrgError`] when the slug, name or domain is outside its bound.
    pub fn create(id: Uuid, input: NewOrg, created_at: String) -> Result<Self, OrgError> {
        let slug = input.slug.trim().to_ascii_lowercase();
        if !is_portable_identifier(&slug, 64) {
            return Err(OrgError::InvalidSlug);
        }
        let name = input.name.trim().to_owned();
        if name.is_empty() || name.chars().count() > 200 {
            return Err(OrgError::InvalidName);
        }
        let verified_domain = match input.verified_domain {
            None => None,
            Some(domain) => Some(normalize_domain(&domain)?),
        };
        Ok(Self {
            id,
            slug,
            name,
            verified_domain,
            seats: 0,
            created_at,
        })
    }
}

/// # Errors
/// Returns [`OrgError::InvalidDomain`] for anything that is not a bounded,
/// lowercase DNS name with at least one dot.
pub fn normalize_domain(value: &str) -> Result<String, OrgError> {
    let domain = value.trim().trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = domain.split('.').collect();
    let valid = domain.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        });
    if valid {
        Ok(domain)
    } else {
        Err(OrgError::InvalidDomain)
    }
}

/// # Errors
/// Returns [`OrgError::InvalidEmail`] for an address that is not a single
/// bounded `local@domain` pair.
pub fn normalize_email(value: &str) -> Result<String, OrgError> {
    let email = value.trim().to_ascii_lowercase();
    if email.len() > 320 {
        return Err(OrgError::InvalidEmail);
    }
    let mut parts = email.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(OrgError::InvalidEmail);
    };
    if local.is_empty() || local.len() > 64 || local.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err(OrgError::InvalidEmail);
    }
    normalize_domain(domain).map_err(|_| OrgError::InvalidEmail)?;
    Ok(email)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OrgMember {
    pub org_id: Uuid,
    pub user_id: Uuid,
    pub role: OrgRole,
    pub joined_at: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvitationState {
    Pending,
    Accepted,
    Revoked,
    Expired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvitationEvent {
    Accept,
    Revoke,
    Expire,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("invitation cannot move from {from:?} via {event:?}")]
pub struct InvitationTransitionError {
    pub from: InvitationState,
    pub event: InvitationEvent,
}

/// The invitation state machine. Terminal states absorb nothing: replaying a
/// delivery on an accepted invitation is an explicit error, not a silent no-op.
///
/// # Errors
/// Returns [`InvitationTransitionError`] for any edge not on the machine.
pub const fn advance_invitation(
    from: InvitationState,
    event: InvitationEvent,
) -> Result<InvitationState, InvitationTransitionError> {
    match (from, event) {
        (InvitationState::Pending, InvitationEvent::Accept) => Ok(InvitationState::Accepted),
        (InvitationState::Pending, InvitationEvent::Revoke) => Ok(InvitationState::Revoked),
        (InvitationState::Pending, InvitationEvent::Expire) => Ok(InvitationState::Expired),
        (InvitationState::Accepted | InvitationState::Revoked | InvitationState::Expired, _) => {
            Err(InvitationTransitionError { from, event })
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Invitation {
    pub id: Uuid,
    pub org_id: Uuid,
    pub email: String,
    pub role: OrgRole,
    pub state: InvitationState,
    pub created_at: String,
    pub expires_at: String,
    /// SHA-256 of the invitation token. The token itself is returned exactly
    /// once, in the creation response, and is never persisted or logged.
    #[serde(skip)]
    pub token_digest: [u8; 32],
}

#[derive(Clone, Debug, Deserialize)]
pub struct NewInvitation {
    pub email: String,
    #[serde(default = "default_invite_role")]
    pub role: String,
}

fn default_invite_role() -> String {
    "member".to_owned()
}

/// Seat accounting is a pure function so the HTTP layer cannot drift from it.
///
/// # Errors
/// Returns [`OrgError::SeatsExhausted`] when the allocation would exceed the
/// purchased seat count.
pub const fn claim_seat(seats: u32, occupied: u32) -> Result<u32, OrgError> {
    if occupied >= seats {
        return Err(OrgError::SeatsExhausted);
    }
    Ok(occupied + 1)
}

/// # Errors
/// Returns [`OrgError::LastOwner`] when removing this member would leave the
/// organisation without an owner.
pub const fn may_remove_member(role: OrgRole, owner_count: u32) -> Result<(), OrgError> {
    if role.is_owner() && owner_count <= 1 {
        return Err(OrgError::LastOwner);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_lattice_orders_owner_above_everything() {
        assert!(OrgRole::Owner.at_least(OrgRole::Admin));
        assert!(OrgRole::Admin.at_least(OrgRole::Member));
        assert!(!OrgRole::Member.at_least(OrgRole::Admin));
        assert!(!OrgRole::Billing.at_least(OrgRole::Member));
    }

    #[test]
    fn role_capabilities_are_explicit() {
        assert!(OrgRole::Admin.can_invite());
        assert!(!OrgRole::Member.can_invite());
        assert!(OrgRole::Billing.can_manage_billing());
        assert!(!OrgRole::Billing.can_run_workloads());
        assert!(OrgRole::Member.can_run_workloads());
    }

    #[test]
    fn roles_round_trip_through_their_wire_form() {
        for role in [
            OrgRole::Owner,
            OrgRole::Admin,
            OrgRole::Member,
            OrgRole::Billing,
        ] {
            assert_eq!(OrgRole::parse(role.as_str()).expect("round trip"), role);
        }
        assert_eq!(OrgRole::parse("ADMIN"), Ok(OrgRole::Admin));
        assert_eq!(OrgRole::parse("root"), Err(OrgError::UnknownRole));
    }

    #[test]
    fn org_creation_rejects_unusable_input() {
        let ok = Org::create(
            Uuid::nil(),
            NewOrg {
                slug: "IndieBuild".into(),
                name: "  Indie Build  ".into(),
                verified_domain: Some("IndieBuild.dev.".into()),
            },
            "2026-01-01T00:00:00Z".into(),
        )
        .expect("valid org");
        assert_eq!(ok.slug, "indiebuild");
        assert_eq!(ok.name, "Indie Build");
        assert_eq!(ok.verified_domain.as_deref(), Some("indiebuild.dev"));
        assert_eq!(ok.seats, 0);

        for bad in ["", "has space", "has/slash"] {
            assert!(Org::create(
                Uuid::nil(),
                NewOrg {
                    slug: bad.into(),
                    name: "ok".into(),
                    verified_domain: None,
                },
                String::new(),
            )
            .is_err());
        }
    }

    #[test]
    fn domains_and_emails_are_normalised_or_rejected() {
        assert_eq!(
            normalize_domain("Example.COM").as_deref(),
            Ok("example.com")
        );
        assert_eq!(normalize_domain("localhost"), Err(OrgError::InvalidDomain));
        assert_eq!(normalize_domain("-bad.com"), Err(OrgError::InvalidDomain));
        assert_eq!(
            normalize_email(" Alex@Example.com ").as_deref(),
            Ok("alex@example.com")
        );
        assert_eq!(normalize_email("a@b@c.com"), Err(OrgError::InvalidEmail));
        assert_eq!(normalize_email("nodomain"), Err(OrgError::InvalidEmail));
    }

    #[test]
    fn invitations_have_exactly_three_edges_out_of_pending() {
        assert_eq!(
            advance_invitation(InvitationState::Pending, InvitationEvent::Accept),
            Ok(InvitationState::Accepted)
        );
        assert_eq!(
            advance_invitation(InvitationState::Pending, InvitationEvent::Revoke),
            Ok(InvitationState::Revoked)
        );
        assert_eq!(
            advance_invitation(InvitationState::Pending, InvitationEvent::Expire),
            Ok(InvitationState::Expired)
        );
        for terminal in [
            InvitationState::Accepted,
            InvitationState::Revoked,
            InvitationState::Expired,
        ] {
            assert!(advance_invitation(terminal, InvitationEvent::Accept).is_err());
        }
    }

    #[test]
    fn seat_and_owner_invariants_hold() {
        assert_eq!(claim_seat(3, 2), Ok(3));
        assert_eq!(claim_seat(3, 3), Err(OrgError::SeatsExhausted));
        assert_eq!(
            may_remove_member(OrgRole::Owner, 1),
            Err(OrgError::LastOwner)
        );
        assert_eq!(may_remove_member(OrgRole::Owner, 2), Ok(()));
        assert_eq!(may_remove_member(OrgRole::Admin, 1), Ok(()));
    }
}
