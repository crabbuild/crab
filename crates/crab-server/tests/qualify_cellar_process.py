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
import ssl
import subprocess
import time
import urllib.error
import urllib.request
import urllib.parse
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

    repository_ids = {}

    def request(path, body=None, *, method=None):
        headers = {"Content-Type": "application/json"}
        parts = urllib.parse.urlsplit(path).path.split("/")
        if body is not None and parts[1:3] == ["api", "repos"] and len(parts) >= 5:
            owner, name = parts[3:5]
            key = (owner, name)
            if key not in repository_ids:
                catalog = request("/api/repos?" + urllib.parse.urlencode({"owner": owner, "name": name}))
                repository_ids[key] = catalog["repositories"][0]["id"]
            headers["X-Crab-Repository-Id"] = repository_ids[key]
        req = urllib.request.Request(
            origin + path,
            data=None if body is None else json.dumps(body).encode(),
            headers=headers,
            method=method,
        )
        with urllib.request.urlopen(req, timeout=35) as r:
            return json.load(r)

    def index_request(body=None):
        context = ssl.create_default_context(cafile=str(run / "ca.crt"))
        context.load_cert_chain(str(run / "peer.crt"), str(run / "peer.key"))
        req = urllib.request.Request(
            f"https://127.0.0.1:{private}/internal/repositories/team/project/discussion-index",
            data=None if body is None else json.dumps(body).encode(),
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req, context=context, timeout=35) as response:
            return json.load(response)

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
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-addext",
                "keyUsage=critical,keyCertSign,cRLSign",
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
            "subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth,clientAuth\nkeyUsage=critical,digitalSignature\nauthorityKeyIdentifier=keyid,issuer\n"
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
        cmd(server_command("cells", "initialize-directory"), env=env)
        organization_id = str(uuid.uuid4())
        organization_bootstrap = run / "organization.json"
        organization_bootstrap.write_text(json.dumps({"owner": {"issuer": "urn:crab:local", "subject": "operator"}, "name": "Qualification organization", "description": "Native authority"}))
        organization_command = server_command("cells", "bootstrap-organization", "--organization", organization_id, "--input-file", organization_bootstrap)
        organization_created = json.loads(subprocess.check_output(organization_command, env=env, text=True))
        organization_root = "/api/organizations/" + organization_id
        organization_before = request(organization_root)
        assert organization_before["role"] == "owner"
        assert organization_before["actor"] == organization_created["owner"]
        assert organization_created["url"] == "/-/organizations/" + organization_id
        assert json.loads(subprocess.check_output(organization_command, env=env, text=True)) == organization_created
        organization_before["organization"] = request(organization_root, {"version": organization_before["organization"]["version"], "name": "Qualification organization renamed", "description": "Persisted across process restart"}, method="PATCH")
        organization_members = request(organization_root + "/members")
        assert len(organization_members["items"]) == 1
        assert organization_members["items"][0]["profile"] is None
        assert organization_members["can_manage"] is True
        try:
            request(organization_root + "/members/" + organization_created["owner"], {"version": 1}, method="DELETE")
        except urllib.error.HTTPError as error:
            assert error.code == 400
        else:
            raise AssertionError("the last organization owner must survive")
        assert request("/api/profile") == {"profile": None}
        profile_input = {"request_id": str(uuid.uuid4()), "handle": "qualification-actor", "name": "Qualification actor"}
        actor_profile = request("/api/profile", profile_input)
        profile_id = actor_profile["id"]
        actor_profile = request("/api/profile", {"version": actor_profile["version"], "handle": "qualification-actor-renamed", "name": "Qualification actor renamed"}, method="PATCH")
        assert actor_profile["id"] == profile_id
        organization_members = request(organization_root + "/members")
        assert organization_members["items"][0]["actor"] == profile_id
        assert organization_members["items"][0]["profile"]["handle"] == actor_profile["handle"]
        assert request("/api/profile", profile_input) == actor_profile
        body = {
            "request_id": str(uuid.uuid4()),
            "title": "Cellar RustFS qualification",
            "body": "Survives real process restart",
        }
        created = request("/api/repos/team/project/issues", body)
        number = created["number"]
        assert request("/api/repos/team/project/issues", body)["number"] == number
        denied_identity = urllib.request.Request(
            origin + "/api/repos/team/project/issues",
            data=json.dumps({"request_id": str(uuid.uuid4()), "title": "Wrong identity must stay absent", "body": "No effect"}).encode(),
            headers={"Content-Type": "application/json", "X-Crab-Repository-Id": str(uuid.uuid4())},
        )
        try:
            urllib.request.urlopen(denied_identity, timeout=35)
            raise AssertionError("Repository identity mismatch executed")
        except urllib.error.HTTPError as error:
            assert error.code == 409 and "X-Crab-Write-Outcome" not in error.headers
            assert json.load(error)["error"]["code"] == "repository_changed"
        absent = request("/api/repos/team/project/issues?" + urllib.parse.urlencode({"q": '"Wrong identity must stay absent"'}))
        assert absent["matching"] == 0
        author_summary = {"name": "Local operator", "profile": {key: actor_profile[key] for key in ["id", "handle", "name"]}}
        assert request("/api/repos/team/project/issues")["items"][0]["author"] == author_summary
        author_search = "/api/repos/team/project/issues?" + urllib.parse.urlencode({"q": "author:" + actor_profile["handle"]})
        self_search = "/api/repos/team/project/issues?q=author%3A%40me"
        assert request(author_search)["matching"] == 1
        assert request(self_search)["authored_by_me"] is True
        author_completion = "/api/repos/team/project/discussion-search?" + urllib.parse.urlencode({"query": "author:qualification", "cursor": 20})
        assert request(author_completion)["items"][0]["value"] == "author:" + actor_profile["handle"]
        issue_path = f"/api/repos/team/project/issues/{number}"
        reaction_path = f"/api/repos/team/project/reactions/issues/{number}"
        reaction = request(reaction_path, {"kind": "heart", "active": True}, method="PUT")
        assert reaction["groups"]["heart"] == {"count": 1, "reacted": True}
        milestone = request("/api/repos/team/project/milestones", {
            "request_id": str(uuid.uuid4()), "title": "Durable collaboration",
            "description": "Process restart qualification", "due_on": "2028-01-01",
        })
        request(issue_path + "/milestone", {"version": 0, "milestone": milestone["number"]}, method="PUT")
        assert created["state_reason"] is None and created["closed_at"] is None
        closed_issue = request(issue_path, {"version": created["version"], "state": "closed", "state_reason": "not_planned"}, method="PATCH")
        assert closed_issue["state_reason"] == "not_planned"
        assert closed_issue["closed_at"] == closed_issue["updated_at"]
        reason_search = "/api/repos/team/project/issues?" + urllib.parse.urlencode({"q": 'is:closed reason:"not planned"'})
        assert request(reason_search)["items"][0]["closed_at"] == closed_issue["closed_at"]
        prepared_issues = json.loads(subprocess.check_output(server_command("cells", "prepare-issue-state", "--owner", "team", "--name", "project"), env=env, text=True))
        assert prepared_issues == {"state": "ready"}
        activity = request(issue_path + "/activity")
        assert [event["action"]["kind"] for event in activity["items"]] == ["issue_closed", "milestone_assigned", "created"]
        qualification_label = request("/api/repos/team/project/labels", {
            "request_id": str(uuid.uuid4()), "name": "RustFS qualification", "color": "abcdef", "description": "Native search suggestions",
        })
        issue_search = "/api/repos/team/project/issues?" + urllib.parse.urlencode({
            "q": '("Cellar RustFS" OR missing) AND no:label -unrelated is:closed',
        })
        assert [item["number"] for item in request(issue_search)["items"]] == [number]
        milestone_search = "/api/repos/team/project/issues?" + urllib.parse.urlencode({"q": 'milestone:"Durable collaboration"'})
        milestone_options = "/api/repos/team/project/discussion-search/milestones?" + urllib.parse.urlencode({"query": "is:closed", "search": "Durable", "state": "open"})
        assert request(milestone_search)["matching"] == 1
        assert request(milestone_options)["items"][0]["title"] == milestone["title"]
        completion_path = "/api/repos/team/project/discussion-search?" + urllib.parse.urlencode({
            "query": "label:rust", "cursor": 10,
        })
        completion = request(completion_path)
        assert completion["items"][0]["value"] == 'label:"RustFS qualification"'

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
        (git / "README.md").write_text("Native Cellar qualification\nRelated issue workflow\n")
        cmd(["git", "-C", git, "commit", "-am", "Add related issue input"])
        cmd(["git", "-C", git, "push", origin + "/git/team/project.git", "HEAD:refs/heads/feature"], env=env)
        pull = request("/api/repos/team/project/pulls", {
            "request_id": str(uuid.uuid4()), "title": "Related issue workflow", "body": "Link without automatic closure",
            "draft": True,
            "base_ref": "refs/heads/main", "head_ref": "refs/heads/feature",
        })
        pull_path = f"/api/repos/team/project/pulls/{pull['number']}"
        assert pull["draft"] is True and pull["can_merge"] is False
        pull_search = "/api/repos/team/project/pulls?" + urllib.parse.urlencode({
            "q": 'is:pr draft:true no:assignee AND "Related issue"',
        })
        assert [item["number"] for item in request(pull_search)["items"]] == [pull["number"]]

        mixed_search = "/api/repos/team/project/issues?" + urllib.parse.urlencode({
            "q": "is:issue OR is:pr", "sort": "comments", "direction": "asc", "limit": 1,
        })
        mixed_first = request(mixed_search)
        mixed_second = request(mixed_search + "&before=" + urllib.parse.quote(mixed_first["next"]))
        assert [(page["items"][0]["kind"], page["items"][0]["number"]) for page in [mixed_first, mixed_second]] == [("issue", number), ("pull", pull["number"])]
        assert mixed_first["matching"] == 2 and mixed_second["next"] is None
        assert mixed_second["items"][0]["draft"] is True
        pull_in_issues = "/api/repos/team/project/issues?" + urllib.parse.urlencode({"q": "is:pr draft:true"})
        assert request(pull_in_issues)["items"][0]["kind"] == "pull"

        merge_input = {"request_id": str(uuid.uuid4()), "version": pull["version"],
                       "method": "fast_forward", "base_oid": pull["base_oid"], "head_oid": pull["head_oid"]}
        try:
            request(pull_path + "/merge", merge_input)
        except urllib.error.HTTPError as error:
            assert error.code == 409 and json.load(error)["error"]["code"] == "pull_draft"
        else:
            raise AssertionError("a draft must not publish a merge")
        before_merge_refs = subprocess.check_output(["git", "ls-remote", origin + "/git/team/project.git", "refs/heads/main"], text=True, env=env)
        assert before_merge_refs.split()[0] == pull["base_oid"]

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
        assert request("/api/profile")["profile"] == actor_profile
        assert request(organization_root) == organization_before
        assert request(organization_root + "/members") == organization_members
        assert request("/api/actors/" + profile_id) == actor_profile
        assert request("/api/repos/team/project/issues?q=is%3Aissue")["items"][0]["author"] == author_summary
        restored = request(f"/api/repos/team/project/issues/{number}")
        assert restored["state_reason"] == "not_planned" and restored["closed_at"] == closed_issue["closed_at"]
        assert request(reason_search)["matching"] == 1
        assert restored["title"] == body["title"]
        assert restored["reactions"]["groups"] == reaction["groups"]
        assert restored["reactions"]["revision"] >= reaction["revision"]
        assert request(milestone_search)["matching"] == 1
        assert request(milestone_options)["items"][0]["title"] == milestone["title"]
        assignment = request(issue_path + "/milestone")
        saved_milestone = assignment["milestone"]
        assert saved_milestone["number"] == milestone["number"]
        assert saved_milestone["due_on"] == milestone["due_on"]
        assert (saved_milestone["open_count"], saved_milestone["closed_count"]) == (0, 1)
        work = request(f"/api/repos/team/project/milestones/{milestone['number']}/items")
        assert work["items"][0]["subject"] == {"kind": "issue", "number": number} and work["items"][0]["state"] == "closed"
        assert request(mixed_search) == mixed_first
        for _ in range(2):
            removed = request(reaction_path, {"kind": "heart", "active": False}, method="PUT")
            assert removed["groups"]["heart"] == {"count": 0, "reacted": False}
        assert request("/api/repos/team/project/issues", body)["number"] == number
        assert request(issue_path + "/activity") == activity
        assert request("/api/repos/team/project/activity") == repository_activity
        assert request(issue_path + "/links") == issue_links
        assert request(pull_path + "/links") == pull_links
        assert request(link_path) == {"active": True, "version": 3}
        assert request(pull_path)["draft"] is True
        assert request(pull_in_issues)["items"][0]["draft"] is True
        assert [item["number"] for item in request(issue_search)["items"]] == [number]
        assert [item["number"] for item in request(pull_search)["items"]] == [pull["number"]]
        assert request(completion_path) == completion

        assert request(link_path, {"active": True, "version": 0}, method="PUT") == {"active": True, "version": 3}
        try:
            request(link_path, {"active": False, "version": 1}, method="PUT")
        except urllib.error.HTTPError as error:
            assert error.code == 409
        else:
            raise AssertionError("stale unlink must not overwrite a later link")
        assert request("/api/repos/team/project/activity") == repository_activity
        ready = request(pull_path + "/readiness", {"version": pull["version"], "draft": False}, method="PUT")
        assert ready["draft"] is False
        merge_input["version"] = ready["version"]
        merged = request(pull_path + "/merge", merge_input)
        assert merged["state"] == "merged" and merged["draft"] is False
        assert merged["closed_at"] == merged["merged_at"] == merged["updated_at"]
        assert request(pull_path + "/merge", merge_input)["merged_at"] == merged["merged_at"]
        merged_activity = request("/api/repos/team/project/activity")
        assert [event["action"]["kind"] for event in merged_activity["items"][:2]] == ["merged", "ready_for_review"]
        assert merged_activity["items"][0]["created_at"] == merged["merged_at"]
        stop()
        server = start()
        wait()
        completed = request(pull_path)
        assert completed["state"] == "merged" and completed["draft"] is False
        assert request(pull_search)["items"] == []
        merged_search = "/api/repos/team/project/pulls?" + urllib.parse.urlencode({
            "q": "is:merged draft:false",
        })
        assert [item["number"] for item in request(merged_search)["items"]] == [pull["number"]]

        for query, expected in [
            ("is:closed", 1), ("is:merged", 1), ("is:unmerged", 0),
            ("is:open", 0), ("is:pr", 1), ("is:issue", 0),
            ("draft:false", 1), ("draft:true", 0), ("is:closed -is:merged", 0),
        ]:
            result = request("/api/repos/team/project/pulls?" + urllib.parse.urlencode({"q": query}))
            assert len(result["items"]) == expected, query
            assert result["matching"] == expected, query
        previous_index = index_request()
        generation = str(uuid.uuid4())
        building = index_request({"action": "begin", "expected": previous_index["generation"], "generation": generation})
        assert building["state"] == "building"
        assert request("/api/profile")["profile"] == actor_profile
        assert request(organization_root) == organization_before
        assert request(organization_root + "/members") == organization_members
        index_request({"action": "step", "generation": generation, "version": building["progress"]["version"]})
        try:
            request(issue_search)
        except urllib.error.HTTPError as error:
            assert error.code == 503
        else:
            raise AssertionError("a partially rebuilt index must not serve results")
        before_rebuild_issue = request(issue_path)
        before_rebuild_pull = request(pull_path)
        stop()
        server = start()
        wait()
        assert index_request()["state"] == "building"
        rebuilt = json.loads(subprocess.check_output(server_command("cells", "rebuild-search", "--owner", "team", "--name", "project"), env=env, text=True))
        assert rebuilt["state"] == "ready" and rebuilt["generation"] == generation
        assert request(issue_path) == before_rebuild_issue
        assert request(pull_path) == before_rebuild_pull
        assert request(issue_search)["matching"] == 1
        assert request(milestone_search)["matching"] == 1
        assert request(milestone_options)["items"][0]["title"] == milestone["title"]
        assert request(merged_search)["counts"] == {"open": 0, "closed": 1, "merged": 1}
        assert request("/api/repos/team/project/activity") == merged_activity
        assert request(issue_path + "/links")["items"][0]["work"]["state"] == "merged"
        refs = subprocess.check_output(
            ["git", "ls-remote", origin + "/git/team/project.git"], text=True, env=env
        )
        assert merged["merge"]["commit_oid"] + "\trefs/heads/main" in refs
        status = subprocess.check_output(
            server_command("cells", "status", "--owner", "team", "--name", "project"),
            text=True,
            env=env,
        )
        runtime = json.loads(status)
        assert runtime["runtime"] == "cellar" and runtime["version"] == "0.1.0"
        follow_up = request("/api/repos/team/project/labels", {
            "request_id": str(uuid.uuid4()), "name": "Follow up", "color": "008672", "description": "Complete filter workflow",
        })
        current_issue = request(issue_path)
        labeled_issue = request(issue_path, {"version": current_issue["version"], "label_ids": [qualification_label["id"], follow_up["id"]]}, method="PATCH")
        label_options = "/api/repos/team/project/discussion-search/labels?" + urllib.parse.urlencode({"query": "is:closed", "search": "RustFS"})
        choices = request(label_options)
        label_query = choices["items"][0]["query"]
        label_search = "/api/repos/team/project/issues?" + urllib.parse.urlencode({"q": label_query})
        assert request(label_search)["matching"] == 1
        both_choices = request("/api/repos/team/project/discussion-search/labels?" + urllib.parse.urlencode({"query": label_query, "search": "Follow up"}))
        both_query = both_choices["items"][0]["query"]
        assert request("/api/repos/team/project/issues?" + urllib.parse.urlencode({"q": both_query}))["matching"] == 1
        excluded = request("/api/repos/team/project/discussion-search/labels?" + urllib.parse.urlencode({"query": both_query, "search": "RustFS"}))["items"][0]["exclude_query"]
        assert request("/api/repos/team/project/issues?" + urllib.parse.urlencode({"q": excluded}))["matching"] == 0
        stop()
        server = start()
        wait()
        assert request(issue_path) == labeled_issue
        assert request(label_options) == choices
        assert request(label_search)["matching"] == 1
        for path in [author_search, self_search]:
            page = request(path)
            assert page["matching"] == 2 and page["counts"] == {"open": 0, "closed": 2, "merged": 1}
        assert request(author_completion)["items"][0]["value"] == "author:" + actor_profile["handle"]
        assert request("/api/repos/team/project/pulls?q=author%3A%40me")["matching"] == 1
        (git / "atomic-state.md").write_text("Atomic comment and state qualification\n")
        cmd(["git", "-C", git, "add", "atomic-state.md"])
        cmd(["git", "-C", git, "commit", "-m", "Prepare atomic state review"])
        cmd(["git", "-C", git, "push", origin + "/git/team/project.git", "HEAD:refs/heads/atomic-state"], env=env)
        state_pull_input = {"request_id": str(uuid.uuid4()), "title": "Atomic state review", "body": "Combined comment actions", "base_ref": "refs/heads/main", "head_ref": "refs/heads/atomic-state"}
        state_pull = request("/api/repos/team/project/pulls", state_pull_input)
        state_pull_path = "/api/repos/team/project/pulls/" + str(state_pull["number"])
        state_actions = []
        for path, state in [(issue_path, "open"), (state_pull_path, "closed")]:
            before = request(path)
            body = {"request_id": str(uuid.uuid4()), "version": before["version"], "state": state, "body": "Atomic context for " + path}
            count_before = len(request(path + "/comments")["items"])
            saved = request(path + "/state", body)
            assert saved["comment"]["body"] == body["body"]
            assert request(path)["state"] == state
            if path == state_pull_path:
                closed_pull = request(path)
                assert closed_pull["closed_at"] == closed_pull["updated_at"]
                assert closed_pull["merged_at"] is None
            state_actions.append((path, body, saved["comment"]["number"], count_before + 1))
        current_issue = request(issue_path)
        request(issue_path + "/state", {"request_id": str(uuid.uuid4()), "version": current_issue["version"], "state": "closed"})
        bulk_issue = request("/api/repos/team/project/issues", {"request_id": str(uuid.uuid4()), "title": "Bulk qualification", "body": "Native bulk workflow"})
        bulk_paths = ["/api/repos/team/project/issues/" + str(bulk_issue["number"]), state_pull_path]
        bulk_actions = []
        bulk_repository = request("/api/repos/team/project/issues")["repository_id"]
        for action in [
            {"kind": "state", "state": "open"},
            {"kind": "labels", "add": [qualification_label["id"]], "remove": []},
            {"kind": "assignees", "add": [request("/api/repos/team/project/assignees")["items"][0]["id"]], "remove": []},
            {"kind": "milestone", "number": milestone["number"]},
        ]:
            targets = []
            for path in bulk_paths:
                value = request(path)
                targets.append(("issue" if "/issues/" in path else "pull") + ":" + str(value["number"]) + ":" + str(value["version"]))
            selected = request("/api/repos/team/project/discussions/selection?" + urllib.parse.urlencode({"repository_id":bulk_repository,"items": ",".join(targets)}))["items"]
            body = {"repository_id":bulk_repository,"request_id": str(uuid.uuid4()), "items": selected, "action": action}
            assert request("/api/repos/team/project/discussions/bulk", body) == {"count": 2}
            bulk_actions.append(body)
        bulk_saved = []
        for path in bulk_paths:
            value = request(path)
            bulk_saved.append({key: value[key] for key in ["state", "version", "labels", "assignees"]})
        pins_path = "/api/repos/team/project/issues/pins"
        assert request(pins_path)["items"] == []
        pin_candidates = [number, bulk_issue["number"]]
        for title in ["Pinned priority", "Fourth pin candidate"]:
            candidate = request("/api/repos/team/project/issues", {"request_id": str(uuid.uuid4()), "title": title, "body": "Pin collection qualification"})
            pin_candidates.append(candidate["number"])
        pin_original = {"request_id": str(uuid.uuid4()), "version": 0, "numbers": pin_candidates[:3]}
        assert request(pins_path, pin_original, method="PUT") == {"version": 1}
        try:
            request(pins_path, {"request_id": str(uuid.uuid4()), "version": 1, "numbers": pin_candidates}, method="PUT")
            raise AssertionError("A fourth pin was accepted")
        except urllib.error.HTTPError as error:
            assert error.code == 400 and error.headers["X-Crab-Write-Outcome"] == "rejected"
        assert request(pins_path, {"request_id": str(uuid.uuid4()), "version": 1, "numbers": list(reversed(pin_candidates[:3]))}, method="PUT") == {"version": 2}
        pinned_issue_path = "/api/repos/team/project/issues/" + str(pin_candidates[2])
        current = request(pinned_issue_path)
        request(pinned_issue_path, {"version": current["version"], "title": "Current pinned priority"}, method="PATCH")
        request(pinned_issue_path + "/comments", {"request_id": str(uuid.uuid4()), "body": "Current pinned comment count"})
        pins_saved = request(pins_path)
        assert pins_saved["items"][0]["title"] == "Current pinned priority"
        assert pins_saved["items"][0]["comment_count"] == 1
        assert any(item["state"] == "closed" for item in pins_saved["items"])
        pin_events = request(pinned_issue_path + "/activity")
        rollup_list = "/api/repos/team/project/pulls?" + urllib.parse.urlencode({"q": '"Atomic state review"'})
        rollup_pull = request(state_pull_path)
        rollup_head = rollup_pull["head_oid"]
        rollup_ref = rollup_pull["head_ref"]
        merged_head = request(pull_path)["head_oid"]
        for oid in {rollup_head, merged_head}:
            request("/api/repos/team/project/statuses/" + oid, {"request_id":str(uuid.uuid4()),"context":"ci/list-summary","state":"success"})
        request("/api/repos/team/project/check-runs", {"request_id":str(uuid.uuid4()),"head_sha":rollup_head,"name":"ci/list-run","status":"completed","conclusion":"skipped","details_url":None,"output":{"title":"Skipped","summary":"List rollup qualification"}})
        rollup = request(rollup_list)["items"][0]["rollup"]
        assert rollup["head_oid"] == rollup_head
        assert rollup["checks"]["success"] >= 1 and rollup["checks"]["skipped"] == 1
        (git / "README.md").write_text("Source head advanced after list checks\n")
        cmd(["git", "-C", git, "commit", "-am", "Advance list summary source"])
        cmd(["git", "-C", git, "push", origin + "/git/team/project.git", "HEAD:" + rollup_ref], env=env)
        advanced_head = subprocess.check_output(["git","-C",git,"rev-parse","HEAD"],text=True).strip()
        advanced_rollup = request(rollup_list)["items"][0]["rollup"]
        assert advanced_rollup["head_oid"] == advanced_head and advanced_head != rollup_head
        assert sum(advanced_rollup["checks"].values()) == 0
        cmd(["git", "-C", git, "push", origin + "/git/team/project.git", "--delete", rollup_ref, "refs/heads/feature"], env=env)
        missing_rollup = request(rollup_list)["items"][0]["rollup"]
        assert missing_rollup == {"number":rollup_pull["number"],"head_oid":None,"checks":None,"reviews":None}
        merged_rollup = next(item["rollup"] for item in request("/api/repos/team/project/pulls?"+urllib.parse.urlencode({"q":"is:merged"}))["items"] if item["number"] == pull["number"])
        assert merged_rollup["head_oid"] == merged_head and merged_rollup["checks"]["success"] >= 1
        lifecycle_saved = {path: {key: request(path)[key] for key in ["closed_at", "merged_at"]} for path in [pull_path, state_pull_path]}
        assert lifecycle_saved[pull_path]["merged_at"] == merged["merged_at"]
        assert lifecycle_saved[state_pull_path]["closed_at"] is None
        assert lifecycle_saved[state_pull_path]["merged_at"] is None

        # State submissions recover after a complete process restart.
        stop()
        server = start()
        wait()
        for path, body, comment, expected_count in state_actions:
            recovered = request(path + "/state", body)
            assert recovered["comment"]["number"] == comment
            assert len(request(path + "/comments")["items"]) == expected_count
            search = "/api/repos/team/project/issues?" + urllib.parse.urlencode({"q": 'in:comments "Atomic context"'})
            assert request(search)["matching"] == 2
        assert request(rollup_list)["items"][0]["rollup"] == missing_rollup
        for path, saved in lifecycle_saved.items():
            assert {key: request(path)[key] for key in saved} == saved

        assert request(pins_path, pin_original, method="PUT") == {"version": 1}
        assert request(pins_path) == pins_saved
        assert request(pinned_issue_path + "/activity") == pin_events
        for body in bulk_actions:
            assert request("/api/repos/team/project/discussions/bulk", body) == {"count": 2}
        for path, saved in zip(bulk_paths, bulk_saved):
            value = request(path)
            assert {key: value[key] for key in saved} == saved
            assert request(path + "/milestone")["milestone"]["number"] == milestone["number"]
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
            "repository_identity_precondition": True,
            "issue_replay": True,
            "pull_lifecycle_completion_and_closure_restart": True,
            "pull_list_rollup_head_restart": True,
            "merged_rollup_survives_source_deletion": True,
            "pins_restart_current_cards": True,
            "pins_limit_reorder_recovery": True,
            "bulk_state_metadata_restart": True,
            "bulk_submission_recovery": True,
            "atomic_comment_state_restart": True,
            "atomic_state_submission_recovery": True,
            "atomic_comment_search": True,
            "issue_close_reason_restart": True,
            "issue_close_timestamp_restart": True,
            "issue_reason_search_restart": True,
            "issue_state_private_preparation": True,
            "organization_explicit_bootstrap": True,
            "organization_metadata_restart": True,
            "organization_last_owner_guard": True,
            "organization_profile_enrichment_restart": True,
            "actor_directory_restart": True,
            "actor_profile_retry": True,
            "actor_attribution_restart": True,
            "author_search_restart_rebuild": True,
            "author_completion_restart": True,
            "discussion_search_restart": True,
            "mixed_discussion_cursor_restart": True,
            "search_suggestions_restart": True,
            "search_index_rebuild_restart": True,
            "activity_restart": True,
            "activity_replay": True,
            "linked_work_restart": True,
            "linked_work_retry": True,
            "draft_restart": True,
            "draft_merge_blocked": True,
            "ready_merge": True,
            "reaction_restart": True,
            "reaction_remove": True,
            "label_choices_restart": True,
            "label_filter_restart": True,
            "milestone_restart": True,
            "milestone_search_restart": True,
            "milestone_options_rebuild": True,
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
