#![forbid(unsafe_code)]

//! Supabase / Neon JWT verification with a TTL-cached JWKS.
//!
//! Two shapes are supported, both mapped onto [`VerifiedActor`]:
//!
//! * **HS256 with the project secret** — legacy Supabase projects
//!   (`SUPABASE_JWT_SECRET`).
//! * **Asymmetric with a JWKS** — Neon Auth and Supabase projects on
//!   asymmetric keys (`NEON_AUTH_JWKS_URL`).
//!
//! The accepted algorithm set is an allow-list, so a token cannot select an
//! algorithm we did not intend to honour, and the header's `alg` is never used
//! to pick a *key type* — the key comes from the configured material.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use tokio::sync::RwLock;
use uuid::Uuid;

use super::{scope_set, AuthError, AuthSource, VerifiedActor};
use crate::config::JwtConfig;

/// Asymmetric algorithms we will verify. `HS*` is deliberately absent here: a
/// JWKS-sourced key is never used with a symmetric algorithm.
const ASYMMETRIC: [Algorithm; 5] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::ES256,
    Algorithm::ES384,
];

/// Claims we read. Everything else in the token is ignored on purpose.
#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    email: Option<String>,
    /// Supabase's single-role claim (`authenticated`, `service_role`, …).
    #[serde(default)]
    role: Option<String>,
    /// Neon Auth / custom multi-role claim.
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    org_id: Option<String>,
    #[serde(default)]
    app_metadata: Option<AppMetadata>,
}

#[derive(Debug, Default, Deserialize)]
struct AppMetadata {
    #[serde(default)]
    org_id: Option<String>,
    #[serde(default)]
    roles: Vec<String>,
}

/// Project the verified claims onto the fleet's one actor type.
///
/// Pure, so the mapping is unit-tested without a network or a signing key.
#[must_use]
fn to_actor(claims: Claims, source: AuthSource) -> VerifiedActor {
    let mut roles: BTreeSet<String> = claims.roles.into_iter().collect();
    if let Some(role) = claims.role {
        if !role.is_empty() {
            roles.insert(role);
        }
    }
    let mut org_id = claims.org_id;
    if let Some(metadata) = claims.app_metadata {
        roles.extend(metadata.roles);
        org_id = org_id.or(metadata.org_id);
    }
    VerifiedActor {
        subject: claims.sub,
        org_id: org_id.and_then(|value| Uuid::parse_str(value.trim()).ok()),
        roles,
        scopes: scope_set(claims.scope.as_deref()),
        source,
        email: claims.email,
    }
}

/// A JWKS plus the instant it was fetched.
#[derive(Clone)]
struct CachedJwks {
    keys: Arc<JwkSet>,
    fetched_at: Instant,
}

/// Verifies Supabase/Neon JWTs. Cloneable and cheap to share: the cache lives
/// behind an `Arc<RwLock<_>>`.
#[derive(Clone)]
pub struct JwtVerifier {
    config: Arc<JwtConfig>,
    http: reqwest::Client,
    cache: Arc<RwLock<Option<CachedJwks>>>,
}

impl std::fmt::Debug for JwtVerifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JwtVerifier")
            .field("jwks_url", &self.config.jwks_url)
            .field(
                "hs256_configured",
                &self.config.supabase_jwt_secret.is_some(),
            )
            .finish()
    }
}

