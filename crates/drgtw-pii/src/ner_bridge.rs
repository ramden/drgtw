//! Bridge: drgtw-ner pool → [`Recognizer`]. WP 4.3.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

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
    /// Cross-request verdict cache. `None` when `cache_capacity == 0`. Keyed on
    /// a 128-bit hash of the input text; values are the raw model spans
    /// (pre-threshold), so no plaintext is retained.
    cache: Option<Mutex<NerCache>>,
}

impl NerRecognizer {
    /// Build a recognizer with the verdict cache disabled.
    pub fn new(pool: Arc<NerPool>, threshold: f32, fail_mode: FailMode) -> Self {
        Self::with_cache(pool, threshold, fail_mode, 0)
    }

    /// Build a recognizer, enabling the verdict cache when `cache_capacity > 0`.
    pub fn with_cache(
        pool: Arc<NerPool>,
        threshold: f32,
        fail_mode: FailMode,
        cache_capacity: usize,
    ) -> Self {
        let cache = (cache_capacity > 0).then(|| Mutex::new(NerCache::new(cache_capacity)));
        Self {
            pool,
            threshold,
            fail_mode,
            cache,
        }
    }

    /// Fetch raw spans for `text`, consulting the verdict cache when enabled.
    /// Only successful detections are cached — errors (timeout/queue-full) are
    /// never cached, so a transient failure cannot poison later requests.
    fn detect_spans(&self, text: &str) -> Result<Vec<NerSpan>, drgtw_ner::NerError> {
        let Some(cache) = &self.cache else {
            return self.pool.detect(text);
        };
        let key = hash_text(text);
        if let Some(spans) = cache.lock().expect("NER cache mutex").get(&key) {
            return Ok(spans);
        }
        let spans = self.pool.detect(text)?;
        cache
            .lock()
            .expect("NER cache mutex")
            .put(key, spans.clone());
        Ok(spans)
    }
}

/// 128-bit hash of `text` built from two `DefaultHasher` (SipHash) passes with
/// distinct prefixes. Deterministic for the process lifetime, which is all the
/// in-memory cache needs. 128 bits makes a collision (→ wrong verdict reuse)
/// astronomically unlikely.
fn hash_text(text: &str) -> u128 {
    let mut h1 = std::collections::hash_map::DefaultHasher::new();
    0u8.hash(&mut h1);
    text.hash(&mut h1);
    let mut h2 = std::collections::hash_map::DefaultHasher::new();
    0xffu8.hash(&mut h2);
    text.hash(&mut h2);
    ((h1.finish() as u128) << 64) | (h2.finish() as u128)
}

/// Bounded LRU cache of NER verdicts. Keys are text hashes (no plaintext);
/// values are raw spans plus a monotonic access stamp for LRU eviction.
struct NerCache {
    capacity: usize,
    seq: u64,
    entries: HashMap<u128, (Vec<NerSpan>, u64)>,
}

impl NerCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            seq: 0,
            entries: HashMap::new(),
        }
    }

    /// Return a clone of the cached spans, marking the entry most-recently-used.
    fn get(&mut self, key: &u128) -> Option<Vec<NerSpan>> {
        self.seq += 1;
        let stamp = self.seq;
        let entry = self.entries.get_mut(key)?;
        entry.1 = stamp;
        Some(entry.0.clone())
    }

    /// Insert spans for `key`, evicting the least-recently-used entry first when
    /// at capacity.
    fn put(&mut self, key: u128, spans: Vec<NerSpan>) {
        self.seq += 1;
        let stamp = self.seq;
        if !self.entries.contains_key(&key)
            && self.entries.len() >= self.capacity
            && let Some(lru) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, s))| *s)
                .map(|(k, _)| *k)
        {
            self.entries.remove(&lru);
        }
        self.entries.insert(key, (spans, stamp));
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
        match self.detect_spans(text) {
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

    // ── NER verdict cache (R2) ─────────────────────────────────────────────

    fn person(start: usize, end: usize) -> NerSpan {
        span(start, end, NerKind::Person, 0.9)
    }

    #[test]
    fn cache_hit_returns_stored_spans() {
        let mut cache = NerCache::new(4);
        let key = hash_text("Angela Merkel");
        assert!(cache.get(&key).is_none(), "cold miss");
        cache.put(key, vec![person(0, 13)]);
        let got = cache.get(&key).expect("hit after put");
        assert_eq!(got, vec![person(0, 13)]);
    }

    #[test]
    fn cache_evicts_least_recently_used() {
        let mut cache = NerCache::new(2);
        let (ka, kb, kc) = (hash_text("a"), hash_text("b"), hash_text("c"));
        cache.put(ka, vec![person(0, 1)]);
        cache.put(kb, vec![person(0, 1)]);
        // Touch `a` so `b` becomes least-recently-used.
        assert!(cache.get(&ka).is_some());
        // Insert `c` → evicts `b`.
        cache.put(kc, vec![person(0, 1)]);
        assert!(cache.get(&ka).is_some(), "a retained (recently used)");
        assert!(cache.get(&kc).is_some(), "c retained (just inserted)");
        assert!(cache.get(&kb).is_none(), "b evicted (LRU)");
    }

    #[test]
    fn cache_reinsert_does_not_grow_past_capacity() {
        let mut cache = NerCache::new(1);
        let k = hash_text("x");
        cache.put(k, vec![person(0, 1)]);
        cache.put(k, vec![person(0, 2)]);
        assert_eq!(cache.entries.len(), 1, "re-insert overwrites, no growth");
        assert_eq!(cache.get(&k), Some(vec![person(0, 2)]));
    }

    #[test]
    fn hash_text_is_deterministic_and_distinct() {
        assert_eq!(hash_text("same text"), hash_text("same text"));
        assert_ne!(hash_text("alice"), hash_text("bob"));
        // The static system prompt and a user message must not collide.
        assert_ne!(hash_text(""), hash_text(" "));
    }
}
