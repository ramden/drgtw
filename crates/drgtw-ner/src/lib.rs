//! ONNX-based NER: model loading, inference, worker pool.
//!
//! Public API contract (Phase 4 / WP 4.1 + 4.2). Frozen — extend, don't break.
//!
//! Artifact layout (one dir per model, see models/ner-multilingual/):
//! - `model_quantized.onnx` (or `model.onnx`) — token-classification model
//! - `tokenizer.json` — HF tokenizers file
//! - `config.json` — HF config with `id2label` (BIO scheme: O, B-PER, I-PER, …)

use std::time::Duration;

mod model;
mod pool;

pub use model::NerModel;
pub use pool::NerPool;

/// Entity categories produced by NER. Mapping from model labels:
/// PER → Person, ORG → Org, LOC → Location; other labels (DATE, MISC)
/// are ignored in v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NerKind {
    Person,
    Org,
    Location,
}

/// A detected named entity. Byte offsets into the input string.
#[derive(Debug, Clone, PartialEq)]
pub struct NerSpan {
    pub start: usize,
    pub end: usize,
    pub kind: NerKind,
    /// Mean token score over the BIO span, 0.0..=1.0 (softmaxed).
    pub score: f32,
}

#[derive(Debug, thiserror::Error)]
pub enum NerError {
    #[error("cannot load model from `{path}`: {message}")]
    Load { path: String, message: String },
    #[error("inference failed: {0}")]
    Inference(String),
    #[error("NER queue full or workers gone")]
    Unavailable,
    #[error("NER inference timed out after {0:?}")]
    Timeout(Duration),
}

/// Bounded worker pool configuration. See [`NerPool`].
#[derive(Debug, Clone)]
pub struct NerPoolConfig {
    /// Worker thread count.
    pub workers: usize,
    /// Max queued requests before `check` returns `Unavailable`.
    pub queue_capacity: usize,
    /// Per-request inference timeout.
    pub timeout: Duration,
}

impl Default for NerPoolConfig {
    fn default() -> Self {
        Self {
            workers: 2,
            queue_capacity: 64,
            timeout: Duration::from_secs(5),
        }
    }
}
