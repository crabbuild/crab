#!/usr/bin/env python3
"""Qualify cold LFS checkout and clone failure propagation against local RustFS."""

import hashlib
import http.server
import json
from pathlib import Path
import threading

from run_mirror_receipt_rustfs_smoke import MarkerProxy, RUNNER


class LfsReadProxy(MarkerProxy):
    def forward(self):
        path = self.path.split("?", 1)[0]
        if (self.server.deny_lfs and self.command in {"GET", "HEAD"}
                and path.startswith(self.server.lfs_prefix)):
            self.server.denied_reads.append(path)
            self.send_error(403, "qualification denies LFS reads")
            self.close_connection = True
            return
        super().forward()

    do_GET = forward
    do_HEAD = forward


def run(smoke, proxy):
    health = smoke.endpoint_health()
    smoke.check("rustfs-ready-through-proxy", health.get("ready") is True)
    smoke.ensure_bucket()
    source = smoke.run_root / "source"
    proxy.lfs_prefix = f"/{smoke.args.bucket}/{RUNNER.REMOTE_PREFIX}/{smoke.run_id}/lfs/objects/"
    smoke.run_git(smoke.run_root, ["init", "-b", "main", str(source)])
    smoke.run_cmd("initialize LFS source", [str(smoke.crab_bin), "init", smoke.remote_url], source)
    smoke.run_cmd("install source LFS filter", [str(smoke.crab_bin), "lfs", "install", "--local"], source)
    # Exercise both sides of crab.toml's checkout ordering, without preloading
    # project configuration or LFS objects into any fresh clone.
    files = {name: RUNNER.deterministic_bytes(128 * 1024, f"{smoke.run_id}:{name}")
             for name in ("a-lfs.bin", "z-lfs.bin")}
    (source / ".gitattributes").write_text("*.bin filter=lfs diff=lfs merge=lfs -text\n", encoding="utf-8")
    for name, content in files.items():
        (source / name).write_bytes(content)
    native_content = RUNNER.deterministic_bytes(256 * 1024, f"{smoke.run_id}:managed")
    (source / "managed.dat").write_bytes(native_content)
    smoke.run_cmd("track Crab payload", [str(smoke.crab_bin), "track", "managed.dat"], source)
    smoke.run_cmd("stage Crab payload", [str(smoke.crab_bin), "add", "managed.dat"], source)
    smoke.run_git(source, ["add", ".gitattributes", "crab.toml", "managed.dat", *files])
    smoke.run_git(source, ["commit", "-m", "cold LFS checkout fixture"])
    revision = smoke.git_value(source, ["rev-parse", "HEAD"], name="source revision")
    smoke.record_provenance(revision, health)
    smoke.run_cmd("publish LFS fixture", [str(smoke.crab_bin), "push"], source)

    for label, options in (("eager", ["--no-lazy"]), ("lazy", ["--lazy"]),
                           ("selective", ["--include", "managed.dat"])):
        clone = smoke.run_root / f"{label}-clone"
        result = smoke.run_cmd(f"cold {label} clone",
                               [str(smoke.crab_bin), "clone", *options, smoke.remote_url, str(clone), "--json"],
                               smoke.run_root)
        smoke.json_data(result, "clone")
        smoke.check(f"{label}-clone-exact-revision",
                    smoke.git_value(clone, ["rev-parse", "HEAD"], name=f"{label} clone revision") == revision)
        for name, content in files.items():
            if label != "eager":
                pointer = smoke.git_value(clone, ["show", f"HEAD:{name}"], name=f"{label} {name} pointer")
                smoke.check(f"{label}-{name}-remains-pointer", (clone / name).read_bytes() == (pointer + "\n").encode())
                output = smoke.artifacts / f"{label}-{name}"
                smoke.run_binary(f"{label} {name} explicit hydration",
                                 [str(smoke.crab_bin), "lfs", "smudge", name], clone, output,
                                 input_data=(pointer + "\n").encode())
            else:
                output = clone / name
            smoke.check(f"{label}-{name}-exact-bytes", output.read_bytes() == content)
        if label == "lazy":
            smoke.run_cmd("explicit Crab hydration after lazy clone",
                          [str(smoke.crab_bin), "hydrate", "managed.dat", "--json"], clone)
        smoke.check(f"{label}-exact-Crab-bytes", (clone / "managed.dat").read_bytes() == native_content)
        smoke.run_git(clone, ["fsck", "--strict", "--full"], name=f"{label} strict Git fsck")

    proxy.deny_lfs = True
    try:
        refused = smoke.run_cmd("cold eager clone with denied LFS content",
                                [str(smoke.crab_bin), "clone", "--no-lazy", smoke.remote_url,
                                 str(smoke.run_root / "denied-clone"), "--json"],
                                smoke.run_root, check=False)
    finally:
        proxy.deny_lfs = False
    smoke.check("denied-content-is-a-terminal-clone-failure",
                bool(proxy.denied_reads) and refused["exit_code"] not in (0, -124),
                {"denied_reads": proxy.denied_reads, "duration_ms": refused["duration_ms"]})
    smoke.check("denied-content-does-not-wait-for-filter-idle-timeout",
                refused["duration_ms"] < 10_000,
                {"duration_ms": refused["duration_ms"], "budget_ms": 10_000})
    failure = json.loads(smoke.stdout(refused))
    smoke.check("failed-clone-emits-error-without-success-data",
                failure.get("schema") == "clone" and isinstance(failure.get("error"), dict)
                and "data" not in failure)
    smoke.redaction_check()


def main():
    args = RUNNER.parse_args()
    if args.endpoint_url != "http://127.0.0.1:9000":
        raise RuntimeError("this scoped fault qualification requires local RustFS")
    proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), LfsReadProxy)
    proxy.marker_prefix = "/unarmed/"
    proxy.deny_lfs = False
    proxy.denied_reads = []
    worker = threading.Thread(target=proxy.serve_forever, daemon=True)
    worker.start()
    args.endpoint_url = f"http://127.0.0.1:{proxy.server_port}"
    smoke = RUNNER.ProtocolV2PartialCloneSmoke(args)
    binary_digest = hashlib.sha256(smoke.crab_bin.read_bytes()).hexdigest()
    smoke.report.update({"schema": "crab.lfs-cold-clone-smoke", "version": "1.0",
                         "candidate_binary_sha256": binary_digest,
                         "driver_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                         "valid_for_comparison": False,
                         "comparison_invalid_reason": "Functional cold checkout, not controlled performance."})
    try:
        run(smoke, proxy)
        smoke.check("candidate-binary-unchanged", hashlib.sha256(smoke.crab_bin.read_bytes()).hexdigest() == binary_digest)
        smoke.report["status"] = "passed"
    except Exception as error:
        smoke.report["status"] = "failed"
        smoke.report["error"] = RUNNER.redact_text(smoke.redact_sensitive(str(error)), smoke.credentials())
    finally:
        smoke.write_report()
        proxy.shutdown()
        proxy.server_close()
        worker.join()
    print(json.dumps({"status": smoke.report["status"], "error": smoke.report.get("error"),
                      "report": str(smoke.artifacts / "report.json")}))
    return 0 if smoke.report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
