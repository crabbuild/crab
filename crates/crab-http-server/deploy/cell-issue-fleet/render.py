#!/usr/bin/env python3
"""Render an isolated 3/5/10/20-node Compose Cell issue service."""

import argparse
import json
import re
from pathlib import Path


ROOT = Path(__file__).resolve().parents[4]
HERE = Path(__file__).resolve().parent
RUSTFS_IMAGE = "ghcr.io/rustfs/rustfs:1.0.0-beta.8-glibc@sha256:040304b66e029a5cde4bed140b41513e925909839a9b912a40a98340610d1f66"
AWS_IMAGE = "public.ecr.aws/aws-cli/aws-cli:2.27.41@sha256:1c2d7a51b1ff4f460bc3f1e1ee46a6cef47c7429ad9089ef99012a95e472a1a5"
CADDY_IMAGE = "caddy:2.10.2-alpine@sha256:4c6e91c6ed0e2fa03efd5b44747b625fec79bc9cd06ac5235a779726618e530d"
BUCKET = "crab-cell-issue-fleet"
CONFIG = "/etc/crab/server.toml"
CPU_LIMIT = 1_000_000_000
MEMORY_LIMIT = 1_073_741_824


def node_name(index: int) -> str:
    return f"node-{index:02d}"


def node_config(index: int) -> str:
    return (
        f'listen = "127.0.0.1:{8200 + index}"\n'
        f'management_listen = "127.0.0.1:{9200 + index}"\n\n'
        '[cells]\n'
        'data_dir = "/var/lib/crab/cells"\n'
        'local_disk_limit_bytes = 32212254720\n'
        f'peer_advertise = "https://localhost:{9200 + index}"\n'
        'failure_zone = "compose-local"\n'
        f'failure_host = "{node_name(index)}"\n'
        'peer_certificate = "/run/secrets/crab-peer/peer.crt"\n'
        'peer_private_key = "/run/secrets/crab-peer/peer.key"\n'
        'peer_ca = "/run/secrets/crab-peer/ca.crt"\n\n'
        '[storage]\n'
        f'url = "s3://{BUCKET}/repositories"\n'
    )


def caddyfile() -> str:
    upstreams = " ".join(f"127.0.0.1:{8200 + index}" for index in range(1, 21))
    result = [
        "{",
        "\tadmin off",
        "\tauto_https off",
        "}",
        "",
        ":8090 {",
        f"\treverse_proxy {upstreams} {{",
        "\t\tlb_policy round_robin",
        "\t\tlb_try_duration 3s",
        "\t\tlb_try_interval 100ms",
        "\t\thealth_uri /livez",
        "\t\thealth_interval 1s",
        "\t\thealth_timeout 1s",
        "\t\theader_down X-Crab-Fleet-Entry {rp.upstream.hostport}",
        "\t\tflush_interval -1",
        "\t}",
        "}",
    ]
    for index in range(1, 21):
        result.extend(
            [
                "",
                f":{8100 + index} {{",
                f"\treverse_proxy 127.0.0.1:{8200 + index} {{",
                "\t\tflush_interval -1",
                "\t}",
                "}",
            ]
        )
    return "\n".join(result) + "\n"


