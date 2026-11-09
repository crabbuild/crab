"""Compare isolated Linux release links; startup results are not push qualification."""

import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import subprocess
import tarfile
import time
import tomllib


root = Path(os.environ["RUNNER_TEMP"]) / "crab-safe-icf-proof"
root.mkdir()
logs = root / "logs"
logs.mkdir()
env = os.environ.copy()
assert not any(env.get(k) for k in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS"))
assert not any(k.startswith("CARGO_PROFILE_RELEASE_") for k in env)
env["CARGO_TARGET_DIR"] = str(Path(os.environ["RUNNER_TEMP"]) / "crab-safe-icf-target")
source = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
env["GIT_SHA"] = source
commands = []


def run(name, args, expected=0):
    result = subprocess.run(args, env=env, capture_output=True, text=True)
    (logs / (name + ".out")).write_text(result.stdout)
    (logs / (name + ".err")).write_text(result.stderr)
    commands.append({"name": name, "args": args, "exit": result.returncode})
    (root / "commands.json").write_text(json.dumps(commands, indent=2) + "\n")
    assert result.returncode == expected, (name, result.returncode, result.stderr[-2000:])
    return result


compiler = run("compiler", ["rustc", "-Vv"]).stdout
assert "rustc 1.99.1" in compiler and "host: x86_64-unknown-linux-gnu" in compiler
profile = tomllib.loads(Path("Cargo.toml").read_text())["profile"]["release"]
assert profile == {"opt-level": 3, "lto": False, "debug": 0, "strip": "symbols"}
for mode, expected in [("safe", 0), ("all", 101)]:
    probe = root / ("contract-" + mode)
    run("contract-build-" + mode, [
        "rustc", "--edition=2024", "--test", ".github/profile/safe_icf_contract.rs",
        "-C", "opt-level=3", "-C", "link-arg=-Wl,--icf=" + mode, "-o", str(probe),
    ])
    result = run("contract-run-" + mode, [str(probe)], expected)
    summary = "ok. 4 passed; 0 failed; 0 ignored" if mode == "safe" else "FAILED. 3 passed; 1 failed; 0 ignored"
    assert summary in result.stdout
    if mode == "all":
        assert "address_significant_ffi_functions_stay_distinct ... FAILED" in result.stdout
    run("contract-symbols-" + mode, ["nm", "-an", str(probe)])
    run("contract-linker-" + mode, ["readelf", "-p", ".comment", str(probe)])

# The aggressive link is only a failing counterfactual for the address probe.
# Product artifacts below use either the default link or safe folding.
variants = {}
for variant in ["normal", "safe"]:
    args = ["cargo", "build" if variant == "normal" else "rustc", "-p", "crab", "--release", "--locked", "--features", "gix-transport"]
    extra = []
    if variant == "safe":
        extra = ["-C", "link-arg=-Wl,--icf=safe", "-C", "link-arg=-Wl,--Map=" + str(root / "safe.map")]
        args += ["--bin", "crab", "--", *extra]
    run("build-" + variant, args)
    binary = Path(env["CARGO_TARGET_DIR"]) / "release/crab"
    directory = root / variant
    directory.mkdir()
    archive = root / (variant + ".tar.gz")
    with tarfile.open(archive, "w:gz") as output:
        output.add(binary, arcname="crab")
    with tarfile.open(archive) as stored:
        assert stored.getnames() == ["crab"]
        stored.extractall(directory, filter="data")
    archive.unlink()
    binary = directory / "crab"
    version = json.loads(run("version-" + variant, [str(binary), "version", "--json"]).stdout)
    assert version["data"]["git_sha"] == source
    run("help-" + variant, [str(binary), "--help"])
    run("push-help-" + variant, [str(binary), "push", "--help"])
    for name, flags in {
        "header": ["-hW"], "program": ["-lW"], "dynamic": ["-dW"],
        "sections": ["-SW"], "versions": ["-VW"], "relocations": ["-rW"],
        "linker": ["-p", ".comment"],
    }.items():
        run("elf-" + variant + "-" + name, ["readelf", *flags, str(binary)])
    record = {
        "source": source, "compiler": compiler, "features": "default,gix-transport",
        "variant": variant, "rustflags": "", "extra_rustc_arguments": extra,
        "sha256": hashlib.file_digest(binary.open("rb"), "sha256").hexdigest(),
        "file_bytes": binary.stat().st_size, "version": version,
        "profile": profile,
    }
    (directory / "candidate-build.json").write_text(json.dumps(record, indent=2) + "\n")
    variants[variant] = record

# Keep an actual Rust input object's section inventory instead of assuming that
# the compiler emits address-significance metadata for Crab's dependencies.
libraries = list((Path(env["CARGO_TARGET_DIR"]) / "release/deps").glob("libcrab-*.rlib"))
assert len(libraries) == 1
members = run("archive-members", ["ar", "t", str(libraries[0])]).stdout.splitlines()
member = next(name for name in members if name.endswith(".o"))
obj = root / "crab-input.o"
with obj.open("wb") as output:
    subprocess.run(["ar", "p", str(libraries[0]), member], env=env, stdout=output, check=True)
run("rust-input-sections", ["readelf", "-SW", str(obj)])
obj.unlink()


def launch(binary):
    start = time.monotonic_ns()
    result = subprocess.run([str(binary), "version", "--json"], env=env, capture_output=True, text=True, timeout=30)
    elapsed = (time.monotonic_ns() - start) / 1_000_000
    assert result.returncode == 0 and json.loads(result.stdout)["data"]["git_sha"] == source
    return {"elapsed_ms": elapsed, "exit": result.returncode, "stdout": result.stdout, "stderr": result.stderr}


trials = []
for index, variant in enumerate(["normal", "safe", "safe", "normal"]):
    binary = root / variant / "crab"
    for actors, rounds in [(1, 50), (50, 4)]:
        before = resource.getrusage(resource.RUSAGE_CHILDREN)
        start = time.monotonic_ns()
        waves = []
        with concurrent.futures.ThreadPoolExecutor(max_workers=actors) as pool:
            for _ in range(rounds):
                waves.append(list(pool.map(lambda _: launch(binary), range(actors))))
        elapsed = (time.monotonic_ns() - start) / 1_000_000
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        trials.append({
            "trial": index, "variant": variant, "actors": actors, "rounds": rounds,
            "wall_ms": elapsed, "user_cpu_ms": (after.ru_utime - before.ru_utime) * 1000,
            "system_cpu_ms": (after.ru_stime - before.ru_stime) * 1000, "waves": waves,
        })
        (root / "startup-raw.json").write_text(json.dumps(trials, indent=2) + "\n")

normal_dynamic = (logs / "elf-normal-dynamic.out").read_text()
safe_dynamic = (logs / "elf-safe-dynamic.out").read_text()
needed = lambda text: re.findall(r"\(NEEDED\).*\[(.*?)\]", text)
assert needed(normal_dynamic) == needed(safe_dynamic)
for text in [normal_dynamic, safe_dynamic]:
    assert "BIND_NOW" in text and "(RELR)" not in text
for variant in variants:
    program = (logs / ("elf-" + variant + "-program.out")).read_text()
    assert "GNU_RELRO" in program
    stack = next(line for line in program.splitlines() if "GNU_STACK" in line)
    assert "RWE" not in stack

(root / "summary.json").write_text(json.dumps({
    "status": "COMPONENT ONLY; independent artifact verification required",
    "source": source, "variants": variants, "startup_trials": len(trials),
    "startup_launches": sum(len(w) for trial in trials for w in trial["waves"]),
    "scope": "No push-performance or production-retention claim. No runtime source changes.",
}, indent=2) + "\n")
