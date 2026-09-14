#![forbid(unsafe_code)]

use crate::routing::{
    RouterConfig, DEFAULT_PATH_FALLBACK_HOSTS, DEFAULT_PATH_PREFIX, DEFAULT_ROUTING_SUFFIX,
};

#[derive(Clone, Debug)]
pub struct ApiConfig {
    pub bind: String,
    pub tcp_bind: Option<String>,
    pub nats_url: Option<String>,
    pub routing_domain_suffix: String,
    pub routing_path_prefix: String,
    pub routing_path_hosts: Vec<String>,
}

impl ApiConfig {
    pub fn from_env() -> Self {
        let default_path_hosts = DEFAULT_PATH_FALLBACK_HOSTS.join(",");
        Self {
            bind: std::env::var("GHA_INDIE_WORKER_API_BIND")
                .unwrap_or_else(|_| "127.0.0.1:8080".into()),
            tcp_bind: std::env::var("GHA_INDIE_WORKER_API_TCP_BIND").ok(),
            nats_url: std::env::var("GHA_INDIE_WORKER_NATS_URL").ok(),
            routing_domain_suffix: std::env::var("GHA_INDIE_WORKER_ROUTING_DOMAIN_SUFFIX")
                .unwrap_or_else(|_| DEFAULT_ROUTING_SUFFIX.into()),
            routing_path_prefix: std::env::var("GHA_INDIE_WORKER_ROUTING_PATH_PREFIX")
                .unwrap_or_else(|_| DEFAULT_PATH_PREFIX.into()),
            routing_path_hosts: std::env::var("GHA_INDIE_WORKER_ROUTING_PATH_HOSTS")
                .unwrap_or(default_path_hosts)
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect(),
        }
    }

    pub fn router_config(&self) -> RouterConfig {
        RouterConfig {
            domain_suffix: self.routing_domain_suffix.clone(),
            path_prefix: self.routing_path_prefix.clone(),
            path_fallback_hosts: self.routing_path_hosts.clone(),
        }
    }
}
