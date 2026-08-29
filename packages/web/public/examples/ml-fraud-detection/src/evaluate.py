from __future__ import annotations

import argparse
import json
import pickle
from pathlib import Path

from common import FEATURE_COLUMNS, read_csv, read_json, sigmoid, write_csv


def score(model: dict[str, object], row: dict[str, str]) -> float:
    values = [float(row[name]) for name in FEATURE_COLUMNS]
    normalized = [
        (value - mean) / scale
        for value, mean, scale in zip(
            values, model["means"], model["scales"], strict=True
        )
    ]
    return sigmoid(
        float(model["bias"])
        + sum(
            weight * value
            for weight, value in zip(model["weights"], normalized, strict=True)
        )
    )


def measures(labels: list[int], scores: list[float], threshold: float) -> dict[str, float]:
    predictions = [int(value >= threshold) for value in scores]
    pairs = list(zip(labels, predictions, strict=True))
    true_positive = sum(label == prediction == 1 for label, prediction in pairs)
    false_positive = sum(label == 0 and prediction == 1 for label, prediction in pairs)
    false_negative = sum(label == 1 and prediction == 0 for label, prediction in pairs)
    correct = sum(label == prediction for label, prediction in pairs)
    precision = true_positive / max(true_positive + false_positive, 1)
    recall = true_positive / max(true_positive + false_negative, 1)
    return {
        "accuracy": correct / len(labels),
        "precision": precision,
        "recall": recall,
        "f1": 2 * precision * recall / max(precision + recall, 1e-12),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", default="params.json")
    args = parser.parse_args()
    config = read_json(args.config)["evaluate"]
    with Path("models/fraud-model.pkl").open("rb") as handle:
        model = pickle.load(handle)
    rows = [row for row in read_csv("data/features.csv") if row["split"] == "test"]
    labels = [int(row["label"]) for row in rows]
    scores = [score(model, row) for row in rows]
    threshold = float(config["threshold"])
    metrics = {
        **measures(labels, scores, threshold),
        "threshold": threshold,
        "samples": len(rows),
    }

    metrics_path = Path("metrics/evaluation.json")
    metrics_path.parent.mkdir(parents=True, exist_ok=True)
    metrics_path.write_text(
        json.dumps(metrics, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    curve = [
        {"threshold": f"{point / 10:.1f}", **measures(labels, scores, point / 10)}
        for point in range(1, 10)
    ]
    write_csv(
        "plots/precision_recall.csv",
        ["threshold", "precision", "recall", "accuracy", "f1"],
        curve,
    )
    print(json.dumps(metrics, sort_keys=True))
    if metrics["recall"] < float(config["minimum_recall"]):
        raise SystemExit("recall is below evaluate.minimum_recall")


if __name__ == "__main__":
    main()
