"""Deterministic synthetic benchmarks; actual fitting and held-out evaluation."""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import random
from pathlib import Path


def write(path, value):
    destination = Path(path)
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(json.dumps(value, separators=(",", ":")) + "\n")


def read(path):
    return json.loads(Path(path).read_text())


def sigmoid(value):
    return 1 / (1 + math.exp(-max(-40, min(40, value))))


def features(row, capacity):
    values = [1.0, row["x1"], row["x2"], row["signal"]]
    if capacity == "interactions":
        values += [row["x1"] * row["x2"], row["x1"] ** 2, row["x2"] ** 2]
    return values


def predict(model, row):
    return sigmoid(sum(w * x for w, x in zip(
        model["weights"], features(row, model["capacity"]), strict=True)))


def prepare():
    rng = random.Random(734)
    rows = []
    for index in range(1800):
        x1, x2, signal = (rng.gauss(0, 1) for _ in range(3))
        cohort = ["Established", "New customer", "Cross-border"][index % 3]
        shift = {"Established": -0.4, "New customer": 0.3, "Cross-border": 0.7}[cohort]
        probability = sigmoid(-0.9 + 1.3*x1 - 0.8*x2 + 1.8*x1*x2 + 0.5*signal + shift)
        rows.append({"id": index, "x1": x1, "x2": x2, "signal": signal,
                     "cohort": cohort, "label": int(rng.random() < probability),
                     "split": "train" if index < 1000 else "validation" if index < 1400 else "test"})
    write("data/customers.json", rows)
    write("data/demand.json", [{"day": day, "actual": 110 + 0.3*day
          + 18*math.sin(2*math.pi*day/7) + rng.gauss(0, 5)} for day in range(126)])
    write("data/benchmark.json", {"version": "results-lab-v1", "seed": 734,
          "sha256": {name: hashlib.sha256(Path(path).read_bytes()).hexdigest() for name, path in
                     [("customers", "data/customers.json"), ("demand", "data/demand.json"),
                      ("evaluator", __file__)]},
          "classification": {"train": [0, 1000], "validation": [1000, 1400], "test": [1400, 1800]},
          "forecast": {"train": [0, 84], "validation": [84, 105], "test": [105, 126]}})
    print("Prepared 1,800 synthetic observations and 126 daily demand values; fixed seed 734.")


def train(config):
    params = config["train"]
    rows = read("data/customers.json")
    training = [r for r in rows if r["split"] == "train"]
    validation = [r for r in rows if r["split"] == "validation"]
    model = {"capacity": params["capacity"], "weights": [0.0]*len(features(training[0], params["capacity"]))}
    history = []
    def loss(subset):
        probabilities = [(r["label"], max(1e-12, min(1-1e-12, predict(model, r)))) for r in subset]
        return -sum(y*math.log(p)+(1-y)*math.log(1-p) for y, p in probabilities)/len(subset)
    for epoch in range(params["epochs"] + 1):
        history.append({"epoch": epoch, "train_loss": loss(training), "validation_loss": loss(validation)})
        if epoch == params["epochs"]:
            break
        gradient = [0.0]*len(model["weights"])
        for row in training:
            error = predict(model, row)-row["label"]
            for index, value in enumerate(features(row, model["capacity"])):
                gradient[index] += error*value
        model["weights"] = [w-params["learning_rate"]*(g/len(training)+(params["l2"]*w if i else 0))
                            for i, (w, g) in enumerate(zip(model["weights"], gradient, strict=True))]
    write("models/classifier.json", model)
    write("plots/learning.json", history)
    print(f"Fit {params['capacity']} classifier; validation log loss {history[-1]['validation_loss']:.4f}.")


def measures(rows, threshold):
    tp = sum(r["label"] == 1 and r["score"] >= threshold for r in rows)
    fp = sum(r["label"] == 0 and r["score"] >= threshold for r in rows)
    fn = sum(r["label"] == 1 and r["score"] < threshold for r in rows)
    tn = len(rows)-tp-fp-fn
    precision, recall = tp/max(tp+fp, 1), tp/max(tp+fn, 1)
    return {"precision": precision, "recall": recall, "accuracy": (tp+tn)/len(rows),
            "fpr": fp/max(fp+tn, 1), "f1": 2*precision*recall/max(precision+recall, 1e-12),
            "tp": tp, "fp": fp, "fn": fn, "tn": tn}


def interval(successes, count):
    # Wilson 95% score interval for a binomial proportion.
    if count == 0:
        return None, None
    z = 1.96
    rate, denominator = successes/count, 1+z*z/count
    center = (rate+z*z/(2*count))/denominator
    half = z*math.sqrt(rate*(1-rate)/count+z*z/(4*count*count))/denominator
    return center-half, center+half


