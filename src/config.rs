#![forbid(unsafe_code)]

#[allow(clippy::match_like_matches_macro)]
#[rustfmt::skip]
#[path = "../generated/rust/runtime.rs"]
mod env_runtime;

const NATS_URL_ENV: &str = "GHA_INDIE_WORKER_NATS_URL";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiConfig {
    pub bind: String,
    pub tcp_bind: Option<String>,
    pub nats_url: Option<String>,
}

impl ApiConfig {
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        // Public argv/env configuration remains generated from `.cli-flags.toml`.
        // NATS may embed credentials, so it is deliberately not a CLI flag and
        // is read only from the injected runtime environment/secret boundary.
        let values = env_runtime::load_from(|key| lookup(key));
        let nats_url = lookup(NATS_URL_ENV).filter(|value| !value.is_empty());
        Self {
            bind: values
                .gha_indie_worker_api_bind
                .unwrap_or_else(|| "127.0.0.1:8080".into()),
            tcp_bind: values.gha_indie_worker_api_tcp_bind,
            nats_url,
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
    fn generated_public_values_and_secret_nats_env_drive_api_config() {
        let config = from_pairs(&[
            ("GHA_INDIE_WORKER_API_BIND", "127.0.0.1:18080"),
            ("GHA_INDIE_WORKER_API_TCP_BIND", "127.0.0.1:19090"),
            (NATS_URL_ENV, "nats://user:secret@127.0.0.1:4222"),
        ]);
        assert_eq!(
            config,
            ApiConfig {
                bind: "127.0.0.1:18080".into(),
                tcp_bind: Some("127.0.0.1:19090".into()),
                nats_url: Some("nats://user:secret@127.0.0.1:4222".into()),
            }
        );
    }

    #[test]
    fn empty_values_follow_runtime_semantics() {
        let config = from_pairs(&[
            ("GHA_INDIE_WORKER_API_BIND", ""),
            ("GHA_INDIE_WORKER_API_TCP_BIND", ""),
            (NATS_URL_ENV, ""),
        ]);
        assert_eq!(config.bind, "127.0.0.1:8080");
        assert_eq!(config.tcp_bind, None);
        assert_eq!(config.nats_url, None);
    }

    #[test]
    fn secret_nats_url_is_not_part_of_generated_public_runtime() {
        let generated = include_str!("../generated/rust/runtime.rs");
        assert!(!generated.contains(NATS_URL_ENV));
        let public_contract = include_str!("../.cli-flags.toml");
        assert!(!public_contract.contains("gha-indie-worker-nats-url"));
    }
}
