#!/usr/bin/env python3
"""Qualify mirror inspection with every remote mutation denied and recorded.

Equal/source-ahead, cold-cache Crab-ahead, incomplete-cache recovery, and
divergence use one isolated prefix with all remote mutations denied.
"""

import importlib.util
from pathlib import Path
import signal
import threading


SPEC = importlib.util.spec_from_file_location(
    "receipt_smoke",
    Path(__file__).with_name("run_mirror_receipt_rustfs_smoke.py"),
)
RECEIPT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RECEIPT)
RUNNER = RECEIPT.RUNNER


class ReadOnlyProxy(RECEIPT.MarkerProxy):
    def forward(self):
        path = self.path.split("?", 1)[0]
        if (self.server.block_reads and self.command == "GET"
                and path.startswith(self.server.fault_prefix) and path.endswith(".idx")):
            self.server.read_entered.set()
            if not self.server.release_reads.wait(30):
                self.send_error(504, "qualification read was not released")
                return
        fault = self.server.read_fault
        if (fault and self.command == "GET" and path.startswith(self.server.fault_prefix)
                and path.endswith(fault[1])):
            self.server.fault_hits.append({"fault": fault[0], "path": path})
            body = (b"<Error><Code>NoSuchKey</Code></Error>" if fault[2] == 404
                    else b"injected invalid immutable Git object")
            self.send_response(fault[2])
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)
            self.close_connection = True
            return
        if self.server.deny_writes and self.command not in {"GET", "HEAD"}:
            self.server.denied.append({"method": self.command, "path": self.path.split("?", 1)[0]})
            body = b"<Error><Code>AccessDenied</Code><Message>Read-only qualification</Message></Error>"
            self.send_response(403)
            self.send_header("Content-Type", "application/xml")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)
            self.close_connection = True
            return
        super().forward()

    do_GET = forward
    do_HEAD = forward
    do_PUT = forward
    do_POST = forward
    do_DELETE = forward


