//! Fail-closed argv and environment resolution through flags-2-env.

use std::collections::BTreeMap;
use std::io::Write;

use flags2env::BundledFlags2Env;
use tempfile::NamedTempFile;

const CONTRACT: &str = include_str!("../.cli-flags.toml");
const NATS_URL_ENV: &str = "GHA_INDIE_WORKER_NATS_URL";

pub fn resolve() -> Result<BTreeMap<String, String>, String> {
    resolve_from(&std::env::args().collect::<Vec<_>>(), std::env::vars())
}

fn resolve_from(
    argv: &[String],
    environment: impl IntoIterator<Item = (String, String)>,
) -> Result<BTreeMap<String, String>, String> {
    let environment = environment.into_iter().collect::<BTreeMap<_, _>>();
    let mut contract = NamedTempFile::new()
        .map_err(|error| format!("cannot create embedded flags-2-env contract: {error}"))?;
    contract
        .write_all(CONTRACT.as_bytes())
        .map_err(|error| format!("cannot materialize embedded flags-2-env contract: {error}"))?;
    let path = contract
        .path()
        .to_str()
        .ok_or_else(|| "flags-2-env contract path is not valid UTF-8".to_owned())?;
    let parser = BundledFlags2Env::new();
    parser
        .audit_config(Some(path))
        .map_err(|error| format!("flags-2-env contract audit failed: {error}"))?;
    let parsed = parser
        .parse_structured(argv, Some(path))
        .map_err(|error| format!("flags-2-env parsing failed: {error}"))?;

    if !parsed.unknown_options.is_empty() {
        let names = parsed
            .unknown_options
            .iter()
            .map(|option| option.split('=').next().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!("unknown command-line option(s): {names}"));
    }
    if !parsed.errors.is_empty() {
        return Err(format!(
            "invalid command-line value(s): {}",
            parsed.errors.join("; ")
        ));
    }
    if !parsed.extras.is_empty() {
        return Err(format!(
            "unexpected positional argument(s): {}",
            parsed.extras.len()
        ));
    }

    // The public flag catalog owns argv normalization, but plaintext dotenv is
    // not an application configuration source for this fleet. Start from the
    // explicit process environment and overlay only values actually supplied as
    // declared public flags.
    let mut raw = environment.clone();
    raw.extend(parsed.provided_flags);
    let typed = parser
        .coerce::<serde_json::Map<String, serde_json::Value>, _>(&raw, Some(path))
        .map_err(|error| format!("flags-2-env typed configuration failed: {error}"))?;
    let mut resolved = typed
        .into_iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(name, value)| scalar_string(&name, value).map(|value| (name, value)))
        .collect::<Result<BTreeMap<_, _>, _>>()?;

    // NATS endpoints can legally embed credentials. Keep that value outside
    // argv/flags-2-env and copy it only from the process secret boundary.
    if let Some(value) = environment.get(NATS_URL_ENV) {
        if !value.trim().is_empty() {
            resolved.insert(NATS_URL_ENV.to_owned(), value.clone());
        }
    }

    Ok(resolved)
}

fn scalar_string(name: &str, value: serde_json::Value) -> Result<String, String> {
    match value {
        serde_json::Value::String(value) => Ok(value),
        serde_json::Value::Bool(value) => Ok(value.to_string()),
        serde_json::Value::Number(value) => Ok(value.to_string()),
        _ => Err(format!(
            "flags-2-env returned a non-scalar value for {name}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_options_fail_closed_without_echoing_values() {
        let error = resolve_from(
            &[
                "server".to_owned(),
                "--definitely-unknown=do-not-echo".to_owned(),
            ],
            std::iter::empty(),
        )
        .expect_err("unknown option");
        assert!(error.contains("--definitely-unknown"));
        assert!(!error.contains("do-not-echo"));
    }

    #[test]
    fn nats_url_is_environment_only() {
        let credentialed = "nats://worker:synthetic-credential@nats.internal:4222";
        let resolved = resolve_from(
            &["server".to_owned()],
            [(NATS_URL_ENV.to_owned(), credentialed.to_owned())],
        )
        .expect("environment-only NATS URL");
        assert_eq!(
            resolved.get(NATS_URL_ENV).map(String::as_str),
            Some(credentialed)
        );

        let error = resolve_from(
            &[
                "server".to_owned(),
                format!("--gha-indie-worker-nats-url={credentialed}"),
            ],
            std::iter::empty(),
        )
        .expect_err("NATS URL must not be accepted through argv");
        assert!(error.contains("--gha-indie-worker-nats-url"));
        assert!(!error.contains("synthetic-credential"));
    }

    #[test]
    fn plaintext_dotenv_is_not_a_runtime_source() {
        const SOURCE: &str = include_str!("flags.rs");
        let production = SOURCE.split("#[cfg(test)]").next().unwrap_or(SOURCE);
        assert!(!production.contains("parsed.dotenv"));
        assert!(!production.contains("dotenv_overrides"));
    }
}
