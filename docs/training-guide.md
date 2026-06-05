# DRGTW NER Training Guide (Phase 5)

The `training/` directory is a self-contained [`uv`](https://docs.astral.sh/uv/)
project (package `drgtw_training`) that fine-tunes a Hugging Face
token-classification model on your labelled data and exports artifacts that
drop straight into the Rust gateway's `models/` directory.

The gateway (`crates/drgtw-ner`) loads, per model directory:

```
model.onnx        # (or model_quantized.onnx) token-classification graph
tokenizer.json    # HF fast-tokenizer file
config.json       # HF config with id2label (BIO labels)
```

It maps `PER → Person`, `ORG → Org`, `LOC → Location`; every other label is
ignored. This module trains exactly that label scheme (custom entity types are
allowed but the current gateway will ignore them).

---

## 0. Setup

```bash
cd training
uv sync                 # core: train + export (torch CPU, transformers, onnx…)
uv sync --extra dev     # + pytest
uv sync --extra annotate  # + presidio-analyzer + spaCy (only for `annotate`)
```

All commands below run through `uv run python -m drgtw_training <subcommand>`.

---

## 1. Data format

JSONL, one object per line. Offsets are **character** offsets (Python
`text[start:end]`, end-exclusive) — *not* byte offsets:

```json
{"text": "Schreib eine Mail an Max Mustermann from Example Corp in München.",
 "entities": [
   {"start": 21, "end": 35, "label": "PER"},
   {"start": 44, "end": 51, "label": "ORG"},
   {"start": 55, "end": 62, "label": "LOC"}
 ]}
```

Labels: `PER`, `ORG`, `LOC` (custom types pass through but are ignored by the
gateway). Validation rejects: missing/typed-wrong fields, offsets out of range,
`start >= end`, and any overlapping spans.

---

## 2. Getting samples

You need labelled JSONL. Three ways to get there (combine freely — they all
emit the same format and can be concatenated):

### a) Synthetic data (`synth`)

Deterministic Faker-based generation (de_DE + en_US) mixing names / companies /
cities into realistic German and English sentences with correct offsets:

```bash
uv run python -m drgtw_training synth -n 500 --seed 0 --out data/synth.jsonl
```

Same `(n, seed)` always produces identical output. Good for bootstrapping and
augmenting a small hand-labelled set.

### b) Pre-annotate raw text (`annotate`, optional extra)

Run [Presidio](https://microsoft.github.io/presidio/) over raw text lines to get
a *draft* annotation for a human to correct. Requires the `annotate` extra and a
spaCy model:

```bash
uv sync --extra annotate
uv run python -m spacy download en_core_web_lg

# one raw sentence per line in raw.txt
uv run python -m drgtw_training annotate \
    --input raw.txt --out data/draft.jsonl \
    --spacy-model en_core_web_lg
```

Presidio `PERSON → PER`, `ORG → ORG`, `LOCATION/GPE → LOC`; other detections are
dropped. **Review `data/draft.jsonl` and fix mistakes before training** — this
is a labelling aid, not a labelled set.

### c) Hand-labelled

Write the JSONL by hand or from your own export. It is validated on load.

---

## 3. Train (`train`)

```bash
uv run python -m drgtw_training train \
    --data data/synth.jsonl \
    --out out/model \
    --base-model distilbert-base-multilingual-cased \
    --epochs 3 --lr 5e-5 --batch-size 16 \
    --eval-fraction 0.2 --seed 0
```

* Base model is configurable (default `distilbert-base-multilingual-cased`).
  Use a BERT-family base (e.g. `bert-base-multilingual-cased`) if you want the
  exported graph to include `token_type_ids`, matching the stock
  `models/ner-multilingual` artifact.
* Tokenizes with offset mapping and aligns char-offset entities to BIO token
  labels: the **first subtoken** of an entity word gets `B-…`, the remaining
  subtokens are masked with `-100` (ignored by the loss).
* Splits train/eval deterministically; writes the fine-tuned HF model +
  tokenizer to `--out`, plus `eval_report.json` (seqeval per-entity-type
  precision/recall/F1).

### Evaluate an existing model (`eval`)

```bash
uv run python -m drgtw_training eval \
    --model-dir out/model --data data/holdout.jsonl
```

Writes/overwrites `out/model/eval_report.json` and prints the headline metrics.

---

## 4. Export to gateway layout (`export`)

```bash
uv run python -m drgtw_training export \
    --model-dir out/model \
    --out out/gateway/ner-custom
```

Produces `out/gateway/ner-custom/` containing exactly:

```
model.onnx        # inputs: input_ids, attention_mask (+ token_type_ids for BERT)
tokenizer.json
config.json       # id2label: {0:O, 1:B-PER, 2:I-PER, 3:B-ORG, 4:I-ORG, 5:B-LOC, 6:I-LOC}
```

ONNX dynamic axes cover batch (dim 0) and sequence (dim 1). The gateway reads
the session's declared input names dynamically and feeds whichever of
`input_ids` / `attention_mask` / `token_type_ids` the graph wants (it sends
all-zeros `token_type_ids` when present), so both DistilBERT (2 inputs) and
BERT (3 inputs) exports work.

---

## 5. Drop into the gateway

Copy the export directory under the gateway's `models/`:

```bash
cp -r out/gateway/ner-custom /path/to/drgtw/models/ner-custom
```

Then point the gateway at it via `drgtw.toml`:

```toml
[pii.ner]
model_dir = "models/ner-custom"
# optional tuning (defaults shown where applicable):
score_threshold = 0.7      # 0.0..=1.0; spans below are dropped
fail_mode = "closed"       # "open" (default) or "closed" on inference error
timeout_ms = 3000          # per-request inference timeout, > 0
workers = 4                # NER worker threads, > 0
queue_capacity = 128       # max pending NER requests, > 0
```

Only `model_dir` is required; the rest have gateway defaults. Restart the
gateway — it will load `model.onnx` (preferring `model_quantized.onnx` if you
add a quantized variant), parse `config.json` for `id2label`, and load
`tokenizer.json`.

---

## 6. End-to-end smoke test

The acceptance pipeline (synth → fine-tune tiny model → export → onnxruntime
inference) runs in a couple of minutes on CPU:

```bash
bash scripts/smoke_e2e.sh
# or
uv run python -m drgtw_training smoke -n 200 --epochs 2
```

It fine-tunes `prajjwal1/bert-tiny`, exports the gateway layout, and runs a real
onnxruntime inference on `"Max Mustermann works at Example Corp in Munich."`,
printing the per-token label sequence and artifact sizes. The tiny model's
accuracy is intentionally poor — this proves the **pipeline and artifact
shape**, not F1.

---

## 7. Tests

```bash
uv run pytest
```

Unit tests cover data validation, synthetic-offset correctness, BIO alignment
(with a real fast tokenizer), and export config shape. They do **not** run real
training.

---

## Notes & limitations

* Offsets are character-based throughout. The gateway works in byte offsets
  internally but re-derives them from the tokenizer's own offset mapping at
  inference time, so this module never needs to emit byte offsets.
* Only the first subtoken of each entity is labelled (`B-`); subsequent
  subtokens are masked. This is the standard HF token-classification convention
  and matches how the gateway decodes BIO spans (subwords sharing a word id are
  merged).
* `annotate` (Presidio/spaCy) is an optional extra and a labelling *aid* only.
* Quantization (`model_quantized.onnx`) is not produced here; the gateway
  accepts a plain `model.onnx`. Add quantization downstream if you need it.
