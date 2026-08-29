from __future__ import annotations

import argparse
import json
import pickle
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("model", nargs="?", default="models/fraud-model.pkl")
    parser.add_argument("--report")
    args = parser.parse_args()
    with Path(args.model).open("rb") as handle:
        model = pickle.load(handle)
    required = {"algorithm", "feature_names", "means", "scales", "weights", "bias"}
    missing = required.difference(model)
    if missing:
        raise ValueError(f"model is missing keys: {sorted(missing)}")
    result = {
        "algorithm": model["algorithm"],
        "features": len(model["weights"]),
        "status": "ok",
    }
    if args.report:
        destination = Path(args.report)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(
            json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    print(f"ok: {result['algorithm']} with {result['features']} weights")


if __name__ == "__main__":
    main()
