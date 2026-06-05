"""Validation tests for the JSONL data format (drgtw_training.data)."""

import json

import pytest

from drgtw_training.data import (
    DataError,
    Entity,
    Sample,
    parse_jsonl,
    split_train_eval,
    validate_sample,
)


def test_valid_sample_parses_and_orders_entities():
    raw = {
        "text": "Anna at ACME in Berlin",
        "entities": [
            {"start": 16, "end": 22, "label": "LOC"},
            {"start": 0, "end": 4, "label": "PER"},
        ],
    }
    s = validate_sample(raw)
    assert isinstance(s, Sample)
    # entities returned sorted by start
    assert [e.start for e in s.entities] == [0, 16]
    assert s.entities[0].text_of(s.text) == "Anna"
    assert s.entities[1].text_of(s.text) == "Berlin"


def test_missing_text_rejected():
    with pytest.raises(DataError, match="missing 'text'"):
        validate_sample({"entities": []})


def test_text_not_string_rejected():
    with pytest.raises(DataError, match="'text' must be a string"):
        validate_sample({"text": 5})


def test_offset_out_of_range_rejected():
    raw = {"text": "short", "entities": [{"start": 0, "end": 99, "label": "PER"}]}
    with pytest.raises(DataError, match="out of range"):
        validate_sample(raw)


def test_negative_start_rejected():
    raw = {"text": "short", "entities": [{"start": -1, "end": 3, "label": "PER"}]}
    with pytest.raises(DataError, match="out of range"):
        validate_sample(raw)


def test_start_ge_end_rejected():
    raw = {"text": "short", "entities": [{"start": 3, "end": 3, "label": "PER"}]}
    with pytest.raises(DataError, match="start >= end"):
        validate_sample(raw)


def test_overlapping_entities_rejected():
    raw = {
        "text": "Anna Smith works",
        "entities": [
            {"start": 0, "end": 10, "label": "PER"},
            {"start": 5, "end": 10, "label": "PER"},
        ],
    }
    with pytest.raises(DataError, match="overlapping"):
        validate_sample(raw)


def test_adjacent_entities_allowed():
    # half-open ranges [0,4) and [4,8) touch but do NOT overlap
    raw = {
        "text": "AnnaACME here",
        "entities": [
            {"start": 0, "end": 4, "label": "PER"},
            {"start": 4, "end": 8, "label": "ORG"},
        ],
    }
    s = validate_sample(raw)
    assert len(s.entities) == 2


def test_bad_label_type_rejected():
    raw = {"text": "hi", "entities": [{"start": 0, "end": 2, "label": 5}]}
    with pytest.raises(DataError, match="label"):
        validate_sample(raw)


def test_bool_offset_rejected():
    # bools are ints in Python; format requires real ints
    raw = {"text": "hi there", "entities": [{"start": True, "end": 2, "label": "PER"}]}
    with pytest.raises(DataError, match="must be int"):
        validate_sample(raw)


def test_parse_jsonl_skips_blank_lines():
    lines = [
        json.dumps({"text": "a Max b", "entities": [{"start": 2, "end": 5, "label": "PER"}]}),
        "",
        "   ",
        json.dumps({"text": "no ents", "entities": []}),
    ]
    samples = parse_jsonl(lines)
    assert len(samples) == 2


def test_parse_jsonl_reports_line_number_on_bad_json():
    lines = ['{"text": "ok", "entities": []}', "{not json"]
    with pytest.raises(DataError, match="line 2"):
        parse_jsonl(lines)


def test_split_train_eval_deterministic_and_nonempty():
    samples = [Sample(text=f"s{i}", entities=[]) for i in range(10)]
    tr1, ev1 = split_train_eval(samples, eval_fraction=0.2, seed=1)
    tr2, ev2 = split_train_eval(samples, eval_fraction=0.2, seed=1)
    assert [s.text for s in tr1] == [s.text for s in tr2]
    assert [s.text for s in ev1] == [s.text for s in ev2]
    assert len(tr1) + len(ev1) == 10
    assert len(ev1) == 2
    assert len(tr1) == 8


def test_split_train_eval_different_seed_differs():
    samples = [Sample(text=f"s{i}", entities=[]) for i in range(20)]
    _, ev1 = split_train_eval(samples, eval_fraction=0.25, seed=1)
    _, ev2 = split_train_eval(samples, eval_fraction=0.25, seed=2)
    assert [s.text for s in ev1] != [s.text for s in ev2]
