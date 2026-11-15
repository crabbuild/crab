#!/usr/bin/env python3
"""Qualify the no-config Docker setup with an isolated RustFS and signed S3 client."""

from __future__ import annotations

import argparse
from contextlib import ExitStack
import json
import os
import secrets
import shlex
import subprocess
import time

import boto3
from botocore.config import Config
from botocore.exceptions import ClientError, EndpointConnectionError


def docker(*args: str, env: dict[str, str] | None = None, check: bool = True):
    return subprocess.run(
        ["docker", *args], env=env, check=check, capture_output=True, text=True, timeout=180
    )


def endpoint(container: str, port: int) -> str:
    address = docker("port", container, f"{port}/tcp").stdout.strip()
    return f"http://{address}"


def client(url: str, access: str, secret: str):
    return boto3.client(
        "s3",
        endpoint_url=url,
        region_name="us-east-1",
        aws_access_key_id=access,
        aws_secret_access_key=secret,
        config=Config(
            signature_version="s3v4", s3={"addressing_style": "path"},
            retries={"max_attempts": 0}, connect_timeout=2, read_timeout=5,
        ),
    )


def wait_ready(container: str, config: str = "/etc/crab/s3-gateway.toml") -> None:
    for _ in range(60):
        result = docker(
            "exec", container, "crab-s3-gateway", "--config",
            config, "--readiness-check", check=False,
        )
        if result.returncode == 0:
            return
        if docker("inspect", "--format", "{{.State.Running}}", container).stdout.strip() != "true":
            raise RuntimeError("container exited before becoming ready")
        time.sleep(1)
    raise RuntimeError("container did not become ready")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", required=True)
    parser.add_argument("--rustfs-image", default="rustfs/rustfs:1.0.0-beta.8-glibc")
    args = parser.parse_args()
    assert docker("run", "--rm", args.image, check=False).returncode != 0
    name = f"crab-gateway-setup-{secrets.token_hex(5)}"
    bucket = f"{name}-bucket"
    env = {
        **os.environ,
        "RUSTFS_ACCESS_KEY": "container-qualification",
        "RUSTFS_SECRET_KEY": "container-qualification-secret",
        "AWS_ACCESS_KEY_ID": "container-qualification",
        "AWS_SECRET_ACCESS_KEY": "container-qualification-secret",
        "AWS_BUCKET": bucket,
        "AWS_ENDPOINT_URL_S3": f"http://{name}-backend:9000",
        "AWS_ALLOW_HTTP": "true",
    }
    # Prove endpoint mode needs no region and never inherits host AWS identity.
    for key in ["AWS_REGION", "AWS_DEFAULT_REGION", "AWS_SESSION_TOKEN"]:
        env.pop(key, None)
    with ExitStack() as cleanup:
        docker("network", "create", name)
        cleanup.callback(docker, "network", "rm", name, check=False)
        for volume in [name, f"{name}-incompatible"]:
            docker("volume", "create", volume)
            cleanup.callback(docker, "volume", "rm", volume, check=False)
        backend = f"{name}-backend"
        docker(
            "run", "-d", "--name", backend, "--network", name,
            "-p", "127.0.0.1::9000", "-e", "RUSTFS_ACCESS_KEY", "-e", "RUSTFS_SECRET_KEY",
            args.rustfs_image, "/data", env=env,
        )
        cleanup.callback(docker, "rm", "-f", "-v", backend, check=False)
        origin = client(endpoint(backend, 9000), env["AWS_ACCESS_KEY_ID"], env["AWS_SECRET_ACCESS_KEY"])
        for attempt in range(60):
            try:
                origin.create_bucket(Bucket=bucket)
                break
            except (ClientError, EndpointConnectionError):
                if attempt == 59:
                    raise
                time.sleep(1)

        def start(volume: str, settings: dict[str, str], *command: str) -> None:
            docker(
                "run", "-d", "--name", name, "--network", name,
                "-p", "127.0.0.1::8080", "-v", f"{volume}:/var/lib/crab/gateway",
                "-e", "AWS_BUCKET", "-e", "AWS_ACCESS_KEY_ID", "-e", "AWS_SECRET_ACCESS_KEY",
                "-e", "AWS_ENDPOINT_URL_S3", "-e", "AWS_ALLOW_HTTP", args.image, *command,
                env=settings,
            )

        cleanup.callback(docker, "rm", "-f", "-v", name, check=False)
        start(name, env)
        wait_ready(name)
        exported = docker("exec", name, "cat", "/var/lib/crab/gateway/client.env").stdout
        credentials = dict(word.split("=", 1) for word in shlex.split(exported) if "=" in word)
        access = credentials["AWS_ACCESS_KEY_ID"]
        secret = credentials["AWS_SECRET_ACCESS_KEY"]
        assert env["AWS_SECRET_ACCESS_KEY"] not in exported
        docker("exec", name, "crab-s3-gateway", "--config", "/etc/crab/s3-gateway.toml", "--healthcheck")
        s3 = client(endpoint(name, 8080), access, secret)
        body = b"Crab container setup: signed write and persistent read\n"
        s3.put_object(Bucket="repository", Key="main/setup.txt", Body=body)
        assert s3.get_object(Bucket="repository", Key="main/setup.txt")["Body"].read() == body
        listed = s3.list_objects_v2(Bucket="repository", Prefix="main/")
        assert [entry["Key"] for entry in listed["Contents"]] == ["main/setup.txt"]
        physical = origin.list_objects_v2(Bucket=bucket, Prefix="repositories/default/")
        assert physical["KeyCount"] > 0
        try:
            client(endpoint(name, 8080), env["AWS_ACCESS_KEY_ID"], env["AWS_SECRET_ACCESS_KEY"]).list_buckets()
        except ClientError as error:
            assert error.response["Error"]["Code"] == "InvalidAccessKeyId"
        else:
            raise RuntimeError("backend AWS credentials authenticated as a gateway client")
        logs = docker("logs", name)
        assert all(value not in logs.stdout + logs.stderr for value in [access, secret, env["AWS_SECRET_ACCESS_KEY"]])

        docker("stop", "--time", "35", name)
        assert docker("inspect", "--format", "{{.State.ExitCode}}", name).stdout.strip() == "0"
        docker("rm", "-v", name)
        start(name, env)
        wait_ready(name)
        assert docker("exec", name, "cat", "/var/lib/crab/gateway/client.env").stdout == exported
        s3 = client(endpoint(name, 8080), access, secret)
        assert s3.get_object(Bucket="repository", Key="main/setup.txt")["Body"].read() == body
        docker("rm", "-f", "-v", name)

        explicit_config = "/var/lib/crab/gateway/gateway.toml"
        # Explicit mode must serve without rewriting the generated exports.
        docker("run", "--rm", "-v", f"{name}:/state", "--entrypoint", "/bin/rm",
               args.image, "/state/client.env")
        start(name, env, "--config", explicit_config)
        wait_ready(name, explicit_config)
        s3 = client(endpoint(name, 8080), access, secret)
        assert s3.get_object(Bucket="repository", Key="main/setup.txt")["Body"].read() == body
        assert docker("exec", name, "test", "-e", "/var/lib/crab/gateway/client.env", check=False).returncode != 0
        docker("rm", "-f", "-v", name)

        start(name, {**env, "AWS_BUCKET": f"{bucket}-other"})
        assert docker("wait", name).stdout.strip() != "0"
        docker("rm", "-v", name)

        incompatible = f"{bucket}-incompatible"
        origin.create_bucket(Bucket=incompatible)
        marker = "repositories/default/legacy-marker"
        origin.put_object(Bucket=incompatible, Key=marker, Body=b"preserve")
        start(f"{name}-incompatible", {**env, "AWS_BUCKET": incompatible})
        assert docker("wait", name).stdout.strip() != "0"
        assert origin.get_object(Bucket=incompatible, Key=marker)["Body"].read() == b"preserve"
        assert origin.list_objects_v2(Bucket=incompatible)["KeyCount"] == 1
    print(json.dumps({"single_container": "passed", "signed_io": "passed", "replacement": "passed", "explicit_config": "passed", "fail_closed": "passed"}))


if __name__ == "__main__":
    main()
