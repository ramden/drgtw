"""Export tests: config.json shape + manifest, without a full ONNX export.

The heavy ONNX export is exercised by the smoke pipeline. Here we keep things
fast: we build a config dict the way export.py would and assert the gateway's
contract (id2label contiguous from 0, BIO labels present), and we verify the
label scheme helpers produce a gateway-compatible map.
"""

import json

from drgtw_training.export import write_manifest
from drgtw_training.labels import build_label_list, label_maps


def test_label_list_is_gateway_compatible():
    labels = build_label_list(["PER", "ORG", "LOC"])
    assert labels[0] == "O"
    assert set(labels) == {"O", "B-PER", "I-PER", "B-ORG", "I-ORG", "B-LOC", "I-LOC"}
    id2label, label2id = label_maps(labels)
    # contiguous 0..n-1 (gateway rejects gaps)
    assert sorted(id2label.keys()) == list(range(len(labels)))
    # round-trips
    for i, lab in id2label.items():
        assert label2id[lab] == i


def test_custom_passthrough_labels():
    labels = build_label_list(["PER", "PHONE"])
    assert "B-PHONE" in labels and "I-PHONE" in labels
    id2label, _ = label_maps(labels)
    assert sorted(id2label.keys()) == list(range(len(labels)))


def test_write_manifest_shape(tmp_path):
    # Simulate an export output dir with the three expected files.
    id2label = {str(i): lab for i, lab in enumerate(build_label_list(["PER", "ORG", "LOC"]))}
    (tmp_path / "config.json").write_text(
        json.dumps({"model_type": "bert", "id2label": id2label}), encoding="utf-8"
    )
    (tmp_path / "model.onnx").write_bytes(b"\x00" * 128)
    (tmp_path / "tokenizer.json").write_text("{}", encoding="utf-8")

    manifest = write_manifest(tmp_path, ["input_ids", "attention_mask", "token_type_ids"])
    assert manifest["files"]["model.onnx"] == 128
    assert manifest["files"]["tokenizer.json"] is not None
    assert manifest["files"]["config.json"] is not None
    assert manifest["onnx_inputs"] == ["input_ids", "attention_mask", "token_type_ids"]
    # id2label preserved and contiguous from "0"
    assert manifest["id2label"]["0"] == "O"
    assert "B-PER" in manifest["id2label"].values()
