"""End-to-end pipeline proof (Phase 5 acceptance).

Runs the whole chain on a *tiny* model so it finishes in minutes on CPU:

    1. generate N synthetic samples
    2. fine-tune ``prajjwal1/bert-tiny`` for a couple of epochs
    3. export the gateway ONNX layout (model.onnx + tokenizer.json + config.json)
    4. load the exported ONNX with onnxruntime and run a real inference on a
       fixed sentence, printing the decoded per-token label sequence

Acceptance is that the PIPELINE runs end-to-end and the artifacts have the
correct shape — NOT that the tiny model achieves good F1 (it will not).
"""

from __future__ import annotations

from pathlib import Path

SMOKE_MODEL = "prajjwal1/bert-tiny"
SMOKE_SENTENCE = "Max Mustermann works at Example Corp in Munich."


def run_smoke(workdir: str = "out/smoke", n: int = 200, epochs: float = 2.0) -> int:
    import json

    import numpy as np
    import onnxruntime as ort
    from transformers import AutoTokenizer

    from .data import write_jsonl
    from .export import export_model
    from .synth import generate
    from .train import TrainConfig, train

    work = Path(workdir)
    work.mkdir(parents=True, exist_ok=True)
    data_path = work / "synth.jsonl"
    model_dir = work / "model"
    export_dir = work / "gateway"

    print(f"[smoke] 1/4 generating {n} synthetic samples")
    samples = generate(n, seed=0)
    write_jsonl(data_path, samples)

    print(f"[smoke] 2/4 fine-tuning {SMOKE_MODEL} ({epochs} epochs, CPU)")
    cfg = TrainConfig(
        base_model=SMOKE_MODEL,
        output_dir=str(model_dir),
        epochs=epochs,
        train_batch_size=16,
        max_length=64,
        seed=0,
    )
    report = train(data_path, cfg)
    print(f"[smoke]     eval f1={report['f1']:.4f} "
          f"precision={report['precision']:.4f} recall={report['recall']:.4f} "
          f"(tiny model — low F1 is expected)")

    print(f"[smoke] 3/4 exporting gateway ONNX artifacts -> {export_dir}")
    manifest = export_model(model_dir, export_dir)
    print("[smoke]     artifact sizes:")
    for name, size in manifest["files"].items():
        print(f"[smoke]       {name}: {size} bytes")
    print(f"[smoke]     onnx inputs: {manifest['onnx_inputs']}")

    print("[smoke] 4/4 onnxruntime inference on the exported artifact")
    tokenizer = AutoTokenizer.from_pretrained(str(export_dir), use_fast=True)
    config = json.loads((export_dir / "config.json").read_text(encoding="utf-8"))
    id2label = {int(k): v for k, v in config["id2label"].items()}

    enc = tokenizer(SMOKE_SENTENCE, return_tensors="np")
    sess = ort.InferenceSession(str(export_dir / "model.onnx"))
    declared = {i.name for i in sess.get_inputs()}
    feeds = {}
    for name in ("input_ids", "attention_mask", "token_type_ids"):
        if name in declared:
            if name in enc:
                feeds[name] = enc[name].astype(np.int64)
            else:  # gateway feeds all-zeros token_type_ids when the graph wants it
                feeds[name] = np.zeros_like(enc["input_ids"]).astype(np.int64)
    logits = sess.run(None, feeds)[0]  # [1, seq, num_labels]
    pred_ids = logits.argmax(axis=-1)[0]
    tokens = tokenizer.convert_ids_to_tokens(enc["input_ids"][0])
    labels = [id2label[int(i)] for i in pred_ids]

    print(f"[smoke]     sentence: {SMOKE_SENTENCE!r}")
    print(f"[smoke]     logits shape: {tuple(logits.shape)} (num_labels={len(id2label)})")
    print("[smoke]     token -> label:")
    for tok, lab in zip(tokens, labels):
        print(f"[smoke]       {tok:<16} {lab}")
    print(f"[smoke]     label sequence: {labels}")
    print("[smoke] OK — pipeline ran end-to-end, artifacts have correct shape")
    return 0
