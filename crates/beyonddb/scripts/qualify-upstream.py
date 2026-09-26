#!/usr/bin/env python3
# /// script
# dependencies = ["boto3", "pytest"]
# requires-python = ">=3.11"
# ///
"""Run unchanged ExtendDB Python tests against a fresh local BeyondDB/RustFS pair."""

import argparse
import hashlib
import json
import os
import shlex
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from importlib.metadata import version
from pathlib import Path

import boto3
from botocore.config import Config
from botocore.exceptions import BotoCoreError, ClientError

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument(
    "--binary", type=Path, required=True, help="Already-built BeyondDB binary"
)
parser.add_argument(
    "--tests-root",
    type=Path,
    required=True,
    help="Read-only ExtendDB tests/python directory",
)
parser.add_argument(
    "--artifacts",
    type=Path,
    required=True,
    help="Existing directory on the workspace volume",
)
parser.add_argument(
    "tests",
    nargs="*",
    help="Relative pytest files/selectors; defaults to the full suite",
)
args = parser.parse_args()
binary = args.binary.resolve(strict=True)
upstream = args.tests_root.resolve(strict=True)
artifacts = args.artifacts.resolve(strict=True)
root = Path(tempfile.mkdtemp(prefix="beyonddb-upstream-", dir=artifacts))
revision = subprocess.check_output(
    ["git", "-C", str(upstream), "rev-parse", "HEAD"], text=True
).strip()
with binary.open("rb") as compiled:
    binary_digest = hashlib.file_digest(compiled, "sha256").hexdigest()
(root / "qualification.json").write_text(
    json.dumps(
        {
            "binary": str(binary),
            "binary_sha256": binary_digest,
            "upstream_revision": revision,
            "tests": args.tests or ["."],
            "boto3": version("boto3"),
            "pytest": version("pytest"),
        },
        indent=2,
    )
)
print("Qualification artifacts:", root, flush=True)
(root / "objects").mkdir()


def port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def run(*args):
    subprocess.run(
        args, cwd=root, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE
    )


