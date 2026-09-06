#![forbid(unsafe_code)]

//! `/v1/embeddings/*` and `/v1/regressions/correlate`.
//!
//! Indexing and search are tenant-scoped by the token's organisation, never by
//! a body field, so one tenant's vectors are unreachable from another's token.
//!
//! Vectors are stored zero-padded into the fleet's fixed `halfvec(4100)` slot
//! count with their true dimension alongside — see
//! [`crate::domain::embeddings`] and `docs/orm-core-entities.md`.

use std::time::Duration;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::guards::{require_scopes, SCOPE_EMBEDDINGS_READ, SCOPE_EMBEDDINGS_WRITE};
use crate::auth::Actor;
use crate::domain::embeddings::{
    correlate, pad_to_slots, rank, validate_batch, CorrelateRequest, EmbedFuture, EmbeddingError,
    EmbeddingProvider, IndexRequest, RegressionFinding, SearchHit, SearchRequest, StoredEmbedding,
};
use crate::domain::now_rfc3339;
use crate::error::ApiError;
use crate::state::AppState;

#[must_use]
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/embeddings/index", post(index))
        .route("/embeddings/search", post(search))
        .route("/regressions/correlate", post(correlate_route))
}

#[derive(Debug, Serialize)]
pub struct Indexed {
    pub indexed: usize,
    pub model: String,
    pub dimensions: usize,
    pub vector_slots: usize,
}

fn tenant_of(actor: &Actor) -> Result<Uuid, ApiError> {
    actor.org_id.ok_or(ApiError::Forbidden)
}

/// `POST /v1/embeddings/index`
async fn index(
    State(state): State<AppState>,
    actor: Actor,
    Json(request): Json<IndexRequest>,
) -> Result<Json<Indexed>, ApiError> {
    require_scopes(&actor, &[SCOPE_EMBEDDINGS_WRITE])?;
    let tenant_id = tenant_of(&actor)?;
    validate_batch(&request.items).map_err(map_embedding_error)?;

    let inputs: Vec<String> = request.items.iter().map(|item| item.text.clone()).collect();
    let vectors = state
        .embeddings
        .embed(&inputs)
        .await
        .map_err(map_embedding_error)?;
    if vectors.len() != inputs.len() {
        return Err(map_embedding_error(EmbeddingError::CountMismatch {
            expected: inputs.len(),
            got: vectors.len(),
        }));
    }

    let dims = state.embeddings.dimensions();
    let model = state.embeddings.model().to_owned();
    let created_at = now_rfc3339();
    let mut rows = Vec::with_capacity(vectors.len());
    for (item, vector) in request.items.iter().zip(vectors) {
        rows.push(StoredEmbedding {
            id: Uuid::now_v7(),
            tenant_id,
            entity_kind: item.entity_kind,
            entity_id: item.entity_id.clone(),
            model: model.clone(),
            dims,
            vector: pad_to_slots(vector, dims).map_err(map_embedding_error)?,
            created_at: created_at.clone(),
        });
    }

    let indexed = state.store.upsert_embeddings(rows).await;
    Ok(Json(Indexed {
        indexed,
        model,
        dimensions: dims,
        vector_slots: crate::domain::embeddings::VECTOR_SLOTS,
    }))
}

#[derive(Debug, Serialize)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
    pub model: String,
    pub dimensions: usize,
}

/// `POST /v1/embeddings/search`
async fn search(
    State(state): State<AppState>,
    actor: Actor,
    Json(request): Json<SearchRequest>,
) -> Result<Json<SearchResults>, ApiError> {
    require_scopes(&actor, &[SCOPE_EMBEDDINGS_READ])?;
    let tenant_id = tenant_of(&actor)?;
    let hits = query(
        &state,
        tenant_id,
        &request.query,
        request.entity_kind,
        request.limit,
    )
    .await?;
    Ok(Json(SearchResults {
        hits,
        model: state.embeddings.model().to_owned(),
        dimensions: state.embeddings.dimensions(),
    }))
}

#[derive(Debug, Serialize)]
pub struct Correlated {
    pub findings: Vec<RegressionFinding>,
    pub model: String,
}

