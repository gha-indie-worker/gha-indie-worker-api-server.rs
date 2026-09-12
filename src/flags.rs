#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::Path;

use crate::env_map::{merge_env, EnvMap};
use flags2env::BundledFlags2Env;

pub fn parse_cli_flags(
    argv: &[String],
    config_path: &Path,
) -> Result<HashMap<String, String>, String> {
    let config_path = config_path
        .to_str()
        .ok_or_else(|| ".cli-flags.toml path is not valid UTF-8".to_string())?;
    let parser = BundledFlags2Env::new();
    parser
        .audit_config(Some(config_path))
        .map_err(|error| format!("flags-2-env configuration audit failed: {error}"))?;
    let parsed = parser
        .parse_structured(argv, Some(config_path))
        .map_err(|error| format!("flags-2-env parse failed: {error}"))?;
    if !parsed.unknown_options.is_empty() {
        return Err(format!(
            "unknown command-line option(s): {}",
            parsed.unknown_options.join(", ")
        ));
    }
    if !parsed.errors.is_empty() {
        return Err(format!(
            "invalid command-line value(s): {}",
            parsed.errors.join("; ")
        ));
    }
    Ok(parsed.flags)
}

pub fn apply_cli_flags() -> Result<EnvMap, String> {
    apply_cli_flags_from(
        std::env::args().collect(),
        std::env::vars().collect(),
        Path::new(".cli-flags.toml"),
    )
}

pub fn apply_cli_flags_from(
    argv: Vec<String>,
    initial: EnvMap,
    config_path: &Path,
) -> Result<EnvMap, String> {
    Ok(merge_env(initial, parse_cli_flags(&argv, config_path)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env_map::value;

    fn config_path() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(".cli-flags.toml")
    }

    #[test]
    fn cli_overrides_merge_into_map_without_mutating_process_env() {
        let before = std::env::var_os("ENV_MAP_PROBE");
        let env = apply_cli_flags_from(
            vec!["svc".into()],
            EnvMap::from([("ENV_MAP_PROBE".into(), "before".into())]),
            &config_path(),
        )
        .expect("valid flags");
        assert_eq!(value(&env, "ENV_MAP_PROBE"), Some("before"));
        assert_eq!(std::env::var_os("ENV_MAP_PROBE"), before);
    }

    #[test]
    fn parse_failure_does_not_mutate_process_environment() {
        let before = std::env::var_os("ENV_MAP_PROBE");
        let initial = EnvMap::from([("ENV_MAP_PROBE".into(), "keep".into())]);
        assert!(apply_cli_flags_from(
            vec!["svc".into(), "--this-flag-is-not-declared".into()],
            initial,
            &config_path(),
        )
        .is_err());
        assert_eq!(std::env::var_os("ENV_MAP_PROBE"), before);
    }

    #[test]
    fn source_does_not_mutate_process_environment() {
        const SRC: &str = include_str!("flags.rs");
        let production = SRC.split("#[cfg(test)]").next().unwrap_or(SRC);
        assert!(!production.contains("std::env::set_var"));
        assert!(!production.contains("env::set_var"));
    }
}
