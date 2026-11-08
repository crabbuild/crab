#!/usr/bin/env python3
"""Qualify a native image on an explicitly selected, disposable three-node Kind cluster."""

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid

import yaml

import large_files
from project_root_backup import copy_root


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kubeconfig", required=True, type=Path)
    parser.add_argument("--cluster", required=True, help="Disposable Kind cluster name")
    parser.add_argument("--image", required=True, help="Locally built Linux image")
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--artifacts", required=True, type=Path)
    parser.add_argument("--large-file-bytes", type=int, default=3 * 1024**3)
    args = parser.parse_args()
    if args.large_file_bytes < 2:
        parser.error("--large-file-bytes must be at least two")
    run = args.artifacts.resolve() / str(uuid.uuid4())
    run.mkdir(parents=True)
    if shutil.disk_usage(run).free < 6 * args.large_file_bytes + 8 * 1024**3:
        parser.error("insufficient artifact disk space for independent large-file copies")
    namespace = "crab-qualification-" + uuid.uuid4().hex[:8]
    registry = namespace + "-registry"
    forwards = []
    nodes = []
    created = False
    registry_created = False
    registry_directory = None
    storage_node = None
    storage_directory = None
    kube = ["kubectl", "--kubeconfig", str(args.kubeconfig.resolve()),
            "--context", "kind-" + args.cluster]
    client_env = os.environ | {"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"}
    with (run / "commands.log").open("w") as log:

        def cmd(arguments, **kwargs):
            return subprocess.run([str(x) for x in arguments], check=True,
                                  stdout=log, stderr=log, **kwargs)

        def output(arguments, **kwargs):
            return subprocess.check_output([str(x) for x in arguments], text=True, stderr=log, **kwargs)

        def kubectl(*arguments):
            return [*kube, "--namespace", namespace, *arguments]

        def apply(value):
            cmd(kubectl("apply", "-f", "-"), input=json.dumps(value), text=True)

        def port():
            with socket.socket() as connection:
                connection.bind(("127.0.0.1", 0))
                return connection.getsockname()[1]

        def forward(resource, remote):
            local = port()
            process = subprocess.Popen(kubectl("port-forward", "--address", "127.0.0.1",
                                              resource, f"{local}:{remote}"), stdout=log, stderr=log)
            forwards.append(process)
            for _ in range(120):
                if process.poll() is not None:
                    raise RuntimeError("port-forward exited; see commands.log")
                try:
                    with socket.create_connection(("127.0.0.1", local), timeout=1):
                        return f"http://127.0.0.1:{local}"
                except OSError:
                    time.sleep(0.25)
            raise RuntimeError("port-forward did not become ready")

        def pods():
            return json.loads(output(kubectl("get", "pods", "--selector",
                              "app.kubernetes.io/instance=qualification", "-o", "json")))["items"]

        def ready():
            cmd(kubectl("rollout", "status", "deployment/qualification-crab-server", "--timeout=300s"))
            current = pods()
            assert len(current) == 3 and all(p["status"]["containerStatuses"][0]["ready"] for p in current)
            assert len({p["spec"]["nodeName"] for p in current}) == 3
            return current

        def request(origin, path, body=None):
            headers = {"Content-Type": "application/json"}
            if body is not None:
                headers["X-Crab-Repository-Id"] = repository_id
            request = urllib.request.Request(origin + path, headers=headers,
                                             data=None if body is None else json.dumps(body).encode())
            with urllib.request.urlopen(request, timeout=35) as response:
                return json.load(response)

        def server(pod, *arguments):
            return kubectl("exec", pod, "--", "crab-server", "--config",
                           "/etc/crab/http-server/server.toml", *arguments)

        try:
            # Kind's labels and Docker node identities are checked before any
            # mutation. This fixture must never operate on a customer cluster.
            nodes = output(["kind", "get", "nodes", "--name", args.cluster]).split()
            actual = json.loads(output([*kube, "get", "nodes", "-o", "json"]))["items"]
            assert len(nodes) == 3 and set(nodes) == {p["metadata"]["name"] for p in actual}
            for node in nodes:
                labels = json.loads(output(["docker", "inspect", node]))[0]["Config"]["Labels"]
                assert labels["io.x-k8s.kind.cluster"] == args.cluster
            image = json.loads(output(["docker", "image", "inspect", args.image]))[0]
            assert image["Os"] == "linux"
            assert image["Config"]["Labels"]["org.opencontainers.image.revision"] == args.source_revision
            assert output([*kube, "config", "current-context"]).strip() == "kind-" + args.cluster
            storage_nodes = [node["metadata"]["name"] for node in actual
                             if node["metadata"]["labels"].get("crab.build/qualification-storage") == "true"]
            assert len(storage_nodes) == 1, "Use tests/kind.yaml with its dedicated object-store mount"
            storage_node = storage_nodes[0]
            blocks, block_size = output(["docker", "exec", storage_node, "stat", "--file-system", "--format=%a:%S", "/crab-objectstore"]).strip().split(":")
            assert int(blocks) * int(block_size) > 2 * args.large_file_bytes + 4 * 1024**3, "Insufficient object-store backing space"
            directory = "/crab-objectstore/" + namespace
            cmd(["docker", "exec", "--user", "10001", storage_node, "mkdir", "--mode=0700", directory])
            storage_directory = directory
            devices = output(["docker", "exec", storage_node, "stat", "--format=%d", directory, "/var/lib/kubelet"]).split()
            cmd([*kube, "create", "namespace", namespace])
            created = True

            # Follow Kind's local-registry contract. The chart still receives
            # an immutable manifest digest, rather than a substituted image tag.
            registry_port = port()
            cmd(["docker", "run", "--detach", "--name", registry, "--network", "kind",
                 "--publish", f"127.0.0.1:{registry_port}:5000",
                 "registry:2.8.3@sha256:a3d8aaa63ed8681a604f1dea0aa03f100d5895b6a58ace528858a7b332415373"])
            registry_created = True
            registry_directory = f"/etc/containerd/certs.d/localhost:{registry_port}"
            for node in nodes:
                cmd(["docker", "exec", node, "mkdir", "-p", registry_directory])
                cmd(["docker", "exec", "--interactive", node, "cp", "/dev/stdin", registry_directory + "/hosts.toml"],
                    input=f'[host."http://{registry}:5000"]\n', text=True)
            repository = f"localhost:{registry_port}/crab-server"
            reference = repository + ":qualification"
            cmd(["docker", "tag", args.image, reference])
            cmd(["docker", "push", reference])
            digests = json.loads(output(["docker", "image", "inspect", reference]))[0]["RepoDigests"]
            immutable = next(value for value in digests if value.startswith(repository + "@sha256:"))
            digest = immutable.split("@", 1)[1]

            secret = uuid.uuid4().hex
            storage_env = {"AWS_ACCESS_KEY_ID": "crab-local-access", "AWS_SECRET_ACCESS_KEY": secret,
                           "AWS_REGION": "us-east-1", "AWS_DEFAULT_REGION": "us-east-1",
                           "AWS_ENDPOINT_URL_S3": "http://rustfs:9000", "AWS_ALLOW_HTTP": "true",
                           "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false", "AWS_EC2_METADATA_DISABLED": "true"}
            apply({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "rustfs", "labels": {"fixture": "rustfs"}},
                   "spec": {"nodeSelector": {"crab.build/qualification-storage": "true"},
                            "securityContext": {"runAsUser": 10001, "runAsGroup": 10001, "fsGroup": 10001},
                            "containers": [{"name": "rustfs", "image": "ghcr.io/rustfs/rustfs:1.0.0-glibc@sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858",
                                            "args": ["/data"], "env": [{"name": "RUSTFS_ACCESS_KEY", "value": storage_env["AWS_ACCESS_KEY_ID"]},
                                                                      {"name": "RUSTFS_SECRET_KEY", "value": secret}],
                                            "readinessProbe": {"tcpSocket": {"port": 9000}, "periodSeconds": 1},
                                            "volumeMounts": [{"name": "data", "mountPath": "/data"}]}],
                            "volumes": [{"name": "data", "hostPath": {"path": directory, "type": "Directory"}}]}})
            apply({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "rustfs"},
                   "spec": {"selector": {"fixture": "rustfs"}, "ports": [{"port": 9000}]}})
            cmd(kubectl("wait", "--for=condition=Ready", "pod/rustfs", "--timeout=180s"))
            endpoint = forward("pod/rustfs", 9000)
            host_env = os.environ | storage_env | {"AWS_ENDPOINT_URL_S3": endpoint}
            host_env.pop("AWS_SESSION_TOKEN", None)
            # RustFS opens its health route before loading the root IAM identity.
            # Admit the fixture only after an authenticated storage operation.
            for _ in range(120):
                try:
                    cmd(["aws", "--endpoint-url", endpoint, "s3api", "list-buckets"], env=host_env)
                    break
                except subprocess.CalledProcessError:
                    time.sleep(0.25)
            else:
                raise RuntimeError("RustFS failed authenticated S3 readiness")
            cmd(["aws", "--endpoint-url", endpoint, "s3api", "create-bucket", "--bucket", "crab-kubernetes-test"], env=host_env)

            cmd(["openssl", "req", "-x509", "-newkey", "ed25519", "-nodes", "-keyout", run / "ca.key",
                 "-out", run / "ca.crt", "-subj", "/CN=Local qualification CA", "-days", "1",
                 "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign"])
            cmd(["openssl", "req", "-new", "-newkey", "ed25519", "-nodes", "-keyout", run / "tls.key",
                 "-out", run / "peer.csr", "-subj", "/CN=crab-server-peer"])
            (run / "peer.ext").write_text("subjectAltName=DNS:crab-server-peer\nextendedKeyUsage=serverAuth,clientAuth\nkeyUsage=critical,digitalSignature\n")
            cmd(["openssl", "x509", "-req", "-in", run / "peer.csr", "-CA", run / "ca.crt", "-CAkey", run / "ca.key",
                 "-CAcreateserial", "-out", run / "tls.crt", "-days", "1", "-extfile", run / "peer.ext"])
            cmd(kubectl("create", "secret", "generic", "peers", "--from-file=tls.crt=" + str(run / "tls.crt"),
                        "--from-file=tls.key=" + str(run / "tls.key"), "--from-file=ca.crt=" + str(run / "ca.crt")))
            cmd(kubectl("create", "secret", "generic", "state", "--from-literal=state-key=" + uuid.uuid4().hex))
            storage = "s3://crab-kubernetes-test/repositories"

            def config():
                text = f'''listen = "127.0.0.1:8788"
management_listen = "0.0.0.0:8789"
[storage]
url = "{storage}"
[cells]
data_dir = "/var/lib/crab/cells"
local_disk_limit_bytes = 34359738368
peer_advertise = "https://127.0.0.1:8789"
peer_tls_server_name = "crab-server-peer"
peer_certificate = "/run/secrets/crab/peer/tls.crt"
peer_private_key = "/run/secrets/crab/peer/tls.key"
peer_ca = "/run/secrets/crab/peer/ca.crt"
'''
                apply({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "server"}, "data": {"server.toml": text}})

            config()
            chart = Path(__file__).resolve().parents[1] / "deploy/helm/crab-server"
            values = {"image": {"repository": repository, "digest": digest},
                      "config": {"existingConfigMap": "server"},
                      "secrets": {"existingSecret": "state", "oidcClientSecretKey": ""},
                      "cells": {"existingSecret": "peers"}}
            (run / "values.json").write_text(json.dumps(values))
            # Static credentials belong only to this isolated RustFS fixture.
            # Production chart admission continues to require workload identity.
            renderer = run / "fixture-renderer"
            renderer.write_text(f"#!{sys.executable}\nimport sys,yaml\nenv={storage_env!r}\n"
                                "for value in yaml.safe_load_all(sys.stdin):\n"
                                " if value and value.get('kind') == 'Deployment':\n"
                                "  value['spec']['template']['spec']['containers'][0]['env'] += [{'name':k,'value':v} for k,v in env.items()]\n"
                                " if value: print(yaml.safe_dump(value)+'---')\n")
            renderer.chmod(0o700)
            helm = ["helm", "--kubeconfig", str(args.kubeconfig.resolve()), "--kube-context", "kind-" + args.cluster]
            rendered = output(["helm", "template", "qualification", chart, "--namespace", namespace,
                               "--values", run / "values.json"])
            deployment = next(value for value in yaml.safe_load_all(rendered) if value and value["kind"] == "Deployment")
            assert deployment["spec"]["strategy"] == {"type": "Recreate"}
            bootstrap = copy.deepcopy(deployment["spec"]["template"]["spec"])
            # This pod only sleeps between completed initializer commands;
            # unlike the server it has no native shutdown work to drain.
            bootstrap["terminationGracePeriodSeconds"] = 1
            container = bootstrap["containers"][0]
            for key in ["startupProbe", "readinessProbe", "livenessProbe", "lifecycle", "ports"]:
                container.pop(key)
            container.update(command=["/usr/bin/sleep"], args=["infinity"])
            container["env"] += [{"name": key, "value": value} for key, value in storage_env.items()]
            bootstrap.pop("topologySpreadConstraints")
            apply({"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {"name": "qualification-crab-server"}})
            apply({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "bootstrap"}, "spec": bootstrap})
            cmd(kubectl("wait", "--for=condition=Ready", "pod/bootstrap", "--timeout=180s"))
            cmd(server("bootstrap", "repository", "create", "--owner", "team", "--name", "large-files", "--prefix", "team/large-files"))
            cmd(kubectl("delete", "pod/bootstrap", "--wait=true"))
            cmd(kubectl("delete", "serviceaccount/qualification-crab-server"))

            def deploy():
                cmd([*helm, "upgrade", "--install", "qualification", chart, "--namespace", namespace,
                     "--values", run / "values.json", "--post-renderer", renderer,
                     "--set-string", "rolloutToken=" + uuid.uuid4().hex, "--wait", "--timeout", "5m"])
                return ready()

            current = deploy()
            pod = current[0]["metadata"]["name"]
            runtime = json.loads(output(server(pod, "cells", "runtime")))
            assert runtime["version"] == "0.1.1"
            cmd(server(pod, "cells", "initialize-directory"))
            origin = forward("pod/" + pod, 8788)
            repository_id = request(origin, "/api/repos")["repositories"][0]["id"]
            body = {"request_id": str(uuid.uuid4()), "title": "Kubernetes recovery", "body": "Durable native state"}
            path = "/api/repos/team/large-files/issues"
            issue = request(origin, path, body)
            (run / "issue.json").write_text(json.dumps(issue, indent=2))
            last_revision = issue["reactions"]["revision"]

            def replay(origin, phase):
                nonlocal last_revision
                recovered = request(origin, path, body)
                (run / ("issue-" + phase + ".json")).write_text(json.dumps(recovered, indent=2))
                expected = copy.deepcopy(issue)
                # Reaction revisions order fresh native reads across repository
                # work; issue data and reaction groups must remain identical.
                expected["reactions"].pop("revision")
                after = recovered["reactions"].pop("revision")
                assert after >= last_revision, {"before": last_revision, "after": after}
                last_revision = after
                assert recovered == expected, {"before": expected, "after": recovered}

            for index, peer in enumerate(current):
                peer_origin = forward("pod/" + peer["metadata"]["name"], 8788)
                replay(peer_origin, "initial-" + str(index))
            content = large_files.prepare(cmd, run, origin, args.large_file_bytes, client_env)
            large_files.verify_resume(run, origin, content)
            refs = output(["git", "ls-remote", origin + "/git/team/large-files.git"], env=client_env)

            status = current[0]["status"]["containerStatuses"][0]
            identity = status["containerID"].split("://", 1)[1]
            cmd(["docker", "exec", current[0]["spec"]["nodeName"], "ctr", "--namespace", "k8s.io",
                 "tasks", "kill", "--signal", "SIGKILL", identity])
            for _ in range(240):
                killed = json.loads(output(kubectl("get", "pod", pod, "-o", "json")))
                state = killed["status"]["containerStatuses"][0]
                if state["restartCount"] > status["restartCount"] and state["ready"]:
                    assert state["lastState"]["terminated"]["exitCode"] == 137
                    break
                time.sleep(0.5)
            else:
                raise RuntimeError("SIGKILL recovery did not become ready")
            origin = forward("pod/" + pod, 8788)
            replay(origin, "crash")
            large_files.verify(cmd, run, origin, content, "kubernetes-crash", client_env)
            (run / "crashed-container.log").write_text(output(kubectl("logs", pod, "--previous")))

            cmd(kubectl("scale", "deployment/qualification-crab-server", "--replicas=0"))
            cmd(kubectl("wait", "--for=delete", "pod", "--selector", "app.kubernetes.io/instance=qualification", "--timeout=180s"))
            assert not pods(), "All native writers must stop before backup"
            (run / "backup").mkdir()
            storage, backup, cleanup_backup = copy_root(storage, run / "backup", host_env)
            config()
            restored = deploy()
            assert not ({p["metadata"]["uid"] for p in current} & {p["metadata"]["uid"] for p in restored})
            origin = forward("pod/" + restored[0]["metadata"]["name"], 8788)
            replay(origin, "restore")
            assert output(["git", "ls-remote", origin + "/git/team/large-files.git"], env=client_env) == refs
            for index, peer in enumerate(restored):
                peer_origin = forward("pod/" + peer["metadata"]["name"], 8788)
                replay(peer_origin, "restored-" + str(index))
            large_files.verify(cmd, run, origin, content, "kubernetes-restore", client_env)
            large_files.verify_resume(run, origin, content)
            cmd(kubectl("scale", "deployment/qualification-crab-server", "--replicas=0"))
            cmd(kubectl("wait", "--for=delete", "pod", "--selector", "app.kubernetes.io/instance=qualification", "--timeout=180s"))
            cleanup_backup()
            receipt = {"schema": 1, "source_revision": args.source_revision,
                       "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                       "large_file_helper_sha256": hashlib.sha256(Path(large_files.__file__).read_bytes()).hexdigest(),
                       "root_backup_helper_sha256": hashlib.sha256(Path(__file__).with_name("project_root_backup.py").read_bytes()).hexdigest(),
                       "deployment_template_sha256": hashlib.sha256((chart / "templates/deployment.yaml").read_bytes()).hexdigest(),
                       "image_id": image["Id"], "image_ref": immutable, "platform": image["Os"] + "/" + image["Architecture"],
                       "kubernetes": json.loads(output([*kube, "version", "--output=json"]))["serverVersion"],
                       "nodes": nodes, "native_pods": len(current), "native_runtime": runtime["version"],
                       "object_store_filesystem": devices[0], "staging_filesystem": devices[1],
                       "recreate_strategy": True, "sigkill_exit_code": 137, "submission_replay": True,
                       "full_root_restore": backup, "empty_cache_restore": True, "git_refs_verified": True,
                       "large_file": content | {"stock_git_lfs": True, "crash_clone": True, "restored_clone": True, "range_resume": True}}
            (run / "receipt.json").write_text(json.dumps(receipt, indent=2))
            print(json.dumps(receipt))
        finally:
            if created:
                for resource, name in [("pods", "pods.json"), ("events", "events.json")]:
                    result = subprocess.run(kubectl("get", resource, "-o", "json"), capture_output=True, text=True)
                    (run / name).write_text(result.stdout + result.stderr)
                try:
                    for pod in pods():
                        result = subprocess.run(kubectl("logs", pod["metadata"]["name"], "--all-containers"), capture_output=True, text=True)
                        (run / (pod["metadata"]["name"] + ".log")).write_text(result.stdout + result.stderr)
                except subprocess.CalledProcessError:
                    pass
            for process in forwards:
                process.terminate()
                process.wait(timeout=10)
            if created:
                # Storage must outlive native drains, including failed startup.
                # Deleting the namespace first can strand shutdown on dead S3.
                subprocess.run(kubectl("scale", "deployment/qualification-crab-server", "--replicas=0"), stdout=log, stderr=log)
                drained = subprocess.run(kubectl("wait", "--for=delete", "pod", "--selector", "app.kubernetes.io/instance=qualification", "--timeout=180s"), stdout=log, stderr=log)
                if drained.returncode == 0:
                    deleted = subprocess.run([*kube, "delete", "namespace", namespace, "--wait=true", "--timeout=180s"], stdout=log, stderr=log)
                    created = deleted.returncode != 0
            if not created and storage_directory:
                subprocess.run(["docker", "exec", "--user", "10001", storage_node, "rm", "-rf", storage_directory], stdout=log, stderr=log)
            if registry_created:
                subprocess.run(["docker", "rm", "--force", "--volumes", registry], stdout=log, stderr=log)
            if registry_directory:
                for node in nodes:
                    subprocess.run(["docker", "exec", node, "rm", "-rf", registry_directory], stdout=log, stderr=log)


if __name__ == "__main__":
    main()
