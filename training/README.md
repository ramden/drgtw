# drgtw_training

Phase 5 of DRGTW: the NER training/tuning module. Fine-tunes a Hugging Face
token-classification model on labelled samples and exports gateway-compatible
ONNX artifacts (`model.onnx` + `tokenizer.json` + `config.json`) that drop into
the Rust gateway's `models/` directory.

See `../docs/training-guide.md` for the end-to-end workflow.
