#![forbid(unsafe_code)]

//! NATS transport: domain events out, optional run requests in.
//!
//! Subjects follow the fleet convention `giw.<env>.<domain>.<event>` — for
//! example `giw.prod.runs.updated`, `giw.prod.workers.presence`. The subject
//! namespace is owned by the environment, so a staging publisher can never be
//! mistaken for production by a subscriber.
//!
//! Publishing is **fail-open**: a NATS outage degrades observability, it does
//! not fail a request that already succeeded. The intake is the opposite —
//! anything it cannot parse is dropped with a log line rather than guessed at.
//!
//! Everything that names `async_nats` is behind the default-on
//! `nats-transport` feature and lives in this file.

use crate::config::DeployEnv;

/// Build a subject from the fleet convention. `domain` and `event` are fixed
/// strings from this crate, never caller input.
#[must_use]
pub fn subject(env: DeployEnv, domain: &str, event: &str) -> String {
    format!("giw.{}.{}.{}", env.as_str(), domain, event)
}

/// The JetStream work-queue subject this service may consume from.
#[must_use]
pub fn intake_subject(env: DeployEnv) -> String {
    subject(env, "runs", "requests")
}

#[cfg(feature = "nats-transport")]
pub use enabled::{connect, Publisher};

#[cfg(feature = "nats-transport")]
mod enabled {
    use std::sync::Arc;

    use crate::config::DeployEnv;
    use crate::state::ServerEvent;

    /// A connected publisher. Cloning shares one connection.
    #[derive(Clone)]
    pub struct Publisher {
        client: async_nats::Client,
        env: DeployEnv,
    }

    impl std::fmt::Debug for Publisher {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("Publisher")
                .field("env", &self.env)
                .finish_non_exhaustive()
        }
    }

    /// Connect to NATS.
    ///
    /// # Errors
    /// Returns a human-readable message when the URL cannot be reached. The URL
    /// is not included: it may carry credentials.
    pub async fn connect(url: &str, env: DeployEnv) -> Result<Publisher, String> {
        match async_nats::connect(url).await {
            Ok(client) => {
                tracing::info!(env = env.as_str(), "nats transport connected");
                Ok(Publisher { client, env })
            }
            Err(error) => Err(format!("nats connection failed: {error}")),
        }
    }

    impl Publisher {
        /// Publish one domain event. Failures are logged, never propagated:
        /// the request that produced this event has already succeeded.
        pub async fn publish(&self, event: &ServerEvent) {
            let subject = super::subject(self.env, event.domain(), event.name());
            let Ok(payload) = serde_json::to_vec(event) else {
                tracing::warn!(%subject, "domain event could not be encoded");
                return;
            };
            if let Err(error) = self
                .client
                .publish(subject.clone(), bytes::Bytes::from(payload))
                .await
            {
                tracing::warn!(%subject, %error, "domain event publish failed");
            }
        }

        /// Subscribe to the run-request intake and forward each parsed request
        /// to `handler`.
        ///
        /// Core NATS, not JetStream: a durable work queue needs a stream that
        /// this service must not create (it would be DDL by another name).
        /// Provision the stream declaratively, then point this at it.
        ///
        /// # Errors
        /// Returns a message when the subscription cannot be established.
        pub async fn run_intake<F>(&self, handler: Arc<F>) -> Result<(), String>
        where
            F: Fn(serde_json::Value) + Send + Sync + 'static,
        {
            use futures_util::StreamExt as _;

            let subject = super::intake_subject(self.env);
            let mut subscriber = self
                .client
                .subscribe(subject.clone())
                .await
                .map_err(|error| format!("nats intake subscription failed: {error}"))?;
            tracing::info!(%subject, "nats intake listening");

            tokio::spawn(async move {
                while let Some(message) = subscriber.next().await {
                    match serde_json::from_slice::<serde_json::Value>(&message.payload) {
                        Ok(value) => (*handler)(value),
                        Err(error) => {
                            tracing::warn!(%error, "dropping unparseable intake message");
                        }
                    }
                }
            });
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects_follow_the_fleet_namespace() {
        assert_eq!(
            subject(DeployEnv::Prod, "runs", "updated"),
            "giw.prod.runs.updated"
        );
        assert_eq!(
            subject(DeployEnv::Staging, "workers", "presence"),
            "giw.staging.workers.presence"
        );
        assert_eq!(intake_subject(DeployEnv::Dev), "giw.dev.runs.requests");
    }

    #[test]
    fn environments_partition_the_subject_space() {
        let prod = subject(DeployEnv::Prod, "runs", "updated");
        let staging = subject(DeployEnv::Staging, "runs", "updated");
        assert_ne!(prod, staging);
        assert!(prod.starts_with("giw.prod."));
        assert!(staging.starts_with("giw.staging."));
    }
}
