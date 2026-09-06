#![forbid(unsafe_code)]

//! Shared application state and the domain-event bus.
//!
//! [`AppState`] is what every handler, extractor and transport receives. It is
//! cheap to clone (everything inside is an `Arc` or an already-shared handle),
//! and it is the single place where "which authorities and carriers is this
//! process actually wired to" is answered.

use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Semaphore};
use uuid::Uuid;

use crate::auth::jwt::JwtVerifier;
use crate::config::ApiConfig;
use crate::domain::embeddings::{DeterministicProvider, EmbeddingProvider};
use crate::rate_limit::GiwRateLimiter;
use crate::store::Store;
use crate::transport::db::Pools;

/// One domain event, published to every subscribed WebSocket and TCP client and
/// (when configured) to NATS on `giw.<env>.<domain>.<event>`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    RunUpdated {
        run_id: Uuid,
        org_id: Uuid,
        state: String,
    },
    JobUpdated {
        run_id: Uuid,
        job_id: Uuid,
        state: String,
    },
    LogAppended {
        run_id: Uuid,
        job_id: Uuid,
        sequence: u64,
    },
    WorkerPresence {
        org_id: Uuid,
        worker_id: Uuid,
        state: String,
    },
    ChatEvent {
        org_id: Uuid,
        surface: String,
        kind: String,
    },
}

impl ServerEvent {
    /// The `<domain>` segment of the NATS subject.
    #[must_use]
    pub const fn domain(&self) -> &'static str {
        match self {
            Self::RunUpdated { .. } => "runs",
            Self::JobUpdated { .. } | Self::LogAppended { .. } => "jobs",
            Self::WorkerPresence { .. } => "workers",
            Self::ChatEvent { .. } => "chat",
        }
    }

    /// The `<event>` segment of the NATS subject.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::RunUpdated { .. } | Self::JobUpdated { .. } => "updated",
            Self::LogAppended { .. } => "log_appended",
            Self::WorkerPresence { .. } => "presence",
            Self::ChatEvent { .. } => "event",
        }
    }
}

/// Startup failures. Every variant is a reason to refuse to bind a listener.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("configuration is invalid: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("database: {0}")]
    Database(String),
    #[error("shared-auth: {0}")]
    SharedAuth(String),
    #[error("http client could not be built: {0}")]
    HttpClient(String),
    #[error("middleware: {0}")]
    Middleware(String),
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ApiConfig>,
    pub store: Store,
    pub jwt: JwtVerifier,
    #[cfg(feature = "shared-auth")]
    pub shared_auth: Option<crate::auth::shared_auth::SharedAuthAuthority>,
    pub http: reqwest::Client,
    pub events: broadcast::Sender<ServerEvent>,
    pub embeddings: Arc<dyn EmbeddingProvider>,
    pub rate_limiter: Arc<GiwRateLimiter>,
    pub db: Pools,
    #[cfg(feature = "nats-transport")]
    pub nats: Option<crate::transport::nats::Publisher>,
    #[cfg(feature = "otel")]
    pub logger: next_loggers::Logger,
    pub ws_slots: Arc<Semaphore>,
    pub readiness: crate::routes::health::ReadinessCache,
    pub started_at: Instant,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppState")
            .field("config", &self.config)
            .field("database", &self.db)
            .field("shared_auth_configured", &self.shared_auth_configured())
            .field("jwt_configured", &self.jwt.is_configured())
            .field("nats_configured", &self.nats_configured())
            .field("embedding_model", &self.embeddings.model())
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// Build the state from configuration, opening every configured dependency.
    ///
    /// A dependency that is *configured but unreachable* is a startup failure;
    /// a dependency that is simply absent is not. That split is what lets a
    /// developer run the server with nothing but a bind address while a
    /// production rollout still fails fast on a bad connection string.
    ///
    /// # Errors
    /// See [`StartupError`].
    pub async fn build(config: ApiConfig) -> Result<Self, StartupError> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(concat!(
                "gha-indie-worker-api-server/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|error| StartupError::HttpClient(error.to_string()))?;

        let db = crate::transport::db::connect(&config.database)
            .await
            .map_err(StartupError::Database)?;

        #[cfg(feature = "shared-auth")]
        let shared_auth = match config.shared_auth.as_ref() {
            None => None,
            Some(settings) => Some(
                crate::auth::shared_auth::SharedAuthAuthority::new(settings)
                    .map_err(|error| StartupError::SharedAuth(error.to_string()))?,
            ),
        };

        #[cfg(feature = "nats-transport")]
        let nats = match config.nats_url.as_deref() {
            None => None,
            // A NATS outage must not stop the API from serving, so a failed
            // connection degrades to "no event bus" with a loud warning.
            Some(url) => match crate::transport::nats::connect(url, config.env).await {
                Ok(publisher) => Some(publisher),
                Err(error) => {
                    tracing::warn!(%error, "nats is configured but unreachable; events will not be published");
                    None
                }
            },
        };

        let embeddings: Arc<dyn EmbeddingProvider> = match config.embeddings.base.as_deref() {
            Some(base) => Arc::new(crate::routes::v1::embeddings::HttpProvider::new(
                http.clone(),
                base.to_owned(),
                config.embeddings.api_key.clone(),
                config.embeddings.model.clone(),
                config.embeddings.dims,
            )),
            None => Arc::new(DeterministicProvider::new(config.embeddings.dims)),
        };

        let rate_limit_secret = config
            .rate_limit_hmac_secret
            .clone()
            .map_or_else(random_secret, String::into_bytes);
        let rate_limiter = Arc::new(GiwRateLimiter::new(
            rate_limit_secret,
            config.rate_limit.capacity,
            u64::try_from(config.rate_limit.window.as_millis()).unwrap_or(60_000),
        ));

        let (events, _) = broadcast::channel(config.websocket.channel_capacity.max(16));
        let store = Store::new(
            u64::try_from(config.webhooks.delivery_ttl.as_millis()).unwrap_or(3_600_000),
            config.webhooks.max_deliveries,
        );
        let jwt = JwtVerifier::new(config.jwt.clone(), http.clone());
        let ws_slots = Arc::new(Semaphore::new(config.websocket.max_connections.max(1)));

        Ok(Self {
            config: Arc::new(config),
            store,
            jwt,
            #[cfg(feature = "shared-auth")]
            shared_auth,
            http,
            events,
            embeddings,
            rate_limiter,
            db,
            #[cfg(feature = "nats-transport")]
            nats,
            #[cfg(feature = "otel")]
            logger: crate::telemetry::ores_logger(),
            ws_slots,
            readiness: crate::routes::health::ReadinessCache::new(),
            started_at: Instant::now(),
        })
    }

    #[cfg(feature = "shared-auth")]
    #[must_use]
    pub const fn shared_auth_configured(&self) -> bool {
        self.shared_auth.is_some()
    }

    #[cfg(not(feature = "shared-auth"))]
    #[must_use]
    pub const fn shared_auth_configured(&self) -> bool {
        false
    }

    #[cfg(feature = "nats-transport")]
    #[must_use]
    pub const fn nats_configured(&self) -> bool {
        self.nats.is_some()
    }

    #[cfg(not(feature = "nats-transport"))]
    #[must_use]
    pub const fn nats_configured(&self) -> bool {
        false
    }

    /// Publish a domain event to every subscribed socket and, when configured,
    /// to NATS. Fail-open: no subscriber and no broker is not an error.
    pub async fn publish(&self, event: ServerEvent) {
        // A broadcast send with no receivers is `Err`; that is normal.
        let _ = self.events.send(event.clone());
        #[cfg(feature = "nats-transport")]
        if let Some(publisher) = self.nats.as_ref() {
            publisher.publish(&event).await;
        }
    }
}

