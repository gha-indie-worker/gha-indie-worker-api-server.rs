#![forbid(unsafe_code)]

//! Rate limiting: opaque principals plus a deterministic fixed-window counter.
//!
//! Two fleet rules shape this module.
//!
//! 1. **Opaque keys only.** A raw IP address, email, bearer token or cookie
//!    never reaches limiter state, a log line, a cache entry or a backend
//!    keyspace. [`derive_principal`] HMACs the signals into a hex digest, and
//!    that digest is the only thing anything downstream sees.
//! 2. **One place.** Everything that touches `ores-rl-lib-core` lives in
//!    [`ores_rl`] below, behind the default-on `ores-rl` cargo feature, so an
//!    upstream API change costs one flag rather than the build.
//!
//! The counter itself is a pure state machine ([`transition`]) with a bounded
//! in-process table in front of it. When `REDIS_URL` is configured the same
//! decisions are meant to come from the distributed authority in
//! `ores-rl-lib-core`; until the shared `LimitPolicy` shape is available here,
//! the local window is authoritative and the module says so rather than
//! pretending to be distributed.

use std::collections::HashMap;
use std::fmt::Write as _;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::sync::Mutex;

type HmacSha256 = Hmac<Sha256>;

/// The signals that identify a caller for limiting purposes. Everything here is
/// hashed, never stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrincipalSignals<'a> {
    pub tenant: Option<&'a str>,
    pub subject: Option<&'a str>,
    /// Only used when there is no authenticated subject, and only ever hashed.
    pub remote_ip: Option<&'a str>,
    pub route: &'a str,
    pub method: &'a str,
}

/// An opaque principal: a hex digest plus the key version that produced it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Principal {
    pub digest: String,
    pub key_version: &'static str,
}

pub const KEY_VERSION: &str = "giw-v1";

/// Derive an opaque principal.
///
/// Identity beats network location: when a subject is present the IP is not
/// mixed in at all, so one user behind a shared egress is not limited as a
/// stranger, and a rotating IP does not buy a fresh budget.
#[must_use]
pub fn derive_principal(secret: &[u8], signals: &PrincipalSignals<'_>) -> Principal {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC-SHA256 accepts any key length");
    mac.update(KEY_VERSION.as_bytes());
    mac.update(b"\x1f");
    match signals.subject {
        Some(subject) => {
            mac.update(b"sub\x1f");
            mac.update(subject.as_bytes());
        }
        None => {
            mac.update(b"ip\x1f");
            mac.update(signals.remote_ip.unwrap_or("anonymous").as_bytes());
        }
    }
    mac.update(b"\x1f");
    mac.update(signals.tenant.unwrap_or("-").as_bytes());
    mac.update(b"\x1f");
    mac.update(signals.method.as_bytes());
    mac.update(b"\x1f");
    mac.update(signals.route.as_bytes());

    let digest = mac.finalize().into_bytes();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    Principal {
        digest: hex,
        key_version: KEY_VERSION,
    }
}

/// One fixed window's state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Window {
    pub started_ms: u64,
    pub count: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Decision {
    pub allowed: bool,
    pub limit: u32,
    pub remaining: u32,
    pub retry_after_ms: u64,
}

/// Pure fixed-window transition.
///
/// A clock that moves backwards restarts the window rather than producing a
/// negative elapsed time, so a leader change or an NTP step cannot hand out an
/// unbounded budget or lock a caller out.
#[must_use]
pub fn transition(
    window: Window,
    now_ms: u64,
    capacity: u32,
    window_ms: u64,
) -> (Window, Decision) {
    let capacity = capacity.max(1);
    let window_ms = window_ms.max(1);
    let elapsed = now_ms.saturating_sub(window.started_ms);
    let fresh = window.count == 0 || elapsed >= window_ms || now_ms < window.started_ms;

    let mut next = if fresh {
        Window {
            started_ms: now_ms,
            count: 0,
        }
    } else {
        window
    };

    if next.count >= capacity {
        let elapsed = now_ms.saturating_sub(next.started_ms);
        return (
            next,
            Decision {
                allowed: false,
                limit: capacity,
                remaining: 0,
                retry_after_ms: window_ms.saturating_sub(elapsed).max(1),
            },
        );
    }

    next.count += 1;
    (
        next,
        Decision {
            allowed: true,
            limit: capacity,
            remaining: capacity - next.count,
            retry_after_ms: 0,
        },
    )
}

/// Bounded in-process window table. The bound is what keeps a flood of unique
/// principals from growing the process; eviction drops the oldest window.
#[derive(Debug)]
pub struct WindowTable {
    windows: HashMap<String, Window>,
    max_entries: usize,
}

impl WindowTable {
    #[must_use]
    pub fn new(max_entries: usize) -> Self {
        Self {
            windows: HashMap::new(),
            max_entries: max_entries.max(1),
        }
    }

