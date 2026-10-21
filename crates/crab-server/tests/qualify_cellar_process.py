#!/usr/bin/env python3
"""Qualify a server binary or Linux image against RustFS and process restart.

Requires Docker, AWS CLI, OpenSSL, and Git. Put --artifacts on the mounted
workspace volume. This creates only synthetic repositories and credentials.
"""

import argparse
import http.client
import json
import os
import pathlib
import socket
import subprocess
import time
import urllib.error
import urllib.request
import uuid

parser = argparse.ArgumentParser(description=__doc__)
executable = parser.add_mutually_exclusive_group(required=True)
executable.add_argument("--binary", type=pathlib.Path)
executable.add_argument("--image", help="Linux image to run using host networking")
parser.add_argument("--artifacts", required=True, type=pathlib.Path)
args = parser.parse_args()
root = args.artifacts.resolve()
root.mkdir(parents=True, exist_ok=True)
run = root / str(uuid.uuid4())
run.mkdir()
binary = args.binary.resolve() if args.binary else None
object_store_image = "ghcr.io/rustfs/rustfs:1.0.0-glibc@sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858"
name = "crab-cellar-process-" + uuid.uuid4().hex[:8]
server = None
with open(run / "commands.log", "w") as log:

    def cmd(args, **kw):
        return subprocess.run(
            [str(x) for x in args], check=True, stdout=log, stderr=log, **kw
        )

    def port():
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            return s.getsockname()[1]

    def request(path, body=None, *, method=None):
        req = urllib.request.Request(
            origin + path,
            data=None if body is None else json.dumps(body).encode(),
            headers={"Content-Type": "application/json"},
            method=method,
        )
        with urllib.request.urlopen(req, timeout=35) as r:
            return json.load(r)

    def wait():
        for i in range(100):
            if server.poll() is not None:
                raise RuntimeError("server exited: " + str(run / "server.log"))
            try:
                request("/api/repos")
                return
            except (urllib.error.URLError, TimeoutError, http.client.RemoteDisconnected):
                time.sleep(0.25)
        raise RuntimeError("server did not become available")

    def server_command(*arguments, long_running=False):
        if binary:
            executable = [str(binary)]
        else:
            executable = [
                "docker",
                "run",
                "--rm",
                "--network",
                "host",
                "--read-only",
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--tmpfs",
                "/var/lib/crab/tmp:rw,size=1g,uid=10001,gid=10001,mode=0700",
                "--volume",
                f"{run}:{run}",
                "--volume",
                f"{run / 'server.toml'}:/etc/crab/server.toml:ro",
            ]
            for variable in [
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_REGION",
                "AWS_DEFAULT_REGION",
                "AWS_ENDPOINT_URL_S3",
                "AWS_ALLOW_HTTP",
                "AWS_VIRTUAL_HOSTED_STYLE_REQUEST",
                "AWS_EC2_METADATA_DISABLED",
            ]:
                executable.extend(["--env", variable])
            if long_running:
                executable.extend(["--name", name + "-server"])
            executable.append(args.image)
        return executable + ["--config", str(run / "server.toml"), *arguments]

    def start():
        return subprocess.Popen(
            server_command("serve", long_running=True),
            env=env,
            stdout=open(run / "server.log", "a"),
            stderr=subprocess.STDOUT,
        )

    def stop():
        global server
        if args.image:
            cmd(["docker", "stop", "--time", "120", name + "-server"])
        else:
            server.terminate()
        result = server.wait(timeout=125)
        server = None
        if result:
            raise RuntimeError("server shutdown failed: " + str(result))

    try:
        cmd(
            [
                "docker",
                "run",
                "--detach",
                "--name",
                name,
                "--publish",
                "127.0.0.1::9000",
                "--env",
                "RUSTFS_ACCESS_KEY=crab-local-access",
                "--env",
                "RUSTFS_SECRET_KEY=crab-local-secret-key",
                "--env",
                "RUSTFS_CONSOLE_ENABLE=false",
                "--tmpfs",
                "/data:rw,size=1g,mode=0777",
                object_store_image,
            ]
        )
        endpoint = subprocess.check_output(
            ["docker", "port", name, "9000/tcp"], text=True
        ).strip()
        endpoint = "http://" + endpoint
        env = os.environ.copy()
        env.pop("AWS_SESSION_TOKEN", None)
        env.update(
            AWS_ACCESS_KEY_ID="crab-local-access",
            AWS_SECRET_ACCESS_KEY="crab-local-secret-key",
            AWS_REGION="us-east-1",
            AWS_DEFAULT_REGION="us-east-1",
            AWS_ENDPOINT_URL_S3=endpoint,
            AWS_ALLOW_HTTP="true",
            AWS_VIRTUAL_HOSTED_STYLE_REQUEST="false",
            AWS_EC2_METADATA_DISABLED="true",
            AWS_PAGER="",
        )
        for i in range(120):
            try:
                with urllib.request.urlopen(endpoint + "/health/ready", timeout=1):
                    break
            except (urllib.error.URLError, TimeoutError, http.client.RemoteDisconnected):
                time.sleep(0.25)
        else:
            raise RuntimeError("RustFS failed readiness")
        cmd(
            [
                "aws",
                "--endpoint-url",
                endpoint,
                "s3api",
                "create-bucket",
                "--bucket",
                "crab-cellar-test",
            ],
            env=env,
        )
        cmd(
            [
                "openssl",
                "req",
                "-x509",
                "-newkey",
                "ed25519",
                "-nodes",
                "-keyout",
                run / "ca.key",
                "-out",
                run / "ca.crt",
                "-subj",
                "/CN=Crab qualification CA",
                "-days",
                "1",
            ]
        )
        cmd(
            [
                "openssl",
                "req",
                "-new",
                "-newkey",
                "ed25519",
                "-nodes",
                "-keyout",
                run / "peer.key",
                "-out",
                run / "peer.csr",
                "-subj",
                "/CN=localhost",
            ]
        )
        (run / "peer.ext").write_text(
            "subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth,clientAuth\n"
        )
        cmd(
            [
                "openssl",
                "x509",
                "-req",
                "-in",
                run / "peer.csr",
                "-CA",
                run / "ca.crt",
                "-CAkey",
                run / "ca.key",
                "-CAcreateserial",
                "-out",
                run / "peer.crt",
                "-days",
                "1",
                "-extfile",
                run / "peer.ext",
            ]
        )
        public = port()
        private = port()
        origin = f"http://127.0.0.1:{public}"
        (run / "cells").mkdir()
        (run / "server.toml").write_text(f'''listen = "127.0.0.1:{public}"
    management_listen = "127.0.0.1:{private}"
    [storage]
    url = "s3://crab-cellar-test/repositories"
    [cells]
    data_dir = "{run}/cells"
    local_disk_limit_bytes = 34359738368
    peer_advertise = "https://127.0.0.1:{private}"
    peer_certificate = "{run}/peer.crt"
    peer_private_key = "{run}/peer.key"
    peer_ca = "{run}/ca.crt"
    ''')
        if args.image:
            # Synthetic fixture files must be readable by the image's unprivileged UID.
            run.chmod(0o777)
            (run / "cells").chmod(0o777)
            for filename in ["server.toml", "peer.crt", "peer.key", "ca.crt"]:
                (run / filename).chmod(0o444)
        cmd(
            server_command(
                "repository",
                "create",
                "--owner",
                "team",
                "--name",
                "project",
                "--prefix",
                "team/project",
            ),
            env=env,
        )
        cmd(server_command("storage-probe"), env=env)
        server = start()
        wait()
        body = {
            "request_id": str(uuid.uuid4()),
            "title": "Cellar RustFS qualification",
            "body": "Survives real process restart",
        }
        created = request("/api/repos/team/project/issues", body)
        number = created["number"]
        assert request("/api/repos/team/project/issues", body)["number"] == number
        issue_path = f"/api/repos/team/project/issues/{number}"
        reaction_path = f"/api/repos/team/project/reactions/issues/{number}"
        reaction = request(reaction_path, {"kind": "heart", "active": True}, method="PUT")
        assert reaction["groups"]["heart"] == {"count": 1, "reacted": True}
        milestone = request("/api/repos/team/project/milestones", {
            "request_id": str(uuid.uuid4()), "title": "Durable collaboration",
            "description": "Process restart qualification", "due_on": "2028-01-01",
        })
        request(issue_path + "/milestone", {"version": 0, "milestone": milestone["number"]}, method="PUT")
        request(issue_path, {"version": created["version"], "state": "closed"}, method="PATCH")
        activity = request(issue_path + "/activity")
        assert [event["action"]["kind"] for event in activity["items"]] == ["closed", "milestone_assigned", "created"]
        git = run / "input"
        git.mkdir()
        cmd(["git", "init", "--initial-branch=main", git])
        cmd(["git", "-C", git, "config", "user.name", "Crab qualification"])
        cmd(["git", "-C", git, "config", "user.email", "qualification@example.invalid"])
        (git / "README.md").write_text("Native Cellar qualification\n")
        cmd(["git", "-C", git, "add", "README.md"])
        cmd(["git", "-C", git, "commit", "-m", "Create qualification input"])
        cmd(
            [
                "git",
                "-C",
                git,
                "push",
                origin + "/git/team/project.git",
                "HEAD:refs/heads/main",
            ],
            env=env,
        )
        commit = subprocess.check_output(
            ["git", "-C", git, "rev-parse", "HEAD"], text=True
        ).strip()
        (git / "README.md").write_text("Native Cellar qualification\nRelated issue workflow\n")
        cmd(["git", "-C", git, "commit", "-am", "Add related issue input"])
        cmd(["git", "-C", git, "push", origin + "/git/team/project.git", "HEAD:refs/heads/feature"], env=env)
        pull = request("/api/repos/team/project/pulls", {
            "request_id": str(uuid.uuid4()), "title": "Related issue workflow", "body": "Link without automatic closure",
            "base_ref": "refs/heads/main", "head_ref": "refs/heads/feature",
        })
        pull_path = f"/api/repos/team/project/pulls/{pull['number']}"
        link_path = pull_path + f"/linked-issues/{number}"
        for active, version in [(True, 0), (False, 1), (True, 2)]:
            linked = request(link_path, {"active": active, "version": version}, method="PUT")
            assert linked == {"active": active, "version": version + 1}
        issue_links = request(issue_path + "/links")
        pull_links = request(pull_path + "/links")
        assert issue_links["items"][0]["work"]["subject"] == {"kind": "pull", "number": pull["number"]}
        assert pull_links["items"][0]["work"]["subject"] == {"kind": "issue", "number": number}
        activity = request(issue_path + "/activity")
        repository_activity = request("/api/repos/team/project/activity")
        stop()
        server = start()
        wait()
        restored = request(f"/api/repos/team/project/issues/{number}")
        assert restored["title"] == body["title"]
        assert restored["reactions"]["groups"] == reaction["groups"]
        assert restored["reactions"]["revision"] >= reaction["revision"]
        assignment = request(issue_path + "/milestone")
        saved_milestone = assignment["milestone"]
        assert saved_milestone["number"] == milestone["number"]
        assert saved_milestone["due_on"] == milestone["due_on"]
        assert (saved_milestone["open_count"], saved_milestone["closed_count"]) == (0, 1)
        work = request(f"/api/repos/team/project/milestones/{milestone['number']}/items")
        assert work["items"][0]["subject"] == {"kind": "issue", "number": number} and work["items"][0]["state"] == "closed"
        for _ in range(2):
            removed = request(reaction_path, {"kind": "heart", "active": False}, method="PUT")
            assert removed["groups"]["heart"] == {"count": 0, "reacted": False}
        assert request("/api/repos/team/project/issues", body)["number"] == number
        assert request(issue_path + "/activity") == activity
        assert request("/api/repos/team/project/activity") == repository_activity
        assert request(issue_path + "/links") == issue_links
        assert request(pull_path + "/links") == pull_links
        assert request(link_path) == {"active": True, "version": 3}
        assert request(link_path, {"active": True, "version": 0}, method="PUT") == {"active": True, "version": 3}
        try:
            request(link_path, {"active": False, "version": 1}, method="PUT")
        except urllib.error.HTTPError as error:
            assert error.code == 409
        else:
            raise AssertionError("stale unlink must not overwrite a later link")
        assert request("/api/repos/team/project/activity") == repository_activity
        refs = subprocess.check_output(
            ["git", "ls-remote", origin + "/git/team/project.git"], text=True, env=env
        )
        assert commit in refs
        status = subprocess.check_output(
            server_command("cells", "status", "--owner", "team", "--name", "project"),
            text=True,
            env=env,
        )
        runtime = json.loads(status)
        assert runtime["runtime"] == "cellar" and runtime["version"] == "0.1.0"
        stop()
        count = subprocess.check_output(
            [
                "aws",
                "--endpoint-url",
                endpoint,
                "s3api",
                "list-objects-v2",
                "--bucket",
                "crab-cellar-test",
                "--prefix",
                "repositories/.crab/cellar/",
                "--query",
                "length(Contents)",
            ],
            text=True,
            env=env,
        ).strip()
        assert int(count) > 0
        report = {
            "schema": 1,
            "runtime": "cellar",
            "version": runtime["version"],
            "image_ref": args.image,
            "repository_creation": True,
            "issue_replay": True,
            "activity_restart": True,
            "activity_replay": True,
            "linked_work_restart": True,
            "linked_work_retry": True,
            "reaction_restart": True,
            "reaction_remove": True,
            "milestone_restart": True,
            "milestone_progress": True,
            "git_push": True,
            "process_restart": True,
            "durable_objects": int(count),
            "artifacts": str(run),
        }
        (run / "receipt.json").write_text(json.dumps(report, indent=2))
        print(json.dumps(report))
    finally:
        if args.image:
            subprocess.run(
                ["docker", "rm", "--force", name + "-server"],
                check=False,
                stdout=log,
                stderr=log,
            )
        if server is not None:
            server.terminate()
            try:
                server.wait(timeout=120)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()
        subprocess.run(
            ["docker", "rm", "--force", name], check=False, stdout=log, stderr=log
        )
