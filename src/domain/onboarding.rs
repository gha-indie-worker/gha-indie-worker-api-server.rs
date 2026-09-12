#![forbid(unsafe_code)]

//! The two onboarding state machines.
//!
//! B2B (organisation): `Created → VerifiedDomain → SeatsAllocated → BillingLinked → Active`
//! B2C (individual):   `Signup → EmailVerified → WorkspaceCreated → Active`
//!
//! Both are total functions over `(state, event)`. There is no `_ => Ok(state)`
//! catch-all anywhere: an event that is not on an edge is a typed error, so a
//! replayed webhook or a double-clicked button cannot skip a step.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::orgs::OrgError;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrgOnboardingState {
    #[default]
    Created,
    VerifiedDomain,
    SeatsAllocated,
    BillingLinked,
    Active,
}

impl OrgOnboardingState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::VerifiedDomain => "verified_domain",
            Self::SeatsAllocated => "seats_allocated",
            Self::BillingLinked => "billing_linked",
            Self::Active => "active",
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Active)
    }

    /// The single event that may be applied next, or `None` when active.
    #[must_use]
    pub const fn next_event(self) -> Option<&'static str> {
        match self {
            Self::Created => Some("verify_domain"),
            Self::VerifiedDomain => Some("allocate_seats"),
            Self::SeatsAllocated => Some("link_billing"),
            Self::BillingLinked => Some("activate"),
            Self::Active => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum OrgOnboardingEvent {
    VerifyDomain,
    AllocateSeats { seats: u32 },
    LinkBilling,
    Activate,
}

impl OrgOnboardingEvent {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VerifyDomain => "verify_domain",
            Self::AllocateSeats { .. } => "allocate_seats",
            Self::LinkBilling => "link_billing",
            Self::Activate => "activate",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserOnboardingState {
    #[default]
    Signup,
    EmailVerified,
    WorkspaceCreated,
    Active,
}

impl UserOnboardingState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Signup => "signup",
            Self::EmailVerified => "email_verified",
            Self::WorkspaceCreated => "workspace_created",
            Self::Active => "active",
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Active)
    }

    #[must_use]
    pub const fn next_event(self) -> Option<&'static str> {
        match self {
            Self::Signup => Some("verify_email"),
            Self::EmailVerified => Some("create_workspace"),
            Self::WorkspaceCreated => Some("activate"),
            Self::Active => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum UserOnboardingEvent {
    VerifyEmail,
    CreateWorkspace,
    Activate,
}

impl UserOnboardingEvent {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VerifyEmail => "verify_email",
            Self::CreateWorkspace => "create_workspace",
            Self::Activate => "activate",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum OnboardingError {
    #[error("onboarding cannot move from {from} via {event}: expected {expected:?}")]
    IllegalTransition {
        from: &'static str,
        event: &'static str,
        expected: Option<&'static str>,
    },
    #[error("organisation domain must be verified before seats are allocated")]
    DomainNotVerified,
    #[error(transparent)]
    Org(#[from] OrgError),
}

/// B2B onboarding transition.
///
/// `verified_domain` is threaded in because "seats allocated" is only reachable
/// for an organisation whose domain is on record — the state alone is not
/// enough evidence, so the caller must supply it.
///
/// # Errors
/// Returns [`OnboardingError::IllegalTransition`] for any edge not on the
/// machine, [`OnboardingError::DomainNotVerified`] when the domain evidence is
/// missing, and [`OnboardingError::Org`] when the seat count is out of bounds.
pub fn advance_org(
    from: OrgOnboardingState,
    event: OrgOnboardingEvent,
    verified_domain: bool,
) -> Result<OrgOnboardingState, OnboardingError> {
    match (from, event) {
        (OrgOnboardingState::Created, OrgOnboardingEvent::VerifyDomain) => {
            Ok(OrgOnboardingState::VerifiedDomain)
        }
        (OrgOnboardingState::VerifiedDomain, OrgOnboardingEvent::AllocateSeats { seats }) => {
            if !verified_domain {
                return Err(OnboardingError::DomainNotVerified);
            }
            if seats == 0 || seats > 10_000 {
                return Err(OnboardingError::Org(OrgError::InvalidSeatCount));
            }
            Ok(OrgOnboardingState::SeatsAllocated)
        }
        (OrgOnboardingState::SeatsAllocated, OrgOnboardingEvent::LinkBilling) => {
            Ok(OrgOnboardingState::BillingLinked)
        }
        (OrgOnboardingState::BillingLinked, OrgOnboardingEvent::Activate) => {
            Ok(OrgOnboardingState::Active)
        }
        (from, event) => Err(OnboardingError::IllegalTransition {
            from: from.as_str(),
            event: event.as_str(),
            expected: from.next_event(),
        }),
    }
}

/// B2C onboarding transition.
///
/// # Errors
/// Returns [`OnboardingError::IllegalTransition`] for any edge not on the
/// machine.
pub const fn advance_user(
    from: UserOnboardingState,
    event: UserOnboardingEvent,
) -> Result<UserOnboardingState, OnboardingError> {
    match (from, event) {
        (UserOnboardingState::Signup, UserOnboardingEvent::VerifyEmail) => {
            Ok(UserOnboardingState::EmailVerified)
        }
        (UserOnboardingState::EmailVerified, UserOnboardingEvent::CreateWorkspace) => {
            Ok(UserOnboardingState::WorkspaceCreated)
        }
        (UserOnboardingState::WorkspaceCreated, UserOnboardingEvent::Activate) => {
            Ok(UserOnboardingState::Active)
        }
        (from, event) => Err(OnboardingError::IllegalTransition {
            from: from.as_str(),
            event: event.as_str(),
            expected: from.next_event(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORG_PATH: [(OrgOnboardingState, OrgOnboardingEvent, OrgOnboardingState); 4] = [
        (
            OrgOnboardingState::Created,
            OrgOnboardingEvent::VerifyDomain,
            OrgOnboardingState::VerifiedDomain,
        ),
        (
            OrgOnboardingState::VerifiedDomain,
            OrgOnboardingEvent::AllocateSeats { seats: 5 },
            OrgOnboardingState::SeatsAllocated,
        ),
        (
            OrgOnboardingState::SeatsAllocated,
            OrgOnboardingEvent::LinkBilling,
            OrgOnboardingState::BillingLinked,
        ),
        (
            OrgOnboardingState::BillingLinked,
            OrgOnboardingEvent::Activate,
            OrgOnboardingState::Active,
        ),
    ];

    #[test]
    fn the_org_happy_path_walks_every_state_in_order() {
        let mut state = OrgOnboardingState::Created;
        for (from, event, expected) in ORG_PATH {
            assert_eq!(state, from);
            state = advance_org(state, event, true).expect("edge on the machine");
            assert_eq!(state, expected);
        }
        assert!(state.is_terminal());
        assert_eq!(state.next_event(), None);
    }

    #[test]
    fn the_org_machine_refuses_every_skipped_step() {
        for (from, _, _) in ORG_PATH {
            for event in [
                OrgOnboardingEvent::VerifyDomain,
                OrgOnboardingEvent::AllocateSeats { seats: 5 },
                OrgOnboardingEvent::LinkBilling,
                OrgOnboardingEvent::Activate,
            ] {
                let on_path = ORG_PATH
                    .iter()
                    .any(|(state, edge, _)| *state == from && edge.as_str() == event.as_str());
                assert_eq!(
                    advance_org(from, event, true).is_ok(),
                    on_path,
                    "{from:?} + {event:?}"
                );
            }
        }
    }

    #[test]
    fn an_active_org_absorbs_nothing() {
        let error = advance_org(
            OrgOnboardingState::Active,
            OrgOnboardingEvent::Activate,
            true,
        )
        .expect_err("terminal states have no outgoing edges");
        assert!(matches!(
            error,
            OnboardingError::IllegalTransition { expected: None, .. }
        ));
    }

    #[test]
    fn seats_require_domain_evidence_and_a_sane_count() {
        assert_eq!(
            advance_org(
                OrgOnboardingState::VerifiedDomain,
                OrgOnboardingEvent::AllocateSeats { seats: 5 },
                false,
            ),
            Err(OnboardingError::DomainNotVerified)
        );
        assert_eq!(
            advance_org(
                OrgOnboardingState::VerifiedDomain,
                OrgOnboardingEvent::AllocateSeats { seats: 0 },
                true,
            ),
            Err(OnboardingError::Org(OrgError::InvalidSeatCount))
        );
        assert_eq!(
            advance_org(
                OrgOnboardingState::VerifiedDomain,
                OrgOnboardingEvent::AllocateSeats { seats: 10_001 },
                true,
            ),
            Err(OnboardingError::Org(OrgError::InvalidSeatCount))
        );
    }

    #[test]
    fn the_user_happy_path_walks_every_state_in_order() {
        let mut state = UserOnboardingState::Signup;
        for (event, expected) in [
            (
                UserOnboardingEvent::VerifyEmail,
                UserOnboardingState::EmailVerified,
            ),
            (
                UserOnboardingEvent::CreateWorkspace,
                UserOnboardingState::WorkspaceCreated,
            ),
            (UserOnboardingEvent::Activate, UserOnboardingState::Active),
        ] {
            state = advance_user(state, event).expect("edge on the machine");
            assert_eq!(state, expected);
        }
        assert!(state.is_terminal());
    }

    #[test]
    fn the_user_machine_refuses_skipped_and_replayed_steps() {
        assert!(advance_user(
            UserOnboardingState::Signup,
            UserOnboardingEvent::CreateWorkspace
        )
        .is_err());
        assert!(advance_user(UserOnboardingState::Signup, UserOnboardingEvent::Activate).is_err());
        assert!(advance_user(UserOnboardingState::Active, UserOnboardingEvent::Activate).is_err());
        assert!(advance_user(
            UserOnboardingState::EmailVerified,
            UserOnboardingEvent::VerifyEmail
        )
        .is_err());
    }

    #[test]
    fn states_round_trip_through_their_wire_form() {
        assert_eq!(
            serde_json::to_string(&OrgOnboardingState::SeatsAllocated).expect("serialise"),
            "\"seats_allocated\""
        );
        assert_eq!(
            serde_json::to_string(&UserOnboardingState::WorkspaceCreated).expect("serialise"),
            "\"workspace_created\""
        );
        let event: OrgOnboardingEvent =
            serde_json::from_str(r#"{"event":"allocate_seats","seats":12}"#).expect("deserialise");
        assert_eq!(event, OrgOnboardingEvent::AllocateSeats { seats: 12 });
    }
}
