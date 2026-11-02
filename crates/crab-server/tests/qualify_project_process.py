#!/usr/bin/env python3
"""Qualify project restart, crash, handoff and full-root restore on ten real repos.

Use a dedicated, stopped local preview with its existing config and AWS
environment. RustFS and the UI development server must already be running.
The preview is left running, including when a qualification assertion fails.
Artifacts contain request payloads and project data; keep them private.
"""

import argparse
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import re
import shutil
import signal
import socket
import ssl
import subprocess
import threading
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
import uuid

from project_root_backup import copy_root


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["binary", "config", "repositories", "artifacts", "pid-file"]:
        parser.add_argument("--" + name, required=True, type=Path)
    for name in ["organization", "project"]:
        parser.add_argument("--" + name, required=True, type=uuid.UUID)
    parser.add_argument("--ui-origin", default="http://127.0.0.1:5175")
    parser.add_argument("--phases", nargs="+", choices=["restart", "crash", "handoff", "backup"], default=["restart", "crash", "handoff", "backup"])
    args = parser.parse_args()
    config = tomllib.loads(args.config.read_text())
    host, port = config["listen"].rsplit(":", 1)
    assert host in ["127.0.0.1", "localhost"], "Use an isolated loopback preview"
    assert urllib.parse.urlsplit(args.ui_origin).hostname in ["127.0.0.1", "localhost"]
    with socket.socket() as probe:
        probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        probe.bind((host, int(port)))
        probe.listen(1)
    repositories = json.loads(args.repositories.read_text())
    names = [(repo["owner"], repo["name"]) for repo in repositories]
    assert len(names) == len(set(names)) == 10, "Expected ten distinct real repositories"
    for repo in repositories:
        assert repo["commits"] >= 100 and repo["files"] >= 100
    run = args.artifacts.resolve() / str(uuid.uuid4())
    run.mkdir(parents=True, mode=0o700)
    origin = f"http://{config['listen']}"
    organization_projects = f"/api/organizations/{args.organization}/projects"
    driver = Path(__file__).resolve().parents[3] / "packages/ui/tests/qualification/project-recovery.mjs"
    with args.binary.open("rb") as executable:
        binary_hash = hashlib.file_digest(executable, "sha256").hexdigest()
    report = {"binary_sha256": binary_hash, "phases": []}
    server = None
    browser = None
    servers = []
    proxy = None
    cleanup_backup = None
    serial = 0
    tls = ssl.create_default_context(cafile=config["cells"]["peer_ca"])
    tls.load_cert_chain(config["cells"]["peer_certificate"], config["cells"]["peer_private_key"])
    management = "https://" + config["management_listen"]

    def private_request(path, address=None, method="GET"):
        return urllib.request.urlopen(urllib.request.Request((address or management) + path, method=method), context=tls, timeout=35)

    def metrics():
        with private_request("/metrics") as response:
            body = response.read().decode()
        for name in ["owned_cells", "resident_cells", "restoring_cells", "ownership_confirmed", "memory_pressured", "native_draining", "shed_cells_total"]:
            assert "\ncrab_cellar_" + name + " " in body, name
        (run / f"metrics-{serial}.prom").write_text(body)

    def save():
        (run / "report.json").write_text(json.dumps(report, indent=2))

    def request(path):
        try:
            with urllib.request.urlopen(origin + path, timeout=35) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            raise RuntimeError(f"{path}: HTTP {error.code}; {error.read().decode()}") from error

    def start(path=args.config):
        nonlocal server, serial, origin, management
        current = tomllib.loads(path.read_text())
        origin = "http://" + current["listen"]
        management = "https://" + current["management_listen"]
        serial += 1
        with (run / f"server-{serial}.log").open("wb") as log:
            server = subprocess.Popen(
                [str(args.binary.resolve()), "--config", str(path.resolve()), "serve"],
                stdout=log, stderr=subprocess.STDOUT, start_new_session=True,
            )
        servers.append(server)
        args.pid_file.write_text(str(server.pid))
        deadline = time.monotonic() + 360
        while time.monotonic() < deadline:
            if server.poll() is not None:
                raise RuntimeError(f"Server exited {server.returncode}; inspect server-{serial}.log")
            try:
                with urllib.request.urlopen(origin + "/api/repos", timeout=2) as response:
                    assert response.status == 200
                    metrics()
                    return
            except (urllib.error.URLError, TimeoutError, ConnectionError):
                time.sleep(0.25)
        raise TimeoutError("Repository recovery did not complete within six minutes")

    def refs():
        result = {}
        for owner, name in names:
            current = request(
                "/api/repos/" + urllib.parse.quote(owner, safe="") + "/" + urllib.parse.quote(name, safe="") + "/refs"
            )
            # Index maintenance can change a generation without changing Git.
            result[owner + "/" + name] = {key: current[key] for key in ["head", "unborn_head", "refs"]}
        return result

    def project_snapshot(project, page_key):
        detail = request(project)
        items = []
        after = None
        revision = None
        while True:
            query = urllib.parse.urlencode({"after": after, page_key: revision}) if after else ""
            page = request(project + "/items?" + query)["page"]
            items.extend(page["items"])
            revision = page["revision"]
            after = page["next"]
            if after is None:
                break
        history = []
        after = 0
        while True:
            page = request(project + "/history?after=" + str(after))
            history.extend(page["items"])
            if page["next"] is None:
                break
            assert page["next"] > after
            after = page["next"]
        return {"detail": detail, "items": items, "history": history}

    def snapshot():
        scopes = [(organization_projects, "version")]
        scopes.extend((f"/api/repos/{urllib.parse.quote(owner, safe='')}/{urllib.parse.quote(name, safe='')}/projects", "revision") for owner, name in names)
        result = {}
        for root, page_key in scopes:
            projects = {}
            for archived in [False, True]:
                after = None
                revision = None
                while True:
                    parameters = {"archived": str(archived).lower()}
                    if after is not None:
                        parameters["after"] = after
                        if revision is not None:
                            parameters["revision"] = revision
                    listing = request(root + "?" + urllib.parse.urlencode(parameters))
                    for entry in listing["items"]:
                        projects[entry["id"]] = project_snapshot(root + "/" + entry["id"], page_key)
                    after = listing["next"]
                    revision = listing.get("revision")
                    if after is None:
                        break
            result[root] = projects
        assert str(args.project) in result[organization_projects]
        return result

    def diagnostics():
        results = []
        for owner, name in names:
            started = time.monotonic()
            path = f"/internal/repositories/{urllib.parse.quote(owner, safe='')}/{urllib.parse.quote(name, safe='')}/cell"
            try:
                with private_request(path) as response:
                    result = {"status": response.status, "body": json.load(response)}
            except urllib.error.HTTPError as error:
                result = {"status": error.code, "body": json.load(error)}
            except (urllib.error.URLError, TimeoutError, ConnectionError) as error:
                result = {"status": None, "error": str(error)}
            results.append(dict(result, repository=owner + "/" + name, elapsed=time.monotonic() - started))
        (run / f"cell-diagnostics-{serial}.json").write_text(json.dumps(results, indent=2))
        return all(result["status"] == 200 and all(result["body"][feature]["state"] == "ready" for feature in ["projects", "projection"]) for result in results)

    def pending_fixture(name):
        nonlocal browser
        artifacts = run / name
        artifacts.mkdir(mode=0o700)
        with (artifacts / "browser.log").open("wb") as log:
            browser = subprocess.Popen([
                "node", str(driver), "--origin", args.ui_origin,
                "--organization", str(args.organization), "--project", str(args.project),
                "--artifacts", str(artifacts), "--resume", str(artifacts / "resume.json"),
                "--resume-timeout-seconds", "7200",
                "--loss", "request" if name in ["crash", "backup"] else "reply",
            ], stdout=log, stderr=subprocess.STDOUT)
        pending_path = artifacts / "pending.json"
        deadline = time.monotonic() + 120
        while not pending_path.exists():
            if browser.poll() is not None:
                raise RuntimeError(f"Browser fixture failed: {artifacts / 'browser.log'}")
            if time.monotonic() >= deadline:
                raise TimeoutError("Browser did not retain a pending submission")
            time.sleep(0.25)
        pending = json.loads(pending_path.read_text())
        before = snapshot()
        (artifacts / "before.json").write_text(json.dumps(before, indent=2))
        return artifacts, pending, before

    def recovered(name, artifacts, pending, before, started, **proof):
        nonlocal browser
        assert refs() == original_refs, "Git references changed across recovery"
        after = snapshot()
        (artifacts / "after.json").write_text(json.dumps(after, indent=2))
        assert after == before, "Project fields, items, or history changed across recovery"
        assert diagnostics(), "Native Cell diagnostics failed"
        resume = artifacts / "resume.json"
        temporary = resume.with_suffix(".tmp")
        temporary.write_text(json.dumps({"request_id": pending["input"]["request_id"]}))
        temporary.replace(resume)
        assert browser.wait(timeout=120) == 0, "Browser did not recover the original submission"
        browser = None
        assert json.loads((artifacts / "verified.json").read_text())["verified"]
        phase = {"phase": name, "recovery_seconds": time.monotonic() - started, "request_id": pending["input"]["request_id"], "verified": True, **proof}
        report["phases"].append(phase)
        save()
        print(json.dumps(phase), flush=True)

    def successor_config(name, storage=None):
        ports = []
        for _ in range(2):
            with socket.socket() as probe:
                probe.bind((host, 0))
                ports.append(probe.getsockname()[1])
        assert ports[0] != ports[1]
        text = args.config.read_text()
        values = {"listen": f"{host}:{ports[0]}", "management_listen": f"{host}:{ports[1]}", "peer_advertise": f"https://{host}:{ports[1]}", "data_dir": str(run / (name + "-cells"))}
        if storage:
            values["url"] = storage
        for key, value in values.items():
            text, count = re.subn(r"(?m)^" + re.escape(key) + r"\s*=.*$", lambda _: key + " = " + json.dumps(value), text)
            assert count == 1, key
        path = run / (name + ".toml")
        path.write_text(text)
        path.chmod(0o600)
        return path

    def forward_ui():
        nonlocal proxy
        upstream = urllib.parse.urlsplit(origin)
        if proxy is not None:
            proxy.shutdown()
            proxy.server_close()
        # Keep the browser's origin and live tab unchanged: sessionStorage is
        # origin-bound and cannot be claimed recovered by opening another origin.
        class Forward(BaseHTTPRequestHandler):
            def do_GET(self):
                self.forward()

            def do_POST(self):
                self.forward()

            def forward(self):
                excluded = {"connection", "transfer-encoding", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer", "upgrade"}
                connection = http.client.HTTPConnection(upstream.hostname, upstream.port, timeout=35)
                try:
                    body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                    connection.request(self.command, self.path, body=body, headers={key: value for key, value in self.headers.items() if key.lower() not in excluded})
                    response = connection.getresponse()
                    payload = response.read()
                    self.send_response(response.status)
                    for key, value in response.getheaders():
                        if key.lower() not in excluded | {"content-length"}:
                            self.send_header(key, value)
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                except (OSError, http.client.HTTPException):
                    self.send_error(503, "Qualification upstream unavailable")
                finally:
                    connection.close()

            def log_message(self, *_):
                pass

        proxy = ThreadingHTTPServer((host, int(port)), Forward)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()

    try:
        start()
        original_refs = refs()
        (run / "repositories.json").write_text(json.dumps(original_refs, indent=2))
        # Load the organization before the browser fixture. Cold first-use
        # availability is checked separately from preserving a submitted write.
        initial = snapshot()
        report["planning_projects"] = sum(len(projects) for projects in initial.values())
        for name, stop_signal in [("restart", signal.SIGTERM), ("crash", signal.SIGKILL)]:
            if name not in args.phases:
                continue
            artifacts, pending, before = pending_fixture(name)
            server.send_signal(stop_signal)
            exit_code = server.wait(timeout=180)
            assert exit_code == (0 if name == "restart" else -signal.SIGKILL), exit_code
            started = time.monotonic()
            start()
            recovered(name, artifacts, pending, before, started, exit_code=exit_code)
        if "handoff" in args.phases:
            artifacts, pending, before = pending_fixture("handoff")
            source, address = server, management
            started = time.monotonic()
            start(successor_config("successor"))
            with private_request("/internal/cells/v1/scale-down", address, "POST") as response:
                assert response.status == 202
            assert source.wait(timeout=180) == 0, "Source did not finish native handoff"
            forward_ui()
            recovered("handoff", artifacts, pending, before, started, source_pid=source.pid, successor_pid=server.pid)
        if "backup" in args.phases:
            artifacts, pending, before = pending_fixture("backup")
            server.terminate()
            assert server.wait(timeout=180) == 0, "Writer did not stop before backup"
            assert all(process.poll() is not None for process in servers), "A writer remains active"
            # The successful handoff already has remote-data proof. Reclaim only
            # this run's stopped successor cache before making a full root copy.
            successor_cache = run / "successor-cells"
            if successor_cache.exists():
                shutil.rmtree(successor_cache)
            started = time.monotonic()
            storage, proof, cleanup_backup = copy_root(config["storage"]["url"], artifacts)
            start(successor_config("restored", storage))
            forward_ui()
            recovered("backup", artifacts, pending, before, started, root=proof)
        report["recovery_verified"] = True
        report["availability_verified"] = diagnostics()
        assert report["availability_verified"], "Native Cell diagnostics failed; inspect cell-diagnostics.json"
        report["verified"] = True
    except Exception as error:
        report["error"] = str(error)
        raise
    finally:
        if browser is not None and browser.poll() is None:
            browser.terminate()
            try:
                browser.wait(timeout=10)
            except subprocess.TimeoutExpired:
                # A failed browser must not prevent draining owned servers or
                # saving the original failure. This is teardown, not crash proof.
                browser.kill()
                browser.wait(timeout=10)
        if proxy is not None:
            proxy.shutdown()
            proxy.server_close()
        # A returned preview always uses its original root/config. Never start
        # over another live writer merely because an observation timed out.
        live = [process for process in servers if process.poll() is None]
        for process in live:
            process.terminate()
            try:
                process.wait(timeout=180)
            except subprocess.TimeoutExpired:
                report.setdefault("shutdown_pending_pids", []).append(process.pid)
        if server is not None and all(process.poll() is not None for process in servers):
            try:
                start()
            except Exception as error:
                report["preview_restart_error"] = str(error)
        if cleanup_backup is not None and not report.get("shutdown_pending_pids"):
            cleanup_backup()
        report["preview_pid"] = server.pid if server is not None else None
        save()
        print(json.dumps({"artifacts": str(run), "preview_pid": report["preview_pid"]}), flush=True)


if __name__ == "__main__":
    main()
