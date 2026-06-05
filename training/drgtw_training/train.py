"""Fine-tune a HF token-classification model and evaluate with seqeval.

Pipeline:
  1. Load + validate JSONL (data.py), split train/eval.
  2. Tokenize with a *fast* tokenizer using ``return_offsets_mapping=True``.
  3. Align character-offset entities to BIO token labels: the first subtoken
     of an entity word gets ``B-``/``I-``, every following subtoken of the
     same span is set to ``-100`` (ignored by the loss). Special tokens and
     non-entity tokens get ``O`` (or ``-100`` for specials).
  4. Train with the HF ``Trainer``.
  5. Evaluate with seqeval -> per-entity-type precision/recall/F1, saved JSON.

The BIO-alignment function :func:`align_labels` is pure and tokenizer-driven
so it can be unit-tested without any training.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path

from .data import Sample, load_jsonl, split_train_eval
from .labels import GATEWAY_ENTITY_TYPES, build_label_list, label_maps

IGNORE_INDEX = -100


@dataclass
class TrainConfig:
    base_model: str = "distilbert-base-multilingual-cased"
    output_dir: str = "out/model"
    epochs: float = 3.0
    learning_rate: float = 5e-5
    train_batch_size: int = 16
    eval_batch_size: int = 32
    max_length: int = 256
    eval_fraction: float = 0.2
    seed: int = 0
    entity_types: list[str] = field(default_factory=lambda: list(GATEWAY_ENTITY_TYPES))


def align_labels(
    text: str,
    entities,  # list[Entity]
    offset_mapping: list[tuple[int, int]],
    label2id: dict[str, int],
) -> list[int]:
    """Map char-offset entities onto per-token BIO label ids.

    Args:
        text: original text (unused for logic, kept for clarity/debugging).
        entities: validated, non-overlapping entities (char offsets).
        offset_mapping: per-token ``(char_start, char_end)`` from a fast
            tokenizer. Special tokens are ``(0, 0)``.
        label2id: BIO-label -> id map.

    Returns a label id per token. Rules:
      * special tokens (offset ``(0, 0)``) -> ``IGNORE_INDEX``
      * a token whose char span starts at the entity start -> ``B-<label>``
      * a later subtoken inside the same entity -> ``IGNORE_INDEX``
        (label only the first subtoken, ignore the rest)
      * a token fully outside any entity -> ``O``
    """
    o_id = label2id["O"]
    # Sort entities by start for a simple sweep.
    ents = sorted(entities, key=lambda e: e.start)
    labels: list[int] = []
    for (tok_start, tok_end) in offset_mapping:
        if tok_start == 0 and tok_end == 0:
            labels.append(IGNORE_INDEX)
            continue
        # Find an entity overlapping this token.
        matched = None
        for e in ents:
            if tok_start < e.end and e.start < tok_end:  # half-open overlap
                matched = e
                break
        if matched is None:
            labels.append(o_id)
            continue
        # The first subtoken of the entity (token starts at/at-or-before the
        # entity start AND covers it) gets B-; subsequent subtokens are ignored.
        if tok_start <= matched.start < tok_end:
            b_label = f"B-{matched.label}"
            labels.append(label2id.get(b_label, o_id))
        else:
            labels.append(IGNORE_INDEX)
    return labels


def _tokenize_and_align(samples: list[Sample], tokenizer, label2id, max_length):
    encodings = tokenizer(
        [s.text for s in samples],
        truncation=True,
        max_length=max_length,
        return_offsets_mapping=True,
    )
    all_labels = []
    for i, sample in enumerate(samples):
        offsets = encodings["offset_mapping"][i]
        all_labels.append(align_labels(sample.text, sample.entities, offsets, label2id))
    encodings.pop("offset_mapping")
    encodings["labels"] = all_labels
    return encodings


def _seqeval_report(predictions, label_ids, id2label) -> dict:
    """Build a seqeval per-entity report from raw logits + label ids."""
    import numpy as np
    from seqeval.metrics import classification_report, f1_score, precision_score, recall_score

    preds = np.argmax(predictions, axis=2)
    true_labels: list[list[str]] = []
    pred_labels: list[list[str]] = []
    for pred_row, label_row in zip(preds, label_ids):
        t, p = [], []
        for p_id, l_id in zip(pred_row, label_row):
            if l_id == IGNORE_INDEX:
                continue
            t.append(id2label[int(l_id)])
            p.append(id2label[int(p_id)])
        true_labels.append(t)
        pred_labels.append(p)
    report = classification_report(
        true_labels, pred_labels, output_dict=True, zero_division=0
    )

    def _jsonable(obj):
        # seqeval's per-type dict carries numpy scalars (support counts etc.).
        if isinstance(obj, dict):
            return {k: _jsonable(v) for k, v in obj.items()}
        if isinstance(obj, np.generic):
            return obj.item()
        return obj

    return {
        "precision": float(precision_score(true_labels, pred_labels, zero_division=0)),
        "recall": float(recall_score(true_labels, pred_labels, zero_division=0)),
        "f1": float(f1_score(true_labels, pred_labels, zero_division=0)),
        "per_type": _jsonable(report),
    }


def train(data_path: str | Path, config: TrainConfig) -> dict:
    """Fine-tune and evaluate. Returns the eval report dict (also saved JSON).

    Heavy ML imports are deferred to call time so importing this module is
    cheap (tests import :func:`align_labels` without pulling in transformers).
    """
    import numpy as np  # noqa: F401  (used by closures)
    from transformers import (
        AutoModelForTokenClassification,
        AutoTokenizer,
        DataCollatorForTokenClassification,
        Trainer,
        TrainingArguments,
    )
    from datasets import Dataset

    labels = build_label_list(config.entity_types)
    id2label, label2id = label_maps(labels)

    samples = load_jsonl(data_path)
    train_s, eval_s = split_train_eval(
        samples, eval_fraction=config.eval_fraction, seed=config.seed
    )

    tokenizer = AutoTokenizer.from_pretrained(config.base_model, use_fast=True)
    if not tokenizer.is_fast:
        raise RuntimeError(
            f"{config.base_model} has no fast tokenizer; offset mapping requires one"
        )

    train_enc = _tokenize_and_align(train_s, tokenizer, label2id, config.max_length)
    eval_enc = _tokenize_and_align(eval_s, tokenizer, label2id, config.max_length)
    train_ds = Dataset.from_dict(train_enc)
    eval_ds = Dataset.from_dict(eval_enc)

    model = AutoModelForTokenClassification.from_pretrained(
        config.base_model,
        num_labels=len(labels),
        id2label=id2label,
        label2id=label2id,
    )

    collator = DataCollatorForTokenClassification(tokenizer)

    out_dir = Path(config.output_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    args = TrainingArguments(
        output_dir=str(out_dir / "trainer"),
        num_train_epochs=config.epochs,
        learning_rate=config.learning_rate,
        per_device_train_batch_size=config.train_batch_size,
        per_device_eval_batch_size=config.eval_batch_size,
        seed=config.seed,
        logging_steps=10,
        report_to=[],
        save_strategy="no",
    )

    def compute_metrics(eval_pred):
        predictions, label_ids = eval_pred
        rep = _seqeval_report(predictions, label_ids, id2label)
        return {"precision": rep["precision"], "recall": rep["recall"], "f1": rep["f1"]}

    trainer = Trainer(
        model=model,
        args=args,
        train_dataset=train_ds,
        eval_dataset=eval_ds,
        data_collator=collator,
        compute_metrics=compute_metrics,
    )
    trainer.train()

    # Full per-type report from raw predictions.
    pred_out = trainer.predict(eval_ds)
    report = _seqeval_report(pred_out.predictions, pred_out.label_ids, id2label)

    # Save the fine-tuned model + tokenizer (HF native) for export.py to consume.
    model.save_pretrained(str(out_dir))
    tokenizer.save_pretrained(str(out_dir))

    report_path = out_dir / "eval_report.json"
    report_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    return report


def evaluate(model_dir: str | Path, data_path: str | Path, max_length: int = 256) -> dict:
    """Evaluate an already-trained HF model dir on a dataset; save + return report."""
    from transformers import AutoModelForTokenClassification, AutoTokenizer, Trainer
    from transformers import DataCollatorForTokenClassification
    from datasets import Dataset

    model_dir = Path(model_dir)
    tokenizer = AutoTokenizer.from_pretrained(str(model_dir), use_fast=True)
    model = AutoModelForTokenClassification.from_pretrained(str(model_dir))
    id2label = {int(k): v for k, v in model.config.id2label.items()}
    label2id = {v: k for k, v in id2label.items()}

    samples = load_jsonl(data_path)
    enc = _tokenize_and_align(samples, tokenizer, label2id, max_length)
    ds = Dataset.from_dict(enc)

    trainer = Trainer(
        model=model, data_collator=DataCollatorForTokenClassification(tokenizer)
    )
    pred_out = trainer.predict(ds)
    report = _seqeval_report(pred_out.predictions, pred_out.label_ids, id2label)
    (model_dir / "eval_report.json").write_text(
        json.dumps(report, indent=2), encoding="utf-8"
    )
    return report
