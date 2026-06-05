#!/usr/bin/env bash
# Phase 5 acceptance: end-to-end pipeline proof.
# synth 200 -> fine-tune prajjwal1/bert-tiny (CPU, minutes) -> export ONNX ->
# onnxruntime inference on a fixed sentence, printing the label sequence.
#
# Usage:  bash scripts/smoke_e2e.sh
set -euo pipefail
cd "$(dirname "$0")/.."
exec uv run python -m drgtw_training smoke --workdir out/smoke -n 200 --epochs 2