impl JwtVerifier {
    #[must_use]
    pub fn new(config: JwtConfig, http: reqwest::Client) -> Self {
        Self {
            config: Arc::new(config),
            http,
            cache: Arc::new(RwLock::new(None)),
        }
    }

    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.config.supabase_jwt_secret.is_some() || self.config.jwks_url.is_some()
    }

    /// Verify one bearer token.
    ///
    /// # Errors
    /// Returns [`AuthError::NotConfigured`] when neither key source is set,
    /// [`AuthError::Rejected`] when the signature, algorithm, issuer, audience
    /// or expiry check fails, and [`AuthError::AuthorityUnavailable`] when the
    /// JWKS endpoint cannot be reached and nothing is cached.
    pub async fn verify(&self, token: &str) -> Result<VerifiedActor, AuthError> {
        if !self.is_configured() {
            return Err(AuthError::NotConfigured);
        }
        let header = decode_header(token).map_err(|_| AuthError::Rejected)?;

        if header.alg == Algorithm::HS256 {
            let secret = self
                .config
                .supabase_jwt_secret
                .as_ref()
                .ok_or(AuthError::Rejected)?;
            let key = DecodingKey::from_secret(secret.as_bytes());
            let claims = self.decode_with(token, &key, Algorithm::HS256)?;
            return Ok(to_actor(claims, AuthSource::SupabaseJwt));
        }

        if !ASYMMETRIC.contains(&header.alg) {
            return Err(AuthError::Rejected);
        }
        let kid = header.kid.ok_or(AuthError::Rejected)?;
        let keys = self.jwks().await?;
        let jwk = keys.find(&kid).ok_or(AuthError::Rejected)?;
        let key = DecodingKey::from_jwk(jwk).map_err(|_| AuthError::Rejected)?;
        let claims = self.decode_with(token, &key, header.alg)?;
        Ok(to_actor(claims, self.asymmetric_source()))
    }

    /// A JWKS served by the Neon Auth endpoint is Neon's; anything else
    /// configured through the same variable is reported as Supabase.
    fn asymmetric_source(&self) -> AuthSource {
        if self.config.neon_auth_url.is_some() || self.config.jwks_url.is_some() {
            AuthSource::NeonJwt
        } else {
            AuthSource::SupabaseJwt
        }
    }

    fn decode_with(
        &self,
        token: &str,
        key: &DecodingKey,
        algorithm: Algorithm,
    ) -> Result<Claims, AuthError> {
        let mut validation = Validation::new(algorithm);
        validation.validate_exp = true;
        // Pin the audience only when one is configured. Leaving `validate_aud`
        // on with no configured value is ambiguous across jsonwebtoken
        // releases, so the decision is made explicitly here.
        if self.config.audiences.is_empty() {
            validation.validate_aud = false;
        } else {
            let audiences: Vec<&str> = self.config.audiences.iter().map(String::as_str).collect();
            validation.set_audience(&audiences);
        }
        if !self.config.issuers.is_empty() {
            let issuers: Vec<&str> = self.config.issuers.iter().map(String::as_str).collect();
            validation.set_issuer(&issuers);
        }
        decode::<Claims>(token, key, &validation)
            .map(|data| data.claims)
            .map_err(|_| AuthError::Rejected)
    }

    /// Fetch the JWKS, honouring the TTL cache. A fetch failure falls back to a
    /// stale cache — a key rotation outage must not lock every caller out —
    /// but an empty cache with a failing fetch is reported as unavailable.
    async fn jwks(&self) -> Result<Arc<JwkSet>, AuthError> {
        let url = self
            .config
            .jwks_url
            .as_deref()
            .ok_or(AuthError::NotConfigured)?;

        if let Some(cached) = self.cache.read().await.as_ref() {
            if cached.fetched_at.elapsed() < self.config.jwks_ttl {
                return Ok(cached.keys.clone());
            }
        }

        match self.fetch_jwks(url).await {
            Ok(keys) => {
                let keys = Arc::new(keys);
                *self.cache.write().await = Some(CachedJwks {
                    keys: keys.clone(),
                    fetched_at: Instant::now(),
                });
                Ok(keys)
            }
            Err(error) => {
                tracing::warn!(error = %error, "jwks fetch failed; falling back to cache");
                match self.cache.read().await.as_ref() {
                    Some(cached) => Ok(cached.keys.clone()),
                    None => Err(AuthError::AuthorityUnavailable),
                }
            }
        }
    }

    async fn fetch_jwks(&self, url: &str) -> Result<JwkSet, reqwest::Error> {
        self.http
            .get(url)
            .timeout(Duration::from_secs(5))
            .send()
            .await?
            .error_for_status()?
            .json::<JwkSet>()
            .await
    }
}

#[cfg(test)]
mod tests {
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;

    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn verifier(config: JwtConfig) -> JwtVerifier {
        JwtVerifier::new(config, reqwest::Client::new())
    }

    fn hs256_config() -> JwtConfig {
        JwtConfig {
            supabase_jwt_secret: Some(SECRET.to_owned()),
            jwks_ttl: Duration::from_secs(300),
            ..JwtConfig::default()
        }
    }

    fn sign(claims: &serde_json::Value) -> String {
        encode(
            &Header::new(Algorithm::HS256),
            claims,
            &EncodingKey::from_secret(SECRET.as_bytes()),
        )
        .expect("signs")
    }

    fn far_future() -> u64 {
        // 2286-11-20; comfortably beyond any CI clock.
        10_000_000_000
    }

    #[test]
    fn claim_mapping_merges_every_role_source() {
        let claims = Claims {
            sub: "sub-1".to_owned(),
            email: Some("a@example.com".to_owned()),
            role: Some("authenticated".to_owned()),
            roles: vec!["member".to_owned()],
            scope: Some("runs:read runs:write".to_owned()),
            org_id: None,
            app_metadata: Some(AppMetadata {
                org_id: Some("00000000-0000-0000-0000-000000000001".to_owned()),
                roles: vec!["admin".to_owned()],
            }),
        };
        let actor = to_actor(claims, AuthSource::SupabaseJwt);
        assert_eq!(actor.subject, "sub-1");
        assert_eq!(
            actor.roles,
            BTreeSet::from([
                "admin".to_owned(),
                "authenticated".to_owned(),
                "member".to_owned()
            ])
        );
        assert_eq!(
            actor.scopes,
            BTreeSet::from(["runs:read".to_owned(), "runs:write".to_owned()])
        );
        assert_eq!(actor.org_id, Some(Uuid::from_u128(1)));
        assert_eq!(actor.email.as_deref(), Some("a@example.com"));
        assert_eq!(actor.source, AuthSource::SupabaseJwt);
    }

