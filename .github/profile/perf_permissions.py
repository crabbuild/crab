"""Temporarily grant only owned perf sampling and ring-buffer capabilities."""

from contextlib import contextmanager
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import time


def capture(command, destination):
    with destination.open("wb") as output, destination.with_suffix(".stderr").open("wb") as error:
        subprocess.run(command, stdout=output, stderr=error, check=True, timeout=30)


@contextmanager
def user_perf(perf_bin, output):
    root = output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    proof = {"status": "failed", "scope": "Userspace event visibility only; no push performance qualification"}
    owned = False
    started = time.monotonic()
    try:
        if sys.platform != "linux":
            raise RuntimeError("userspace profiling requires Linux")
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
        # Fifty small per-command rings exceed the shared perf mlock budget.
        # Grant only sampling and ring-locking capabilities, then restore them.
        subprocess.run(["sudo", "-n", "setcap", "cap_perfmon,cap_ipc_lock=ep", str(perf)],
                       check=True, timeout=15)
        capture([str(perf), "version"], root / "perf-version.txt")
        capture(["uname", "-a"], root / "kernel-version.txt")
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
