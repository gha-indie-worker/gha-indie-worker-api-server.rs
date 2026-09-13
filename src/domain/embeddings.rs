#![forbid(unsafe_code)]

//! Embedding indexing, search, and regression correlation.
//!
//! Storage follows the fleet's *model-space v3* convention: one physical column
//! wide enough for every model we might use (`halfvec(4100)`), with each vector
//! zero-padded from its real dimension into the fixed slot count. `dims` is
//! stored alongside so a query can never compare across model spaces by
//! accident. See `docs/orm-core-entities.md` for the `embeddings` table.
//!
//! The provider is a trait so the HTTP client is one swappable edge. It uses a
//! hand-written boxed future rather than `async_trait` to keep the dependency
//! set small and the object safety explicit.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Physical slot count of the `halfvec` column. Fleet-wide constant.
pub const VECTOR_SLOTS: usize = 4_100;
/// Largest logical dimension a model may report.
pub const MAX_DIMENSIONS: usize = 4_096;
/// Largest batch a single index call may carry.
pub const MAX_BATCH: usize = 64;
/// Largest single input, in bytes.
pub const MAX_INPUT_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum EmbeddingError {
    #[error("embedding provider is not configured")]
    NotConfigured,
    #[error("input batch must contain between 1 and 64 items")]
    InvalidBatch,
    #[error("an input exceeds 32768 bytes")]
    InputTooLarge,
    #[error("model reported {got} dimensions; the maximum is 4096")]
    DimensionsTooLarge { got: usize },
    #[error("model reported {got} dimensions; {expected} were configured")]
    DimensionMismatch { expected: usize, got: usize },
    #[error("provider returned {got} vectors for {expected} inputs")]
    CountMismatch { expected: usize, got: usize },
    #[error("embedding provider is unavailable")]
    Unavailable,
    #[error("embedding provider returned an unusable response")]
    MalformedResponse,
}

/// What an embedding row points at. Kept as a closed enum so a caller cannot
/// invent an entity kind that no query understands.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Run,
    Job,
    LogChunk,
    Workflow,
    RegressionFinding,
    Document,
}

