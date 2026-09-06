#![forbid(unsafe_code)]

//! ores-otel structured logging bridged into this service's JSON tracing stream.
//!
//! Shape copied from the verified adoption template
//! (`ref/canonical-api-server.rs/src/telemetry.rs`), with
//! `service.namespace = gha-indie-worker`. One subscriber is installed; the
//! bridge never attaches credentials, URLs, request bodies, identity values or
//! upstream response bodies.
//!
//! The `next-loggers` dependency is behind the default-on `otel` feature and is
//! named nowhere else, so a change in the pinned `ores.otel.log` revision is
//! contained to this file. Without the feature the service still logs — it just
//! logs through `tracing` alone.

use tracing_subscriber::EnvFilter;

pub const SERVICE_NAME: &str = "gha-indie-worker-api-server";
pub const SERVICE_NAMESPACE: &str = "gha-indie-worker";

#[cfg(feature = "otel")]
pub use ores::logger as ores_logger;

#[cfg(feature = "otel")]
mod ores {
    use std::sync::Arc;

    use next_loggers::{
        json, JsonObject, LogLevel, LogRecord, Logger, LoggerError, Options, Transport,
    };

    use super::{SERVICE_NAME, SERVICE_NAMESPACE};

    /// Build the ores logger and announce startup on both sinks.
    pub fn logger() -> Logger {
        let logger = Logger::new(Options {
            app_name: SERVICE_NAME.to_string(),
            name: Some("server".to_string()),
            console: false,
            transports: vec![Arc::new(TracingBridgeTransport)],
            ..Options::default()
        });
        let _ = logger
            .info(vec![json!("telemetry initialized")])
            .add_fields(JsonObject::from_iter([
                ("service.name".to_string(), json!(SERVICE_NAME)),
                ("service.namespace".to_string(), json!(SERVICE_NAMESPACE)),
                ("log.destination".to_string(), json!("tracing-bridge")),
            ]))
            .send();
        logger
    }

    /// Every ores record is re-emitted as a `tracing` event at the matching
    /// level, so one process has exactly one log stream.
    struct TracingBridgeTransport;

    impl Transport for TracingBridgeTransport {
        fn write(&self, record: &LogRecord) -> Result<(), LoggerError> {
            let encoded = record.to_json()?;
            match record.level {
                LogLevel::Trace => tracing::trace!(ores.record = %encoded, "ores structured log"),
                LogLevel::Debug => tracing::debug!(ores.record = %encoded, "ores structured log"),
                LogLevel::Info => tracing::info!(ores.record = %encoded, "ores structured log"),
                LogLevel::Warn => tracing::warn!(ores.record = %encoded, "ores structured log"),
                LogLevel::Error => tracing::error!(ores.record = %encoded, "ores structured log"),
                LogLevel::Fatal => {
                    tracing::error!(ores.record = %encoded, "ores fatal structured log");
                }
            }
            Ok(())
        }

        fn is_open_telemetry(&self) -> bool {
            true
        }
    }
}

/// Held for the process lifetime; flushes the ores logger on drop.
pub struct TelemetryGuard {
    #[cfg(feature = "otel")]
    logger: next_loggers::Logger,
}

impl TelemetryGuard {
    /// The ores logger, for handing to `ores-middleware`'s axum adapter.
    #[cfg(feature = "otel")]
    #[must_use]
    pub fn logger(&self) -> next_loggers::Logger {
        self.logger.clone()
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        #[cfg(feature = "otel")]
        if self.logger.close().is_err() {
            eprintln!("telemetry: ores logger shutdown failed; final records may be incomplete");
        }
    }
}

/// Install the process-wide subscriber. Call once, from `main`.
#[must_use]
pub fn init() -> TelemetryGuard {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .json()
        .with_ansi(false)
        .with_target(true)
        .init();

    #[cfg(feature = "otel")]
    let guard = TelemetryGuard {
        logger: ores::logger(),
    };
    #[cfg(not(feature = "otel"))]
    let guard = TelemetryGuard {};

    tracing::info!(
        service.name = SERVICE_NAME,
        service.namespace = SERVICE_NAMESPACE,
        log.format = "json",
        log.destination = "stderr",
        otel.bridge = cfg!(feature = "otel"),
        "telemetry initialized"
    );
    guard
}
