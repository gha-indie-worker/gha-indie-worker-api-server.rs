#![forbid(unsafe_code)]

//! Environment-only configuration.
//!
//! Every variable is declared in `.cli-flags.toml` (flags-2-env) and documented
//! in `.env.example`. Secrets are read here and never logged: [`ApiConfig`]
//! implements [`fmt::Debug`] by hand and redacts every credential-bearing
//! field. Parsing is a pure function of a lookup closure so it is unit-testable
//! without touching the process environment.

use std::collections::BTreeSet;
use std::fmt;
use std::time::Duration;

use thiserror::Error;

/// Deployment environment. Used for the NATS subject namespace and to decide
/// how strict startup validation is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeployEnv {
    Dev,
    Staging,
    Prod,
}

impl DeployEnv {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Staging => "staging",
            Self::Prod => "prod",
        }
    }

    fn parse(value: &str) -> Result<Self, ConfigError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "dev" | "development" | "local" => Ok(Self::Dev),
            "staging" | "stage" => Ok(Self::Staging),
            "prod" | "production" => Ok(Self::Prod),
            _ => Err(ConfigError::InvalidValue {
                variable: "GHA_INDIE_WORKER_ENV",
                reason: "expected one of dev, staging, prod",
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum ConfigError {
    #[error("{variable} is invalid: {reason}")]
    InvalidValue {
        variable: &'static str,
        reason: &'static str,
    },
    #[error("{a} and {b} must be configured together")]
    Incomplete { a: &'static str, b: &'static str },
    #[error("{variable} must contain at least {minimum} non-whitespace bytes")]
    WeakSecret {
        variable: &'static str,
        minimum: usize,
    },
}

/// shared-auth introspection settings (product-server triple).
#[derive(Clone)]
pub struct SharedAuthConfig {
    pub base: String,
    pub audience: String,
    pub introspect_secret: String,
}

/// Supabase / Neon JWT verification settings (the second half of dual auth).
#[derive(Clone, Default)]
pub struct JwtConfig {
    /// Supabase HS256 shared secret (legacy Supabase projects).
    pub supabase_jwt_secret: Option<String>,
    pub supabase_url: Option<String>,
    pub supabase_anon_key: Option<String>,
    pub neon_auth_url: Option<String>,
    /// RS256 JWKS endpoint (Neon Auth, or a Supabase project using asymmetric keys).
    pub jwks_url: Option<String>,
    /// Accepted `iss` values. Empty means "do not pin the issuer".
    pub issuers: BTreeSet<String>,
    /// Accepted `aud` values. Empty means "do not pin the audience".
    pub audiences: BTreeSet<String>,
    pub jwks_ttl: Duration,
}

#[derive(Clone, Default)]
pub struct DatabaseConfig {
    pub canonical_url: Option<String>,
    pub auth_url: Option<String>,
    pub max_connections: u32,
    pub connect_timeout: Duration,
}

#[derive(Clone, Default)]
pub struct WebhookConfig {
    pub github_secret: Option<String>,
    pub delivery_ttl: Duration,
    pub max_deliveries: usize,
}

#[derive(Clone, Default)]
pub struct ChatConfig {
    pub api_base: Option<String>,
    pub timeout: Duration,
}

#[derive(Clone)]
pub struct EmbeddingConfig {
    pub base: Option<String>,
    pub api_key: Option<String>,
    pub model: String,
    pub dims: usize,
    pub timeout: Duration,
}

#[derive(Clone, Copy)]
pub struct RateLimitConfig {
    pub capacity: u32,
    pub window: Duration,
}

#[derive(Clone, Copy)]
pub struct WebSocketConfig {
    pub max_connections: usize,
    pub channel_capacity: usize,
    pub heartbeat: Duration,
}

#[derive(Clone)]
pub struct ApiConfig {
    pub env: DeployEnv,
    pub bind: String,
    pub tcp_bind: Option<String>,
    pub nats_url: Option<String>,
    pub nats_intake: bool,
    pub shutdown_grace: Duration,
    pub request_timeout: Duration,
    pub max_body_bytes: usize,
    pub heartbeat_ttl: Duration,
    pub database: DatabaseConfig,
    pub shared_auth: Option<SharedAuthConfig>,
    pub jwt: JwtConfig,
    pub webhooks: WebhookConfig,
    pub chat: ChatConfig,
    pub embeddings: EmbeddingConfig,
    pub rate_limit: RateLimitConfig,
    pub websocket: WebSocketConfig,
    /// HMAC key for opaque rate-limit principals. Never a raw IP or token.
    pub rate_limit_hmac_secret: Option<String>,
}

const MIN_SECRET_BYTES: usize = 32;

impl ApiConfig {
    /// Reads the process environment.
    ///
    /// # Errors
    /// Returns [`ConfigError`] when a declared variable is present but unusable,
    /// or when a pair of variables that must travel together is half-configured.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Parse a map already resolved by flags-2-env ([`crate::flags::resolve`]).
    ///
    /// # Errors
    /// See [`Self::from_env`].
    pub fn from_map(
        environment: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self, ConfigError> {
        Self::from_lookup(|name| environment.get(name).cloned())
    }

    /// Pure parser over an arbitrary lookup, so tests never mutate the
    /// process environment.
    ///
    /// # Errors
    /// See [`Self::from_env`].
    pub fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let get = |name: &str| -> Option<String> {
            lookup(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };

        let env = match get("GHA_INDIE_WORKER_ENV") {
            Some(value) => DeployEnv::parse(&value)?,
            None => DeployEnv::Dev,
        };

        let shared_auth = match (
            get("SHARED_AUTH_BASE"),
            get("SHARED_AUTH_INTROSPECT_SECRET"),
        ) {
            (None, None) => None,
            (Some(base), Some(secret)) => {
                require_strong("SHARED_AUTH_INTROSPECT_SECRET", &secret)?;
                Some(SharedAuthConfig {
                    base,
                    audience: get("SHARED_AUTH_AUDIENCE")
                        .unwrap_or_else(|| "gha-indie-worker-api".to_owned()),
                    introspect_secret: secret,
                })
            }
            _ => {
                return Err(ConfigError::Incomplete {
                    a: "SHARED_AUTH_BASE",
                    b: "SHARED_AUTH_INTROSPECT_SECRET",
                })
            }
        };

        let jwt = JwtConfig {
            supabase_jwt_secret: get("SUPABASE_JWT_SECRET"),
            supabase_url: get("SUPABASE_URL"),
            supabase_anon_key: get("SUPABASE_ANON_KEY"),
            neon_auth_url: get("NEON_AUTH_URL"),
            jwks_url: get("NEON_AUTH_JWKS_URL"),
            issuers: csv_set(get("GHA_INDIE_WORKER_JWT_ISSUERS").as_deref()),
            audiences: csv_set(get("GHA_INDIE_WORKER_JWT_AUDIENCES").as_deref()),
            jwks_ttl: seconds(
                "GHA_INDIE_WORKER_JWKS_TTL_SECONDS",
                get("GHA_INDIE_WORKER_JWKS_TTL_SECONDS").as_deref(),
                300,
            )?,
        };

        let database = DatabaseConfig {
            canonical_url: get("DATABASE_URL_CANONICAL"),
            auth_url: get("DATABASE_URL_AUTH"),
            max_connections: number(
                "GHA_INDIE_WORKER_DB_MAX_CONNECTIONS",
                get("GHA_INDIE_WORKER_DB_MAX_CONNECTIONS").as_deref(),
                10,
            )?,
            connect_timeout: seconds(
                "GHA_INDIE_WORKER_DB_CONNECT_TIMEOUT_SECONDS",
                get("GHA_INDIE_WORKER_DB_CONNECT_TIMEOUT_SECONDS").as_deref(),
                5,
            )?,
        };

        let github_secret = get("GHA_INDIE_WORKER_GITHUB_WEBHOOK_SECRET");
        if let Some(secret) = github_secret.as_deref() {
            require_strong("GHA_INDIE_WORKER_GITHUB_WEBHOOK_SECRET", secret)?;
        }
        let webhooks = WebhookConfig {
            github_secret,
            delivery_ttl: seconds(
                "GHA_INDIE_WORKER_WEBHOOK_DELIVERY_TTL_SECONDS",
                get("GHA_INDIE_WORKER_WEBHOOK_DELIVERY_TTL_SECONDS").as_deref(),
                3_600,
            )?,
            max_deliveries: number(
                "GHA_INDIE_WORKER_MAX_WEBHOOK_DELIVERIES",
                get("GHA_INDIE_WORKER_MAX_WEBHOOK_DELIVERIES").as_deref(),
                10_000,
            )? as usize,
        };

        let embeddings = EmbeddingConfig {
            base: get("GHA_INDIE_WORKER_EMBEDDINGS_BASE"),
            api_key: get("GHA_INDIE_WORKER_EMBEDDINGS_API_KEY"),
            model: get("GHA_INDIE_WORKER_EMBEDDINGS_MODEL")
                .unwrap_or_else(|| "text-embedding-3-small".to_owned()),
            dims: number(
                "GHA_INDIE_WORKER_EMBEDDINGS_DIMS",
                get("GHA_INDIE_WORKER_EMBEDDINGS_DIMS").as_deref(),
                1_536,
            )? as usize,
            timeout: seconds(
                "GHA_INDIE_WORKER_EMBEDDINGS_TIMEOUT_SECONDS",
                get("GHA_INDIE_WORKER_EMBEDDINGS_TIMEOUT_SECONDS").as_deref(),
                30,
            )?,
        };
        if embeddings.dims == 0 || embeddings.dims > crate::domain::embeddings::MAX_DIMENSIONS {
            return Err(ConfigError::InvalidValue {
                variable: "GHA_INDIE_WORKER_EMBEDDINGS_DIMS",
                reason: "must be between 1 and 4096 inclusive",
            });
        }

        Ok(Self {
            env,
            bind: get("GHA_INDIE_WORKER_API_BIND").unwrap_or_else(|| "0.0.0.0:8080".to_owned()),
            tcp_bind: get("GHA_INDIE_WORKER_API_TCP_BIND"),
            nats_url: get("GHA_INDIE_WORKER_NATS_URL"),
            nats_intake: flag(get("GHA_INDIE_WORKER_NATS_INTAKE_ENABLED").as_deref()),
            shutdown_grace: seconds(
                "GHA_INDIE_WORKER_API_SHUTDOWN_GRACE_SECONDS",
                get("GHA_INDIE_WORKER_API_SHUTDOWN_GRACE_SECONDS").as_deref(),
                30,
            )?,
            request_timeout: seconds(
                "GHA_INDIE_WORKER_API_REQUEST_TIMEOUT_SECONDS",
                get("GHA_INDIE_WORKER_API_REQUEST_TIMEOUT_SECONDS").as_deref(),
                30,
            )?,
            max_body_bytes: number(
                "GHA_INDIE_WORKER_API_MAX_BODY_BYTES",
                get("GHA_INDIE_WORKER_API_MAX_BODY_BYTES").as_deref(),
                1_048_576,
            )? as usize,
            heartbeat_ttl: seconds(
                "GHA_INDIE_WORKER_HEARTBEAT_TTL_SECONDS",
                get("GHA_INDIE_WORKER_HEARTBEAT_TTL_SECONDS").as_deref(),
                90,
            )?,
            database,
            shared_auth,
            jwt,
            webhooks,
            chat: ChatConfig {
                api_base: get("ORES_CHAT_API_BASE"),
                timeout: seconds(
                    "ORES_CHAT_TIMEOUT_SECONDS",
                    get("ORES_CHAT_TIMEOUT_SECONDS").as_deref(),
                    15,
                )?,
            },
            embeddings,
            rate_limit: RateLimitConfig {
                capacity: number(
                    "GHA_INDIE_WORKER_RATE_LIMIT_CAPACITY",
                    get("GHA_INDIE_WORKER_RATE_LIMIT_CAPACITY").as_deref(),
                    120,
                )?,
                window: seconds(
                    "GHA_INDIE_WORKER_RATE_LIMIT_WINDOW_SECONDS",
                    get("GHA_INDIE_WORKER_RATE_LIMIT_WINDOW_SECONDS").as_deref(),
                    60,
                )?,
            },
            websocket: WebSocketConfig {
                max_connections: number(
                    "GHA_INDIE_WORKER_WS_MAX_CONNECTIONS",
                    get("GHA_INDIE_WORKER_WS_MAX_CONNECTIONS").as_deref(),
                    1_024,
                )? as usize,
                channel_capacity: number(
                    "GHA_INDIE_WORKER_WS_CHANNEL_CAPACITY",
                    get("GHA_INDIE_WORKER_WS_CHANNEL_CAPACITY").as_deref(),
                    256,
                )? as usize,
                heartbeat: seconds(
                    "GHA_INDIE_WORKER_WS_HEARTBEAT_SECONDS",
                    get("GHA_INDIE_WORKER_WS_HEARTBEAT_SECONDS").as_deref(),
                    20,
                )?,
            },
            rate_limit_hmac_secret: get("GHA_INDIE_WORKER_RATE_LIMIT_HMAC_SECRET")
                .or_else(|| get("ORES_MIDDLEWARE_RATE_LIMIT_HMAC_SECRET")),
        })
    }

    /// True when both product pools are configured. Readiness is fail-closed on
    /// the canonical pool only; the auth pool is advisory.
    #[must_use]
    pub const fn database_configured(&self) -> bool {
        self.database.canonical_url.is_some()
    }

    #[must_use]
    pub const fn shared_auth_configured(&self) -> bool {
        self.shared_auth.is_some()
    }

    #[must_use]
    pub const fn jwt_configured(&self) -> bool {
        self.jwt.supabase_jwt_secret.is_some() || self.jwt.jwks_url.is_some()
    }
}

fn require_strong(variable: &'static str, value: &str) -> Result<(), ConfigError> {
    let strength = value
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .count();
    if strength < MIN_SECRET_BYTES {
        return Err(ConfigError::WeakSecret {
            variable,
            minimum: MIN_SECRET_BYTES,
        });
    }
    Ok(())
}

fn number(variable: &'static str, raw: Option<&str>, default: u32) -> Result<u32, ConfigError> {
    match raw {
        None => Ok(default),
        Some(value) => value.parse::<u32>().map_err(|_| ConfigError::InvalidValue {
            variable,
            reason: "expected a non-negative integer",
        }),
    }
}

fn seconds(
    variable: &'static str,
    raw: Option<&str>,
    default: u64,
) -> Result<Duration, ConfigError> {
    match raw {
        None => Ok(Duration::from_secs(default)),
        Some(value) => {
            value
                .parse::<u64>()
                .map(Duration::from_secs)
                .map_err(|_| ConfigError::InvalidValue {
                    variable,
                    reason: "expected a whole number of seconds",
                })
        }
    }
}

fn flag(raw: Option<&str>) -> bool {
    matches!(
        raw.map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

fn csv_set(raw: Option<&str>) -> BTreeSet<String> {
    raw.map(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

const REDACTED: &str = "[redacted]";

impl fmt::Debug for ApiConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApiConfig")
            .field("env", &self.env)
            .field("bind", &self.bind)
            .field("tcp_bind", &self.tcp_bind)
            .field("nats_url", &self.nats_url.as_ref().map(|_| REDACTED))
            .field("nats_intake", &self.nats_intake)
            .field("shutdown_grace", &self.shutdown_grace)
            .field("request_timeout", &self.request_timeout)
            .field("max_body_bytes", &self.max_body_bytes)
            .field("heartbeat_ttl", &self.heartbeat_ttl)
            .field("database", &self.database)
            .field("shared_auth", &self.shared_auth)
            .field("jwt", &self.jwt)
            .field("webhooks", &self.webhooks)
            .field("chat", &self.chat)
            .field("embeddings", &self.embeddings)
            .field("rate_limit_capacity", &self.rate_limit.capacity)
            .field("rate_limit_window", &self.rate_limit.window)
            .field("ws_max_connections", &self.websocket.max_connections)
            .field(
                "rate_limit_hmac_secret",
                &self.rate_limit_hmac_secret.as_ref().map(|_| REDACTED),
            )
            .finish()
    }
}

impl fmt::Debug for SharedAuthConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SharedAuthConfig")
            .field("base", &self.base)
            .field("audience", &self.audience)
            .field("introspect_secret", &REDACTED)
            .finish()
    }
}

impl fmt::Debug for JwtConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JwtConfig")
            .field(
                "supabase_jwt_secret",
                &self.supabase_jwt_secret.as_ref().map(|_| REDACTED),
            )
            .field("supabase_url", &self.supabase_url)
            .field(
                "supabase_anon_key",
                &self.supabase_anon_key.as_ref().map(|_| REDACTED),
            )
            .field("neon_auth_url", &self.neon_auth_url)
            .field("jwks_url", &self.jwks_url)
            .field("issuers", &self.issuers)
            .field("audiences", &self.audiences)
            .field("jwks_ttl", &self.jwks_ttl)
            .finish()
    }
}

impl fmt::Debug for DatabaseConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatabaseConfig")
            .field(
                "canonical_url",
                &self.canonical_url.as_ref().map(|_| REDACTED),
            )
            .field("auth_url", &self.auth_url.as_ref().map(|_| REDACTED))
            .field("max_connections", &self.max_connections)
            .field("connect_timeout", &self.connect_timeout)
            .finish()
    }
}

