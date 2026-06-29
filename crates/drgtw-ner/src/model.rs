//! WP 4.1 — ONNX NER model: load, tokenize, window, infer, BIO-decode.

use std::path::Path;
use std::sync::Mutex;

use ort::session::{builder::GraphOptimizationLevel, Session, SessionInputValue};
use ort::value::Tensor;
use tokenizers::Tokenizer;

use crate::{NerError, NerKind, NerSpan};

/// Max sequence length (including the two special tokens) the BERT model accepts.
const MAX_SEQ_LEN: usize = 512;
/// Number of word tokens kept per window (room for [CLS] + [SEP]).
const WINDOW_TOKENS: usize = MAX_SEQ_LEN - 2;
/// Overlap (in word tokens) between consecutive windows.
const STRIDE_OVERLAP: usize = 64;

/// One decoded entity label, mapped to a public [`NerKind`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    Outside,
    Begin(NerKind),
    Inside(NerKind),
}

/// Loaded model: tokenizer + ONNX session (behind a `Mutex`, since
/// [`Session::run`] needs `&mut self`) + label map. Thread-safe.
pub struct NerModel {
    tokenizer: Tokenizer,
    session: Mutex<Session>,
    /// Per-label-id decoded tag (index = class id from `id2label`).
    tags: Vec<Tag>,
    /// Input names the graph actually wants (e.g. input_ids, attention_mask,
    /// token_type_ids), in declaration order.
    input_names: Vec<String>,
    /// Output name holding the logits tensor.
    output_name: String,
    cls_id: u32,
    sep_id: u32,
}

impl NerModel {
    pub fn load(model_dir: &Path) -> Result<Self, NerError> {
        let load_err = |path: &Path, message: String| NerError::Load {
            path: path.display().to_string(),
            message,
        };

        // 1. Locate the ONNX file: prefer quantized.
        let quantized = model_dir.join("model_quantized.onnx");
        let plain = model_dir.join("model.onnx");
        let onnx_path = if quantized.is_file() {
            quantized
        } else if plain.is_file() {
            plain
        } else {
            return Err(load_err(
                model_dir,
                "neither model_quantized.onnx nor model.onnx found".to_string(),
            ));
        };

        // 2. Parse config.json -> id2label.
        let config_path = model_dir.join("config.json");
        let config_raw = std::fs::read_to_string(&config_path)
            .map_err(|e| load_err(&config_path, format!("reading config.json: {e}")))?;
        let config: serde_json::Value = serde_json::from_str(&config_raw)
            .map_err(|e| load_err(&config_path, format!("parsing config.json: {e}")))?;
        let tags = parse_id2label(&config)
            .map_err(|msg| load_err(&config_path, msg))?;

        // 3. Load tokenizer.
        let tokenizer_path = model_dir.join("tokenizer.json");
        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| load_err(&tokenizer_path, format!("loading tokenizer: {e}")))?;
        let cls_id = tokenizer
            .token_to_id("[CLS]")
            .ok_or_else(|| load_err(&tokenizer_path, "tokenizer has no [CLS] token".to_string()))?;
        let sep_id = tokenizer
            .token_to_id("[SEP]")
            .ok_or_else(|| load_err(&tokenizer_path, "tokenizer has no [SEP] token".to_string()))?;

        // 4. Build session. intra_threads(1): parallelism comes from the pool.
        // allow_spinning=0: ORT thread pools busy-wait (spin) on the work queue
        // between inferences by default, pinning ~1 core at 100% even when idle.
        // Disable so worker threads sleep when there is no traffic.
        let build_session = || -> ort::Result<Session> {
            Session::builder()?
                .with_optimization_level(GraphOptimizationLevel::Level3)?
                .with_intra_threads(1)?
                .with_config_entry("session.intra_op.allow_spinning", "0")?
                .with_config_entry("session.inter_op.allow_spinning", "0")?
                .commit_from_file(&onnx_path)
        };
        let session = build_session()
            .map_err(|e| load_err(&onnx_path, format!("building ONNX session: {e}")))?;

        let input_names: Vec<String> =
            session.inputs().iter().map(|i| i.name().to_string()).collect();
        let output_name = session
            .outputs()
            .first()
            .map(|o| o.name().to_string())
            .ok_or_else(|| load_err(&onnx_path, "model has no outputs".to_string()))?;

