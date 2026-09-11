#![forbid(unsafe_code)]

#[allow(clippy::match_like_matches_macro)]
#[path = "../generated/rust/runtime.rs"]
mod env_runtime;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiConfig {
    pub bind: String,
    pub tcp_bind: Option<String>,
    pub nats_url: Option<String>,
}

impl ApiConfig {
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let values = env_runtime::load_from(lookup);
        Self {
            bind: values
                .gha_indie_worker_api_bind
                .unwrap_or_else(|| "127.0.0.1:8080".into()),
            tcp_bind: values.gha_indie_worker_api_tcp_bind,
            nats_url: values.gha_indie_worker_nats_url,
        }
    }

    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn from_pairs(values: &[(&str, &str)]) -> ApiConfig {
        let values = values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>();
        ApiConfig::from_lookup(|key| values.get(key).cloned())
    }

    #[test]
    fn generated_runtime_values_drive_api_config() {
        let config = from_pairs(&[
            ("GHA_INDIE_WORKER_API_BIND", "127.0.0.1:18080"),
            ("GHA_INDIE_WORKER_API_TCP_BIND", "127.0.0.1:19090"),
            ("GHA_INDIE_WORKER_NATS_URL", "nats://127.0.0.1:4222"),
        ]);
        assert_eq!(
            config,
            ApiConfig {
                bind: "127.0.0.1:18080".into(),
                tcp_bind: Some("127.0.0.1:19090".into()),
                nats_url: Some("nats://127.0.0.1:4222".into()),
            }
        );
    }

    #[test]
    fn empty_values_follow_generated_runtime_semantics() {
        let config = from_pairs(&[
            ("GHA_INDIE_WORKER_API_BIND", ""),
            ("GHA_INDIE_WORKER_API_TCP_BIND", ""),
            ("GHA_INDIE_WORKER_NATS_URL", ""),
        ]);
        assert_eq!(config.bind, "127.0.0.1:8080");
        assert_eq!(config.tcp_bind, None);
        assert_eq!(config.nats_url, None);
    }
}
