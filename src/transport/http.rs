#![forbid(unsafe_code)]

//! The HTTP listener and its graceful shutdown.
//!
//! `axum::serve` over a `tokio::net::TcpListener`, with connect info so the
//! middleware can see a peer address without trusting a header.
//!
//! Shutdown is an explicit state machine rather than "wait for ctrl-c":
//!
//! * `Running` — serving.
//! * `Draining` — the listener is closed and in-flight work is finishing. A
//!   `watch` channel tells the TCP transport and background tasks to drain too.
//! * `Forced` — the grace deadline passed, or a second signal arrived. Whatever
//!   is still in flight is dropped, and the outcome says so.
//!
//! Reporting `Forced` rather than pretending a timeout was graceful is the
//! whole point: a deploy that silently truncates requests looks healthy.

use std::future::IntoFuture;
use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Running,
    Draining,
    Forced,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Signal {
    Interrupt,
    Terminate,
    Deadline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Ignore,
    Drain,
    Force,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Graceful,
    Forced(Signal),
}

/// The shutdown reducer. Pure, so the policy is unit-tested without binding a
/// port: the first signal drains, a second signal or the grace deadline forces.
#[must_use]
pub const fn reduce(phase: Phase, signal: Signal) -> (Phase, Action) {
    match (phase, signal) {
        (Phase::Running, Signal::Interrupt | Signal::Terminate) => (Phase::Draining, Action::Drain),
        // A deadline while still running cannot happen: the timer is armed when
        // draining starts. Treat it as a no-op rather than a forced kill.
        (Phase::Running, Signal::Deadline) => (Phase::Running, Action::Ignore),
        (Phase::Draining, _) => (Phase::Forced, Action::Force),
        (Phase::Forced | Phase::Stopped, _) => (phase, Action::Ignore),
    }
}

/// Bind and serve `router` until a signal arrives.
///
/// # Errors
/// Returns the bind or serve error. A shutdown, graceful or forced, is `Ok` —
/// the [`Outcome`] says which, so a deploy that truncated work is visible.
pub async fn serve(
    bind: &str,
    router: Router,
    grace: Duration,
    drain: watch::Sender<bool>,
) -> io::Result<Outcome> {
    let listener = TcpListener::bind(bind).await?;
    let local = listener.local_addr()?;
    tracing::info!(bind = %local, grace_ms = millis(grace), "http listening");
    let started = Instant::now();

    // The shutdown future is what `axum::serve` waits on to stop accepting. It
    // reports the signal it saw so the loop below can arm the grace deadline,
    // and it tells every other transport to drain at the same moment.
    let (signal_tx, mut signal_rx) = mpsc::unbounded_channel::<Signal>();
    let drain_on_signal = drain.clone();
    let shutdown = async move {
        let signal = next_os_signal().await;
        let _ = signal_tx.send(signal);
        let _ = drain_on_signal.send(true);
    };

    let server = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .into_future();
    tokio::pin!(server);

    let mut phase = Phase::Running;
    let mut deadline: Option<tokio::time::Instant> = None;

    let outcome = loop {
        tokio::select! {
            result = &mut server => {
                result?;
                break Outcome::Graceful;
            }
            received = signal_rx.recv(), if deadline.is_none() => {
                // A closed channel can only mean the shutdown future finished,
                // so treat it exactly like the terminate it was.
                let signal = received.unwrap_or(Signal::Terminate);
                let (next, action) = reduce(phase, signal);
                phase = next;
                if action == Action::Drain {
                    tracing::info!(
                        signal = ?signal,
                        grace_ms = millis(grace),
                        "shutdown requested; the listener is closed and work is draining"
                    );
                    deadline = Some(tokio::time::Instant::now() + grace);
                }
            }
            () = sleep_until_deadline(deadline), if deadline.is_some() => {
                let (next, _) = reduce(phase, Signal::Deadline);
                phase = next;
                tracing::warn!(
                    grace_ms = millis(grace),
                    "grace deadline passed; dropping in-flight connections"
                );
                break Outcome::Forced(Signal::Deadline);
            }
            signal = next_os_signal(), if deadline.is_some() => {
                let (next, _) = reduce(phase, signal);
                phase = next;
                tracing::warn!(signal = ?signal, "second signal; dropping in-flight connections");
                break Outcome::Forced(signal);
            }
        }
    };

    tracing::info!(
        outcome = ?outcome,
        phase = ?phase,
        elapsed_ms = millis(started.elapsed()),
        "http shutdown complete"
    );
    Ok(outcome)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Await the grace deadline, or never when one is not armed. The deadline is
/// passed by value so no `select!` branch holds a borrow of it.
async fn sleep_until_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

#[cfg(unix)]
async fn next_os_signal() -> Signal {
    use tokio::signal::unix::{signal, SignalKind};

    let mut interrupt = match signal(SignalKind::interrupt()) {
        Ok(stream) => stream,
        Err(error) => {
            tracing::error!(%error, "failed to install SIGINT handler");
            return std::future::pending().await;
        }
    };
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(stream) => stream,
        Err(error) => {
            tracing::error!(%error, "failed to install SIGTERM handler");
            return std::future::pending().await;
        }
    };
    tokio::select! {
        _ = interrupt.recv() => Signal::Interrupt,
        _ = terminate.recv() => Signal::Terminate,
    }
}

#[cfg(not(unix))]
async fn next_os_signal() -> Signal {
    match tokio::signal::ctrl_c().await {
        Ok(()) => Signal::Interrupt,
        Err(error) => {
            tracing::error!(%error, "failed to wait for ctrl-c");
            std::future::pending().await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_signal_drains() {
        for signal in [Signal::Interrupt, Signal::Terminate] {
            assert_eq!(
                reduce(Phase::Running, signal),
                (Phase::Draining, Action::Drain)
            );
        }
    }

    #[test]
    fn a_second_signal_or_the_deadline_forces() {
        for signal in [Signal::Interrupt, Signal::Terminate, Signal::Deadline] {
            assert_eq!(
                reduce(Phase::Draining, signal),
                (Phase::Forced, Action::Force)
            );
        }
    }

    #[test]
    fn a_deadline_before_draining_is_ignored() {
        assert_eq!(
            reduce(Phase::Running, Signal::Deadline),
            (Phase::Running, Action::Ignore)
        );
    }

    #[test]
    fn terminal_phases_absorb_everything() {
        for phase in [Phase::Forced, Phase::Stopped] {
            for signal in [Signal::Interrupt, Signal::Terminate, Signal::Deadline] {
                assert_eq!(reduce(phase, signal), (phase, Action::Ignore));
            }
        }
    }

    #[test]
    fn a_forced_outcome_names_the_signal_that_forced_it() {
        assert_ne!(
            Outcome::Forced(Signal::Deadline),
            Outcome::Forced(Signal::Interrupt)
        );
        assert_ne!(Outcome::Graceful, Outcome::Forced(Signal::Deadline));
    }
}
