"""Create a NEW local repository, queue four real experiments, export Results."""
from __future__ import annotations

import argparse
import json
import shutil
import subprocess
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", type=Path, required=True, help="New directory; must not exist")
    parser.add_argument("--crab", default="crab", help="Crab executable")
    args = parser.parse_args()
    root = args.destination.resolve()
    if root.exists():
        parser.error("destination already exists; choose a new directory")
    executable = shutil.which(args.crab)
    if not executable:
        parser.error("Crab executable was not found")
    executable = str(Path(executable).resolve())
    source = Path(__file__).resolve().parent
    shutil.copytree(source, root, ignore=shutil.ignore_patterns(
        ".git", ".crab", "data", "models", "metrics", "plots", "reports", "crab.lock", "__pycache__"))

    def command(*argv):
        result = subprocess.run(argv, cwd=root, text=True, capture_output=True)
        if result.returncode:
            raise RuntimeError(f"{Path(argv[0]).name} failed ({result.returncode}): {result.stderr}")
        return result.stdout

    def crab(*argv):
        return json.loads(command(executable, *argv, "--json"))["data"]

    command("git", "init", "--initial-branch=main")
    command("git", "config", "user.name", "Crab Results example")
    command("git", "config", "user.email", "results@example.invalid")
    command("git", "config", "commit.gpgsign", "false")
    command("git", "add", ".")
    command("git", "commit", "-m", "Add reproducible Results lab")
    crab("run", "--validate")
    ids = []
    for capacity in ["linear", "interactions"]:
        for l2 in [0.001, 0.1]:
            queued = crab("exp", "run", "--queue", "-S", f"train.capacity={capacity}", "-S", f"train.l2={l2}",
                          "--name", f"{capacity} · L2 {l2}")
            ids.extend(queued["experiment_ids"])
    started = crab("exp", "start", "--jobs", "2")
    if started["succeeded"] != 4 or started["failed"] != 0:
        raise RuntimeError(f"Experiment sweep failed: {started}")
    catalog = crab("results", "list")
    wanted = ["/f1", "/brier", "/mae", "/rmse"]
    columns = [c["key"].removeprefix("metric:") for c in catalog["columns"] if c["pointer"] in wanted]
    objectives = {c["key"].removeprefix("metric:"): "maximize" if c["pointer"] == "/f1" else "minimize"
                  for c in catalog["columns"] if c["pointer"] in ["/f1", "/brier", "/mae", "/rmse"]}
    runs = [crab("results", "show", experiment) for experiment in ids]
    identities = []
    input_identities = []
    for run in runs:
        stages = run["snapshot"]["stages"]
        identity = next(w["hash"] for stage in stages for w in stage["inputs"]
                        if w["path"] == "data/benchmark.json")
        identities.append(identity)
        inputs = {w["path"]: w["hash"] for stage in stages for w in stage["inputs"]}
        input_identities.append(tuple(inputs[path] for path in
            ["data/customers.json", "data/demand.json", "src/lab.py"]))
    if len(set(identities)) != 1 or len(set(input_identities)) != 1:
        raise RuntimeError("Benchmark identities differ")
    datasets = {experiment: {"hash": identity, "split": "classification:test[1400:1800];forecast:test[105:126]",
                            "evaluator": "results-lab-v1:"+input_identities[0][2]}
                for experiment, identity in zip(ids, identities, strict=True)}
    reports = root/"reports"
    reports.mkdir()
    spec = reports/"comparison.json"
    spec.write_text(json.dumps({"baseline": ids[0], "columns": columns, "objectives": objectives,
                               "include_logs": True, "datasets": datasets}, indent=2)+"\n")
    report = crab("results", "report", "create", *ids, "--title", "Results lab · classification + forecasting",
                  "--spec", str(spec))
    report_id = report["id"]
    crab("results", "report", "export", report_id, "--output", str(reports/"results-lab.html"))
    charts = crab("results", "charts", *ids)
    if len(charts) != 5 or any(c["state"] != "present" or c["reduction"] for c in charts):
        raise RuntimeError("Expected five complete, unsampled chart groups")
    (reports/"charts.json").write_text(json.dumps(charts, indent=2)+"\n")
    (reports/"runs.json").write_text(json.dumps(runs, indent=2)+"\n")
    summary = {"repository": str(root), "experiments": ids, "report": report_id,
               "html": str(reports/"results-lab.html"), "charts": len(charts),
               "points": sum(c["points"] for c in charts), "synthetic_data": True}
    (reports/"demo.json").write_text(json.dumps(summary, indent=2)+"\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
