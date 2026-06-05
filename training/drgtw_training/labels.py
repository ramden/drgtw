"""Label scheme shared across the training module.

The gateway (crates/drgtw-ner) maps the BIO labels ``PER`` -> Person,
``ORG`` -> Org, ``LOC`` -> Location and *ignores* every other entity type
(it decodes them to ``Outside``). We therefore train on PER/ORG/LOC by
default but allow arbitrary custom entity types to pass through: a custom
type produces ``B-<TYPE>``/``I-<TYPE>`` labels that the gateway will simply
ignore at inference time unless its own label map is extended.

Character offsets, not byte offsets, are used everywhere in this module
(see data.py / the JSONL format). Python string slicing is character-based,
which keeps annotation and validation native and unambiguous across
multibyte (German umlaut, etc.) text.
"""

from __future__ import annotations

# Entity types the gateway understands today. Anything else is "custom
# passthrough": still trainable, but ignored by the current gateway decoder.
GATEWAY_ENTITY_TYPES: tuple[str, ...] = ("PER", "ORG", "LOC")


def build_label_list(entity_types: list[str]) -> list[str]:
    """Build the BIO label list (``O`` first, then ``B-``/``I-`` per type).

    The ordering is deterministic: ``O`` at index 0, then each entity type in
    the given order with its ``B-`` immediately before its ``I-``. This index
    ordering becomes the ``id2label`` map and must stay contiguous from 0 (the
    gateway rejects gaps).
    """
    labels = ["O"]
    for et in entity_types:
        labels.append(f"B-{et}")
        labels.append(f"I-{et}")
    return labels


def label_maps(labels: list[str]) -> tuple[dict[int, str], dict[str, int]]:
    """Return ``(id2label, label2id)`` for a BIO label list."""
    id2label = {i: lab for i, lab in enumerate(labels)}
    label2id = {lab: i for i, lab in enumerate(labels)}
    return id2label, label2id
