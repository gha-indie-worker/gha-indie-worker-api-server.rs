#![forbid(unsafe_code)]

//! Immutable application environment snapshot.
//!
//! Process environment and CLI overrides are copied into an ordinary map.
//! This module never writes the process environment.

use std::collections::BTreeMap;

pub type EnvMap = BTreeMap<String, String>;

pub fn merge_env(
    mut initial: EnvMap,
    overrides: impl IntoIterator<Item = (String, String)>,
) -> EnvMap {
    initial.extend(overrides);
    initial
}

pub fn value<'a>(env: &'a EnvMap, key: &str) -> Option<&'a str> {
    env.get(key)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub fn truthy(env: &EnvMap, key: &str) -> bool {
    value(env, key).is_some_and(|raw| {
        matches!(
            raw.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_overrides_win_without_mutating_process_environment() {
        let before = std::env::var_os("ENV_MAP_PROBE");
        let env = merge_env(
            EnvMap::from([("ENV_MAP_PROBE".into(), "before".into())]),
            [("ENV_MAP_PROBE".into(), "after".into())],
        );
        assert_eq!(value(&env, "ENV_MAP_PROBE"), Some("after"));
        assert_eq!(std::env::var_os("ENV_MAP_PROBE"), before);
    }

    #[test]
    fn empty_and_whitespace_values_are_absent() {
        for raw in ["", " ", "\t"] {
            let env = EnvMap::from([("ENV_MAP_PROBE".into(), raw.into())]);
            assert_eq!(value(&env, "ENV_MAP_PROBE"), None, "raw={raw:?}");
        }
        let env = EnvMap::from([("ENV_MAP_PROBE".into(), "  kept  ".into())]);
        assert_eq!(value(&env, "ENV_MAP_PROBE"), Some("kept"));
    }

    #[test]
    fn source_does_not_mutate_process_environment() {
        const SRC: &str = include_str!("env_map.rs");
        let production = SRC.split("#[cfg(test)]").next().unwrap_or(SRC);
        assert!(!production.contains("std::env::set_var"));
        assert!(!production.contains("env::set_var"));
    }
}