/// `POST /v1/regressions/correlate`
///
/// The same vector search, projected onto confidence bands so a caller does not
/// have to interpret a raw cosine score.
async fn correlate_route(
    State(state): State<AppState>,
    actor: Actor,
    Json(request): Json<CorrelateRequest>,
) -> Result<Json<Correlated>, ApiError> {
    require_scopes(&actor, &[SCOPE_EMBEDDINGS_READ])?;
    let tenant_id = tenant_of(&actor)?;
    let hits = query(&state, tenant_id, &request.symptom, None, request.limit).await?;
    Ok(Json(Correlated {
        findings: correlate(hits),
        model: state.embeddings.model().to_owned(),
    }))
}

/// The shared search path: embed the query, then rank stored vectors.
async fn query(
    state: &AppState,
    tenant_id: Uuid,
    text: &str,
    kind: Option<crate::domain::embeddings::EntityKind>,
    limit: usize,
) -> Result<Vec<SearchHit>, ApiError> {
    if text.trim().is_empty() {
        return Err(ApiError::bad_request("query text must not be empty"));
    }
    let dims = state.embeddings.dimensions();
    let inputs = vec![text.to_owned()];
    let mut vectors = state
        .embeddings
        .embed(&inputs)
        .await
        .map_err(map_embedding_error)?;
    let vector = if vectors.is_empty() {
        return Err(map_embedding_error(EmbeddingError::CountMismatch {
            expected: 1,
            got: 0,
        }));
    } else {
        vectors.remove(0)
    };
    let padded = pad_to_slots(vector, dims).map_err(map_embedding_error)?;
    let candidates = state.store.embeddings_of(tenant_id, kind).await;
    Ok(rank(&padded, dims, &candidates, limit))
}

fn map_embedding_error(error: EmbeddingError) -> ApiError {
    match error {
        EmbeddingError::NotConfigured | EmbeddingError::Unavailable => {
            ApiError::Unavailable("embedding provider")
        }
        EmbeddingError::InvalidBatch
        | EmbeddingError::InputTooLarge
        | EmbeddingError::DimensionsTooLarge { .. } => ApiError::bad_request(error.to_string()),
        EmbeddingError::DimensionMismatch { .. }
        | EmbeddingError::CountMismatch { .. }
        | EmbeddingError::MalformedResponse => {
            tracing::error!(%error, "embedding provider returned an inconsistent response");
            ApiError::Unavailable("embedding provider")
        }
    }
}

// ---- the OpenAI-compatible provider ---------------------------------------

/// An OpenAI-compatible `/v1/embeddings` client.
///
/// "Compatible" is the whole point: the same code reaches OpenAI, a local
/// vLLM/Ollama gateway, or any other server speaking that request shape. The
/// dimension is asserted, not trusted — a provider that quietly changes model
/// width would otherwise corrupt the index.
#[derive(Clone)]
pub struct HttpProvider {
    http: reqwest::Client,
    base: String,
    api_key: Option<String>,
    model: String,
    dims: usize,
}

impl std::fmt::Debug for HttpProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpProvider")
            .field("model", &self.model)
            .field("dims", &self.dims)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .finish_non_exhaustive()
    }
}

impl HttpProvider {
    #[must_use]
    pub fn new(
        http: reqwest::Client,
        base: String,
        api_key: Option<String>,
        model: String,
        dims: usize,
    ) -> Self {
        Self {
            http,
            base,
            api_key,
            model,
            dims,
        }
    }

    fn url(&self) -> String {
        format!("{}/v1/embeddings", self.base.trim_end_matches('/'))
    }
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedDatum>,
}

#[derive(Deserialize)]
struct EmbedDatum {
    embedding: Vec<f32>,
    #[serde(default)]
    index: usize,
}

