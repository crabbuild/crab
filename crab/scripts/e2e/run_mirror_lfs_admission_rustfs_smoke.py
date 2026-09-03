#!/usr/bin/env python3
"""Reject writer admission through real installed hooks, then retry and hydrate.

Only the run's disposable repository prefix is faulted. Requires local RustFS;
uses the shared protocol runner for isolated fixtures, logs and provenance.
"""

import hashlib
import http.server
import json
from pathlib import Path
import threading

from run_mirror_receipt_rustfs_smoke import MarkerProxy, RUNNER


class AdmissionProxy(MarkerProxy):
    def forward(self):
        path = self.path.split("?", 1)[0]
        if self.server.deny_admission:
            if self.command in {"PUT", "POST", "DELETE"} and path.startswith(self.server.lfs_prefix):
                self.server.lfs_writes.append(path)
            if self.command == "PUT" and path.startswith(self.server.admission_prefix):
                self.server.denied_writes.append(path)
                # Close this connection without forwarding or consuming its
                # body; a retry must encounter the same scoped refusal.
                self.send_error(403, "qualification denies writer admission")
                self.close_connection = True
                return
        super().forward()

    do_GET = forward
    do_HEAD = forward
    do_PUT = forward
    do_POST = forward
    do_DELETE = forward


def run(smoke, proxy):
    health = smoke.endpoint_health()
    smoke.check("rustfs-ready-through-proxy", health.get("ready") is True)
    smoke.ensure_bucket()
    source = smoke.run_root / "source"
    upstream = smoke.run_root / "upstream.git"
    destination = smoke.remote_url
    prefix = f"{RUNNER.REMOTE_PREFIX}/{smoke.run_id}"
    proxy.admission_prefix = f"/{smoke.args.bucket}/{prefix}/locks/internal/push-admission/slots/"
    proxy.lfs_prefix = f"/{smoke.args.bucket}/{prefix}/lfs/objects/"
    smoke.run_git(smoke.run_root, ["init", "-b", "main", str(source)])
    smoke.run_git(smoke.run_root, ["init", "--bare", "-b", "main", str(upstream)])
    smoke.run_git(source, ["remote", "add", "origin", str(upstream)])
    smoke.run_git(source, ["config", "core.hooksPath", ".hooks"])
    smoke.run_cmd("initialize mirror", [str(smoke.crab_bin), "init", "--mirror=origin", destination], source)
    smoke.run_cmd("install LFS over mirror hook", [str(smoke.crab_bin), "lfs", "install", "--local"], source)
    (source / "baseline.txt").write_text("admission baseline\n", encoding="utf-8")
    smoke.run_git(source, ["add", "baseline.txt", "crab.toml"])
    smoke.run_git(source, ["commit", "-m", "admission baseline"])
    smoke.run_git(source, ["push", "origin", "main"])
    revision = smoke.git_value(source, ["rev-parse", "HEAD"], name="fixture baseline")
    smoke.record_provenance(revision, health)

    def refs(remote, label):
        return smoke.git_value(source, ["ls-remote", "--refs", remote], name=label)

    for label, remote in (("collaboration", "origin"), ("native", destination)):
        filename = f"{label}.bin"
        content = RUNNER.deterministic_bytes(128 * 1024, f"{smoke.run_id}:{label}")
        oid = hashlib.sha256(content).hexdigest()
        object_key = f"{prefix}/lfs/objects/{oid[:2]}/{oid[2:4]}/{oid}"
        (source / filename).write_bytes(content)
        attributes = source / ".gitattributes"
        with attributes.open("a", encoding="utf-8") as stream:
            stream.write(f"\n{filename} filter=lfs diff=lfs merge=lfs -text\n")
        smoke.run_git(source, ["add", ".gitattributes", filename])
        smoke.run_git(source, ["commit", "-m", f"{label} LFS dependency"])
        before = {target: refs(target, f"{label} {target} before denial")
                  for target in ("origin", destination)}
        proxy.denied_writes.clear()
        proxy.lfs_writes.clear()
        proxy.deny_admission = True
        try:
            refused = smoke.run_git(source, ["push", remote, "main"], check=False,
                                    name=f"{label} push with admission denied")
        finally:
            proxy.deny_admission = False
        inventory = smoke.run_aws(
            ["list-objects-v2", "--bucket", smoke.args.bucket, "--prefix", object_key],
            name=f"{label} rejected dependency inventory")
        detail = {"denied_writes": list(proxy.denied_writes),
                  "lfs_writes": list(proxy.lfs_writes),
                  "remote_objects": json.loads(smoke.stdout(inventory)).get("Contents", [])}
        smoke.report.setdefault("admission_cases", {})[label] = detail
        smoke.check(f"{label}-reaches-admission-and-fails", refused["exit_code"] != 0
                    and refused["exit_code"] != -124 and bool(proxy.denied_writes))
        smoke.check(f"{label}-denial-preserves-both-remotes",
                    all(refs(target, f"{label} {target} after denial") == value
                        for target, value in before.items()))
        smoke.check(f"{label}-no-LFS-upload-outside-admission",
                    not proxy.lfs_writes and not detail["remote_objects"], detail)
        smoke.run_git(source, ["push", remote, "main"], name=f"{label} retry after admission restored")
        if remote != "origin":
            smoke.run_git(source, ["push", "origin", "main"], name="converge collaboration after native retry")
        smoke.check(f"{label}-retry-converges", refs("origin", f"{label} retried origin")
                    == refs(destination, f"{label} retried Crab"))
        clone = smoke.run_root / f"{label}-clone"
        smoke.run_git(smoke.run_root, ["clone", destination, str(clone)], name=f"{label} fresh clone")
        pointer = smoke.git_value(clone, ["show", f"HEAD:{filename}"], name=f"{label} committed LFS pointer")
        hydrated = smoke.artifacts / f"{label}-hydrated.bin"
        smoke.run_binary(f"{label} fresh clone LFS smudge", [str(smoke.crab_bin), "lfs", "smudge", filename],
                         clone, hydrated, input_data=(pointer + "\n").encode())
        smoke.check(f"{label}-fresh-clone-exact-LFS-bytes", hydrated.read_bytes() == content)
        smoke.run_git(clone, ["fsck", "--strict", "--full"], name=f"{label} strict Git fsck")
    smoke.redaction_check()


def main():
    args = RUNNER.parse_args()
    if args.endpoint_url != "http://127.0.0.1:9000":
        raise RuntimeError("this scoped fault qualification requires local RustFS")
    proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), AdmissionProxy)
    proxy.marker_prefix = "/unarmed/"
    proxy.deny_admission = False
    proxy.denied_writes = []
    proxy.lfs_writes = []
    worker = threading.Thread(target=proxy.serve_forever, daemon=True)
    worker.start()
    args.endpoint_url = f"http://127.0.0.1:{proxy.server_port}"
    smoke = RUNNER.ProtocolV2PartialCloneSmoke(args)
    binary_digest = hashlib.sha256(smoke.crab_bin.read_bytes()).hexdigest()
    smoke.report.update({"schema": "crab.mirror-lfs-admission-smoke", "version": "1.0",
                         "candidate_binary_sha256": binary_digest,
                         "driver_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                         "valid_for_comparison": False,
                         "comparison_invalid_reason": "Functional fault injection, not controlled performance."})
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