/// A process-lifetime HMAC key for rate-limit principals, used when none is
/// configured. Ephemeral by design: budgets reset on restart rather than the
/// service inventing a stable secret nobody rotated.
fn random_secret() -> Vec<u8> {
    Uuid::new_v4()
        .as_bytes()
        .iter()
        .chain(Uuid::new_v4().as_bytes().iter())
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_maps_to_a_subject_pair() {
        let cases = [
            (
                ServerEvent::RunUpdated {
                    run_id: Uuid::nil(),
                    org_id: Uuid::nil(),
                    state: "running".to_owned(),
                },
                ("runs", "updated"),
            ),
            (
                ServerEvent::JobUpdated {
                    run_id: Uuid::nil(),
                    job_id: Uuid::nil(),
                    state: "running".to_owned(),
                },
                ("jobs", "updated"),
            ),
            (
                ServerEvent::LogAppended {
                    run_id: Uuid::nil(),
                    job_id: Uuid::nil(),
                    sequence: 1,
                },
                ("jobs", "log_appended"),
            ),
            (
                ServerEvent::WorkerPresence {
                    org_id: Uuid::nil(),
                    worker_id: Uuid::nil(),
                    state: "idle".to_owned(),
                },
                ("workers", "presence"),
            ),
            (
                ServerEvent::ChatEvent {
                    org_id: Uuid::nil(),
                    surface: "internal".to_owned(),
                    kind: "message".to_owned(),
                },
                ("chat", "event"),
            ),
        ];
        for (event, (domain, name)) in cases {
            assert_eq!(event.domain(), domain);
            assert_eq!(event.name(), name);
        }
    }

    #[test]
    fn events_round_trip_through_their_wire_form() {
        let event = ServerEvent::LogAppended {
            run_id: Uuid::from_u128(1),
            job_id: Uuid::from_u128(2),
            sequence: 7,
        };
        let encoded = serde_json::to_string(&event).expect("serialise");
        assert!(encoded.contains("\"type\":\"log_appended\""));
        assert_eq!(
            serde_json::from_str::<ServerEvent>(&encoded).expect("deserialise"),
            event
        );
    }

    #[test]
    fn the_fallback_rate_limit_secret_is_long_and_not_constant() {
        let first = random_secret();
        assert_eq!(first.len(), 32);
        assert_ne!(first, random_secret());
    }
}
