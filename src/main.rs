#![forbid(unsafe_code)]

use gha_indie_worker_api_server::{config::ApiConfig, error::ApiError, flags, server};

fn main() -> Result<(), ApiError> {
    let environment = flags::resolve().map_err(ApiError::ConfigurationResolution)?;
    let cfg = ApiConfig::from_map(&environment);
    server::run(&cfg)
}
