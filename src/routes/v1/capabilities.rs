#![forbid(unsafe_code)]

//! `GET /v1/capabilities` — what this deployment can actually do.
//!
//! Unauthenticated on purpose: a client needs this *before* it can choose an
//! auth flow. It publishes the scope vocabulary, the executable profiles, the
//! carriers, the limits and — importantly — the explicit exclusions, so a
//! caller never has to discover an unsupported construct by having a run fail.

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::auth::guards::ALL_SCOPES;
use crate::domain::embeddings::{MAX_BATCH, MAX_DIMENSIONS, VECTOR_SLOTS};
use crate::domain::plans::{Profile, MAX_JOBS, MAX_WORKFLOW_BYTES};
use crate::domain::runs::{MAX_LOG_CHUNKS_PER_JOB, MAX_LOG_CHUNK_BYTES};
use crate::state::AppState;
use crate::transport::ws::PROTOCOL_VERSION;

#[derive(Debug, Serialize)]
pub struct Capabilities {
    pub service: &'static str,
    pub version: &'static str,
    pub protocol: &'static str,
    pub environment: &'static str,
    pub contracts: Contracts,
    pub auth: Auth,
    pub carriers: Carriers,
    pub profiles: Vec<&'static str>,
    pub exclusions: Vec<&'static str>,
    pub limits: Limits,
    pub embeddings: Embeddings,
    pub scopes: Vec<&'static str>,
}

/// The contract identifiers this build was compiled against.
///
/// They come from `gha-indie-worker-interfaces` when the default-on
/// `interfaces` feature is enabled, so a client can tell which generated
/// contract revision a deployment actually speaks rather than assuming.
#[derive(Debug, Serialize)]
pub struct Contracts {
    pub schema_id: &'static str,
    pub schema_revision: &'static str,
    pub protocol_version: &'static str,
}

#[cfg(feature = "interfaces")]
#[must_use]
pub const fn contracts() -> Contracts {
    Contracts {
        schema_id: gha_indie_worker_interfaces::SCHEMA_ID,
        schema_revision: gha_indie_worker_interfaces::SCHEMA_REVISION,
        protocol_version: gha_indie_worker_interfaces::PROTOCOL_VERSION,
    }
}

/// Without the `interfaces` dependency this build cannot name a contract
/// revision, and says so rather than reporting a version it did not compile
/// against.
#[cfg(not(feature = "interfaces"))]
#[must_use]
pub const fn contracts() -> Contracts {
    Contracts {
        schema_id: "unpinned",
        schema_revision: "unpinned",
        protocol_version: "unpinned",
    }
}

#[derive(Debug, Serialize)]
pub struct Auth {
    pub shared_auth: bool,
    pub jwt: bool,
    pub sources: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct Carriers {
    pub http: bool,
    pub websocket: bool,
    pub tcp: bool,
    pub nats: bool,
    pub websocket_protocol: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Limits {
    pub max_workflow_bytes: usize,
    pub max_jobs: usize,
    pub max_log_chunk_bytes: usize,
    pub max_log_chunks_per_job: usize,
    pub max_request_body_bytes: usize,
    pub rate_limit_capacity: u32,
    pub rate_limit_window_seconds: u64,
}

#[derive(Debug, Serialize)]
pub struct Embeddings {
    pub model: String,
    pub dimensions: usize,
    pub max_dimensions: usize,
    pub vector_slots: usize,
    pub max_batch: usize,
}

/// The independent lane's explicit refusals, mirroring `gha-clone-server`.
/// Published so a caller can see what will never be approximated.
pub const EXCLUSIONS: [&str; 8] = [
    "branch or tag execution instead of a 40-hex commit SHA",
    "secret or OIDC expressions in env, with, or commands",
    "dynamic matrices and conditional jobs or steps",
    "arbitrary marketplace actions",
    "job or service containers",
    "macOS/iOS and Windows native execution",
    "environments, deployments, and reusable workflows",
    "caller-selected commands, shells, and runner images",
];

pub async fn capabilities(State(state): State<AppState>) -> Json<Capabilities> {
    Json(Capabilities {
        service: crate::routes::health::SERVICE,
        version: env!("CARGO_PKG_VERSION"),
        protocol: crate::routes::health::PROTOCOL,
        environment: state.config.env.as_str(),
        contracts: contracts(),
        auth: Auth {
            shared_auth: state.shared_auth_configured(),
            jwt: state.jwt.is_configured(),
            sources: vec!["shared-auth", "supabase-jwt", "neon-jwt"],
        },
        carriers: Carriers {
            http: true,
            websocket: true,
            tcp: state.config.tcp_bind.is_some(),
            nats: state.nats_configured(),
            websocket_protocol: PROTOCOL_VERSION,
        },
        profiles: Profile::all().iter().map(|p| p.as_str()).collect(),
        exclusions: EXCLUSIONS.to_vec(),
        limits: Limits {
            max_workflow_bytes: MAX_WORKFLOW_BYTES,
            max_jobs: MAX_JOBS,
            max_log_chunk_bytes: MAX_LOG_CHUNK_BYTES,
            max_log_chunks_per_job: MAX_LOG_CHUNKS_PER_JOB,
            max_request_body_bytes: state.config.max_body_bytes,
            rate_limit_capacity: state.config.rate_limit.capacity,
            rate_limit_window_seconds: state.config.rate_limit.window.as_secs(),
        },
        embeddings: Embeddings {
            model: state.embeddings.model().to_owned(),
            dimensions: state.embeddings.dimensions(),
            max_dimensions: MAX_DIMENSIONS,
            vector_slots: VECTOR_SLOTS,
            max_batch: MAX_BATCH,
        },
        scopes: ALL_SCOPES.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_is_published_by_its_wire_name() {
        let published: Vec<&str> = Profile::all().iter().map(|p| p.as_str()).collect();
        assert!(published.contains(&"rust-verify"));
        assert!(published.contains(&"flutter-android-debug"));
        assert_eq!(published.len(), Profile::all().len());
    }

    #[test]
    fn the_exclusions_name_the_refusals_that_matter() {
        let joined = EXCLUSIONS.join(" | ");
        for required in ["commit SHA", "secret", "matrices", "marketplace", "macOS"] {
            assert!(joined.contains(required), "{required}");
        }
    }

    #[test]
    fn the_contract_revision_is_published_not_assumed() {
        let contracts = contracts();
        assert!(!contracts.schema_id.is_empty());
        assert!(!contracts.schema_revision.is_empty());
        assert!(!contracts.protocol_version.is_empty());
        // With the dependency compiled in, the values are the real ones.
        #[cfg(feature = "interfaces")]
        assert_ne!(contracts.schema_revision, "unpinned");
    }

    #[test]
    fn the_published_scope_list_matches_the_guard_vocabulary() {
        assert_eq!(ALL_SCOPES.len(), 11);
        assert!(ALL_SCOPES.contains(&"runs:write"));
    }
}
