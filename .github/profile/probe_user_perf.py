"""Probe actual userspace leaf offsets before preparing a push workload."""

import argparse
import hashlib
import json
from pathlib import Path
import signal
import sys

from diagnose_push_user import ProfiledPush, parse_args
from perf_permissions import user_perf
from user_cpu_symbols import Symbols, classify, decode, NORMAL_SHA256


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--perf-bin", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--normal-bin", type=Path, required=True)
    parser.add_argument("--link-map", type=Path, required=True)
    args = parser.parse_args()
    binary = args.normal_bin.resolve(strict=True)
    manifest = json.loads(binary.with_name("candidate-build.json").read_text())
    assert manifest["sha256"] == NORMAL_SHA256
    assert manifest["source"] == "40d4bc0d745a2e1405bf5d5004d05a295b1c0245"
    with user_perf(args.perf_bin, args.output) as (perf, root, proof):
        owner_args = parse_args(["--root", str(root), "--run-id", "probe-owner",
                                 "--crab-bin", str(binary)])
        owner = ProfiledPush(owner_args, perf, root)
        owner.artifacts = owner.run_root / "receipts"
        previous = {number: signal.signal(number, lambda *_: owner.cancel())
                    for number in (signal.SIGINT, signal.SIGTERM)}

        def managed_capture(command, destination):
            row = owner.run_cmd(destination.name, command, root, timeout=30)
            destination.write_bytes(Path(row.stdout_log).read_bytes())
            destination.with_suffix(".stderr").write_bytes(Path(row.stderr_log).read_bytes())

        try:
            # No frame-pointer build flag is added. Only the sampled leaf is attributed;
            # incomplete parent chains remain raw evidence and are never guessed.
            runner = ("import json,subprocess,sys\n"
                      "for _ in range(64):\n"
                      " value=json.loads(subprocess.check_output([sys.argv[1],'version','--json']))\n"
                      " print(json.dumps(value),flush=True)\n")
            command = [str(perf), "record", "--strict-freq", "-e", "cpu-clock:u", "-F", "999",
                       "-m", "8", "--no-buildid", "--call-graph", "fp", "--user-callchains",
                       "--sample-cpu", "-P", "-o", str(root / "probe.data"), "--",
                       sys.executable, "-c", runner, str(binary)]
            proof["record_command"] = command
            managed_capture(command, root / "record.stdout")
            versions = [json.loads(line) for line in (root / "record.stdout").read_text().splitlines()]
            assert len(versions) == 64
            assert all(value["schema"] == "version" and value["data"] == manifest["version"]["data"]
                       for value in versions)
            render = [str(perf), "script", "--ns", "--show-kernel-path", "--show-task-events", "--show-mmap-events",
                      "--show-lost-events", "-i", str(root / "probe.data"),
                      "-F", "comm,pid,tid,cpu,time,event,ip,sym,dso,dsoff,period"]
            proof["render_command"] = render
            managed_capture(render, root / "probe.script")
            samples, metadata = decode((root / "probe.script").read_text())
            symbols = Symbols(binary, args.link_map)
            summary = classify(samples, metadata, symbols)
            lost = [line for line in metadata if "PERF_RECORD_LOST" in line]
            (root / "samples.json").write_text(json.dumps({"samples": samples, "metadata": metadata,
                                                         "summary": summary}, indent=2) + "\n")
            proof.update(summary)
            proof.update({"symbol_index": symbols.proof, "versions_checked": len(versions),
                          "data_sha256": hashlib.file_digest((root / "probe.data").open("rb"), "sha256").hexdigest(),
                          "lost_records": lost})
            if owner.stopping or len(samples) < 50 or summary["normal_mapped_samples"] < 10 or lost:
                raise RuntimeError("insufficient actual normal userspace samples or recorded loss")
        finally:
            owner.cancel()
            for number, handler in previous.items():
                signal.signal(number, handler)


if __name__ == "__main__":
    main()
