#![forbid(unsafe_code)]

//! SeaORM connection pools.
//!
//! Two independent pools, exactly as the fleet's provider isolation requires:
//!
//! * `DATABASE_URL_CANONICAL` — product data (Neon project `crimson-cell-…`).
//! * `DATABASE_URL_AUTH` — the federated auth projection (`fancy-brook-…`).
//!
//! Admin data is never reachable from this process; the firewall enforces that
//! and this module simply never opens such a pool.
//!
//! **This service never runs DDL.** There is no migrator, no `CREATE`, no
//! `ALTER` and no schema check that would tempt one. Schema convergence is the
//! declarative pipeline's job; readiness only asks whether the pool answers.
//!
//! Direct `sqlx`/`tokio-postgres` use is forbidden fleet-wide — everything goes
//! through SeaORM, and everything SeaORM goes through this module.

use std::time::Duration;

use crate::config::DatabaseConfig;

/// Both pools. Either may be absent: the service starts without a database and
/// reports itself *not ready* rather than refusing to boot, so a rollout can
/// surface a bad connection string on `/readyz` instead of in a crash loop.
#[derive(Clone, Debug, Default)]
pub struct Pools {
    #[cfg(feature = "db")]
    pub canonical: Option<sea_orm::DatabaseConnection>,
    #[cfg(feature = "db")]
    pub auth: Option<sea_orm::DatabaseConnection>,
}

impl Pools {
    /// Whether the canonical pool exists. Readiness is fail-closed on this.
    #[cfg(feature = "db")]
    #[must_use]
    pub const fn canonical_configured(&self) -> bool {
        self.canonical.is_some()
    }

    /// Whether the canonical pool exists. Readiness is fail-closed on this.
    #[cfg(not(feature = "db"))]
    #[must_use]
    pub const fn canonical_configured(&self) -> bool {
        false
    }

    #[cfg(feature = "db")]
    #[must_use]
    pub const fn auth_configured(&self) -> bool {
        self.auth.is_some()
    }

    #[cfg(not(feature = "db"))]
    #[must_use]
    pub const fn auth_configured(&self) -> bool {
        false
    }
}

/// Open the configured pools.
///
/// A configured-but-unreachable database is a **startup failure**: a service
/// that silently starts with no persistence is worse than one that does not
/// start. A database that is simply not configured is not an error.
///
/// # Errors
/// Returns a human-readable message when a configured URL cannot be opened.
/// The message never contains the URL, which carries credentials.
#[cfg(feature = "db")]
pub async fn connect(config: &DatabaseConfig) -> Result<Pools, String> {
    let canonical = open(config.canonical_url.as_deref(), config, "canonical").await?;
    let auth = open(config.auth_url.as_deref(), config, "auth").await?;
    Ok(Pools { canonical, auth })
}

#[cfg(not(feature = "db"))]
#[allow(clippy::unused_async)]
pub async fn connect(_config: &DatabaseConfig) -> Result<Pools, String> {
    tracing::warn!("the `db` feature is disabled; no database pool will be opened");
    Ok(Pools::default())
}

#[cfg(feature = "db")]
async fn open(
    url: Option<&str>,
    config: &DatabaseConfig,
    label: &'static str,
) -> Result<Option<sea_orm::DatabaseConnection>, String> {
    let Some(url) = url else {
        tracing::info!(pool = label, "database pool is not configured");
        return Ok(None);
    };
    let mut options = sea_orm::ConnectOptions::new(url.to_owned());
    options
        .max_connections(config.max_connections.max(1))
        .min_connections(1)
        .connect_timeout(config.connect_timeout)
        .acquire_timeout(config.connect_timeout)
        .idle_timeout(Duration::from_secs(300))
        .sqlx_logging(false);

    match sea_orm::Database::connect(options).await {
        Ok(connection) => {
            tracing::info!(pool = label, "database pool opened");
            Ok(Some(connection))
        }
        // The URL is deliberately absent from this message: it carries a password.
        Err(error) => Err(format!(
            "{label} database pool could not be opened: {error}"
        )),
    }
}

/// Liveness probe for `/readyz`.
///
/// # Errors
/// Returns a message when the pool is absent or does not answer.
#[cfg(feature = "db")]
pub async fn probe(pool: Option<&sea_orm::DatabaseConnection>) -> Result<(), String> {
    let Some(pool) = pool else {
        return Err("database pool is not configured".to_owned());
    };
    pool.ping()
        .await
        .map_err(|error| format!("database ping failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unconfigured_pool_set_is_not_ready() {
        let pools = Pools::default();
        assert!(!pools.canonical_configured());
        assert!(!pools.auth_configured());
    }

    #[tokio::test]
    async fn no_configuration_opens_no_pool_and_is_not_an_error() {
        let pools = connect(&DatabaseConfig::default())
            .await
            .expect("an unconfigured database is not a startup failure");
        assert!(!pools.canonical_configured());
    }
}
