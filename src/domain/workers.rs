#![forbid(unsafe_code)]

//! Worker registration, capability matching and heartbeat liveness.
//!
//! Workers are `gha-indie-worker.rs` processes: fixed-profile, immutable-SHA
//! executors. They register their capabilities, take a lease for one job, and
//! must heartbeat inside a TTL or the control plane declares them offline and
//! reclaims the lease.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use super::{is_portable_identifier, plans::Profile};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    /// Accepted by the control plane; has not yet reported readiness.
    #[default]
    Registered,
    Idle,
    Busy,
    /// Finishing its lease, then leaving. Never handed new work.
    Draining,
    Offline,
}

impl WorkerState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Draining => "draining",
            Self::Offline => "offline",
        }
    }

    #[must_use]
    pub const fn accepts_work(self) -> bool {
        matches!(self, Self::Idle)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum WorkerEvent {
    Ready,
    LeaseAcquired,
    LeaseReleased,
    Drain,
    HeartbeatExpired,
    Deregister,
}

impl WorkerEvent {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::LeaseAcquired => "lease_acquired",
            Self::LeaseReleased => "lease_released",
            Self::Drain => "drain",
            Self::HeartbeatExpired => "heartbeat_expired",
            Self::Deregister => "deregister",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("worker in {from} cannot handle {event}")]
pub struct WorkerTransitionError {
    pub from: &'static str,
    pub event: &'static str,
}

/// The worker lifecycle.
///
/// `HeartbeatExpired` and `Deregister` are reachable from every live state:
/// liveness is the control plane's decision, not the worker's.
///
/// # Errors
/// Returns [`WorkerTransitionError`] for any edge not on the machine.
pub const fn advance(
    from: WorkerState,
    event: WorkerEvent,
) -> Result<WorkerState, WorkerTransitionError> {
    match (from, event) {
        (WorkerState::Registered | WorkerState::Idle, WorkerEvent::Ready) => Ok(WorkerState::Idle),
        (WorkerState::Idle, WorkerEvent::LeaseAcquired) => Ok(WorkerState::Busy),
        (WorkerState::Busy, WorkerEvent::LeaseReleased) => Ok(WorkerState::Idle),
        (WorkerState::Registered | WorkerState::Idle | WorkerState::Busy, WorkerEvent::Drain) => {
            Ok(WorkerState::Draining)
        }
        (WorkerState::Draining, WorkerEvent::LeaseReleased)
        | (
            WorkerState::Registered | WorkerState::Idle | WorkerState::Busy | WorkerState::Draining,
            WorkerEvent::HeartbeatExpired | WorkerEvent::Deregister,
        ) => Ok(WorkerState::Offline),
        (from, event) => Err(WorkerTransitionError {
            from: from.as_str(),
            event: event.as_str(),
        }),
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Worker {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    /// Profiles this worker's pinned image can execute.
    pub profiles: BTreeSet<Profile>,
    /// Free-form capability labels (architecture, region, hardware).
    pub labels: BTreeSet<String>,
    pub state: WorkerState,
    pub registered_at: String,
    pub last_heartbeat_at: String,
    /// Monotonic milliseconds since registration, used for TTL arithmetic. The
    /// wall-clock string above is for humans only.
    pub last_heartbeat_ms: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RegisterWorker {
    pub name: String,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum WorkerError {
    #[error("worker name must be 1-128 characters of [A-Za-z0-9._-]")]
    InvalidName,
    #[error("a worker must advertise at least one profile")]
    NoProfiles,
    #[error("a worker may advertise at most 32 labels of at most 64 characters")]
    InvalidLabels,
    #[error("unknown execution profile")]
    UnknownProfile,
}

pub const MAX_LABELS: usize = 32;

/// Validate and normalise a registration request.
///
/// # Errors
/// Returns [`WorkerError`] when the name, profile list or labels are outside
/// their bounds.
pub fn validate_registration(
    input: &RegisterWorker,
) -> Result<(BTreeSet<Profile>, BTreeSet<String>), WorkerError> {
    if !is_portable_identifier(input.name.trim(), 128) {
        return Err(WorkerError::InvalidName);
    }
    let mut profiles = BTreeSet::new();
    for raw in &input.profiles {
        profiles.insert(Profile::parse(raw.trim()).map_err(|_| WorkerError::UnknownProfile)?);
    }
    if profiles.is_empty() {
        return Err(WorkerError::NoProfiles);
    }
    if input.labels.len() > MAX_LABELS
        || !input
            .labels
            .iter()
            .all(|label| is_portable_identifier(label.trim(), 64))
    {
        return Err(WorkerError::InvalidLabels);
    }
    let labels = input
        .labels
        .iter()
        .map(|label| label.trim().to_ascii_lowercase())
        .collect();
    Ok((profiles, labels))
}

/// A worker may take a job when it is idle, advertises the profile, and carries
/// every required label. Missing labels are a mismatch, never a "best effort".
#[must_use]
pub fn can_take(worker: &Worker, profile: Profile, required_labels: &BTreeSet<String>) -> bool {
    worker.state.accepts_work()
        && worker.profiles.contains(&profile)
        && required_labels.is_subset(&worker.labels)
}

/// Heartbeat liveness. Uses saturating arithmetic so a clock that moves
/// backwards reads as "fresh" rather than instantly evicting the fleet.
#[must_use]
pub const fn heartbeat_expired(now_ms: u64, last_heartbeat_ms: u64, ttl_ms: u64) -> bool {
    now_ms.saturating_sub(last_heartbeat_ms) > ttl_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(state: WorkerState) -> Worker {
        Worker {
            id: Uuid::nil(),
            org_id: Uuid::nil(),
            name: "runner-1".to_owned(),
            profiles: BTreeSet::from([Profile::RustVerify, Profile::NodeVerify]),
            labels: BTreeSet::from(["arm64".to_owned(), "hetzner".to_owned()]),
            state,
            registered_at: String::new(),
            last_heartbeat_at: String::new(),
            last_heartbeat_ms: 0,
        }
    }

    #[test]
    fn the_happy_lease_cycle_is_idle_busy_idle() {
        let state = advance(WorkerState::Registered, WorkerEvent::Ready).expect("ready");
        assert_eq!(state, WorkerState::Idle);
        let state = advance(state, WorkerEvent::LeaseAcquired).expect("lease");
        assert_eq!(state, WorkerState::Busy);
        let state = advance(state, WorkerEvent::LeaseReleased).expect("release");
        assert_eq!(state, WorkerState::Idle);
    }

    #[test]
    fn a_busy_worker_cannot_take_a_second_lease() {
        assert!(advance(WorkerState::Busy, WorkerEvent::LeaseAcquired).is_err());
    }

    #[test]
    fn draining_finishes_its_lease_then_goes_offline() {
        let state = advance(WorkerState::Busy, WorkerEvent::Drain).expect("drain");
        assert_eq!(state, WorkerState::Draining);
        assert!(advance(state, WorkerEvent::LeaseAcquired).is_err());
        assert_eq!(
            advance(state, WorkerEvent::LeaseReleased),
            Ok(WorkerState::Offline)
        );
    }

    #[test]
    fn liveness_loss_is_reachable_from_every_live_state() {
        for from in [
            WorkerState::Registered,
            WorkerState::Idle,
            WorkerState::Busy,
            WorkerState::Draining,
        ] {
            assert_eq!(
                advance(from, WorkerEvent::HeartbeatExpired),
                Ok(WorkerState::Offline)
            );
            assert_eq!(
                advance(from, WorkerEvent::Deregister),
                Ok(WorkerState::Offline)
            );
        }
    }

    #[test]
    fn an_offline_worker_must_register_again() {
        for event in [
            WorkerEvent::Ready,
            WorkerEvent::LeaseAcquired,
            WorkerEvent::LeaseReleased,
            WorkerEvent::Drain,
            WorkerEvent::HeartbeatExpired,
            WorkerEvent::Deregister,
        ] {
            assert!(advance(WorkerState::Offline, event).is_err(), "{event:?}");
        }
    }

    #[test]
    fn registration_is_validated_and_normalised() {
        let (profiles, labels) = validate_registration(&RegisterWorker {
            name: " runner-1 ".to_owned(),
            profiles: vec!["rust-verify".to_owned(), " node-verify".to_owned()],
            labels: vec!["ARM64".to_owned()],
        })
        .expect("valid registration");
        assert_eq!(
            profiles,
            BTreeSet::from([Profile::RustVerify, Profile::NodeVerify])
        );
        assert_eq!(labels, BTreeSet::from(["arm64".to_owned()]));
    }

    #[test]
    fn registration_rejects_bad_names_profiles_and_labels() {
        let base = RegisterWorker {
            name: "runner-1".to_owned(),
            profiles: vec!["rust-verify".to_owned()],
            labels: Vec::new(),
        };

        let mut bad = base.clone();
        bad.name = "runner one".to_owned();
        assert_eq!(validate_registration(&bad), Err(WorkerError::InvalidName));

        let mut bad = base.clone();
        bad.profiles = Vec::new();
        assert_eq!(validate_registration(&bad), Err(WorkerError::NoProfiles));

        let mut bad = base.clone();
        bad.profiles = vec!["root-shell".to_owned()];
        assert_eq!(
            validate_registration(&bad),
            Err(WorkerError::UnknownProfile)
        );

        let mut bad = base;
        bad.labels = (0..=MAX_LABELS).map(|index| format!("l{index}")).collect();
        assert_eq!(validate_registration(&bad), Err(WorkerError::InvalidLabels));
    }

    #[test]
    fn capability_matching_requires_state_profile_and_every_label() {
        let idle = worker(WorkerState::Idle);
        assert!(can_take(&idle, Profile::RustVerify, &BTreeSet::new()));
        assert!(can_take(
            &idle,
            Profile::RustVerify,
            &BTreeSet::from(["arm64".to_owned()])
        ));
        assert!(!can_take(
            &idle,
            Profile::RustVerify,
            &BTreeSet::from(["gpu".to_owned()])
        ));
        assert!(!can_take(&idle, Profile::Playwright, &BTreeSet::new()));
        assert!(!can_take(
            &worker(WorkerState::Busy),
            Profile::RustVerify,
            &BTreeSet::new()
        ));
    }

    #[test]
    fn heartbeat_expiry_is_saturating() {
        assert!(!heartbeat_expired(1_000, 500, 1_000));
        assert!(heartbeat_expired(2_001, 1_000, 1_000));
        // A clock that moved backwards must not evict the whole fleet.
        assert!(!heartbeat_expired(100, 5_000, 1_000));
    }
}