        Ok(Self {
            tokenizer,
            session: Mutex::new(session),
            tags,
            input_names,
            output_name,
            cls_id,
            sep_id,
        })
    }

    pub fn detect(&self, text: &str) -> Result<Vec<NerSpan>, NerError> {
        if text.is_empty() {
            return Ok(Vec::new());
        }

        // Encode WITHOUT special tokens so we own windowing & offsets are
        // raw byte offsets into `text`. tokenizers default OffsetType::Byte.
        let encoding = self
            .tokenizer
            .encode(text, false)
            .map_err(|e| NerError::Inference(format!("tokenization failed: {e}")))?;

        let ids = encoding.get_ids();
        let offsets = encoding.get_offsets();
        let word_ids = encoding.get_word_ids();
        let n = ids.len();
        if n == 0 {
            return Ok(Vec::new());
        }

        // Window the token stream. Each window prepends [CLS] and appends [SEP].
        let step = WINDOW_TOKENS.saturating_sub(STRIDE_OVERLAP).max(1);
        let mut all_spans: Vec<NerSpan> = Vec::new();
        let mut start = 0usize;
        loop {
            let end = (start + WINDOW_TOKENS).min(n);
            self.run_window(
                text,
                &ids[start..end],
                &offsets[start..end],
                &word_ids[start..end],
                &mut all_spans,
            )?;
            if end >= n {
                break;
            }
            start += step;
        }

        Ok(dedupe_spans(all_spans))
    }

    /// Run one window of word tokens through the model and append decoded spans.
    fn run_window(
        &self,
        text: &str,
        window_ids: &[u32],
        window_offsets: &[(usize, usize)],
        window_word_ids: &[Option<u32>],
        out: &mut Vec<NerSpan>,
    ) -> Result<(), NerError> {
        let seq_len = window_ids.len() + 2; // [CLS] ... [SEP]

        let mut input_ids: Vec<i64> = Vec::with_capacity(seq_len);
        input_ids.push(self.cls_id as i64);
        input_ids.extend(window_ids.iter().map(|&id| id as i64));
        input_ids.push(self.sep_id as i64);

        let attention_mask: Vec<i64> = vec![1; seq_len];
        let token_type_ids: Vec<i64> = vec![0; seq_len];

        let shape = [1_i64, seq_len as i64];

        // Build inputs in the order the graph declares them.
        let mut inputs: Vec<(&str, SessionInputValue)> = Vec::with_capacity(self.input_names.len());
        for name in &self.input_names {
            let data: Vec<i64> = match name.as_str() {
                "input_ids" => input_ids.clone(),
                "attention_mask" => attention_mask.clone(),
                "token_type_ids" => token_type_ids.clone(),
                other => {
                    return Err(NerError::Inference(format!(
                        "model requested unknown input `{other}`"
                    )))
                }
            };
            let tensor = Tensor::from_array((shape, data.into_boxed_slice()))
                .map_err(|e| NerError::Inference(format!("building tensor `{name}`: {e}")))?;
            inputs.push((name.as_str(), tensor.into()));
        }

        let mut session = self
            .session
            .lock()
            .map_err(|_| NerError::Inference("session mutex poisoned".to_string()))?;

        let outputs = session
            .run(inputs)
            .map_err(|e| NerError::Inference(format!("session run failed: {e}")))?;

        let logits = &outputs[self.output_name.as_str()];
        let (out_shape, data) = logits
            .try_extract_tensor::<f32>()
            .map_err(|e| NerError::Inference(format!("extracting logits: {e}")))?;

        // Expect [1, seq, num_labels]. Shape derefs to [i64].
        let dims: &[i64] = out_shape;
        if dims.len() != 3 {
            return Err(NerError::Inference(format!(
                "unexpected logits rank {} (shape {:?})",
                dims.len(),
                dims
            )));
        }
        let num_labels = dims[2] as usize;
        if num_labels != self.tags.len() {
            return Err(NerError::Inference(format!(
                "logits num_labels {} != id2label len {}",
                num_labels,
                self.tags.len()
            )));
        }

        // Per-token argmax over softmax, skipping [CLS] (row 0) and [SEP] (last
        // row): window token i corresponds to logits row i+1. We do this while
        // holding the lock (cheap) so we don't have to copy the logits buffer.
        let mut tokens: Vec<TokenPred> = Vec::with_capacity(window_ids.len());
        for (i, (&(byte_start, byte_end), &word_id)) in
            window_offsets.iter().zip(window_word_ids.iter()).enumerate()
        {
            let row = i + 1;
            let base = row * num_labels;
            let row_logits = &data[base..base + num_labels];
            let (label, score) = softmax_argmax(row_logits);
            tokens.push(TokenPred {
                tag: self.tags[label],
                score,
                byte_start,
                byte_end,
                // Tokens sharing a word_id are subwords of one word.
                word_id,
            });
        }

        // Release the lock (drops `outputs` borrow) before BIO decoding.
        drop(outputs);
        drop(session);

        decode_bio(text, &tokens, out);
        Ok(())
    }
}