impl EntityKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Job => "job",
            Self::LogChunk => "log_chunk",
            Self::Workflow => "workflow",
            Self::RegressionFinding => "regression_finding",
            Self::Document => "document",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct IndexItem {
    pub entity_kind: EntityKind,
    pub entity_id: String,
    pub text: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct IndexRequest {
    pub items: Vec<IndexItem>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub entity_kind: Option<EntityKind>,
}

const fn default_limit() -> usize {
    10
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SearchHit {
    pub entity_kind: EntityKind,
    pub entity_id: String,
    pub score: f32,
}

/// A stored embedding, already padded into the physical slot count.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredEmbedding {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub entity_kind: EntityKind,
    pub entity_id: String,
    pub model: String,
    pub dims: usize,
    /// Exactly [`VECTOR_SLOTS`] values; positions `dims..` are zero.
    pub vector: Vec<f32>,
    pub created_at: String,
}

/// Zero-pad a model vector into the fixed physical slot count.
///
/// # Errors
/// Returns [`EmbeddingError`] when the vector is empty, wider than
/// [`MAX_DIMENSIONS`], or does not match the configured dimension.
pub fn pad_to_slots(vector: Vec<f32>, expected_dims: usize) -> Result<Vec<f32>, EmbeddingError> {
    let got = vector.len();
    if got == 0 || got > MAX_DIMENSIONS {
        return Err(EmbeddingError::DimensionsTooLarge { got });
    }
    if got != expected_dims {
        return Err(EmbeddingError::DimensionMismatch {
            expected: expected_dims,
            got,
        });
    }
    let mut padded = vector;
    padded.resize(VECTOR_SLOTS, 0.0);
    Ok(padded)
}

/// Validate an index batch before any provider call is made.
///
/// # Errors
/// Returns [`EmbeddingError`] when the batch is empty, oversized, or contains
/// an input above [`MAX_INPUT_BYTES`].
pub fn validate_batch(items: &[IndexItem]) -> Result<(), EmbeddingError> {
    if items.is_empty() || items.len() > MAX_BATCH {
        return Err(EmbeddingError::InvalidBatch);
    }
    if items.iter().any(|item| item.text.len() > MAX_INPUT_BYTES) {
        return Err(EmbeddingError::InputTooLarge);
    }
    if items
        .iter()
        .any(|item| item.entity_id.is_empty() || item.entity_id.len() > 256)
    {
        return Err(EmbeddingError::InvalidBatch);
    }
    Ok(())
}

/// Cosine similarity over the *logical* prefix of two padded vectors.
///
/// Comparing padded tails would silently succeed across model spaces, so the
/// caller passes the logical dimension and only that prefix is scored. A
/// zero-magnitude vector scores `0.0` rather than producing `NaN`.
#[must_use]
pub fn cosine_similarity(left: &[f32], right: &[f32], dims: usize) -> f32 {
    let dims = dims.min(left.len()).min(right.len());
    let mut dot = 0.0_f32;
    let mut left_norm = 0.0_f32;
    let mut right_norm = 0.0_f32;
    for (left_value, right_value) in left.iter().zip(right.iter()).take(dims) {
        dot += left_value * right_value;
        left_norm += left_value * left_value;
        right_norm += right_value * right_value;
    }
    let magnitude = left_norm.sqrt() * right_norm.sqrt();
    if magnitude <= f32::EPSILON {
        return 0.0;
    }
    (dot / magnitude).clamp(-1.0, 1.0)
}

/// Rank stored embeddings against a query vector. Deterministic: equal scores
/// break ties by `(entity_kind, entity_id)`.
#[must_use]
pub fn rank(
    query: &[f32],
    dims: usize,
    candidates: &[StoredEmbedding],
    limit: usize,
) -> Vec<SearchHit> {
    let mut scored: Vec<SearchHit> = candidates
        .iter()
        .map(|candidate| SearchHit {
            entity_kind: candidate.entity_kind,
            entity_id: candidate.entity_id.clone(),
            score: cosine_similarity(query, &candidate.vector, dims.min(candidate.dims)),
        })
        .collect();
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.entity_kind.cmp(&b.entity_kind))
            .then_with(|| a.entity_id.cmp(&b.entity_id))
    });
    scored.truncate(limit.clamp(1, 100));
    scored
}

#[derive(Clone, Debug, Deserialize)]
pub struct CorrelateRequest {
    /// Free-text description of the regression (a failing job's tail, usually).
    pub symptom: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub run_id: Option<Uuid>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RegressionFinding {
    pub entity_kind: EntityKind,
    pub entity_id: String,
    pub score: f32,
    /// `strong` ≥ 0.85, `likely` ≥ 0.7, otherwise `weak`. Reported so a caller
    /// never has to guess what a raw cosine score means.
    pub confidence: &'static str,
}

#[must_use]
pub fn confidence_for(score: f32) -> &'static str {
    if score >= 0.85 {
        "strong"
    } else if score >= 0.70 {
        "likely"
    } else {
        "weak"
    }
}

#[must_use]
pub fn correlate(hits: Vec<SearchHit>) -> Vec<RegressionFinding> {
    hits.into_iter()
        .map(|hit| RegressionFinding {
            confidence: confidence_for(hit.score),
            entity_kind: hit.entity_kind,
            entity_id: hit.entity_id,
            score: hit.score,
        })
        .collect()
}

/// A boxed future, so [`EmbeddingProvider`] stays object-safe without pulling
/// in `async_trait`.
pub type EmbedFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<Vec<f32>>, EmbeddingError>> + Send + 'a>>;

/// One embedding backend. Implementors must return exactly one vector per
/// input, in order.
pub trait EmbeddingProvider: Send + Sync {
    fn model(&self) -> &str;
    fn dimensions(&self) -> usize;
    fn embed<'a>(&'a self, inputs: &'a [String]) -> EmbedFuture<'a>;
}

