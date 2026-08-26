#![forbid(unsafe_code)]

use gha_indie_worker_api_server::{config::ApiConfig, server};

fn main() {
    let cfg = ApiConfig::from_env();
    server::run(&cfg);
}