struct TokenPred {
    tag: Tag,
    score: f32,
    byte_start: usize,
    byte_end: usize,
    word_id: Option<u32>,
}

/// Numerically stable softmax + argmax over one logits row. Returns
/// (best_index, best_probability).
fn softmax_argmax(logits: &[f32]) -> (usize, f32) {
    let mut best_i = 0usize;
    let mut max = f32::NEG_INFINITY;
    for (i, &v) in logits.iter().enumerate() {
        if v > max {
            max = v;
            best_i = i;
        }
    }
    let mut sum = 0.0f32;
    for &v in logits {
        sum += (v - max).exp();
    }
    let prob = 1.0 / sum; // exp(max-max)=1 for the best logit
    (best_i, prob)
}

/// BIO decode a window's token predictions into byte-offset spans.
///
/// Subtleties handled:
/// - B-X opens a span; I-X continues it.
/// - I-X with no open span (or after O / different kind) opens a new span —
///   quantized models frequently emit I-* without a leading B-*.
/// - Subword continuations (same `word_id`, label O or matching) extend the
///   current span so we never split a word.
fn decode_bio(text: &str, tokens: &[TokenPred], out: &mut Vec<NerSpan>) {
    let mut cur: Option<OpenSpan> = None;

    for tok in tokens {
        match tok.tag {
            Tag::Outside => {
                // A subword continuation of the active word, even if labeled O,
                // belongs to the open span: extend through it.
                if let Some(open) = cur.as_mut()
                    && is_continuation(open.word_id, tok.word_id) {
                        open.end = tok.byte_end;
                        open.word_id = tok.word_id;
                        continue;
                    }
                if let Some(open) = cur.take() {
                    out.push(open.finish());
                }
            }
            Tag::Begin(kind) | Tag::Inside(kind) => {
                let continues = match (&cur, tok.tag) {
                    // I-X continuing the same kind extends the open span.
                    (Some(open), Tag::Inside(_)) if open.kind == kind => true,
                    // Subword of the open word, regardless of B/I, extends it.
                    (Some(open), _) if is_continuation(open.word_id, tok.word_id) => true,
                    _ => false,
                };
                if continues {
                    let open = cur.as_mut().expect("continues implies open");
                    open.end = tok.byte_end;
                    open.score_sum += tok.score;
                    open.count += 1;
                    open.word_id = tok.word_id;
                } else {
                    if let Some(open) = cur.take() {
                        out.push(open.finish());
                    }
                    cur = Some(OpenSpan {
                        start: tok.byte_start,
                        end: tok.byte_end,
                        kind,
                        score_sum: tok.score,
                        count: 1,
                        word_id: tok.word_id,
                    });
                }
            }
        }
    }
    if let Some(open) = cur.take() {
        out.push(open.finish());
    }

    debug_assert!(out.iter().all(|s| text.is_char_boundary(s.start) && text.is_char_boundary(s.end)));
    let _ = text;
}

