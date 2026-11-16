#!/usr/bin/env python3
"""Qualify private single-container setup, UI/API writes, and cold replacement."""

import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import subprocess
import time
import urllib.error
import urllib.request
import uuid

import boto3


def docker(*args, env=None, check=True):
    return subprocess.run(["docker", *args], env=env, check=check,
                          capture_output=True, text=True, timeout=180)


def endpoint(container, port):
    return "http://" + docker("port", container, f"{port}/tcp").stdout.strip()


def request(origin, path, body=None, repository=None):
    headers = {"Content-Type": "application/json", "Origin": origin}
    if repository:
        headers["X-Crab-Repository-Id"] = repository
    req = urllib.request.Request(origin + path, headers=headers,
                                 data=None if body is None else json.dumps(body).encode())
    with urllib.request.urlopen(req, timeout=90) as response:
        return json.load(response)


def wait_ready(container):
    for _ in range(120):
        if docker("inspect", "--format", "{{.State.Running}}", container).stdout.strip() != "true":
            raise RuntimeError("server exited before browser readiness")
        try:
            origin = endpoint(container, 8080)
            request(origin, "/api/profile")
            return origin
        except (urllib.error.URLError, TimeoutError, ConnectionError):
            time.sleep(1)
    raise RuntimeError("browser/API did not become ready")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", required=True)
    parser.add_argument("--artifacts", required=True, type=Path)
    args = parser.parse_args()
    args.artifacts.mkdir(parents=True, exist_ok=True, mode=0o700)
    name = "crab-container-setup-" + uuid.uuid4().hex[:10]
    bucket = name + "-bucket"
    backend = name + "-backend"
    env = {**os.environ, "AWS_BUCKET": bucket,
           "AWS_ACCESS_KEY_ID": "container-qualification",
           "AWS_SECRET_ACCESS_KEY": "container-qualification-secret",
           "RUSTFS_ACCESS_KEY": "container-qualification",
           "RUSTFS_SECRET_KEY": "container-qualification-secret",
           "AWS_ENDPOINT_URL_S3": f"http://{backend}:9000", "AWS_ALLOW_HTTP": "true"}
    for key in ["AWS_REGION", "AWS_DEFAULT_REGION", "AWS_SESSION_TOKEN"]:
        env.pop(key, None)
    assert docker("run", "--rm", args.image, check=False).returncode != 0
    with ExitStack() as cleanup:
        docker("network", "create", name)
        cleanup.callback(docker, "network", "rm", name, check=False)
        docker("volume", "create", name)
        cleanup.callback(docker, "volume", "rm", name, check=False)
        docker("run", "-d", "--name", backend, "--network", name,
               "-p", "127.0.0.1::9000", "-e", "RUSTFS_ACCESS_KEY", "-e", "RUSTFS_SECRET_KEY",
               "ghcr.io/rustfs/rustfs:1.0.0-glibc@sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858",
               "/data", env=env)
        cleanup.callback(docker, "rm", "-f", "-v", backend, check=False)
        origin_store = boto3.client("s3", endpoint_url=endpoint(backend, 9000), region_name="us-east-1",
                                    aws_access_key_id=env["AWS_ACCESS_KEY_ID"],
                                    aws_secret_access_key=env["AWS_SECRET_ACCESS_KEY"])
        for attempt in range(60):
            try:
                origin_store.create_bucket(Bucket=bucket)
                break
            except Exception:
                if attempt == 59:
                    raise
                time.sleep(1)

        def start(settings):
            docker("run", "-d", "--name", name, "--network", name, "--stop-timeout", "180",
                   "-p", "127.0.0.1::8080", "-v", f"{name}:/var/lib/crab",
                   "-e", "AWS_BUCKET", "-e", "AWS_ACCESS_KEY_ID", "-e", "AWS_SECRET_ACCESS_KEY",
                   "-e", "AWS_ENDPOINT_URL_S3", "-e", "AWS_ALLOW_HTTP", args.image, env=settings)

        cleanup.callback(docker, "rm", "-f", "-v", name, check=False)
        start(env)
        try:
            origin = wait_ready(name)
            with urllib.request.urlopen(origin, timeout=30) as page:
                assert page.status == 200 and b"<html" in page.read()
            assert request(origin, "/api/repos") == {"repositories": []}
            profile = request(origin, "/api/profile", {"request_id": str(uuid.uuid4()),
                              "handle": "container-operator", "name": "Container operator"})
            identity = docker("exec", name, "sha256sum", "/var/lib/crab/server/peer/ca.crt",
                              "/var/lib/crab/server/peer/peer.key").stdout
            # Exec does not inherit startup exports. The generated configuration
            # must supply the signing region to both Git storage and Cellar.
            created = docker("exec", "-e", "AWS_REGION=", "-e", "AWS_DEFAULT_REGION=", name,
                             "crab-server", "--config", "/etc/crab/server.toml", "repository", "create",
                             "--owner", "team", "--name", "project", "--prefix", "team/project")
            repository = json.loads(created.stdout)["id"]
            for attempt in range(90):
                if request(origin, "/api/repos")["repositories"]:
                    break
                if attempt == 89:
                    raise RuntimeError("created repository did not become visible")
                time.sleep(1)
            issue = request(origin, "/api/repos/team/project/issues",
                            {"request_id": str(uuid.uuid4()), "title": "Container setup",
                             "body": "Persists through a cold container replacement"}, repository)
            issue_path = f"/api/repos/team/project/issues/{issue['number']}"
            assert request(origin, issue_path)["title"] == "Container setup"
            assert origin_store.list_objects_v2(Bucket=bucket, Prefix="repositories/")["KeyCount"] > 0
            assert docker("exec", name, "stat", "-c", "%a", "/var/lib/crab/server/server.toml").stdout.strip() == "600"
            docker("stop", "--time", "180", name)
            assert docker("inspect", "--format", "{{.State.ExitCode}}", name).stdout.strip() == "0"
            logs = docker("logs", name)
            (args.artifacts / "startup.log").write_text(logs.stdout + logs.stderr)
            docker("rm", "-v", name)
            # Only the disposable cache in this test's private volume is removed.
            docker("run", "--rm", "-v", f"{name}:/state", "--entrypoint", "/bin/rm",
                   args.image, "-rf", "/state/cells")
            start(env)
            origin = wait_ready(name)
            assert request(origin, "/api/profile")["profile"]["id"] == profile["id"]
            assert request(origin, issue_path)["title"] == "Container setup"
            assert docker("exec", name, "sha256sum", "/var/lib/crab/server/peer/ca.crt",
                          "/var/lib/crab/server/peer/peer.key").stdout == identity
            # A dead public proxy must retire the native server as well.
            docker("exec", name, "bash", "-c",
                   'for path in /proc/[0-9]*/comm; do read -r process < "$path" || continue; '
                   'if [[ "$process" == caddy ]]; then pid=${path#/proc/}; kill -TERM "${pid%/comm}"; fi; done')
            docker("wait", name)
            assert docker("inspect", "--format", "{{.State.Running}}", name).stdout.strip() == "false"
            logs = docker("logs", name)
            (args.artifacts / "replacement.log").write_text(logs.stdout + logs.stderr)
            docker("rm", "-v", name)
            start({**env, "AWS_BUCKET": bucket + "-other"})
            assert docker("wait", name).stdout.strip() != "0"
            docker("rm", "-v", name)
            docker("run", "--rm", "-v", f"{name}:/state", "--entrypoint", "/bin/rm",
                   args.image, "/state/server/peer/peer.key")
            start(env)
            assert docker("wait", name).stdout.strip() != "0"
        finally:
            logs = docker("logs", name, check=False)
            (args.artifacts / "server.log").write_text(logs.stdout + logs.stderr)
    proof = {"single_container": True, "embedded_ui": True, "repository_and_issue": True,
             "directory": True, "cold_replacement": True, "stable_peer_identity": True,
             "fail_closed": True, "proxy_failure_shutdown": True}
    image = json.loads(docker("image", "inspect", args.image).stdout)[0]
    proof["image_id"] = image["Id"]
    labels = image["Config"].get("Labels") or {}
    proof["source_revision"] = labels.get("org.opencontainers.image.revision")
    (args.artifacts / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")
    print(json.dumps(proof))


if __name__ == "__main__":
    main()