/// Deterministic provider for tests and for local development without an API
/// key: a hash-derived unit vector. Never used when a base URL is configured.
#[derive(Clone, Debug)]
pub struct DeterministicProvider {
    dims: usize,
}

impl DeterministicProvider {
    #[must_use]
    pub const fn new(dims: usize) -> Self {
        Self { dims }
    }

    #[must_use]
    pub fn vector_for(&self, input: &str) -> Vec<f32> {
        // A tiny FNV-1a walk gives a stable, well-spread vector without a
        // dependency. This is emphatically not a semantic embedding.
        let mut state: u64 = 0xcbf2_9ce4_8422_2325;
        let mut vector = Vec::with_capacity(self.dims);
        for index in 0..self.dims {
            for byte in input.as_bytes() {
                state ^= u64::from(*byte);
                state = state.wrapping_mul(0x0000_0100_0000_01b3);
            }
            state ^= index as u64;
            state = state.wrapping_mul(0x0000_0100_0000_01b3);
            let scaled = ((state >> 11) as f64 / (1_u64 << 53) as f64) as f32;
            vector.push(scaled.mul_add(2.0, -1.0));
        }
        normalize(&mut vector);
        vector
    }
}

fn normalize(vector: &mut [f32]) {
    let magnitude = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if magnitude > f32::EPSILON {
        for value in vector.iter_mut() {
            *value /= magnitude;
        }
    }
}

