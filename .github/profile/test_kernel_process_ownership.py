"""Linux ownership proof for the temporary kernel collector; no Crab timing claim."""

from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
from unittest.mock import patch

from diagnose_push_kernel import KernelPush, parse_args


def owner(root, name):
    args = parse_args(["--root", str(root), "--run-id", name, "--crab-bin", sys.executable])
    return KernelPush(args, Path("/unused/perf"), root)


def gone(pids):
    assert all(not Path(f"/proc/{pid}").exists() for pid in pids), pids


def main(root):
    root.mkdir(parents=True, exist_ok=False)
    proof = {"scope": "Process ownership only; not a Crab or performance qualification"}
    external = int(os.environ["EXTERNAL_PID"])
    assert KernelPush.parent(external) != os.getpid()
    os.kill(external, 0)
    child_code = (
        "import json,os,subprocess,sys,time;from pathlib import Path;"
        "p=subprocess.Popen([sys.executable,'-c','import time;time.sleep(120)'],start_new_session=True);"
        "Path(sys.argv[1]).write_text(json.dumps([os.getpid(),p.pid]));"
        "print('escaped-ready',flush=True);time.sleep(120)"
    )
    parent_code = (
        "import subprocess,sys;"
        f"p=subprocess.Popen([sys.executable,'-c',{child_code!r},sys.argv[1]],start_new_session=True);"
        "p.wait()"
    )
    workload = owner(root, "timeout")
    pids = root / "timeout-pids.json"
    try:
        workload.run_cmd("escaped timeout", [sys.executable, "-c", parent_code, str(pids)], root, timeout=2)
    except subprocess.TimeoutExpired:
        pass
    else:
        raise AssertionError("timeout did not fire")
    gone(json.loads(pids.read_text()))
    assert not workload.processes and not workload.children()
    ledger = [json.loads(line) for line in (root / "timeout/artifacts/commands.jsonl").read_text().splitlines()]
    assert ledger[-1]["exit_code"] < 0
    assert "escaped-ready" in Path(ledger[-1]["stdout_log"]).read_text()
    proof["timeout_reaps_escaped_child_and_grandchild"] = True

    workload = owner(root, "signal")
    previous = signal.signal(signal.SIGTERM, lambda *_: workload.cancel())
    try:
        with ThreadPoolExecutor(max_workers=2) as pool:
            files = [root / f"signal-{i}.json" for i in range(2)]
            futures = [pool.submit(workload.run_cmd, f"escaped signal {i}",
                                   [sys.executable, "-c", parent_code, str(path)], root, check=False)
                       for i, path in enumerate(files)]
            deadline = time.monotonic() + 5
            while not all(path.is_file() for path in files):
                assert time.monotonic() < deadline, "fixtures never became ready"
                time.sleep(.01)
            os.kill(os.getpid(), signal.SIGTERM)
            assert all(future.result(timeout=5).exit_code < 0 for future in futures)
        gone([pid for path in files for pid in json.loads(path.read_text())])
        assert not workload.processes and not workload.children()
    finally:
        workload.cancel()
        signal.signal(signal.SIGTERM, previous)
    proof["signal_reaps_two_escaped_process_trees"] = True

    workload = owner(root, "registration")
    original_popen = subprocess.Popen

    def interrupted_popen(*args, **kwargs):
        process = original_popen(*args, **kwargs)
        os.kill(os.getpid(), signal.SIGTERM)
        return process

    previous = signal.signal(signal.SIGTERM, lambda *_: workload.cancel())
    try:
        with patch.object(subprocess, "Popen", interrupted_popen):
            row = workload.run_cmd("signal before registration", [sys.executable, "-c", "import time;time.sleep(120)"], root, check=False)
        assert row.exit_code < 0 and workload.stopping and not workload.processes
        assert not workload.children()
    finally:
        workload.cancel()
        signal.signal(signal.SIGTERM, previous)
    proof["signal_during_popen_registration_reaped"] = True

    workload = owner(root, "pipes")
    code = "import sys;sys.stdout.write('o'*262144);sys.stderr.write('e'*262144)"
    try:
        with ThreadPoolExecutor(max_workers=50) as pool:
            rows = list(pool.map(lambda i: workload.run_cmd(f"pipes {i}", [sys.executable, "-c", code], root), range(100)))
        assert all(row.exit_code == 0 and Path(row.stdout_log).stat().st_size == 262144
                   and Path(row.stderr_log).stat().st_size == 262144 for row in rows)
    finally:
        workload.cancel()
    assert not workload.children()
    proof["dual_pipe_commands"] = 100
    proof["concurrency"] = 50

    workload = owner(root, "orphan")
    orphan = root / "orphan-pid.txt"
    code = (
        "import subprocess,sys;from pathlib import Path;"
        "p=subprocess.Popen([sys.executable,'-c','import time;time.sleep(120)'],"
        "start_new_session=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL);"
        "Path(sys.argv[1]).write_text(str(p.pid))"
    )
    workload.run_cmd("parent exits first", [sys.executable, "-c", code, str(orphan)], root)
    assert int(orphan.read_text()) in workload.children()
    workload.cancel()
    gone([int(orphan.read_text())])
    assert not workload.children()
    proof["already_orphaned_child_reaped"] = True
    os.kill(external, 0)
    proof["unrelated_sibling_survived"] = True
    proof["uid"] = os.getuid()
    try:
        os.waitpid(-1, os.WNOHANG)
    except ChildProcessError:
        pass
    else:
        raise AssertionError("unreaped child remains")
    proof["status"] = "passed"
    (root / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")
    print(json.dumps(proof))


if __name__ == "__main__":
    main(Path(sys.argv[1]))
