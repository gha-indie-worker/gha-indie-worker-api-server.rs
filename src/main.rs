#![forbid(unsafe_code)]

use gha_indie_worker_api_server::{config::ApiConfig, server};
use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cfg = ApiConfig::from_env();
    server::run(&cfg).await
}
