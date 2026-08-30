#![forbid(unsafe_code)]

use gha_indie_worker_api_server::{config::ApiConfig, error::ApiError, server};

fn main() -> Result<(), ApiError> {
    let cfg = ApiConfig::from_env();
    server::run(&cfg)
}