impl EmbeddingProvider for DeterministicProvider {
    fn model(&self) -> &str {
        "deterministic-local"
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn embed<'a>(&'a self, inputs: &'a [String]) -> EmbedFuture<'a> {
        Box::pin(async move {
            Ok(inputs
                .iter()
                .map(|input| self.vector_for(input))
                .collect::<Vec<_>>())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(entity_id: &str, vector: Vec<f32>) -> StoredEmbedding {
        let dims = vector.len();
        StoredEmbedding {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            entity_kind: EntityKind::Run,
            entity_id: entity_id.to_owned(),
            model: "test".to_owned(),
            dims,
            vector: pad_to_slots(vector, dims).expect("pads"),
            created_at: String::new(),
        }
    }

    #[test]
    fn padding_fills_exactly_the_physical_slot_count() {
        let padded = pad_to_slots(vec![1.0, 2.0, 3.0], 3).expect("pads");
        assert_eq!(padded.len(), VECTOR_SLOTS);
        assert_eq!(&padded[..3], &[1.0, 2.0, 3.0]);
        assert!(padded[3..].iter().all(|value| *value == 0.0));
    }

    #[test]
    fn a_full_width_model_still_fits_the_slot_count() {
        let padded = pad_to_slots(vec![0.5; MAX_DIMENSIONS], MAX_DIMENSIONS).expect("pads");
        assert_eq!(padded.len(), VECTOR_SLOTS);
        assert_eq!(padded[MAX_DIMENSIONS], 0.0);
    }

    #[test]
    fn oversized_and_mismatched_dimensions_are_typed_errors() {
        assert_eq!(
            pad_to_slots(vec![0.0; MAX_DIMENSIONS + 1], MAX_DIMENSIONS + 1),
            Err(EmbeddingError::DimensionsTooLarge {
                got: MAX_DIMENSIONS + 1
            })
        );
        assert_eq!(
            pad_to_slots(Vec::new(), 0),
            Err(EmbeddingError::DimensionsTooLarge { got: 0 })
        );
        assert_eq!(
            pad_to_slots(vec![1.0, 2.0], 3),
            Err(EmbeddingError::DimensionMismatch {
                expected: 3,
                got: 2
            })
        );
    }

    #[test]
    fn batches_are_bounded() {
        let item = |text: &str| IndexItem {
            entity_kind: EntityKind::Run,
            entity_id: "run-1".to_owned(),
            text: text.to_owned(),
            metadata: BTreeMap::new(),
        };
        assert_eq!(validate_batch(&[]), Err(EmbeddingError::InvalidBatch));
        assert_eq!(validate_batch(&[item("hello")]), Ok(()));
        let too_many: Vec<IndexItem> = (0..=MAX_BATCH).map(|_| item("x")).collect();
        assert_eq!(validate_batch(&too_many), Err(EmbeddingError::InvalidBatch));
        assert_eq!(
            validate_batch(&[item(&"x".repeat(MAX_INPUT_BYTES + 1))]),
            Err(EmbeddingError::InputTooLarge)
        );
    }

    #[test]
    fn cosine_similarity_ignores_the_padded_tail() {
        let left = pad_to_slots(vec![1.0, 0.0], 2).expect("pads");
        let right = pad_to_slots(vec![1.0, 0.0], 2).expect("pads");
        assert!((cosine_similarity(&left, &right, 2) - 1.0).abs() < 1e-6);

        let orthogonal = pad_to_slots(vec![0.0, 1.0], 2).expect("pads");
        assert!(cosine_similarity(&left, &orthogonal, 2).abs() < 1e-6);

        let opposite = pad_to_slots(vec![-1.0, 0.0], 2).expect("pads");
        assert!((cosine_similarity(&left, &opposite, 2) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_zero_vector_scores_zero_rather_than_nan() {
        let zero = vec![0.0_f32; 4];
        let unit = vec![1.0_f32, 0.0, 0.0, 0.0];
        let score = cosine_similarity(&zero, &unit, 4);
        assert!(score.abs() < f32::EPSILON, "{score}");
        assert!(!score.is_nan());
    }

    #[test]
    fn ranking_is_ordered_bounded_and_deterministic() {
        let candidates = vec![
            stored("far", vec![0.0, 1.0]),
            stored("near", vec![1.0, 0.0]),
            stored("mid", vec![0.7071, 0.7071]),
        ];
        let query = pad_to_slots(vec![1.0, 0.0], 2).expect("pads");
        let hits = rank(&query, 2, &candidates, 2);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].entity_id, "near");
        assert_eq!(hits[1].entity_id, "mid");
        assert_eq!(rank(&query, 2, &candidates, 2), hits);
    }

    #[test]
    fn ranking_ties_break_deterministically_by_identity() {
        let candidates = vec![
            stored("b", vec![1.0, 0.0]),
            stored("a", vec![1.0, 0.0]),
            stored("c", vec![1.0, 0.0]),
        ];
        let query = pad_to_slots(vec![1.0, 0.0], 2).expect("pads");
        let hits = rank(&query, 2, &candidates, 3);
        assert_eq!(
            hits.iter()
                .map(|hit| hit.entity_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn confidence_bands_are_explicit() {
        assert_eq!(confidence_for(0.99), "strong");
        assert_eq!(confidence_for(0.85), "strong");
        assert_eq!(confidence_for(0.70), "likely");
        assert_eq!(confidence_for(0.69), "weak");
        assert_eq!(confidence_for(-1.0), "weak");
    }

    #[test]
    fn correlation_preserves_order_and_labels_confidence() {
        let findings = correlate(vec![
            SearchHit {
                entity_kind: EntityKind::Job,
                entity_id: "job-1".to_owned(),
                score: 0.9,
            },
            SearchHit {
                entity_kind: EntityKind::Job,
                entity_id: "job-2".to_owned(),
                score: 0.5,
            },
        ]);
        assert_eq!(findings[0].confidence, "strong");
        assert_eq!(findings[1].confidence, "weak");
        assert_eq!(findings[0].entity_id, "job-1");
    }

    #[tokio::test]
    async fn the_deterministic_provider_is_stable_and_unit_length() {
        let provider = DeterministicProvider::new(8);
        let inputs = vec!["cargo test failed".to_owned(), "npm ci failed".to_owned()];
        let first = provider.embed(&inputs).await.expect("embeds");
        let second = provider.embed(&inputs).await.expect("embeds");
        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].len(), 8);
        let magnitude: f32 = first[0].iter().map(|value| value * value).sum();
        assert!((magnitude - 1.0).abs() < 1e-4, "{magnitude}");
        assert_ne!(first[0], first[1]);
    }
}
