"""BIO token-label alignment tests.

The first set uses a hand-built offset mapping (no tokenizer needed, always
runs). The second set uses a real fast tokenizer (prajjwal1/bert-tiny, a small
download) to confirm alignment against actual subword splits; it skips if the
tokenizer cannot be loaded (e.g. offline).
"""

import pytest

from drgtw_training.data import Entity
from drgtw_training.labels import build_label_list, label_maps
from drgtw_training.train import IGNORE_INDEX, align_labels

LABELS = build_label_list(["PER", "ORG", "LOC"])
ID2LABEL, LABEL2ID = label_maps(LABELS)


def _ids_to_labels(ids):
    return [IGNORE_INDEX if i == IGNORE_INDEX else ID2LABEL[i] for i in ids]


def test_specials_ignored_others_O():
    text = "hello world"
    offsets = [(0, 0), (0, 5), (6, 11), (0, 0)]  # [CLS] hello world [SEP]
    ids = align_labels(text, [], offsets, LABEL2ID)
    assert _ids_to_labels(ids) == [IGNORE_INDEX, "O", "O", IGNORE_INDEX]


def test_single_word_entity_gets_B():
    text = "Max works"
    offsets = [(0, 0), (0, 3), (4, 9), (0, 0)]
    ents = [Entity(start=0, end=3, label="PER")]
    ids = align_labels(text, ents, offsets, LABEL2ID)
    assert _ids_to_labels(ids) == [IGNORE_INDEX, "B-PER", "O", IGNORE_INDEX]


def test_subwords_first_labeled_rest_ignored():
    # "Mustermann" split into 3 subtokens [0,4)[4,8)[8,10): first->B-PER, rest->ignore
    text = "Mustermann"
    offsets = [(0, 0), (0, 4), (4, 8), (8, 10), (0, 0)]
    ents = [Entity(start=0, end=10, label="PER")]
    ids = align_labels(text, ents, offsets, LABEL2ID)
    assert _ids_to_labels(ids) == [
        IGNORE_INDEX,
        "B-PER",
        IGNORE_INDEX,
        IGNORE_INDEX,
        IGNORE_INDEX,
    ]


def test_two_entities_distinct_labels():
    text = "ACME Berlin"
    offsets = [(0, 0), (0, 4), (5, 11), (0, 0)]
    ents = [
        Entity(start=0, end=4, label="ORG"),
        Entity(start=5, end=11, label="LOC"),
    ]
    ids = align_labels(text, ents, offsets, LABEL2ID)
    assert _ids_to_labels(ids) == [IGNORE_INDEX, "B-ORG", "B-LOC", IGNORE_INDEX]


def test_custom_label_falls_back_to_O_when_not_in_map():
    # A label not present in label2id (custom passthrough not configured) -> O
    text = "Foo"
    offsets = [(0, 0), (0, 3), (0, 0)]
    ents = [Entity(start=0, end=3, label="MISC")]
    ids = align_labels(text, ents, offsets, LABEL2ID)
    assert _ids_to_labels(ids) == [IGNORE_INDEX, "O", IGNORE_INDEX]


# --- real fast tokenizer (small download; skip if unavailable) ---

TINY = "prajjwal1/bert-tiny"


@pytest.fixture(scope="module")
def tokenizer():
    try:
        from transformers import AutoTokenizer

        return AutoTokenizer.from_pretrained(TINY, use_fast=True)
    except Exception as exc:  # pragma: no cover - network/offline path
        pytest.skip(f"cannot load {TINY}: {exc}")


def test_real_tokenizer_alignment(tokenizer):
    text = "Max Mustermann works at Example Corp in Berlin"
    # entity spans (char offsets)
    ents = [
        Entity(start=0, end=14, label="PER"),    # "Max Mustermann"
        Entity(start=24, end=36, label="ORG"),   # "Example Corp"
        Entity(start=40, end=46, label="LOC"),   # "Berlin"
    ]
    assert text[0:14] == "Max Mustermann"
    assert text[24:36] == "Example Corp"
    assert text[40:46] == "Berlin"

    enc = tokenizer(text, return_offsets_mapping=True)
    offsets = enc["offset_mapping"]
    ids = align_labels(text, ents, offsets, LABEL2ID)
    labels = _ids_to_labels(ids)

    # Exactly one B- per entity; no I- emitted (we only label first subtoken).
    assert labels.count("B-PER") == 1
    assert labels.count("B-ORG") == 1
    assert labels.count("B-LOC") == 1
    assert all(not lab.startswith("I-") for lab in labels if isinstance(lab, str))
    # Same number of labels as tokens.
    assert len(labels) == len(offsets)
