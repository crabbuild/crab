"""Probe kernel sampling on an owned Linux runner; no Crab workload or timing claim."""

import argparse
from contextlib import contextmanager
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
import time


def capture(command, destination):
    with destination.open("wb") as output, destination.with_suffix(".stderr").open("wb") as error:
        subprocess.run(command, stdout=output, stderr=error, check=True, timeout=30)


@contextmanager
def kernel_perf(perf_bin, output):
    root = output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    proof = {"status": "failed", "scope": "Kernel visibility only; no Crab or performance qualification"}
    owned = False
    started = time.monotonic()
    try:
        if sys.platform != "linux":
            raise RuntimeError("kernel profiling requires Linux")
        perf = perf_bin.resolve(strict=True)
        if perf.name != "perf" or not perf.is_relative_to(Path("/usr/lib")):
            raise RuntimeError("expected the installed versioned perf under /usr/lib")
        with perf.open("rb") as binary:
            if binary.read(4) != b"\x7fELF":
                raise RuntimeError("perf must be an actual ELF executable, not the uname wrapper")
            binary.seek(0)
            perf_digest = hashlib.file_digest(binary, "sha256").hexdigest()
        initial = subprocess.check_output(["getcap", str(perf)], text=True).strip()
        proof.update({"perf": str(perf), "initial_capabilities": initial,
                      "perf_sha256": perf_digest})
        if initial:
            raise RuntimeError("existing perf capabilities must remain unchanged")
        # The marker authorizes only cleanup of this runner's previously empty
        # capability set, including when Actions stops the Python process.
        (root / "owned-perf.txt").write_text(str(perf) + "\n")
        owned = True
        subprocess.run(["sudo", "-n", "setcap", "cap_perfmon,cap_ipc_lock,cap_syslog=ep", str(perf)],
                       check=True, timeout=15)
        capture([str(perf), "version"], root / "perf-version.txt")
        capture(["uname", "-a"], root / "kernel-version.txt")
        capture(["sudo", "-n", "cat", "/proc/kallsyms"], root / "kallsyms.txt")
        yield perf, root, proof
        proof["status"] = "passed"
    except BaseException as error:
        proof["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        restored = True
        if owned:
            result = subprocess.run(["sudo", "-n", "setcap", "-r", str(perf)],
                                    capture_output=True, timeout=15)
            restored = result.returncode == 0 and not subprocess.check_output(
                ["getcap", str(perf)], text=True).strip()
            if restored:
                (root / "owned-perf.txt").unlink()
        proof["capabilities_restored"] = restored
        proof["elapsed_seconds"] = time.monotonic() - started
        if not restored:
            proof["status"] = "failed"
        (root / "probe.json").write_text(json.dumps(proof, indent=2) + "\n")
        print(json.dumps({key: proof[key] for key in ("status", "capabilities_restored")}))
        if not restored:
            raise RuntimeError("could not restore owned perf capabilities")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--perf-bin", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    with kernel_perf(args.perf_bin, args.output) as (perf, root, proof):
        # Inherited events isolate the command; kernel chains use the runner's unwinder.
        # Per-task events omit CPU identity by default, but the report requires it.
        command = [str(perf), "record", "--strict-freq", "-e", "cpu-clock:k", "-F", "999",
                   "-m", "8", "--no-buildid", "--call-graph", "fp", "--kernel-callchains", "--sample-cpu", "-P",
                   "-o", str(root / "probe.data"), "--", sys.executable, "-c",
                   "import os\nfor _ in range(1000000): os.stat('/proc/self/status')"]
        proof["record_command"] = command
        capture(command, root / "record.stdout")
        capture([str(perf), "script", "--ns", "--kallsyms", str(root / "kallsyms.txt"),
                 "-i", str(root / "probe.data"), "-F", "comm,pid,tid,cpu,time,event,ip,sym,dso,period"],
                root / "probe.script")
        script = (root / "probe.script").read_text()
        samples = sum("cpu-clock:k:" in line for line in script.splitlines())
        frames = re.findall(r"\b[0-9a-f]{12,16}\s+[A-Za-z_][^\n]*\(\[kernel[^)]*\]\)", script)
        with (root / "probe.data").open("rb") as data:
            data_digest = hashlib.file_digest(data, "sha256").hexdigest()
        proof.update({"samples": samples, "symbolized_kernel_frames": len(frames),
                      "data_sha256": data_digest})
        if samples < 50 or len(frames) < 50:
            raise RuntimeError("insufficient symbolized kernel samples; do not infer zero kernel cost")


if __name__ == "__main__":
    main()
