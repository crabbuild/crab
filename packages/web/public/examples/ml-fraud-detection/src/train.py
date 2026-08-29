from __future__ import annotations

import argparse
import pickle
import random
from pathlib import Path

from common import FEATURE_COLUMNS, read_csv, read_json, sigmoid


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", default="params.json")
    args = parser.parse_args()
    params = read_json(args.config)["train"]
    if params["algorithm"] != "logistic_regression":
        raise ValueError("this example supports algorithm=logistic_regression")

    rows = [row for row in read_csv("data/features.csv") if row["split"] == "train"]
    vectors = [[float(row[name]) for name in FEATURE_COLUMNS] for row in rows]
    labels = [float(row["label"]) for row in rows]
    columns = list(zip(*vectors, strict=True))
    means = [sum(values) / len(values) for values in columns]
    scales = [
        max(
            (sum((value - mean) ** 2 for value in values) / len(values)) ** 0.5,
            1e-9,
        )
        for values, mean in zip(columns, means, strict=True)
    ]
    normalized = [
        [
            (value - mean) / scale
            for value, mean, scale in zip(row, means, scales, strict=True)
        ]
        for row in vectors
    ]

    learning_rate = float(params["learning_rate"])
    l2 = float(params["l2"])
    weights = [0.0] * len(FEATURE_COLUMNS)
    bias = 0.0
    order = list(range(len(normalized)))
    rng = random.Random(int(params["seed"]))

    for _ in range(int(params["max_iter"])):
        rng.shuffle(order)
        gradient = [0.0] * len(weights)
        bias_gradient = 0.0
        for index in order:
            vector = normalized[index]
            score = bias + sum(
                weight * value
                for weight, value in zip(weights, vector, strict=True)
            )
            error = sigmoid(score) - labels[index]
            bias_gradient += error
            for position, value in enumerate(vector):
                gradient[position] += error * value
        sample_count = len(normalized)
        bias -= learning_rate * bias_gradient / sample_count
        for position in range(len(weights)):
            weights[position] -= learning_rate * (
                gradient[position] / sample_count + l2 * weights[position]
            )

    model = {
        "algorithm": params["algorithm"],
        "feature_names": FEATURE_COLUMNS,
        "means": means,
        "scales": scales,
        "weights": weights,
        "bias": bias,
    }
    destination = Path("models/fraud-model.pkl")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with destination.open("wb") as handle:
        pickle.dump(model, handle, protocol=5)
    print(f"trained on {len(rows)} transactions")


if __name__ == "__main__":
    main()