def compose(
    state: Path, project: str, gateway_port: int, node_port_base: int, rustfs_port: int
) -> dict:
    server_image = f"{project}:local"
    storage_env = {
        "AWS_ACCESS_KEY_ID": "crab",
        "AWS_SECRET_ACCESS_KEY": "crab",
        "AWS_DEFAULT_REGION": "us-east-1",
        "AWS_ENDPOINT_URL_S3": "http://rustfs:9000",
        "AWS_ALLOW_HTTP": "true",
        "AWS_VIRTUAL_HOSTED_STYLE_REQUEST": "false",
        "AWS_EC2_METADATA_DISABLED": "true",
    }
    services = {
        "rustfs": {
            "image": RUSTFS_IMAGE,
            "ports": [f"127.0.0.1:{rustfs_port}:9000"],
            "environment": {
                "RUSTFS_ACCESS_KEY": "crab",
                "RUSTFS_SECRET_KEY": "crab",
                "RUSTFS_CONSOLE_ENABLE": "false",
                "RUSTFS_OBS_LOG_DIRECTORY": "/data/logs",
            },
            "volumes": ["rustfs-data:/data"],
            "ulimits": {"nofile": {"soft": 65535, "hard": 65535}},
            "healthcheck": {
                "test": ["CMD", "curl", "--fail", "--silent", "http://127.0.0.1:9000/health/ready"],
                "interval": "2s",
                "timeout": "2s",
                "retries": 30,
                "start_period": "5s",
            },
        },
        "bucket-init": {
            "image": AWS_IMAGE,
            "entrypoint": ["/bin/sh", "-ec"],
            "command": [(
                f"aws --endpoint-url http://rustfs:9000 s3api head-bucket --bucket {BUCKET} "
                f"2>/dev/null || aws --endpoint-url http://rustfs:9000 s3api "
                f"create-bucket --bucket {BUCKET}"
            )],
            "environment": storage_env,
            "depends_on": {"rustfs": {"condition": "service_healthy"}},
            "restart": "no",
        },
        "peer-pki-init": {
            "image": RUSTFS_IMAGE,
            "user": "0:0",
            "entrypoint": ["/bin/sh", "/scripts/peer-pki-init.sh"],
            "volumes": ["peer-identity:/identity", f"{state / 'peer-pki-init.sh'}:/scripts/peer-pki-init.sh:ro"],
            "cap_drop": ["ALL"],
            "cap_add": ["CHOWN"],
            "restart": "no",
        },
    }
    init_service = {
        "image": server_image,
        "environment": storage_env,
        "volumes": [
            f"{state / 'config' / 'node-01.toml'}:{CONFIG}:ro",
            "init-data:/var/lib/crab/cells",
        ],
        "read_only": True,
        "tmpfs": ["/var/lib/crab/tmp:rw,noexec,nosuid,nodev,size=64m,uid=10001,gid=10001,mode=0700"],
        "restart": "no",
    }
    services["release-init"] = {
        **init_service,
        "build": {
            "context": str(ROOT),
            "dockerfile": "crates/crab-http-server/deploy/Dockerfile",
        },
        "command": ["--config", CONFIG, "cells", "release", "bootstrap", "--image", "sha256:" + "1" * 64],
        "depends_on": {"bucket-init": {"condition": "service_completed_successfully"}},
    }
    services["repository-init"] = {
        **init_service,
        "command": ["--config", CONFIG, "repository", "list"],
        "depends_on": {"release-init": {"condition": "service_completed_successfully"}},
    }
    services["fleet-net"] = {
        "image": server_image,
        "entrypoint": ["/bin/sh", "-ec"],
        "command": ["while :; do sleep 3600; done"],
        "healthcheck": {
            "test": ["CMD", "/usr/bin/test", "-x", "/usr/bin/sleep"],
            "interval": "2s",
            "timeout": "2s",
            "retries": 15,
        },
        "ports": [f"127.0.0.1:{gateway_port}:8090"]
        + [f"127.0.0.1:{node_port_base + index}:{8100 + index}" for index in range(1, 21)],
        "read_only": True,
        "restart": "no",
    }
    for index in range(1, 21):
        service = {
            "image": server_image,
            "environment": storage_env,
            "network_mode": "service:fleet-net",
            "volumes": [
                f"{state / 'config' / f'{node_name(index)}.toml'}:{CONFIG}:ro",
                "peer-identity:/run/secrets/crab-peer:ro",
            ],
            "cpus": 1.0,
            "mem_limit": MEMORY_LIMIT,
            "memswap_limit": MEMORY_LIMIT,
            "read_only": True,
            "cap_drop": ["ALL"],
            "security_opt": ["no-new-privileges:true"],
            "tmpfs": ["/var/lib/crab/tmp:rw,noexec,nosuid,nodev,size=64m,uid=10001,gid=10001,mode=0700"],
            "depends_on": {
                "fleet-net": {"condition": "service_started"},
                "repository-init": {"condition": "service_completed_successfully"},
                "peer-pki-init": {"condition": "service_completed_successfully"},
            },
            "stop_grace_period": "2m",
            "restart": "no",
        }
        service["volumes"].append(f"{node_name(index)}-data:/var/lib/crab/cells")
        if index > 10:
            service["profiles"] = ["twenty"]
        elif index > 5:
            service["profiles"] = ["ten"]
        elif index > 3:
            service["profiles"] = ["five"]
        services[node_name(index)] = service
    services["gateway"] = {
        "image": CADDY_IMAGE,
        "network_mode": "service:fleet-net",
        "user": "65534:65534",
        "volumes": [f"{state / 'Caddyfile'}:/etc/caddy/Caddyfile:ro"],
        "read_only": True,
        "cap_drop": ["ALL"],
        "cap_add": ["NET_BIND_SERVICE"],
        "tmpfs": [
            "/config:rw,noexec,nosuid,nodev,size=8m,uid=65534,gid=65534,mode=0700",
            "/data:rw,noexec,nosuid,nodev,size=8m,uid=65534,gid=65534,mode=0700",
        ],
        "healthcheck": {
            "test": ["CMD", "curl", "--fail", "--silent", "http://127.0.0.1:8090/livez"],
            "interval": "2s",
            "timeout": "2s",
            "retries": 15,
        },
        "depends_on": {"node-01": {"condition": "service_healthy"}},
        "restart": "no",
    }
    volumes = {"rustfs-data": {}, "peer-identity": {}, "init-data": {}}
    volumes.update({f"{node_name(index)}-data": {} for index in range(1, 21)})
    return {"name": project, "services": services, "volumes": volumes}


def render(
    state: Path, project: str, gateway_port: int, node_port_base: int, rustfs_port: int
) -> Path:
    state = state.expanduser().resolve()
    if state.is_relative_to(ROOT):
        raise ValueError("state must be outside the repository")
    if not re.fullmatch(r"crab-cell-issue-[a-z0-9-]+", project):
        raise ValueError("project must be named crab-cell-issue-*")
    if not all(
        1024 <= port <= 65535
        for port in (gateway_port, node_port_base + 1, node_port_base + 20, rustfs_port)
    ):
        raise ValueError("host ports must be unprivileged and valid")
    node_ports = range(node_port_base + 1, node_port_base + 21)
    if gateway_port == rustfs_port or gateway_port in node_ports or rustfs_port in node_ports:
        raise ValueError("host ports overlap")
    (state / "config").mkdir(parents=True, exist_ok=True)
    for index in range(1, 21):
        (state / "config" / f"{node_name(index)}.toml").write_text(node_config(index))
    (state / "Caddyfile").write_text(caddyfile())
    (state / "peer-pki-init.sh").write_text((HERE / "peer-pki-init.sh").read_text())
    path = state / "compose.yaml"
    rendered = compose(state, project, gateway_port, node_port_base, rustfs_port)
    path.write_text(json.dumps(rendered, indent=2) + "\n")
    return path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--project", required=True)
    parser.add_argument("--gateway-port", type=int, default=18080)
    parser.add_argument("--node-port-base", type=int, default=18100)
    parser.add_argument("--rustfs-port", type=int, default=19010)
    args = parser.parse_args()
    print(
        render(
            args.state, args.project, args.gateway_port, args.node_port_base, args.rustfs_port
        )
    )


if __name__ == "__main__":
    main()
