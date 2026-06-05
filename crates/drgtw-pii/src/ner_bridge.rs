//! Bridge: drgtw-ner pool → [`Recognizer`]. WP 4.3.

use std::sync::Arc;

use drgtw_config::FailMode;
use drgtw_ner::{NerKind, NerPool, NerSpan};

use crate::{DetectError, Detection, EntityKind, Recognizer};

/// Adapts a [`NerPool`] to the PII [`Recognizer`] trait.
///
/// Failure semantics: pool errors (timeout, queue full, inference) are
/// handled per configured [`FailMode`]:
/// - `Open`: warn + no detections (request proceeds without NER masking)
/// - `Closed`: error propagates through [`Recognizer::try_detect`] →
///   the request fails rather than leaking unmasked entities.
///
/// NOTE: fail-closed only takes effect through the fallible scan path
/// (`PiiEngine::try_scan` / `try_pseudonymize_body`); the infallible
/// `detect` degrades to fail-open with a warning.
pub struct NerRecognizer {
    pool: Arc<NerPool>,
    threshold: f32,
    fail_mode: FailMode,
}

impl NerRecognizer {
    pub fn new(pool: Arc<NerPool>, threshold: f32, fail_mode: FailMode) -> Self {
        Self {
            pool,
            threshold,
            fail_mode,
        }
    }
}

/// Filter spans by score threshold and map to PII detections.
/// Pure function — unit-testable without a model.
pub(crate) fn map_spans(spans: Vec<NerSpan>, threshold: f32) -> Vec<Detection> {
    spans
        .into_iter()
        .filter(|s| s.score >= threshold)
        .map(|s| Detection {
            start: s.start,
            end: s.end,
            kind: match s.kind {
                NerKind::Person => EntityKind::Person,
                NerKind::Org => EntityKind::Org,
                NerKind::Location => EntityKind::Location,
            },
        })
        .collect()
}

impl Recognizer for NerRecognizer {
    fn name(&self) -> &str {
        "ner"
    }

    fn detect(&self, text: &str) -> Vec<Detection> {
        // Infallible path: degrade to fail-open regardless of mode.
        self.try_detect(text).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "NER failed on infallible path; continuing without NER detections");
            Vec::new()
        })
    }

    fn try_detect(&self, text: &str) -> Result<Vec<Detection>, DetectError> {
        match self.pool.detect(text) {
            Ok(spans) => Ok(map_spans(spans, self.threshold)),
            Err(e) => match self.fail_mode {
                FailMode::Open => {
                    tracing::warn!(error = %e, "NER failed (fail_mode=open); continuing without NER detections");
                    Ok(Vec::new())
                }
                FailMode::Closed => Err(DetectError(format!("NER failed (fail_mode=closed): {e}"))),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: usize, end: usize, kind: NerKind, score: f32) -> NerSpan {
        NerSpan {
            start,
            end,
            kind,
            score,
        }
    }

    #[test]
    fn map_spans_filters_by_threshold() {
        let spans = vec![
            span(0, 3, NerKind::Person, 0.9),
            span(4, 8, NerKind::Org, 0.4),
            span(9, 12, NerKind::Location, 0.5),
        ];
        let dets = map_spans(spans, 0.5);
        assert_eq!(dets.len(), 2);
        assert_eq!(dets[0].kind, EntityKind::Person);
        assert_eq!(dets[1].kind, EntityKind::Location);
    }

    #[test]
    fn map_spans_kind_mapping() {
        let dets = map_spans(
            vec![
                span(0, 1, NerKind::Person, 1.0),
                span(2, 3, NerKind::Org, 1.0),
                span(4, 5, NerKind::Location, 1.0),
            ],
            0.0,
        );
        assert_eq!(
            dets.iter().map(|d| d.kind.clone()).collect::<Vec<_>>(),
            vec![EntityKind::Person, EntityKind::Org, EntityKind::Location]
        );
    }

    #[test]
    fn map_spans_preserves_offsets() {
        let dets = map_spans(vec![span(7, 21, NerKind::Person, 0.99)], 0.5);
        assert_eq!((dets[0].start, dets[0].end), (7, 21));
    }

    #[test]
    fn map_spans_empty_input() {
        assert!(map_spans(vec![], 0.5).is_empty());
    }
}
