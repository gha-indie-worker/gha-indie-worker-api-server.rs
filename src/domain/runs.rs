#![forbid(unsafe_code)]

//! Run / job / log-chunk lifecycle, mirroring `gha-clone-server`'s concepts.
//!
//! A **plan** compiles a workflow to fixed profiles (see [`super::plans`]); a
//! **run** executes one plan; a run owns **jobs** in a topological order; a job
//! streams **log chunks**. Runs and jobs share one lifecycle lattice so the
//! aggregate rollup in [`rollup`] is a total function.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use super::plans::Profile;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    #[default]
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl LifecycleState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum LifecycleEvent {
    Start,
    Succeed,
    Fail,
    Cancel,
}

impl LifecycleEvent {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Succeed => "succeed",
            Self::Fail => "fail",
            Self::Cancel => "cancel",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
#[error("{from} cannot move via {event}")]
pub struct RunTransitionError {
    pub from: &'static str,
    pub event: &'static str,
}

/// The run/job lifecycle. Terminal states are absorbing *errors*, not no-ops:
/// a worker reporting success twice is a bug worth surfacing.
///
/// # Errors
/// Returns [`RunTransitionError`] for any edge not on the machine.
pub const fn advance(
    from: LifecycleState,
    event: LifecycleEvent,
) -> Result<LifecycleState, RunTransitionError> {
    match (from, event) {
        (LifecycleState::Queued, LifecycleEvent::Start) => Ok(LifecycleState::Running),
        (LifecycleState::Queued | LifecycleState::Running, LifecycleEvent::Cancel) => {
            Ok(LifecycleState::Cancelled)
        }
        (LifecycleState::Running, LifecycleEvent::Succeed) => Ok(LifecycleState::Succeeded),
        (LifecycleState::Running, LifecycleEvent::Fail) => Ok(LifecycleState::Failed),
        (from, event) => Err(RunTransitionError {
            from: from.as_str(),
            event: event.as_str(),
        }),
    }
}

/// Aggregate a run's state from its jobs. Fail-closed: any failure fails the
/// run, a cancellation without a failure cancels it, and the run is only
/// `Succeeded` once every job succeeded.
#[must_use]
pub fn rollup(jobs: &[LifecycleState]) -> LifecycleState {
    if jobs.is_empty() {
        return LifecycleState::Queued;
    }
    if jobs.iter().any(|state| *state == LifecycleState::Failed) {
        return LifecycleState::Failed;
    }
    if jobs.iter().any(|state| *state == LifecycleState::Cancelled) {
        return LifecycleState::Cancelled;
    }
    if jobs.iter().all(|state| *state == LifecycleState::Succeeded) {
        return LifecycleState::Succeeded;
    }
    if jobs.iter().any(|state| *state == LifecycleState::Running) {
        return LifecycleState::Running;
    }
    LifecycleState::Queued
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: Uuid,
    pub org_id: Uuid,
    pub repository: String,
    pub revision: String,
    pub workflow_path: String,
    pub state: LifecycleState,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: Uuid,
    pub run_id: Uuid,
    pub name: String,
    pub profile: Profile,
    pub needs: Vec<String>,
    pub state: LifecycleState,
    /// Position in the deterministic topological order computed at plan time.
    pub sequence: u32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// Hard ceiling on a single appended chunk. Larger appends are rejected rather
/// than truncated, so a caller never silently loses output.
pub const MAX_LOG_CHUNK_BYTES: usize = 64 * 1024;
/// Hard ceiling on retained chunks per job, so an unbounded log cannot grow the
/// process without limit.
pub const MAX_LOG_CHUNKS_PER_JOB: usize = 4_096;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LogChunk {
    pub run_id: Uuid,
    pub job_id: Uuid,
    pub sequence: u64,
    pub stream: LogStream,
    pub at: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum LogError {
    #[error("log chunk exceeds 65536 bytes")]
    TooLarge,
    #[error("log chunk sequence must be strictly increasing")]
    OutOfOrder,
    #[error("job log retention limit reached")]
    RetentionExceeded,
}

/// Validate one appended chunk against the last accepted sequence.
///
/// # Errors
/// Returns [`LogError`] when the chunk is oversized, replayed/out of order, or
/// would exceed the per-job retention bound.
pub const fn accept_log_chunk(
    last_sequence: Option<u64>,
    sequence: u64,
    byte_len: usize,
    retained: usize,
) -> Result<(), LogError> {
    if byte_len > MAX_LOG_CHUNK_BYTES {
        return Err(LogError::TooLarge);
    }
    if retained >= MAX_LOG_CHUNKS_PER_JOB {
        return Err(LogError::RetentionExceeded);
    }
    match last_sequence {
        Some(last) if sequence <= last => Err(LogError::OutOfOrder),
        Some(_) | None => Ok(()),
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct NewRun {
    pub repository: String,
    pub revision: String,
    pub workflow_path: String,
    pub workflow_yaml: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AppendLog {
    pub job_id: Uuid,
    pub sequence: u64,
    #[serde(default = "default_stream")]
    pub stream: LogStream,
    pub text: String,
}

const fn default_stream() -> LogStream {
    LogStream::Stdout
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STATES: [LifecycleState; 5] = [
        LifecycleState::Queued,
        LifecycleState::Running,
        LifecycleState::Succeeded,
        LifecycleState::Failed,
        LifecycleState::Cancelled,
    ];
    const ALL_EVENTS: [LifecycleEvent; 4] = [
        LifecycleEvent::Start,
        LifecycleEvent::Succeed,
        LifecycleEvent::Fail,
        LifecycleEvent::Cancel,
    ];

    #[test]
    fn the_lifecycle_has_exactly_the_five_documented_edges() {
        let mut edges = Vec::new();
        for from in ALL_STATES {
            for event in ALL_EVENTS {
                if let Ok(to) = advance(from, event) {
                    edges.push((from, event, to));
                }
            }
        }
        assert_eq!(edges.len(), 5, "{edges:?}");
        assert!(edges.contains(&(
            LifecycleState::Queued,
            LifecycleEvent::Start,
            LifecycleState::Running
        )));
        assert!(edges.contains(&(
            LifecycleState::Running,
            LifecycleEvent::Succeed,
            LifecycleState::Succeeded
        )));
    }

    #[test]
    fn terminal_states_have_no_outgoing_edges() {
        for from in ALL_STATES.into_iter().filter(|state| state.is_terminal()) {
            for event in ALL_EVENTS {
                assert!(advance(from, event).is_err(), "{from:?} + {event:?}");
            }
        }
    }

    #[test]
    fn a_queued_job_cannot_report_a_result_without_starting() {
        assert!(advance(LifecycleState::Queued, LifecycleEvent::Succeed).is_err());
        assert!(advance(LifecycleState::Queued, LifecycleEvent::Fail).is_err());
    }

    #[test]
    fn rollup_is_fail_closed() {
        assert_eq!(rollup(&[]), LifecycleState::Queued);
        assert_eq!(
            rollup(&[LifecycleState::Succeeded, LifecycleState::Succeeded]),
            LifecycleState::Succeeded
        );
        assert_eq!(
            rollup(&[LifecycleState::Succeeded, LifecycleState::Failed]),
            LifecycleState::Failed
        );
        assert_eq!(
            rollup(&[LifecycleState::Cancelled, LifecycleState::Succeeded]),
            LifecycleState::Cancelled
        );
        assert_eq!(
            rollup(&[LifecycleState::Failed, LifecycleState::Cancelled]),
            LifecycleState::Failed
        );
        assert_eq!(
            rollup(&[LifecycleState::Running, LifecycleState::Queued]),
            LifecycleState::Running
        );
        assert_eq!(
            rollup(&[LifecycleState::Queued, LifecycleState::Queued]),
            LifecycleState::Queued
        );
    }

    #[test]
    fn log_chunks_are_bounded_and_strictly_ordered() {
        assert_eq!(accept_log_chunk(None, 0, 10, 0), Ok(()));
        assert_eq!(accept_log_chunk(Some(4), 5, 10, 0), Ok(()));
        assert_eq!(
            accept_log_chunk(Some(5), 5, 10, 0),
            Err(LogError::OutOfOrder)
        );
        assert_eq!(
            accept_log_chunk(Some(5), 4, 10, 0),
            Err(LogError::OutOfOrder)
        );
        assert_eq!(
            accept_log_chunk(None, 0, MAX_LOG_CHUNK_BYTES + 1, 0),
            Err(LogError::TooLarge)
        );
        assert_eq!(
            accept_log_chunk(None, 0, 10, MAX_LOG_CHUNKS_PER_JOB),
            Err(LogError::RetentionExceeded)
        );
    }
}
