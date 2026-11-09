"""Record the release build and verify the frozen control before timing."""

import hashlib
import json
import os
import re
import subprocess
import sys
import tomllib
from pathlib import Path

CONTROL_SOURCE = "40d4bc0d745a2e1405bf5d5004d05a295b1c0245"
CONTROL_HASH = "a4fdf8e9669ea0338f1145dbd117eee5cb592209b4b8b48c01ff2fd4bd01f6da"


def command(*args):
    return subprocess.check_output([str(arg) for arg in args], text=True)


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def inspect(binary):
    elf = command("readelf", "-W", "-h", "-l", "-d", "-V", "-p", ".comment", binary)
    assert "DYN (Position-Independent Executable file)" in elf
    assert "GNU_RELRO" in elf and "BIND_NOW" in elf
    stack = next(line for line in elf.splitlines() if "GNU_STACK" in line)
    assert "RWE" not in stack and " RW " in stack
    assert "(RELR)" not in elf
    versions = re.findall(r"GLIBC_([0-9]+\.[0-9]+)", elf)
    abi = {
        "needed": sorted(re.findall(r"Shared library: \[(.*?)\]", elf)),
        "max_glibc": max(versions, key=lambda x: tuple(map(int, x.split(".")))),
    }
    assert abi["max_glibc"] == "2.39" and len(abi["needed"]) == 6, abi
    binary.with_name("elf.txt").write_text(elf)
    binary.with_name("sections.txt").write_text(command("readelf", "-W", "-S", binary))
    return abi


def build_receipt(log_path):
    assert not os.environ.get("RUSTFLAGS") and not os.environ.get("CARGO_ENCODED_RUSTFLAGS")
    assert not any(key.startswith("CARGO_PROFILE_RELEASE_") for key in os.environ)
    source = command("git", "rev-parse", "HEAD").strip()
    changed = command("git", "diff", "--name-only", CONTROL_SOURCE, source).splitlines()
    assert all(path == "Cargo.toml" or path.startswith(".github/") for path in changed), changed
    profile = tomllib.loads(Path("Cargo.toml").read_text())
    control = tomllib.loads(command("git", "show", f"{CONTROL_SOURCE}:Cargo.toml"))
    assert profile["profile"]["release"].pop("codegen-units") == 1
    assert profile == control
    records = [json.loads(line) for line in Path(log_path).read_text().splitlines()]
    assert records[-1] == {"reason": "build-finished", "success": True}
    artifacts = [row for row in records if row.get("reason") == "compiler-artifact"
                 and row.get("target", {}).get("name") == "crab"]
    executable = next(row for row in artifacts if row.get("executable"))
    library = next(row for row in artifacts if row["target"]["kind"] == ["lib"])
    binary = Path(executable["executable"])
    archive = next(path for path in library["filenames"] if path.endswith(".rlib"))
    members = command("ar", "t", archive).splitlines()
    objects = [member for member in members if member.endswith(".o")]
    assert len(objects) == 1, objects
    version = json.loads(command(binary, "version", "--json"))
    assert version["data"]["git_sha"] == source
    assert "push" in command(binary, "push", "--help")
    receipt = {
        "source": source,
        "compiler": command("rustc", "-Vv"),
        "features": "default,gix-transport",
        "actual_features": executable["features"],
        "cargo_profile": executable["profile"],
        "profile": tomllib.loads(Path("Cargo.toml").read_text())["profile"]["release"],
        "codegen_objects": objects,
        "sha256": digest(binary),
        "file_bytes": binary.stat().st_size,
        "version": version,
        "rustflags": "",
        "extra_rustc_arguments": [],
        "abi": inspect(binary),
        "runtime_changed_files": [path for path in changed if not path.startswith(".github/")],
    }
    assert "rustc 1.99.1 " in receipt["compiler"] and "LLVM version: 23.1.1" in receipt["compiler"]
    binary.with_name("candidate-build.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))


def verify_pair():
    control = Path(os.environ["PAIR_CONTROL"])
    candidate = Path(os.environ["PAIR_CANDIDATE"])
    a = json.loads(control.with_name("candidate-build.json").read_text())
    b = json.loads(candidate.with_name("candidate-build.json").read_text())
    assert a["source"] == CONTROL_SOURCE and a["sha256"] == CONTROL_HASH
    assert digest(control) == CONTROL_HASH and digest(candidate) == b["sha256"]
    assert a["compiler"] == b["compiler"] and a["features"] == b["features"]
    profile = b["profile"].copy()
    assert profile.pop("codegen-units") == 1 and profile == a["profile"]
    assert b["runtime_changed_files"] == ["Cargo.toml"]
    assert a["rustflags"] == b["rustflags"] == ""
    assert a["extra_rustc_arguments"] == b["extra_rustc_arguments"] == []
    control.chmod(0o755)
    assert json.loads(command(control, "version", "--json"))["data"]["git_sha"] == CONTROL_SOURCE
    assert json.loads(command(candidate, "version", "--json"))["data"]["git_sha"] == b["source"]
    assert inspect(control) == inspect(candidate) == b["abi"]
    Path(os.environ["RUNNER_TEMP"], "control-source.json").write_text(json.dumps(a, indent=2) + "\n")
    print(json.dumps({"control": a["source"], "candidate": b["source"], "verified": True}))


if __name__ == "__main__":
    if sys.argv[1] == "build":
        build_receipt(sys.argv[2])
    else:
        assert sys.argv[1] == "pair"
        verify_pair()