def evaluate(config):
    model = read("models/classifier.json")
    rows = [{"id": r["id"], "cohort": r["cohort"], "label": r["label"], "score": predict(model, r)}
            for r in read("data/customers.json") if r["split"] == "test"]
    threshold = config["evaluate"]["threshold"]
    result = measures(rows, threshold)
    result["brier"] = sum((r["score"]-r["label"])**2 for r in rows)/len(rows)
    result.update({"samples": len(rows), "threshold": threshold, "dataset": "customers-seed734-v1", "split": "test"})
    write("metrics/classification.json", result)
    write("plots/predictions.json", rows)
    diagnostics = []
    for i in range(101):
        point = measures(rows, i/100)
        for panel, x, y in [("Precision / recall", point["recall"], point["precision"]),
                             ("ROC", point["fpr"], point["recall"])]:
            diagnostics.append({"panel": panel, "x": x, "y": y, "threshold": i/100})
    for i in range(10):
        subset = [r for r in rows if min(int(r["score"]*10), 9) == i]
        if subset:
            lower, upper = interval(sum(r["label"] for r in subset), len(subset))
            diagnostics.append({"panel": "Calibration", "x": sum(r["score"] for r in subset)/len(subset),
                                "y": sum(r["label"] for r in subset)/len(subset),
                                "lower": lower, "upper": upper, "count": len(subset)})
    write("plots/diagnostics.json", diagnostics)
    cohorts = []
    for cohort in sorted({r["cohort"] for r in rows}):
        subset = [r for r in rows if r["cohort"] == cohort]
        point = measures(subset, threshold)
        for metric, numerator, denominator in [
            ("Precision", point["tp"], point["tp"]+point["fp"]),
            ("Recall", point["tp"], point["tp"]+point["fn"]),
            ("Accuracy", point["tp"]+point["tn"], len(subset))]:
            lower, upper = interval(numerator, denominator)
            cohorts.append({"cohort": cohort, "measure": metric, "score": numerator/denominator if denominator else None,
                            "lower": lower, "upper": upper, "count": denominator})
    write("plots/cohorts.json", cohorts)
    print(f"Held-out classification: F1 {result['f1']:.3f}; Brier {result['brier']:.3f}; {len(rows)} test observations.")


def forecast(config):
    rows = read("data/demand.json")
    params = config["train"]
    def basis(day):
        values = [1, day/100]
        if params["capacity"] == "interactions":
            values += [math.sin(2*math.pi*day/7), math.cos(2*math.pi*day/7)]
        return values
    weights = [0.0]*len(basis(0))
    training = rows[:84]
    for _ in range(1600):
        gradient = [0.0]*len(weights)
        for row in training:
            vector = basis(row["day"])
            error = sum(w*x for w, x in zip(weights, vector, strict=True))-row["actual"]
            for i, value in enumerate(vector):
                gradient[i] += error*value
        weights = [w-0.08*(g/len(training)+(params["l2"]*w if i else 0))
                   for i, (w, g) in enumerate(zip(weights, gradient, strict=True))]
    def estimate(day):
        return sum(w*x for w, x in zip(weights, basis(day), strict=True))
    # Calibrate residual bands on a disjoint 21-day validation period.
    residuals = [r["actual"]-estimate(r["day"]) for r in rows[84:105]]
    radius = 1.96*math.sqrt(sum(r*r for r in residuals)/len(residuals))
    forecast_rows = [{**r, "prediction": estimate(r["day"]), "lower": estimate(r["day"])-radius,
                      "upper": estimate(r["day"])+radius, "residual": r["actual"]-estimate(r["day"]),
                      "split": "train" if r["day"] < 84 else "validation" if r["day"] < 105 else "test"}
                     for r in rows]
    testing = forecast_rows[105:]
    write("plots/forecast.json", forecast_rows)
    write("models/forecaster.json", {"weights": weights, "capacity": params["capacity"], "radius": radius})
    write("metrics/forecast.json", {"mae": sum(abs(r["residual"]) for r in testing)/len(testing),
          "rmse": math.sqrt(sum(r["residual"]**2 for r in testing)/len(testing)),
          "band_coverage": sum(r["lower"] <= r["actual"] <= r["upper"] for r in testing)/len(testing),
          "dataset": "demand-seed734-v1", "split": "test", "samples": len(testing)})
    print(f"Demand forecast: validation-calibrated residual band ±{radius:.2f}; 21 held-out days.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("stage", choices=["prepare", "train", "evaluate", "forecast"])
    args = parser.parse_args()
    config = read("params.json")
    if args.stage == "prepare":
        prepare()
    else:
        {"train": train, "evaluate": evaluate, "forecast": forecast}[args.stage](config)
