"""Export a trained HF token-classification model to the gateway layout.

Output directory (drop into the gateway's ``models/<name>/``)::

    model.onnx        # token-classification graph
    tokenizer.json    # fast-tokenizer file
    config.json       # HF config incl. id2label (BIO labels)

ONNX graph:
  * inputs: ``input_ids``, ``attention_mask`` (+ ``token_type_ids`` for
    BERT-family bases — ``type_vocab_size > 1``). The gateway reads the
    session's declared input names dynamically and feeds whichever of those
    three it finds, so including token_type_ids for BERT matches what the
    gateway sends (all-zeros) and omitting it for DistilBERT is also fine.
  * dynamic axes: batch (dim 0) and sequence (dim 1) on every input + output.
  * output: ``logits`` shape ``[batch, seq, num_labels]``.

``config.json`` is written via ``model.config.to_json_file`` so it carries the
HF ``id2label`` / ``label2id`` the gateway parses.
"""

from __future__ import annotations

import json
import shutil
from pathlib import Path

# BERT-family model_types that consume token_type_ids.
_TOKEN_TYPE_MODELS = {"bert", "albert", "electra", "roberta-bert"}


def export_model(model_dir: str | Path, out_dir: str | Path, opset: int = 14) -> dict:
    """Export the HF model at ``model_dir`` into gateway layout at ``out_dir``.

    Returns a small manifest dict of the written files + their sizes.
    """
    import torch
    from transformers import AutoModelForTokenClassification, AutoTokenizer

    model_dir = Path(model_dir)
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    tokenizer = AutoTokenizer.from_pretrained(str(model_dir), use_fast=True)
    model = AutoModelForTokenClassification.from_pretrained(str(model_dir))
    model.eval()

    model_type = getattr(model.config, "model_type", "")
    uses_token_type = (
        model_type in _TOKEN_TYPE_MODELS
        and getattr(model.config, "type_vocab_size", 1) > 1
    )

    # Build a dummy batch (batch=1, seq=8).
    seq = 8
    input_ids = torch.ones(1, seq, dtype=torch.long)
    attention_mask = torch.ones(1, seq, dtype=torch.long)

    input_names = ["input_ids", "attention_mask"]
    dynamic_axes = {
        "input_ids": {0: "batch", 1: "seq"},
        "attention_mask": {0: "batch", 1: "seq"},
        "logits": {0: "batch", 1: "seq"},
    }

    if uses_token_type:
        token_type_ids = torch.zeros(1, seq, dtype=torch.long)
        args = (input_ids, attention_mask, token_type_ids)
        input_names.append("token_type_ids")
        dynamic_axes["token_type_ids"] = {0: "batch", 1: "seq"}
    else:
        args = (input_ids, attention_mask)

    onnx_path = out_dir / "model.onnx"

    # Wrap so forward() takes positional tensors in our declared order and
    # returns just the logits tensor (named "logits" in the graph).
    class _Wrapper(torch.nn.Module):
        def __init__(self, m, with_tt):
            super().__init__()
            self.m = m
            self.with_tt = with_tt

        def forward(self, input_ids, attention_mask, token_type_ids=None):
            if self.with_tt:
                out = self.m(
                    input_ids=input_ids,
                    attention_mask=attention_mask,
                    token_type_ids=token_type_ids,
                )
            else:
                out = self.m(input_ids=input_ids, attention_mask=attention_mask)
            return out.logits

    wrapper = _Wrapper(model, uses_token_type)
    wrapper.eval()

    # Force the legacy TorchScript exporter (dynamo=False): it emits the static
    # named-input graph the gateway loads, and avoids the optional `onnxscript`
    # dependency that the torch>=2.5 dynamo default pulls in.
    with torch.no_grad():
        torch.onnx.export(
            wrapper,
            args,
            str(onnx_path),
            input_names=input_names,
            output_names=["logits"],
            dynamic_axes=dynamic_axes,
            opset_version=opset,
            do_constant_folding=True,
            dynamo=False,
        )

    # tokenizer.json (fast tokenizer file).
    tokenizer.save_pretrained(str(out_dir))
    # save_pretrained writes several files; the gateway needs tokenizer.json.
    if not (out_dir / "tokenizer.json").is_file():
        # Some tokenizers only write vocab files; force a fast-tokenizer dump.
        tokenizer.backend_tokenizer.save(str(out_dir / "tokenizer.json"))

    # config.json with id2label / label2id.
    model.config.to_json_file(str(out_dir / "config.json"))

    # Prune extraneous tokenizer artifacts the gateway does not read, keeping
    # the output dir clean (model.onnx + tokenizer.json + config.json).
    keep = {"model.onnx", "tokenizer.json", "config.json"}
    for child in out_dir.iterdir():
        if child.is_file() and child.name not in keep:
            child.unlink()

    manifest = write_manifest(out_dir, input_names)
    return manifest


def write_manifest(out_dir: Path, input_names: list[str]) -> dict:
    """Build (and return) a manifest of the exported artifacts."""
    files = {}
    for name in ("model.onnx", "tokenizer.json", "config.json"):
        p = out_dir / name
        files[name] = p.stat().st_size if p.is_file() else None
    cfg = json.loads((out_dir / "config.json").read_text(encoding="utf-8"))
    manifest = {
        "out_dir": str(out_dir),
        "files": files,
        "onnx_inputs": input_names,
        "id2label": cfg.get("id2label"),
    }
    return manifest