    #[test]
    fn a_top_level_org_claim_wins_over_app_metadata() {
        let claims = Claims {
            sub: "sub-1".to_owned(),
            email: None,
            role: None,
            roles: Vec::new(),
            scope: None,
            org_id: Some("00000000-0000-0000-0000-000000000002".to_owned()),
            app_metadata: Some(AppMetadata {
                org_id: Some("00000000-0000-0000-0000-000000000003".to_owned()),
                roles: Vec::new(),
            }),
        };
        assert_eq!(
            to_actor(claims, AuthSource::NeonJwt).org_id,
            Some(Uuid::from_u128(2))
        );
    }

    #[test]
    fn a_non_uuid_org_claim_is_dropped_rather_than_guessed() {
        let claims = Claims {
            sub: "sub-1".to_owned(),
            email: None,
            role: None,
            roles: Vec::new(),
            scope: None,
            org_id: Some("acme".to_owned()),
            app_metadata: None,
        };
        assert_eq!(to_actor(claims, AuthSource::NeonJwt).org_id, None);
    }

    #[tokio::test]
    async fn an_unconfigured_verifier_accepts_nothing() {
        let verifier = verifier(JwtConfig::default());
        assert!(!verifier.is_configured());
        assert_eq!(
            verifier.verify("anything").await,
            Err(AuthError::NotConfigured)
        );
    }

    #[tokio::test]
    async fn a_valid_hs256_token_produces_an_actor() {
        let verifier = verifier(hs256_config());
        let token = sign(&json!({
            "sub": "sub-1",
            "exp": far_future(),
            "role": "authenticated",
            "scope": "runs:read",
        }));
        let actor = verifier.verify(&token).await.expect("verifies");
        assert_eq!(actor.subject, "sub-1");
        assert_eq!(actor.source, AuthSource::SupabaseJwt);
        assert!(actor.has_role("authenticated"));
        assert!(actor.has_scope("runs:read"));
    }

    #[tokio::test]
    async fn a_token_signed_with_another_key_is_rejected() {
        let verifier = verifier(hs256_config());
        let token = encode(
            &Header::new(Algorithm::HS256),
            &json!({ "sub": "sub-1", "exp": far_future() }),
            &EncodingKey::from_secret(b"a-completely-different-secret-key"),
        )
        .expect("signs");
        assert_eq!(verifier.verify(&token).await, Err(AuthError::Rejected));
    }

    #[tokio::test]
    async fn an_expired_token_is_rejected() {
        let verifier = verifier(hs256_config());
        let token = sign(&json!({ "sub": "sub-1", "exp": 1_000 }));
        assert_eq!(verifier.verify(&token).await, Err(AuthError::Rejected));
    }

    #[tokio::test]
    async fn a_pinned_issuer_is_enforced() {
        let mut config = hs256_config();
        config.issuers = BTreeSet::from(["https://auth.indiebuild.dev".to_owned()]);
        let verifier = verifier(config);

        let good = sign(&json!({
            "sub": "sub-1",
            "exp": far_future(),
            "iss": "https://auth.indiebuild.dev",
        }));
        assert!(verifier.verify(&good).await.is_ok());

        let bad = sign(&json!({
            "sub": "sub-1",
            "exp": far_future(),
            "iss": "https://evil.example",
        }));
        assert_eq!(verifier.verify(&bad).await, Err(AuthError::Rejected));
    }

    #[tokio::test]
    async fn a_pinned_audience_is_enforced() {
        let mut config = hs256_config();
        config.audiences = BTreeSet::from(["gha-indie-worker-api".to_owned()]);
        let verifier = verifier(config);

        let good = sign(&json!({
            "sub": "sub-1",
            "exp": far_future(),
            "aud": "gha-indie-worker-api",
        }));
        assert!(verifier.verify(&good).await.is_ok());

        let bad = sign(&json!({
            "sub": "sub-1",
            "exp": far_future(),
            "aud": "some-other-service",
        }));
        assert_eq!(verifier.verify(&bad).await, Err(AuthError::Rejected));
    }

    #[tokio::test]
    async fn an_asymmetric_token_without_a_configured_jwks_is_refused_offline() {
        // HS256 is configured, so `is_configured` passes; the RS256 header then
        // has no key source and must be refused without any network call.
        let verifier = verifier(hs256_config());
        let token = format!(
            "{}.{}.{}",
            "eyJhbGciOiJSUzI1NiIsImtpZCI6ImsxIn0", "eyJzdWIiOiJhIn0", "c2ln"
        );
        assert_eq!(verifier.verify(&token).await, Err(AuthError::NotConfigured));
    }

    #[test]
    fn the_asymmetric_allow_list_excludes_symmetric_algorithms() {
        assert!(!ASYMMETRIC.contains(&Algorithm::HS256));
        assert!(!ASYMMETRIC.contains(&Algorithm::HS384));
        assert!(!ASYMMETRIC.contains(&Algorithm::HS512));
        assert!(ASYMMETRIC.contains(&Algorithm::RS256));
    }
}