run("openssl", *shlex.split("genpkey -algorithm ED25519 -out ca.key"))
run(
    "openssl",
    *shlex.split(
        "req -x509 -new -days 1 -subj '/CN=BeyondDB qualification CA' "
        "-addext basicConstraints=critical,CA:TRUE -key ca.key -out ca.crt"
    ),
)
run("openssl", *shlex.split("genpkey -algorithm ED25519 -out peer.key"))
run("openssl", *shlex.split("req -new -subj /CN=localhost -key peer.key -out peer.csr"))
(root / "peer.ext").write_text(
    "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n"
    "extendedKeyUsage=serverAuth,clientAuth\nsubjectAltName=DNS:localhost\n"
)
run(
    "openssl",
    *shlex.split(
        "x509 -req -days 1 -CAcreateserial -in peer.csr -CA ca.crt -CAkey ca.key "
        "-extfile peer.ext -out peer.crt"
    ),
)
(root / "encryption.key").write_bytes(bytes([23]) * 32)
(root / "policy.json").write_text(
    json.dumps(
        {
            "Version": "2012-10-17",
            "Statement": [{"Effect": "Allow", "Action": "dynamodb:*", "Resource": "*"}],
        }
    )
)
s3, peer, public = port(), port(), port()
config = {
    "storage_url": "s3://beyonddb-qualification/beyonddb",
    "node_id": "01994f26-5966-7b20-8b58-2fddf198a321",
    "data_dir": str(root / "data"),
    "disk_budget_bytes": 4 * 1024**3,
    "encryption_key_file": str(root / "encryption.key"),
    "region": "us-east-1",
    "peer_bind": f"127.0.0.1:{peer}",
    "peer_endpoint": f"https://127.0.0.1:{peer}",
    "peer_certificate": str(root / "peer.crt"),
    "peer_private_key": str(root / "peer.key"),
    "peer_ca": str(root / "ca.crt"),
    "peer_server_name": "localhost",
    "public_bind": f"127.0.0.1:{public}",
    "public_endpoint": f"http://127.0.0.1:{public}",
    "owned_accounts": ["123456789012"],
    "owned_access_keys": ["AKIAIOSFODNN7EXAMPLE"],
    "initial_partitions": 2,
    "bootstrap": {
        "account_id": "123456789012",
        "access_key_id": "AKIAIOSFODNN7EXAMPLE",
        "principal_name": "qualification-user",
        "policy_name": "qualification",
        "policy_file": str(root / "policy.json"),
    },
}
(root / "config.json").write_text(json.dumps(config))
children = []
try:
    rustfs = subprocess.Popen(
        ["rustfs", "server", "--address", f"127.0.0.1:{s3}", str(root / "objects")],
        env=dict(os.environ, RUSTFS_ACCESS_KEY="crab", RUSTFS_SECRET_KEY="crab"),
        stdout=subprocess.DEVNULL,
        stderr=(root / "rustfs.log").open("w"),
    )
    children.append(rustfs)
    client = boto3.client(
        "s3",
        endpoint_url=f"http://127.0.0.1:{s3}",
        aws_access_key_id="crab",
        aws_secret_access_key="crab",
        region_name="us-east-1",
        config=Config(retries={"max_attempts": 0}, connect_timeout=1, read_timeout=1),
    )
    for _ in range(100):
        if rustfs.poll() is not None:
            raise RuntimeError("RustFS exited")
        try:
            client.list_buckets()
            break
        except (BotoCoreError, ClientError):
            time.sleep(0.1)
    client.create_bucket(Bucket="beyonddb-qualification")
    env = dict(
        os.environ,
        AWS_ACCESS_KEY_ID="crab",
        AWS_SECRET_ACCESS_KEY="crab",
        AWS_REGION="us-east-1",
        AWS_ALLOW_HTTP="true",
        AWS_ENDPOINT_URL_S3=f"http://127.0.0.1:{s3}",
        AWS_VIRTUAL_HOSTED_STYLE_REQUEST="false",
    )
    env.pop("AWS_SESSION_TOKEN", None)
    server = subprocess.Popen(
        [str(binary), str(root / "config.json"), "--bootstrap"],
        env=env,
        stdin=subprocess.PIPE,
        stdout=(root / "server.log").open("w"),
        stderr=subprocess.STDOUT,
    )
    children.append(server)
    server.stdin.write(b"wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n")
    server.stdin.close()
    for _ in range(450):
        if server.poll() is not None:
            raise RuntimeError("BeyondDB exited; see server.log")
        try:
            with urllib.request.urlopen(
                f"http://127.0.0.1:{public}/health", timeout=1
            ) as r:
                if r.status == 200:
                    break
        except OSError:
            time.sleep(0.1)
    else:
        raise RuntimeError("BeyondDB readiness timed out")
    env = dict(
        os.environ,
        AWS_ACCESS_KEY_ID="AKIAIOSFODNN7EXAMPLE",
        AWS_SECRET_ACCESS_KEY="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        AWS_DEFAULT_REGION="us-east-1",
        DYNAMODB_ENDPOINT=f"http://127.0.0.1:{public}",
        EXTENDDB_TEST_ENDPOINT=f"http://127.0.0.1:{public}",
        PYTHONDONTWRITEBYTECODE="1",
        EXTENDDB_VALIDATION_MODE="false",
    )
    env.pop("AWS_SESSION_TOKEN", None)
    result = subprocess.run(
        [
            sys.executable,
            "-m",
            "pytest",
            "-p",
            "no:cacheprovider",
            f"--confcutdir={upstream}",
            "-x",
            "-v",
            "--tb=short",
            f"--junitxml={root / 'results.xml'}",
            *[str(upstream / name) for name in (args.tests or ["."])],
        ],
        env=env,
        cwd=root,
        check=False,
    )
    print("Result:", result.returncode, "artifacts:", root, flush=True)
    sys.exit(result.returncode)
finally:
    for child in reversed(children):
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=20)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