/// Two adjacent tokens belong to the same word (subword continuation)?
fn is_continuation(open_word: Option<u32>, tok_word: Option<u32>) -> bool {
    match (open_word, tok_word) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

struct OpenSpan {
    start: usize,
    end: usize,
    kind: NerKind,
    score_sum: f32,
    count: u32,
    word_id: Option<u32>,
}

impl OpenSpan {
    fn finish(self) -> NerSpan {
        NerSpan {
            start: self.start,
            end: self.end,
            kind: self.kind,
            score: self.score_sum / self.count.max(1) as f32,
        }
    }
}

/// Drop duplicate/overlapping spans produced by windowing overlap; keep the
/// higher-scoring one when two spans cover overlapping byte ranges.
fn dedupe_spans(mut spans: Vec<NerSpan>) -> Vec<NerSpan> {
    // Sort by start, then by descending score so the first seen at a position
    // is the strongest.
    spans.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then(b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut kept: Vec<NerSpan> = Vec::with_capacity(spans.len());
    for span in spans {
        // Exact duplicate (same range + kind) -> keep higher score (already first).
        if let Some(existing) = kept.iter_mut().find(|k| {
            k.kind == span.kind && ranges_overlap(k.start, k.end, span.start, span.end)
        }) {
            if span.score > existing.score {
                *existing = span;
            }
        } else {
            kept.push(span);
        }
    }
    kept.sort_by_key(|s| s.start);
    kept
}

fn ranges_overlap(a0: usize, a1: usize, b0: usize, b1: usize) -> bool {
    a0 < b1 && b0 < a1
}

/// Parse `id2label` from a HF config into an index-ordered tag vector.
fn parse_id2label(config: &serde_json::Value) -> Result<Vec<Tag>, String> {
    let map = config
        .get("id2label")
        .and_then(|v| v.as_object())
        .ok_or_else(|| "config.json missing object `id2label`".to_string())?;

    let mut pairs: Vec<(usize, Tag)> = Vec::with_capacity(map.len());
    for (k, v) in map {
        let id: usize = k
            .parse()
            .map_err(|_| format!("id2label key `{k}` is not an integer"))?;
        let label = v
            .as_str()
            .ok_or_else(|| format!("id2label[{k}] is not a string"))?;
        pairs.push((id, label_to_tag(label)));
    }
    pairs.sort_by_key(|(id, _)| *id);

    // Ensure contiguous 0..n.
    for (expected, (id, _)) in pairs.iter().enumerate() {
        if *id != expected {
            return Err(format!(
                "id2label ids are not contiguous from 0 (gap at {expected})"
            ));
        }
    }
    Ok(pairs.into_iter().map(|(_, t)| t).collect())
}

/// Map a HF BIO label string to an internal [`Tag`]. DATE/MISC/unknown -> Outside.
fn label_to_tag(label: &str) -> Tag {
    let (prefix, body) = match label.split_once('-') {
        Some((p, b)) => (p, b),
        None => return Tag::Outside, // "O" and any bare label
    };
    let kind = match body {
        "PER" => NerKind::Person,
        "ORG" => NerKind::Org,
        "LOC" => NerKind::Location,
        _ => return Tag::Outside, // DATE, MISC, etc.
    };
    match prefix {
        "B" => Tag::Begin(kind),
        "I" => Tag::Inside(kind),
        _ => Tag::Outside,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_mapping() {
        assert!(matches!(label_to_tag("O"), Tag::Outside));
        assert!(matches!(label_to_tag("B-PER"), Tag::Begin(NerKind::Person)));
        assert!(matches!(label_to_tag("I-ORG"), Tag::Inside(NerKind::Org)));
        assert!(matches!(label_to_tag("B-LOC"), Tag::Begin(NerKind::Location)));
        assert!(matches!(label_to_tag("B-DATE"), Tag::Outside));
        assert!(matches!(label_to_tag("I-MISC"), Tag::Outside));
    }

    #[test]
    fn id2label_parse_orders_by_id() {
        let cfg = serde_json::json!({
            "id2label": { "0": "O", "1": "B-PER", "2": "I-PER" }
        });
        let tags = parse_id2label(&cfg).unwrap();
        assert_eq!(tags.len(), 3);
        assert!(matches!(tags[1], Tag::Begin(NerKind::Person)));
    }

    #[test]
    fn id2label_rejects_gaps() {
        let cfg = serde_json::json!({ "id2label": { "0": "O", "2": "B-PER" } });
        assert!(parse_id2label(&cfg).is_err());
    }

    #[test]
    fn softmax_picks_max_and_normalizes() {
        let (i, p) = softmax_argmax(&[0.0, 5.0, 1.0]);
        assert_eq!(i, 1);
        assert!(p > 0.9 && p <= 1.0);
    }

    #[test]
    fn dedupe_keeps_higher_score() {
        let spans = vec![
            NerSpan { start: 0, end: 5, kind: NerKind::Person, score: 0.6 },
            NerSpan { start: 0, end: 5, kind: NerKind::Person, score: 0.9 },
        ];
        let out = dedupe_spans(spans);
        assert_eq!(out.len(), 1);
        assert!((out[0].score - 0.9).abs() < 1e-6);
    }
}
