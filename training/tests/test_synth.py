"""Synthetic generation tests: offsets correct + determinism."""

import pytest

from drgtw_training.data import validate_sample
from drgtw_training.synth import generate


@pytest.mark.parametrize("seed", [0, 1, 7, 42, 123])
def test_offsets_slice_to_entity_text(seed):
    samples = generate(50, seed=seed)
    assert len(samples) == 50
    for s in samples:
        # every emitted span must slice back to a non-empty substring
        for e in s.entities:
            assert s.text[e.start : e.end] != ""
            assert 0 <= e.start < e.end <= len(s.text)
            assert e.label in ("PER", "ORG", "LOC")


@pytest.mark.parametrize("seed", [0, 1, 7, 42, 123])
def test_generated_samples_pass_validation(seed):
    # The data validator rejects overlaps / out-of-range; synth must satisfy it.
    samples = generate(50, seed=seed)
    for s in samples:
        validate_sample(s.to_jsonable())  # raises on any problem


def test_deterministic_same_seed():
    a = generate(30, seed=5)
    b = generate(30, seed=5)
    assert [s.to_jsonable() for s in a] == [s.to_jsonable() for s in b]


def test_different_seed_differs():
    a = generate(30, seed=5)
    b = generate(30, seed=6)
    assert [s.to_jsonable() for s in a] != [s.to_jsonable() for s in b]


def test_covers_all_three_labels_and_both_languages():
    samples = generate(200, seed=0)
    labels = {e.label for s in samples for e in s.entities}
    assert labels == {"PER", "ORG", "LOC"}
    # German templates contain umlaut-free markers; check both langs appear by
    # looking for a distinctly-German and distinctly-English token.
    texts = " ".join(s.text for s in samples)
    assert "arbeitet" in texts or "Schreib" in texts or "Bitte" in texts
    assert "works" in texts or "Write" in texts or "Meeting" in texts


def test_multibyte_offsets_are_character_based():
    # Find a sample whose text contains a multibyte char and confirm char
    # offsets (not byte offsets) round-trip.
    samples = generate(300, seed=3)
    multibyte = [s for s in samples if any(ord(c) > 127 for c in s.text)]
    assert multibyte, "expected at least one sample with a non-ASCII char"
    for s in multibyte[:20]:
        for e in s.entities:
            sliced = s.text[e.start : e.end]
            assert sliced and sliced.strip() == sliced


def test_zero_samples():
    assert generate(0, seed=0) == []
