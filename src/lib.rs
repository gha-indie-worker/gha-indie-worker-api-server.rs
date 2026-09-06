#![forbid(unsafe_code)]

//! `gha-indie-worker-api-server` — the JSON API for `api.indiebuild.dev`.
//!
//! # Layout
//!
//! | module | role |
//! |---|---|
//! | [`domain`] | the functional core: pure state machines and typed errors |
//! | [`store`] | the effect boundary in front of the domain |
//! | [`routes`] | HTTP surfaces (`/healthz`, `/readyz`, `/metrics`, `/v1/*`) |
//! | [`transport`] | the four avenues: SeaORM, HTTP, TCP + WebSocket, NATS |
//! | [`auth`] | dual authentication onto one [`auth::VerifiedActor`] |
//! | [`middleware`] | the `ores-middleware` stack |
//! | [`rate_limit`] | opaque principals and a bounded window |
//! | [`telemetry`] | the ores-otel → `tracing` bridge |
//! | [`config`] | env-only configuration, redacted in `Debug` |
//!
//! # Feature containment
//!
//! Every fleet dependency is behind a default-on cargo feature *and* confined
//! to one module, so an upstream API change costs one flag rather than the
//! build: `shared-auth` → [`auth::shared_auth`], `otel` → [`telemetry`],
//! `ores-mw` → [`middleware`], `ores-rl` → [`rate_limit::ores_rl`],
//! `db` → [`transport::db`], `nats-transport` → [`transport::nats`],
//! `tcp-transport` → [`transport::tcp`].

pub mod auth;
pub mod config;
pub mod domain;
pub mod error;
pub mod middleware;
pub mod rate_limit;
pub mod routes;
pub mod server;
pub mod state;
pub mod store;
pub mod telemetry;
pub mod transport;

use std::sync::OnceLock;
use std::time::Instant;

/// Milliseconds since the first call, for rate-limit and heartbeat arithmetic.
///
/// A monotonic clock, deliberately: wall-clock time can step backwards, and a
/// limiter or a liveness check that trusts it can be made to hand out a fresh
/// budget or evict a healthy fleet.
#[must_use]
pub fn monotonic_ms() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let origin = ORIGIN.get_or_init(Instant::now);
    u64::try_from(origin.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_monotonic_clock_never_moves_backwards() {
        let first = monotonic_ms();
        let second = monotonic_ms();
        assert!(second >= first, "{second} < {first}");
    }
}