def main():
    args = RUNNER.parse_args()
    if args.endpoint_url != "http://127.0.0.1:9000":
        raise RuntimeError("requires local RustFS")
    proxy = RECEIPT.http.server.ThreadingHTTPServer(("127.0.0.1", 0), ReadOnlyProxy)
    proxy.marker_prefix = "/unarmed/"
    proxy.marker_writes = []
    proxy.armed = False
    proxy.deny_writes = False
    proxy.denied = []
    proxy.read_fault = None
    proxy.fault_hits = []
    proxy.fault_prefix = "/unarmed/"
    proxy.block_reads = False
    proxy.read_entered = threading.Event()
    proxy.release_reads = threading.Event()
    worker = threading.Thread(target=proxy.serve_forever, daemon=True)
    worker.start()
    args.endpoint_url = f"http://127.0.0.1:{proxy.server_port}"
    smoke = RUNNER.ProtocolV2PartialCloneSmoke(args)
    smoke.report["schema"] = "crab.mirror-readonly-smoke"
    smoke.report["version"] = "1.0"
    proxy.fault_prefix = f"/{args.bucket}/{RUNNER.REMOTE_PREFIX}/{smoke.run_id}/packs/"
    try:
        health = smoke.endpoint_health()
        smoke.ensure_bucket()
        source = smoke.run_root / "source"
        upstream = smoke.run_root / "upstream.git"
        smoke.run_git(smoke.run_root, ["init", "-b", "main", str(source)])
        smoke.run_git(smoke.run_root, ["init", "--bare", str(upstream)])
        smoke.run_git(source, ["remote", "add", "origin", str(upstream)])
        smoke.run_git(source, ["config", "core.hooksPath", ".git/hooks"])
        (source / "readme.txt").write_text("original content\n")
        smoke.run_git(source, ["add", "readme.txt"])
        smoke.run_git(source, ["commit", "-m", "initial read-only fixture"])
        revision = smoke.git_value(source, ["rev-parse", "HEAD"], name="fixture revision")
        smoke.record_provenance(revision, health)
        smoke.run_cmd("initialize isolated mirror destination",
                      [str(smoke.crab_bin), "init", "--mirror=origin", smoke.remote_url], source)
        smoke.run_git(source, ["push", smoke.remote_url, "main:main"], name="seed remote")
        mirror = [str(smoke.crab_bin), "mirror", str(source), smoke.remote_url, "--check", "--json"]
        proxy.deny_writes = True
        equal = smoke.json_data(smoke.run_cmd("read-only equal check", mirror, smoke.run_root),
                                "mirror.check")
        smoke.check("equal-check-passes-with-zero-write-attempts",
                    equal.get("state") == "equal" and equal.get("ci_passed") is True
                    and not proxy.denied)
        (source / "readme.txt").write_text("new source content\n")
        smoke.run_git(source, ["add", "readme.txt"])
        smoke.run_git(source, ["commit", "-m", "source advance"])
        ahead = smoke.json_data(smoke.run_cmd("read-only source-ahead check", mirror, smoke.run_root),
                                "mirror.check")
        smoke.report["source_ahead_result"] = ahead
        smoke.report["denied_write_attempts"] = proxy.denied
        smoke.check("source-ahead-check-passes-with-zero-write-attempts",
                    ahead.get("state") == "source_ahead"
                    and ahead.get("pointers", {}).get("state") == "verified"
                    and not proxy.denied)
        intermediate = smoke.git_value(source, ["rev-parse", "HEAD"], name="intermediate revision")
        (source / "remote-only.txt").write_text("second remote commit\n")
        smoke.run_git(source, ["add", "remote-only.txt"])
        smoke.run_git(source, ["commit", "-m", "second remote advance"])
        proxy.deny_writes = False
        smoke.run_git(source, ["push", smoke.remote_url, "main:main"], name="advance Crab destination")
        smoke.run_git(source, ["reset", "--hard", revision], name="restore disposable source's older tip")
        proxy.deny_writes = True
        fresh = [*mirror, "--cache-dir", str(smoke.run_root / "fresh-crab-ahead-cache.git")]
        crab_ahead = smoke.json_data(
            smoke.run_cmd("read-only Crab-ahead check with missing local history", fresh, smoke.run_root),
            "mirror.check")
        smoke.report["crab_ahead_result"] = crab_ahead
        smoke.report["denied_write_attempts"] = proxy.denied
        smoke.check("crab-ahead-check-passes-with-zero-write-attempts",
                    crab_ahead.get("state") == "crab_ahead"
                    and crab_ahead.get("pointers", {}).get("state") == "verified"
                    and not proxy.denied)
        # Only this run's imported loose object is removed. Retaining its child
        # models an interrupted import and proves cached tips are not a frontier.
        cached_parent = (smoke.run_root / "fresh-crab-ahead-cache.git" / "objects"
                         / intermediate[:2] / intermediate[2:])
        cached_parent.unlink()
        recovered = smoke.json_data(
            smoke.run_cmd("read-only incomplete-cache recovery", fresh, smoke.run_root), "mirror.check")
        smoke.check("cached-tip-does-not-hide-missing-parent",
                    recovered.get("state") == "crab_ahead" and cached_parent.is_file()
                    and not proxy.denied)
        (source / "local-only.txt").write_text("divergent local commit\n")
        smoke.run_git(source, ["add", "local-only.txt"])
        smoke.run_git(source, ["commit", "-m", "divergent source"])
        diverged = smoke.json_data(
            smoke.run_cmd("read-only divergent check", [*mirror, "--cache-dir",
                          str(smoke.run_root / "fresh-diverged-cache.git")], smoke.run_root), "mirror.check")
        smoke.check("divergent-check-passes-with-zero-write-attempts",
                    diverged.get("state") == "diverged"
                    and diverged.get("pointers", {}).get("state") == "verified"
                    and not proxy.denied)
        for fault in [("missing-index", ".idx", 404), ("corrupt-index", ".idx", 200),
                      ("missing-pack", ".pack", 404), ("corrupt-pack", ".pack", 200)]:
            proxy.read_fault = fault
            before_faults = len(proxy.fault_hits)
            failed = smoke.json_data(
                smoke.run_cmd(f"read-only {fault[0]} refusal", [*mirror, "--cache-dir",
                              str(smoke.run_root / f"{fault[0]}-cache.git")], smoke.run_root),
                "mirror.check")
            smoke.check(f"{fault[0]}-cannot-produce-clean-result",
                        failed.get("state") == "unverifiable" and not failed.get("ci_passed")
                        and len(proxy.fault_hits) > before_faults and not proxy.denied)
        proxy.read_fault = None
        interrupted_cache = smoke.run_root / "cancelled-history-cache.git"
        proxy.block_reads = True

        def cancel_blocked_read(process):
            if not proxy.read_entered.wait(10):
                raise RuntimeError("inspection did not reach the blocked canonical index read")
            process.send_signal(signal.SIGTERM)

        try:
            cancelled = smoke.run_cmd(
                "cancel read-only inspection during canonical index read",
                [*mirror, "--cache-dir", str(interrupted_cache)], smoke.run_root,
                check=False, timeout=15, on_started=cancel_blocked_read)
            smoke.check("cancelled-read-returns-before-origin-response",
                        cancelled["exit_code"] == 10
                        and proxy.read_entered.is_set() and not proxy.release_reads.is_set()
                        and not proxy.denied)
        finally:
            proxy.block_reads = False
            proxy.release_reads.set()
        resumed = smoke.json_data(
            smoke.run_cmd("reuse cache after cancelled canonical read",
                          [*mirror, "--cache-dir", str(interrupted_cache)], smoke.run_root),
            "mirror.check")
        smoke.check("cancelled-read-releases-cache-for-retry",
                    resumed.get("state") == "diverged"
                    and resumed.get("pointers", {}).get("state") == "verified"
                    and not proxy.denied)
        smoke.report["status"] = "passed"
        return 0
    except Exception as error:
        smoke.report["status"] = "failed"
        smoke.report["error"] = RUNNER.redact_text(str(error), smoke.credentials())
        return 1
    finally:
        proxy.release_reads.set()
        smoke.report["denied_write_attempts"] = proxy.denied
        smoke.report["injected_read_faults"] = proxy.fault_hits
        smoke.write_report()
        proxy.shutdown()
        proxy.server_close()
        worker.join()


if __name__ == "__main__":
    raise SystemExit(main())
