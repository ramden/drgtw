"""Load and validate the training data format; split into train/eval.

Data format — JSONL, one JSON object per line::

    {"text": "Schreib an Max Mustermann.",
     "entities": [{"start": 11, "end": 25, "label": "PER"}]}

* ``start`` / ``end`` are **CHARACTER** offsets into ``text`` (Python-native
  ``text[start:end]`` slicing), end-exclusive.
* ``label`` is an entity type (PER/ORG/LOC, or a custom passthrough type).

Validation rejects: missing fields, wrong types, offsets out of range,
``start >= end``, and any pair of overlapping entity spans (the BIO tagger
needs disjoint spans).
"""

from __future__ import annotations

import json
import random
from dataclasses import dataclass, field
from pathlib import Path


class DataError(ValueError):
    """Raised when a sample or the dataset fails validation."""


@dataclass(frozen=True)
class Entity:
    start: int
    end: int
    label: str

    def text_of(self, text: str) -> str:
        return text[self.start : self.end]


@dataclass
class Sample:
    text: str
    entities: list[Entity] = field(default_factory=list)

    def to_jsonable(self) -> dict:
        return {
            "text": self.text,
            "entities": [
                {"start": e.start, "end": e.end, "label": e.label}
                for e in self.entities
            ],
        }


def _validate_entity(raw: object, idx: int, line_no: int) -> Entity:
    if not isinstance(raw, dict):
        raise DataError(f"line {line_no}: entity {idx} is not an object")
    for key in ("start", "end", "label"):
        if key not in raw:
            raise DataError(f"line {line_no}: entity {idx} missing '{key}'")
    start, end, label = raw["start"], raw["end"], raw["label"]
    if not isinstance(start, int) or isinstance(start, bool):
        raise DataError(f"line {line_no}: entity {idx} 'start' must be int")
    if not isinstance(end, int) or isinstance(end, bool):
        raise DataError(f"line {line_no}: entity {idx} 'end' must be int")
    if not isinstance(label, str) or not label:
        raise DataError(f"line {line_no}: entity {idx} 'label' must be a non-empty string")
    return Entity(start=start, end=end, label=label)


def validate_sample(raw: dict, line_no: int = 0) -> Sample:
    """Validate one raw object into a :class:`Sample`. Raises :class:`DataError`."""
    if not isinstance(raw, dict):
        raise DataError(f"line {line_no}: top-level value is not an object")
    if "text" not in raw:
        raise DataError(f"line {line_no}: missing 'text'")
    text = raw["text"]
    if not isinstance(text, str):
        raise DataError(f"line {line_no}: 'text' must be a string")

    raw_entities = raw.get("entities", [])
    if not isinstance(raw_entities, list):
        raise DataError(f"line {line_no}: 'entities' must be a list")

    n = len(text)
    entities: list[Entity] = []
    for idx, raw_ent in enumerate(raw_entities):
        ent = _validate_entity(raw_ent, idx, line_no)
        if ent.start < 0 or ent.end > n:
            raise DataError(
                f"line {line_no}: entity {idx} offsets [{ent.start},{ent.end}] "
                f"out of range for text of length {n}"
            )
        if ent.start >= ent.end:
            raise DataError(
                f"line {line_no}: entity {idx} has start >= end "
                f"({ent.start} >= {ent.end})"
            )
        entities.append(ent)

    # Reject overlaps. Sort by start, then check adjacency.
    ordered = sorted(entities, key=lambda e: (e.start, e.end))
    for prev, cur in zip(ordered, ordered[1:]):
        if cur.start < prev.end:  # half-open ranges overlap iff cur.start < prev.end
            raise DataError(
                f"line {line_no}: overlapping entities "
                f"[{prev.start},{prev.end}] and [{cur.start},{cur.end}]"
            )

    return Sample(text=text, entities=ordered)


def parse_jsonl(lines: list[str]) -> list[Sample]:
    """Parse + validate a list of JSONL lines (blank lines skipped)."""
    samples: list[Sample] = []
    for i, line in enumerate(lines, start=1):
        if not line.strip():
            continue
        try:
            raw = json.loads(line)
        except json.JSONDecodeError as exc:
            raise DataError(f"line {i}: invalid JSON: {exc}") from exc
        samples.append(validate_sample(raw, line_no=i))
    return samples


def load_jsonl(path: str | Path) -> list[Sample]:
    """Load and validate a JSONL dataset file."""
    text = Path(path).read_text(encoding="utf-8")
    return parse_jsonl(text.splitlines())


def write_jsonl(path: str | Path, samples: list[Sample]) -> None:
    """Write samples as JSONL (UTF-8, one compact object per line)."""
    p = Path(path)
    p.parent.mkdir(parents=True, exist_ok=True)
    with p.open("w", encoding="utf-8") as fh:
        for s in samples:
            fh.write(json.dumps(s.to_jsonable(), ensure_ascii=False) + "\n")


def split_train_eval(
    samples: list[Sample], eval_fraction: float = 0.2, seed: int = 0
) -> tuple[list[Sample], list[Sample]]:
    """Deterministically shuffle + split into (train, eval).

    With a non-empty dataset and ``0 < eval_fraction < 1`` both splits are
    guaranteed non-empty (eval gets at least 1 sample, train keeps at least 1).
    """
    if not 0.0 <= eval_fraction < 1.0:
        raise DataError("eval_fraction must be in [0.0, 1.0)")
    if not samples:
        return [], []
    shuffled = list(samples)
    random.Random(seed).shuffle(shuffled)
    n_eval = int(round(len(shuffled) * eval_fraction))
    if eval_fraction > 0.0:
        n_eval = max(1, n_eval)
    n_eval = min(n_eval, len(shuffled) - 1) if len(shuffled) > 1 else 0
    eval_split = shuffled[:n_eval]
    train_split = shuffled[n_eval:]
    return train_split, eval_split
