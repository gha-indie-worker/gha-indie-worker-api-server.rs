#![forbid(unsafe_code)]

use crate::env_map::{value, EnvMap};

#[derive(Clone, Debug)]
pub struct ApiConfig {
    pub bind: String,
    pub tcp_bind: Option<String>,
    pub nats_url: Option<String>,
}

impl ApiConfig {
    pub fn from_env_map(env: &EnvMap) -> Self {
        Self {
            bind: value(env, "GHA_INDIE_WORKER_API_BIND")
                .unwrap_or("127.0.0.1:8080")
                .to_owned(),
            tcp_bind: value(env, "GHA_INDIE_WORKER_API_TCP_BIND").map(str::to_owned),
            nats_url: value(env, "GHA_INDIE_WORKER_NATS_URL").map(str::to_owned),
        }
    }

    pub fn from_env() -> Self {
        Self::from_env_map(&std::env::vars().collect())
    }
}
