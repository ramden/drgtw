"""CLI for the DRGTW NER training module.

Subcommands:
  synth     generate synthetic JSONL samples
  annotate  pre-annotate raw text lines with Presidio -> JSONL (needs extra)
  train     fine-tune a HF model on JSONL, write model + eval report
  eval      evaluate a trained model dir on JSONL
  export    export a trained model dir to gateway ONNX layout
  smoke     end-to-end pipeline proof (synth -> train tiny -> export -> infer)
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def _cmd_synth(args) -> int:
    from .synth import generate
    from .data import write_jsonl

    samples = generate(args.n, seed=args.seed)
    write_jsonl(args.out, samples)
    print(f"wrote {len(samples)} synthetic samples -> {args.out}")
    return 0


def _cmd_annotate(args) -> int:
    from .annotate import annotate_lines, AnnotateUnavailable
    from .data import write_jsonl

    lines = Path(args.input).read_text(encoding="utf-8").splitlines()
    try:
        samples = annotate_lines(
            lines, spacy_model=args.spacy_model, score_threshold=args.score_threshold
        )
    except AnnotateUnavailable as exc:
        print(str(exc), file=sys.stderr)
        return 2
    write_jsonl(args.out, samples)
    n_ent = sum(len(s.entities) for s in samples)
    print(f"annotated {len(samples)} lines ({n_ent} entities) -> {args.out}")
    print("Review and correct the JSONL before training.")
    return 0


def _cmd_train(args) -> int:
    from .train import TrainConfig, train

    cfg = TrainConfig(
        base_model=args.base_model,
        output_dir=args.out,
        epochs=args.epochs,
        learning_rate=args.lr,
        train_batch_size=args.batch_size,
        eval_fraction=args.eval_fraction,
        max_length=args.max_length,
        seed=args.seed,
    )
    report = train(args.data, cfg)
    print(f"trained -> {args.out}")
    print(f"eval f1={report['f1']:.4f} precision={report['precision']:.4f} "
          f"recall={report['recall']:.4f}")
    print(f"eval report -> {Path(args.out) / 'eval_report.json'}")
    return 0


def _cmd_eval(args) -> int:
    from .train import evaluate

    report = evaluate(args.model_dir, args.data, max_length=args.max_length)
    print(json.dumps(
        {k: report[k] for k in ("precision", "recall", "f1")}, indent=2
    ))
    return 0


def _cmd_export(args) -> int:
    from .export import export_model

    manifest = export_model(args.model_dir, args.out, opset=args.opset)
    print(json.dumps(manifest, indent=2))
    return 0


def _cmd_smoke(args) -> int:
    from .smoke import run_smoke

    return run_smoke(workdir=args.workdir, n=args.n, epochs=args.epochs)


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="drgtw-training", description=__doc__)
    sub = p.add_subparsers(dest="command", required=True)

    s = sub.add_parser("synth", help="generate synthetic JSONL samples")
    s.add_argument("-n", type=int, default=200, help="number of samples")
    s.add_argument("--seed", type=int, default=0)
    s.add_argument("--out", required=True, help="output JSONL path")
    s.set_defaults(func=_cmd_synth)

    a = sub.add_parser("annotate", help="pre-annotate raw text with Presidio")
    a.add_argument("--input", required=True, help="raw text, one line per sample")
    a.add_argument("--out", required=True, help="output JSONL path")
    a.add_argument("--spacy-model", default="en_core_web_lg")
    a.add_argument("--score-threshold", type=float, default=0.35)
    a.set_defaults(func=_cmd_annotate)

    t = sub.add_parser("train", help="fine-tune a HF token-classification model")
    t.add_argument("--data", required=True, help="training JSONL")
    t.add_argument("--out", required=True, help="output model dir")
    t.add_argument("--base-model", default="distilbert-base-multilingual-cased")
    t.add_argument("--epochs", type=float, default=3.0)
    t.add_argument("--lr", type=float, default=5e-5)
    t.add_argument("--batch-size", type=int, default=16)
    t.add_argument("--eval-fraction", type=float, default=0.2)
    t.add_argument("--max-length", type=int, default=256)
    t.add_argument("--seed", type=int, default=0)
    t.set_defaults(func=_cmd_train)

    e = sub.add_parser("eval", help="evaluate a trained model dir on JSONL")
    e.add_argument("--model-dir", required=True)
    e.add_argument("--data", required=True)
    e.add_argument("--max-length", type=int, default=256)
    e.set_defaults(func=_cmd_eval)

    x = sub.add_parser("export", help="export trained model to gateway ONNX layout")
    x.add_argument("--model-dir", required=True, help="HF model dir from `train`")
    x.add_argument("--out", required=True, help="gateway artifact output dir")
    x.add_argument("--opset", type=int, default=14)
    x.set_defaults(func=_cmd_export)

    k = sub.add_parser("smoke", help="end-to-end pipeline proof on a tiny model")
    k.add_argument("--workdir", default="out/smoke")
    k.add_argument("-n", type=int, default=200)
    k.add_argument("--epochs", type=float, default=2.0)
    k.set_defaults(func=_cmd_smoke)

    return p


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