impl fmt::Debug for WebhookConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebhookConfig")
            .field(
                "github_secret",
                &self.github_secret.as_ref().map(|_| REDACTED),
            )
            .field("delivery_ttl", &self.delivery_ttl)
            .field("max_deliveries", &self.max_deliveries)
            .finish()
    }
}

impl fmt::Debug for ChatConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChatConfig")
            .field("api_base", &self.api_base)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl fmt::Debug for EmbeddingConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EmbeddingConfig")
            .field("base", &self.base)
            .field("api_key", &self.api_key.as_ref().map(|_| REDACTED))
            .field("model", &self.model)
            .field("dims", &self.dims)
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn defaults_are_usable_without_any_environment() {
        let config = ApiConfig::from_lookup(empty).expect("defaults parse");
        assert_eq!(config.bind, "0.0.0.0:8080");
        assert_eq!(config.env, DeployEnv::Dev);
        assert!(!config.database_configured());
        assert!(!config.shared_auth_configured());
        assert_eq!(config.embeddings.dims, 1_536);
    }

    #[test]
    fn a_resolved_flags_map_parses_like_the_environment() {
        let map = std::collections::BTreeMap::from([
            (
                "GHA_INDIE_WORKER_API_BIND".to_owned(),
                "127.0.0.1:9000".to_owned(),
            ),
            (
                "GHA_INDIE_WORKER_NATS_URL".to_owned(),
                "nats://127.0.0.1:4222".to_owned(),
            ),
        ]);
        let config = ApiConfig::from_map(&map).expect("map parses");
        assert_eq!(config.bind, "127.0.0.1:9000");
        assert_eq!(config.nats_url.as_deref(), Some("nats://127.0.0.1:4222"));
    }

    #[test]
    fn half_configured_shared_auth_is_rejected() {
        let error = ApiConfig::from_lookup(|name| {
            (name == "SHARED_AUTH_BASE").then(|| "https://auth.indiebuild.dev".to_owned())
        })
        .expect_err("half configuration must fail");
        assert_eq!(
            error,
            ConfigError::Incomplete {
                a: "SHARED_AUTH_BASE",
                b: "SHARED_AUTH_INTROSPECT_SECRET",
            }
        );
    }

    #[test]
    fn short_introspection_secrets_are_rejected() {
        let error = ApiConfig::from_lookup(|name| match name {
            "SHARED_AUTH_BASE" => Some("https://auth.indiebuild.dev".to_owned()),
            "SHARED_AUTH_INTROSPECT_SECRET" => Some("too-short".to_owned()),
            _ => None,
        })
        .expect_err("weak secret must fail");
        assert!(matches!(error, ConfigError::WeakSecret { .. }));
    }

    #[test]
    fn debug_never_prints_a_secret() {
        let config = ApiConfig::from_lookup(|name| match name {
            "SHARED_AUTH_BASE" => Some("https://auth.indiebuild.dev".to_owned()),
            "SHARED_AUTH_INTROSPECT_SECRET" => Some("0123456789abcdef0123456789abcdef".to_owned()),
            "DATABASE_URL_CANONICAL" => {
                Some("postgres://user:hunter2@db.example/canonical".to_owned())
            }
            "SUPABASE_JWT_SECRET" => Some("super-secret-value".to_owned()),
            _ => None,
        })
        .expect("config parses");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("hunter2"));
        assert!(!rendered.contains("0123456789abcdef"));
        assert!(!rendered.contains("super-secret-value"));
        assert!(rendered.contains(REDACTED));
    }

    #[test]
    fn deploy_environments_round_trip() {
        for (raw, expected) in [
            ("dev", DeployEnv::Dev),
            ("Staging", DeployEnv::Staging),
            ("PRODUCTION", DeployEnv::Prod),
        ] {
            assert_eq!(DeployEnv::parse(raw).expect("parses"), expected);
        }
        assert!(DeployEnv::parse("qa").is_err());
    }

    #[test]
    fn embedding_dimensions_are_bounded() {
        let error = ApiConfig::from_lookup(|name| {
            (name == "GHA_INDIE_WORKER_EMBEDDINGS_DIMS").then(|| "9000".to_owned())
        })
        .expect_err("oversized dimensions must fail");
        assert!(matches!(error, ConfigError::InvalidValue { .. }));
    }
}
