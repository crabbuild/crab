from __future__ import annotations

import argparse
import json
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("metrics", nargs="?", default="metrics/evaluation.json")
    parser.add_argument("--minimum-recall", type=float, default=0.82)
    args = parser.parse_args()
    metrics = json.loads(Path(args.metrics).read_text(encoding="utf-8"))
    if float(metrics["recall"]) < args.minimum_recall:
        raise SystemExit(
            f"recall {metrics['recall']:.3f} is below {args.minimum_recall:.3f}"
        )
    print(f"ok: recall {metrics['recall']:.3f}")


if __name__ == "__main__":
    main()