impl EmbeddingProvider for HttpProvider {
    fn model(&self) -> &str {
        &self.model
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn embed<'a>(&'a self, inputs: &'a [String]) -> EmbedFuture<'a> {
        Box::pin(async move {
            if inputs.is_empty() {
                return Err(EmbeddingError::InvalidBatch);
            }
            let mut request = self
                .http
                .post(self.url())
                .timeout(Duration::from_secs(60))
                .json(&EmbedRequest {
                    model: &self.model,
                    input: inputs,
                });
            if let Some(key) = self.api_key.as_deref() {
                request = request.bearer_auth(key);
            }

            let response = request.send().await.map_err(|error| {
                tracing::warn!(%error, "embedding provider request failed");
                EmbeddingError::Unavailable
            })?;
            if !response.status().is_success() {
                tracing::warn!(
                    status = response.status().as_u16(),
                    "embedding provider refused"
                );
                return Err(EmbeddingError::Unavailable);
            }
            let body: EmbedResponse = response
                .json()
                .await
                .map_err(|_| EmbeddingError::MalformedResponse)?;
            order_vectors(body.data, inputs.len(), self.dims)
        })
    }
}

/// Restore the request order and assert the model's width.
///
/// Providers may return data out of order, and the `index` field is how they
/// say so. Trusting arrival order would silently attach one input's vector to
/// another's entity.
fn order_vectors(
    data: Vec<EmbedDatum>,
    expected: usize,
    dims: usize,
) -> Result<Vec<Vec<f32>>, EmbeddingError> {
    if data.len() != expected {
        return Err(EmbeddingError::CountMismatch {
            expected,
            got: data.len(),
        });
    }
    let mut slots: Vec<Option<Vec<f32>>> = vec![None; expected];
    for datum in data {
        if datum.index >= expected {
            return Err(EmbeddingError::MalformedResponse);
        }
        if datum.embedding.len() != dims {
            return Err(EmbeddingError::DimensionMismatch {
                expected: dims,
                got: datum.embedding.len(),
            });
        }
        slots[datum.index] = Some(datum.embedding);
    }
    slots
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or(EmbeddingError::MalformedResponse)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datum(index: usize, value: f32) -> EmbedDatum {
        EmbedDatum {
            embedding: vec![value, value],
            index,
        }
    }

    #[test]
    fn vectors_are_restored_to_the_request_order() {
        let ordered = order_vectors(vec![datum(1, 2.0), datum(0, 1.0)], 2, 2).expect("orders");
        assert_eq!(ordered[0], vec![1.0, 1.0]);
        assert_eq!(ordered[1], vec![2.0, 2.0]);
    }

    #[test]
    fn a_short_or_long_response_is_a_count_mismatch() {
        assert_eq!(
            order_vectors(vec![datum(0, 1.0)], 2, 2),
            Err(EmbeddingError::CountMismatch {
                expected: 2,
                got: 1
            })
        );
    }

    #[test]
    fn a_provider_that_changes_model_width_is_refused() {
        let wide = EmbedDatum {
            embedding: vec![1.0, 2.0, 3.0],
            index: 0,
        };
        assert_eq!(
            order_vectors(vec![wide], 1, 2),
            Err(EmbeddingError::DimensionMismatch {
                expected: 2,
                got: 3
            })
        );
    }

    #[test]
    fn an_out_of_range_or_duplicated_index_is_malformed() {
        assert_eq!(
            order_vectors(vec![datum(5, 1.0), datum(0, 1.0)], 2, 2),
            Err(EmbeddingError::MalformedResponse)
        );
        assert_eq!(
            order_vectors(vec![datum(0, 1.0), datum(0, 2.0)], 2, 2),
            Err(EmbeddingError::MalformedResponse)
        );
    }

    #[test]
    fn provider_failures_never_surface_as_a_client_error() {
        assert_eq!(
            map_embedding_error(EmbeddingError::Unavailable).status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            map_embedding_error(EmbeddingError::MalformedResponse).status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            map_embedding_error(EmbeddingError::InvalidBatch).status(),
            axum::http::StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn the_provider_url_is_openai_compatible() {
        let provider = HttpProvider::new(
            reqwest::Client::new(),
            "https://api.example/".to_owned(),
            None,
            "text-embedding-3-small".to_owned(),
            1_536,
        );
        assert_eq!(provider.url(), "https://api.example/v1/embeddings");
        assert_eq!(provider.model(), "text-embedding-3-small");
        assert_eq!(provider.dimensions(), 1_536);
    }

    #[test]
    fn debug_never_prints_the_api_key() {
        let provider = HttpProvider::new(
            reqwest::Client::new(),
            "https://api.example".to_owned(),
            Some("sk-super-secret".to_owned()),
            "m".to_owned(),
            2,
        );
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains("sk-super-secret"));
        assert!(rendered.contains("[redacted]"));
    }
}
