#![forbid(unsafe_code)]

//! Process entry point.
//!
//! Order matters: telemetry first (so a configuration failure is *logged* in
//! the same structured stream as everything else), then configuration, then
//! state, then listeners. Nothing binds a port until every configured
//! dependency has been opened.

use std::process::ExitCode;

use gha_indie_worker_api_server::{config::ApiConfig, flags, server, state::AppState, telemetry};

#[tokio::main]
async fn main() -> ExitCode {
    let _telemetry = telemetry::init();

    // flags-2-env resolves argv + environment against the embedded contract and
    // fails closed on unknown options without echoing their values.
    let environment = match flags::resolve() {
        Ok(environment) => environment,
        Err(error) => {
            tracing::error!(%error, "configuration resolution failed");
            return ExitCode::FAILURE;
        }
    };
    let config = match ApiConfig::from_map(&environment) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(%error, "configuration is invalid");
            return ExitCode::FAILURE;
        }
    };
    // `ApiConfig`'s `Debug` redacts every credential-bearing field, so this is
    // safe to emit at startup and is the fastest way to answer "what is this
    // process actually wired to".
    tracing::info!(config = ?config, "configuration loaded");

    let state = match AppState::build(config).await {
        Ok(state) => state,
        Err(error) => {
            tracing::error!(%error, "startup failed");
            return ExitCode::FAILURE;
        }
    };

    match server::run(state).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "server failed");
            ExitCode::FAILURE
        }
    }
}
