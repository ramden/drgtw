"""DRGTW Phase 5 — NER training/tuning module.

Public surface:
  * :mod:`drgtw_training.data`   — JSONL load/validate/split
  * :mod:`drgtw_training.synth`  — deterministic synthetic generation
  * :mod:`drgtw_training.annotate` — Presidio pre-annotation (optional extra)
  * :mod:`drgtw_training.train`  — fine-tune + seqeval report
  * :mod:`drgtw_training.export` — export to gateway ONNX layout
  * :mod:`drgtw_training.labels` — BIO label scheme
"""

from __future__ import annotations

__version__ = "0.1.0"
