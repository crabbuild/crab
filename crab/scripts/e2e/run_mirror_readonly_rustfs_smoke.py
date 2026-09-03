#!/usr/bin/env python3
"""Qualify mirror inspection with every remote mutation denied and recorded.

Equal/source-ahead and a cold-cache Crab-ahead case use one isolated prefix.
The cold-cache case currently exposes the upload-pack read-admission write gap;
retain a failing report until that production path supports read-only grants.
"""

import importlib.util
from pathlib import Path
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
    worker = threading.Thread(target=proxy.serve_forever, daemon=True)
    worker.start()
    args.endpoint_url = f"http://127.0.0.1:{proxy.server_port}"
    smoke = RUNNER.ProtocolV2PartialCloneSmoke(args)
    smoke.report["schema"] = "crab.mirror-readonly-smoke"
    smoke.report["version"] = "1.0"
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
        smoke.report["status"] = "passed"
        return 0
    except Exception as error:
        smoke.report["status"] = "failed"
        smoke.report["error"] = RUNNER.redact_text(str(error), smoke.credentials())
        return 1
    finally:
        smoke.report["denied_write_attempts"] = proxy.denied
        smoke.write_report()
        proxy.shutdown()
        proxy.server_close()
        worker.join()


if __name__ == "__main__":
    raise SystemExit(main())
