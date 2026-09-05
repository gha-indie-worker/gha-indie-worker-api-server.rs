#![forbid(unsafe_code)]

use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct ApiConfig {
    pub bind: String,
    pub tcp_bind: Option<String>,
    pub nats_url: Option<String>,
}

impl ApiConfig {
    pub fn from_map(environment: &BTreeMap<String, String>) -> Self {
        Self {
            bind: environment
                .get("GHA_INDIE_WORKER_API_BIND")
                .cloned()
                .unwrap_or_else(|| "127.0.0.1:8080".into()),
            tcp_bind: environment.get("GHA_INDIE_WORKER_API_TCP_BIND").cloned(),
            nats_url: environment.get("GHA_INDIE_WORKER_NATS_URL").cloned(),
        }
    }
}