    pub fn evaluate(&mut self, key: &str, now_ms: u64, capacity: u32, window_ms: u64) -> Decision {
        if !self.windows.contains_key(key) && self.windows.len() >= self.max_entries {
            self.evict_oldest();
        }
        let current = self.windows.get(key).copied().unwrap_or_default();
        let (next, decision) = transition(current, now_ms, capacity, window_ms);
        self.windows.insert(key.to_owned(), next);
        decision
    }

    fn evict_oldest(&mut self) {
        let oldest = self
            .windows
            .iter()
            .min_by_key(|(_, window)| window.started_ms)
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            self.windows.remove(&key);
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }
}

pub const DEFAULT_MAX_PRINCIPALS: usize = 50_000;

/// The limiter the middleware stack calls.
#[derive(Debug)]
pub struct GiwRateLimiter {
    secret: Vec<u8>,
    table: Mutex<WindowTable>,
    capacity: u32,
    window_ms: u64,
}

impl GiwRateLimiter {
    #[must_use]
    pub fn new(secret: Vec<u8>, capacity: u32, window_ms: u64) -> Self {
        Self {
            secret,
            table: Mutex::new(WindowTable::new(DEFAULT_MAX_PRINCIPALS)),
            capacity: capacity.max(1),
            window_ms: window_ms.max(1),
        }
    }

    #[must_use]
    pub fn principal(&self, signals: &PrincipalSignals<'_>) -> Principal {
        derive_principal(&self.secret, signals)
    }

    /// Evaluate one request against an already-derived opaque key.
    pub async fn evaluate(&self, key: &str, now_ms: u64, capacity: u32) -> Decision {
        self.table
            .lock()
            .await
            .evaluate(key, now_ms, capacity, self.window_ms)
    }

    #[must_use]
    pub const fn capacity(&self) -> u32 {
        self.capacity
    }

    #[must_use]
    pub const fn window_ms(&self) -> u64 {
        self.window_ms
    }
}

/// `ores-middleware`'s limiter contract, implemented over the window table.
///
/// Only the required `allow` method is provided; the trait's default
/// `evaluate` wraps it into a structured decision, which keeps this
/// implementation independent of the upstream decision struct's field set.
#[cfg(feature = "ores-mw")]
impl ores_middleware::RateLimiter for GiwRateLimiter {
    fn allow<'a>(
        &'a self,
        key: &'a str,
        capacity: u32,
        _refill_per_second: f64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            let now_ms = crate::monotonic_ms();
            self.evaluate(key, now_ms, capacity.max(1)).await.allowed
        })
    }
}

/// The bridge to `ores-rl-lib-core`.
///
/// Deliberately narrow: it re-exports the deterministic state machine and its
/// types, and nothing here constructs an upstream struct literal. `LimitPolicy`
/// is threaded through from the caller, so when the shared policy definitions
/// land (they live in the crate's private `model` module today) the fleet
/// limiter can be adopted here without touching any other file.
#[cfg(feature = "ores-rl")]
pub mod ores_rl {
    use ores_rl_lib_core::{transition, Decision, LimitPolicy, LimitState, TransitionError};

    /// The state a principal starts from.
    #[must_use]
    pub const fn initial_state() -> LimitState {
        LimitState::Empty
    }

