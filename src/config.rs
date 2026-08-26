#![forbid(unsafe_code)]

#[derive(Clone, Debug)]
pub struct ApiConfig {
    pub bind: String,
    pub tcp_bind: Option<String>,
    pub nats_url: Option<String>,
}

impl ApiConfig {
    pub fn from_env() -> Self {
        Self {
            bind: std::env::var("GHA_INDIE_WORKER_API_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into()),
            tcp_bind: std::env::var("GHA_INDIE_WORKER_API_TCP_BIND").ok(),
            nats_url: std::env::var("GHA_INDIE_WORKER_NATS_URL").ok(),
        }
    }
}

