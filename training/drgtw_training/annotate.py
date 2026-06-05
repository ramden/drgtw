"""Pre-annotate raw text lines with Presidio for human correction.

Presidio + spaCy are **optional** (the ``annotate`` extra). They are imported
lazily inside :func:`annotate_lines` so that the core train/export path never
requires them. If they are missing, a clear :class:`AnnotateUnavailable` is
raised telling the user how to install the extra.

Presidio entity types are mapped to the gateway scheme:

    PERSON   -> PER
    ORG      -> ORG   (only emitted by some spaCy models)
    LOCATION -> LOC

Other Presidio entities (EMAIL_ADDRESS, DATE_TIME, ...) are dropped here —
NER training in this module targets PER/ORG/LOC; emails etc. are handled by
the gateway's regex layer, not the model.

Output is JSONL (the same format as :mod:`drgtw_training.data`) with
**character** offsets, ready for a human to correct before training.
"""

from __future__ import annotations

from .data import Entity, Sample

# Presidio entity type -> gateway label.
PRESIDIO_TO_GATEWAY = {
    "PERSON": "PER",
    "ORG": "ORG",
    "ORGANIZATION": "ORG",
    "NRP": "ORG",  # spaCy sometimes surfaces orgs/groups as NRP; best-effort
    "LOCATION": "LOC",
    "GPE": "LOC",
    "LOC": "LOC",
}

_DEFAULT_PRESIDIO_ENTITIES = ["PERSON", "ORG", "ORGANIZATION", "LOCATION", "NRP", "GPE", "LOC"]


class AnnotateUnavailable(RuntimeError):
    """Raised when the optional presidio/spacy dependencies are not installed."""


def _build_analyzer(spacy_model: str):
    """Construct a Presidio AnalyzerEngine backed by ``spacy_model``.

    Imports happen here so the module is importable without presidio/spacy.
    """
    try:
        from presidio_analyzer import AnalyzerEngine
        from presidio_analyzer.nlp_engine import NlpEngineProvider
    except ImportError as exc:  # pragma: no cover - exercised only without extra
        raise AnnotateUnavailable(
            "presidio-analyzer / spaCy not installed. Install the extra:\n"
            "  uv sync --extra annotate\n"
            f"  uv run python -m spacy download {spacy_model}"
        ) from exc

    nlp_config = {
        "nlp_engine_name": "spacy",
        "models": [{"lang_code": "en", "model_name": spacy_model}],
    }
    provider = NlpEngineProvider(nlp_configuration=nlp_config)
    nlp_engine = provider.create_engine()
    return AnalyzerEngine(nlp_engine=nlp_engine, supported_languages=["en"])


def annotate_lines(
    lines: list[str],
    spacy_model: str = "en_core_web_lg",
    language: str = "en",
    score_threshold: float = 0.35,
) -> list[Sample]:
    """Pre-annotate each non-empty line into a :class:`Sample`.

    Presidio offsets are already character offsets, so they map directly onto
    our format. Overlapping/contained results from Presidio are resolved by
    keeping the higher-scoring span (the data validator would otherwise reject
    overlaps); ties keep the longer span.
    """
    analyzer = _build_analyzer(spacy_model)

    samples: list[Sample] = []
    for line in lines:
        if not line.strip():
            continue
        results = analyzer.analyze(
            text=line, language=language, score_threshold=score_threshold
        )
        # Map + filter to gateway labels.
        mapped = [
            (r.start, r.end, PRESIDIO_TO_GATEWAY[r.entity_type], r.score)
            for r in results
            if r.entity_type in PRESIDIO_TO_GATEWAY
        ]
        # Resolve overlaps: prefer higher score, then longer span.
        mapped.sort(key=lambda t: (-t[3], -(t[1] - t[0])))
        kept: list[tuple[int, int, str]] = []
        for start, end, label, _score in mapped:
            if any(start < ke and ks < end for ks, ke, _ in kept):
                continue
            kept.append((start, end, label))
        entities = [Entity(start=s, end=e, label=lab) for s, e, lab in kept]
        entities.sort(key=lambda e: e.start)
        samples.append(Sample(text=line, entities=entities))
    return samples