    /// One deterministic transition of the fleet limiter.
    ///
    /// # Errors
    /// Returns [`TransitionError`] when the policy is invalid, the cost is zero
    /// or above capacity, the stored state does not match the policy's
    /// algorithm, or the monotonic clock moved backwards.
    pub fn step(
        policy: LimitPolicy,
        state: LimitState,
        now_ms: u64,
        cost: u64,
    ) -> Result<(LimitState, Decision), TransitionError> {
        transition(policy, state, now_ms, cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn signals<'a>(subject: Option<&'a str>, ip: Option<&'a str>) -> PrincipalSignals<'a> {
        PrincipalSignals {
            tenant: Some("acme"),
            subject,
            remote_ip: ip,
            route: "/v1/runs",
            method: "POST",
        }
    }

    #[test]
    fn a_principal_is_an_opaque_digest_of_fixed_width() {
        let principal = derive_principal(SECRET, &signals(Some("sub-1"), None));
        assert_eq!(principal.digest.len(), 64);
        assert!(principal.digest.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(principal.key_version, KEY_VERSION);
    }

    #[test]
    fn a_principal_never_contains_the_signals_that_produced_it() {
        let principal =
            derive_principal(SECRET, &signals(Some("alex@example.com"), Some("10.0.0.1")));
        assert!(!principal.digest.contains("alex"));
        assert!(!principal.digest.contains("example"));
        assert!(!principal.digest.contains("10.0.0.1"));
        assert!(!principal.digest.contains("acme"));
    }

    #[test]
    fn derivation_is_deterministic_and_separates_every_signal() {
        let base = derive_principal(SECRET, &signals(Some("sub-1"), None));
        assert_eq!(
            base,
            derive_principal(SECRET, &signals(Some("sub-1"), None))
        );
        assert_ne!(
            base,
            derive_principal(SECRET, &signals(Some("sub-2"), None))
        );

        let mut other_route = signals(Some("sub-1"), None);
        other_route.route = "/v1/plans";
        assert_ne!(base, derive_principal(SECRET, &other_route));

        let mut other_method = signals(Some("sub-1"), None);
        other_method.method = "GET";
        assert_ne!(base, derive_principal(SECRET, &other_method));

        let mut other_tenant = signals(Some("sub-1"), None);
        other_tenant.tenant = Some("other");
        assert_ne!(base, derive_principal(SECRET, &other_tenant));

        assert_ne!(
            base,
            derive_principal(b"another-key", &signals(Some("sub-1"), None))
        );
    }

    #[test]
    fn an_identified_caller_is_not_re_keyed_by_their_address() {
        let from_office = derive_principal(SECRET, &signals(Some("sub-1"), Some("10.0.0.1")));
        let from_home = derive_principal(SECRET, &signals(Some("sub-1"), Some("192.0.2.7")));
        assert_eq!(from_office, from_home);
    }

    #[test]
    fn anonymous_callers_fall_back_to_the_address_and_stay_separate() {
        let first = derive_principal(SECRET, &signals(None, Some("10.0.0.1")));
        let second = derive_principal(SECRET, &signals(None, Some("10.0.0.2")));
        assert_ne!(first, second);
        // A subject is never confusable with an address-derived principal.
        assert_ne!(
            first,
            derive_principal(SECRET, &signals(Some("10.0.0.1"), None))
        );
    }

    #[test]
    fn a_window_allows_exactly_its_capacity() {
        let mut window = Window::default();
        for expected_remaining in (0..3).rev() {
            let (next, decision) = transition(window, 1_000, 3, 60_000);
            window = next;
            assert!(decision.allowed);
            assert_eq!(decision.remaining, expected_remaining);
        }
        let (_, denied) = transition(window, 1_000, 3, 60_000);
        assert!(!denied.allowed);
        assert_eq!(denied.remaining, 0);
        assert_eq!(denied.retry_after_ms, 60_000);
    }

    #[test]
    fn a_window_resets_after_its_duration() {
        let (window, _) = transition(Window::default(), 0, 1, 1_000);
        let (window, denied) = transition(window, 500, 1, 1_000);
        assert!(!denied.allowed);
        assert_eq!(denied.retry_after_ms, 500);

        let (_, allowed) = transition(window, 1_000, 1, 1_000);
        assert!(allowed.allowed);
    }

    #[test]
    fn a_backwards_clock_restarts_the_window_rather_than_underflowing() {
        let (window, _) = transition(Window::default(), 10_000, 1, 1_000);
        let (next, decision) = transition(window, 5_000, 1, 1_000);
        assert!(decision.allowed);
        assert_eq!(next.started_ms, 5_000);
        assert_eq!(next.count, 1);
    }

    #[test]
    fn degenerate_capacities_and_windows_are_clamped_not_divided_by_zero() {
        let (_, decision) = transition(Window::default(), 0, 0, 0);
        assert!(decision.allowed);
        assert_eq!(decision.limit, 1);
    }

    #[test]
    fn the_window_table_is_bounded() {
        let mut table = WindowTable::new(4);
        for index in 0..64_u64 {
            table.evaluate(&format!("key-{index}"), index, 10, 1_000);
        }
        assert_eq!(table.len(), 4);
        assert!(!table.is_empty());
    }

    #[test]
    fn the_window_table_keeps_separate_budgets_per_principal() {
        let mut table = WindowTable::new(64);
        assert!(table.evaluate("a", 0, 1, 1_000).allowed);
        assert!(!table.evaluate("a", 0, 1, 1_000).allowed);
        assert!(table.evaluate("b", 0, 1, 1_000).allowed);
    }

    #[tokio::test]
    async fn the_limiter_enforces_its_configured_capacity() {
        let limiter = GiwRateLimiter::new(SECRET.to_vec(), 2, 60_000);
        assert_eq!(limiter.capacity(), 2);
        assert_eq!(limiter.window_ms(), 60_000);
        let key = limiter.principal(&signals(Some("sub-1"), None)).digest;
        assert!(limiter.evaluate(&key, 0, 2).await.allowed);
        assert!(limiter.evaluate(&key, 0, 2).await.allowed);
        assert!(!limiter.evaluate(&key, 0, 2).await.allowed);
    }
}
