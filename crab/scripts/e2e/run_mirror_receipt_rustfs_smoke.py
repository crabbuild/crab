#!/usr/bin/env python3
"""Qualify mirror receipt recovery after publisher death and tagged compaction.

Requires local RustFS, a candidate binary and a tagged rollback binary. Every
mutation and injected failure targets the run's disposable repository prefix.
"""

import http.client
import http.server
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import threading


SPEC = importlib.util.spec_from_file_location(
    "protocol_smoke", Path(__file__).with_name("run_protocol_v2_partial_clone_rustfs_smoke.py")
)
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)


class MarkerProxy(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def forward(self):
        if self.headers.get("Transfer-Encoding"):
            self.send_error(501, "qualification proxy requires content length")
            return
        length = int(self.headers.get("Content-Length", "0"))
        if length > 8 * 1024 * 1024:
            self.send_error(413, "qualification fixture exceeded body bound")
            return
        body = self.rfile.read(length)
        connection = http.client.HTTPConnection("127.0.0.1", 9000, timeout=30)
        try:
            connection.request(self.command, self.path, body=body,
                               headers=dict(self.headers.items()))
            response = connection.getresponse()
            data = response.read()
            headers = response.getheaders()
            path = self.path.split("?", 1)[0]
            if (self.command == "PUT" and path.startswith(self.server.marker_prefix)
                    and 200 <= response.status < 300):
                self.server.marker_writes.append(path)
                if self.server.armed:
                    self.server.armed = False
                    self.server.accepted.set()
                    if not self.server.caller_dead.wait(30):
                        raise RuntimeError("marker was accepted but caller was not killed")
            self.send_response(response.status)
            for key, value in headers:
                if key.lower() not in {"transfer-encoding", "connection"}:
                    self.send_header(key, value)
            if not any(key.lower() == "content-length" for key, _ in headers):
                self.send_header("Content-Length", str(len(data)))
            self.send_header("Connection", "close")
            self.end_headers()
            if self.command != "HEAD":
                self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            connection.close()
            self.close_connection = True

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
    remote = smoke.remote_url
    prefix = f"{RUNNER.REMOTE_PREFIX}/{smoke.run_id}"
    proxy.marker_prefix = f"/{smoke.args.bucket}/{prefix}/refs/journal/active/"
    smoke.run_git(smoke.run_root, ["init", "-b", "main", str(source)])
    smoke.run_git(smoke.run_root, ["init", "--bare", str(upstream)])
    smoke.run_git(source, ["remote", "add", "origin", str(upstream)])
    smoke.run_git(source, ["config", "core.hooksPath", ".git/hooks"])
    (source / "readme.txt").write_text("exact committed content\n")
    smoke.run_git(source, ["add", "readme.txt"])
    smoke.run_git(source, ["commit", "-m", "atomic planned import"])
    smoke.run_git(source, ["tag", "v1"])
    revision = smoke.git_value(source, ["rev-parse", "HEAD"], name="fixture revision")
    smoke.record_provenance(revision, health)
    smoke.run_cmd("initialize empty mirror destination",
                  [str(smoke.crab_bin), "init", "--mirror=origin", remote], source)
    plan = smoke.artifacts / "plan.json"
    mirror = [str(smoke.crab_bin), "mirror", str(source), remote]
    smoke.run_cmd("plan exact first import",
                  [*mirror, "--check", "--write-plan", str(plan), "--json"], smoke.run_root)
    plan_id = json.loads(plan.read_text())["plan_id"]
    journal = f"{prefix}/refs/journal/"

    def inventory(name, suffix):
        result = smoke.run_aws(
            ["list-objects-v2", "--bucket", smoke.args.bucket, "--prefix", journal + suffix],
            name=name)
        return [item["Key"] for item in json.loads(smoke.stdout(result)).get("Contents", [])]

    threads = []

    def interrupt_after_commit(process):
        def interrupt():
            if proxy.accepted.wait(60):
                # Git supervision deliberately creates child process groups.
                # Pin the still-blocked publisher tree before killing its root.
                rows = subprocess.check_output(
                    ["ps", "-axo", "pid=,ppid="], text=True
                ).splitlines()
                children = {}
                for row in rows:
                    pid, parent = map(int, row.split())
                    children.setdefault(parent, []).append(pid)
                descendants = []
                pending = [process.pid]
                while pending:
                    parent = pending.pop()
                    descendants.append(parent)
                    pending.extend(children.get(parent, []))
                smoke.report["interrupted_process_count"] = len(descendants)
                for pid in reversed(descendants):
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                os.killpg(process.pid, signal.SIGKILL)
                proxy.caller_dead.set()
        thread = threading.Thread(target=interrupt)
        thread.start()
        threads.append(thread)

    apply = [*mirror, "--apply-plan", str(plan), "--json"]
    proxy.armed = True
    interrupted = smoke.run_cmd("kill caller after accepted marker, before reply", apply,
                                smoke.run_root, check=False, on_started=interrupt_after_commit)
    for thread in threads:
        thread.join(65)
    smoke.check("caller-killed-after-exactly-one-accepted-marker",
                interrupted["exit_code"] == -signal.SIGKILL
                and proxy.caller_dead.is_set() and len(proxy.marker_writes) == 1)
    plans = inventory("plan evidence before recovery", f"plans/v1/{plan_id}/")
    smoke.check("intent-exists-without-terminal-receipt",
                len(plans) == 1 and "/attempts/00000001.json" in plans[0])
    active = inventory("active markers before tagged compaction", "active/")
    smoke.check("accepted-transaction-is-active", len(active) == 1)
    transaction = Path(active[0]).stem

    clone = smoke.run_root / "tagged-clone"
    try:
        smoke.install_helper_alias(smoke.rollback_crab_bin)
        smoke.run_git(smoke.run_root, ["clone", remote, str(clone)],
                      name="tagged v1.0.1 clone and journal compaction")
    finally:
        smoke.install_helper_alias()
    smoke.run_git(clone, ["fsck", "--strict", "--full"], name="tagged clone strict fsck")
    smoke.check("tagged-clone-reconstructs-original-bytes",
                (clone / "readme.txt").read_bytes() == (source / "readme.txt").read_bytes())
    smoke.check("tagged-compactor-removes-only-active-marker",
                not inventory("active markers after tagged compaction", "active/")
                and inventory("plan evidence after tagged compaction", f"plans/v1/{plan_id}/")
                == plans)

    recovered = smoke.json_data(
        smoke.run_cmd("restart caller and recover the planned terminal result", apply,
                      smoke.run_root), "mirror.apply")
    smoke.check("recovery-attributes-exact-transaction-without-republication",
                recovered.get("already_applied") is True
                and recovered.get("actions_applied") == 0
                and recovered.get("transaction_id") == transaction
                and recovered.get("final_state") == "equal"
                and len(proxy.marker_writes) == 1)
    repeated = smoke.json_data(
        smoke.run_cmd("repeat recovered plan", apply, smoke.run_root), "mirror.apply")
    smoke.check("terminal-replay-retains-commit-identity",
                repeated.get("transaction_id") == transaction
                and repeated.get("already_applied") is True
                and len(proxy.marker_writes) == 1)
    smoke.check("one-transaction-and-one-terminal-receipt-remain",
                len(inventory("retained transactions", "transactions/")) == 1
                and len(inventory("retained plan objects", f"plans/v1/{plan_id}/")) == 2)
    smoke.redaction_check()


def main():
    args = RUNNER.parse_args()
    if args.endpoint_url != "http://127.0.0.1:9000" or args.rollback_crab_bin is None:
        raise RuntimeError("this isolated qualification requires local RustFS and a tagged binary")
    proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), MarkerProxy)
    proxy.marker_prefix = "/unarmed/"
    proxy.marker_writes = []
    proxy.armed = False
    proxy.accepted = threading.Event()
    proxy.caller_dead = threading.Event()
    worker = threading.Thread(target=proxy.serve_forever, daemon=True)
    worker.start()
    args.endpoint_url = f"http://127.0.0.1:{proxy.server_port}"
    smoke = RUNNER.ProtocolV2PartialCloneSmoke(args)
    smoke.report["schema"] = "crab.mirror-marker-loss-smoke"
    smoke.report["version"] = "1.0"
    try:
        run(smoke, proxy)
        smoke.report["status"] = "passed"
        return 0
    except Exception as error:
        smoke.report["status"] = "failed"
        smoke.report["error"] = RUNNER.redact_text(
            smoke.redact_sensitive(str(error)), smoke.credentials()
        )
        print(type(error).__name__, file=sys.stderr)
        return 1
    finally:
        smoke.report["accepted_marker_writes"] = proxy.marker_writes
        smoke.write_report()
        proxy.shutdown()
        proxy.server_close()
        worker.join()


if __name__ == "__main__":
    raise SystemExit(main())
