#!/usr/bin/env python3
"""Run a native-mTLS cache-service smoke with the real Crab CLI.

The harness creates a disposable CA, server certificate, and client
certificates; starts ``crab-cache-server`` with native mTLS and an
authorization policy; verifies that a client without a certificate cannot
complete TLS; verifies authenticated object warming/readback plus policy
denials; then configures a real Crab repo with ``crab config set`` and checks
``crab doctor --json``.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import socket
import ssl
import subprocess
import time
import urllib.error
import urllib.request
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


WORKSPACE_ROOT = Path(__file__).resolve().parents[3]
DEFAULT_ROOT = Path(os.environ.get("TMPDIR", "/tmp")) / "crab-cache-service-mtls-smoke"
DEFAULT_CRAB_BIN = WORKSPACE_ROOT / "target" / "debug" / "crab"
DEFAULT_CACHE_SERVER_BIN = WORKSPACE_ROOT / "target" / "debug" / "crab-cache-server"


class SmokeError(RuntimeError):
    """Raised when a smoke step fails."""


@dataclass
class CommandRecord:
    name: str
    args: list[str]
    cwd: str
    exit_code: int
    duration_ms: int
    stdout_log: str
    stderr_log: str


@dataclass
class SmokeReport:
    run_id: str
    status: str
    root: str
    cache_service_url: str = ""
    checks: list[dict[str, Any]] = field(default_factory=list)
    commands: list[dict[str, Any]] = field(default_factory=list)
    artifacts: dict[str, str] = field(default_factory=dict)
    updated_at: str = ""


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def make_run_id() -> str:
    return "cache-service-mtls-" + datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S")


def slug(value: str) -> str:
    out = "".join(c if c.isalnum() or c in "._-" else "-" for c in value.lower())
    return out.strip("-") or "command"


def find_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def resolve_executable(value: str) -> str | None:
    if os.sep in value:
        path = Path(value)
        if path.is_file() and os.access(path, os.X_OK):
            return str(path.resolve())
        return None
    return shutil.which(value)


def file_url(path: Path) -> str:
    return path.resolve().as_uri()


class CacheServiceMtlsSmoke:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.run_id = args.run_id or make_run_id()
        self.run_root = args.root / self.run_id
        self.logs = self.run_root / "logs"
        self.artifacts = self.run_root / "artifacts"
        self.certs = self.run_root / "certs"
        self.origin = self.run_root / "origin"
        self.cache_root = self.run_root / "server-cache"
        self.client_cache = self.run_root / "client-cache"
        self.command_index = 0
        self.crab_bin = resolve_executable(args.crab_bin) or args.crab_bin
        self.cache_server_bin = resolve_executable(args.cache_server_bin) or args.cache_server_bin
        self.cache_proc: subprocess.Popen[bytes] | None = None
        self.allowed_principal = ""
        self.denied_principal = ""
        self.report = SmokeReport(
            run_id=self.run_id,
            status="running",
            root=str(self.run_root),
            updated_at=utc_now(),
        )

    def write_report(self) -> None:
        self.artifacts.mkdir(parents=True, exist_ok=True)
        self.report.updated_at = utc_now()
        path = self.artifacts / "report.json"
        path.write_text(json.dumps(asdict(self.report), indent=2, sort_keys=True) + "\n")
        self.report.artifacts["report"] = str(path)

    def check(self, name: str, ok: bool, detail: dict[str, Any] | None = None) -> None:
        self.report.checks.append(
            {
                "name": name,
                "ok": ok,
                "detail": detail or {},
                "timestamp": utc_now(),
            }
        )
        self.write_report()
        if not ok:
            raise SmokeError(f"check failed: {name}")
        print(f"[ok] {name}")

    def next_log_paths(self, name: str) -> tuple[Path, Path]:
        self.command_index += 1
        base = f"{self.command_index:03d}-{slug(name)}"
        return self.logs / f"{base}.out.log", self.logs / f"{base}.err.log"

    def run_cmd(
        self,
        name: str,
        args: list[str],
        cwd: Path,
        *,
        check: bool = True,
        timeout: int | None = None,
        env: dict[str, str] | None = None,
    ) -> CommandRecord:
        stdout_log, stderr_log = self.next_log_paths(name)
        started = time.monotonic()
        with stdout_log.open("wb") as stdout, stderr_log.open("wb") as stderr:
            try:
                proc = subprocess.run(
                    args,
                    cwd=cwd,
                    env=env,
                    stdout=stdout,
                    stderr=stderr,
                    timeout=timeout or self.args.timeout,
                    check=False,
                )
            except FileNotFoundError as exc:
                raise SmokeError(f"{name} could not start: {exc}") from exc
        record = CommandRecord(
            name=name,
            args=args,
            cwd=str(cwd),
            exit_code=proc.returncode,
            duration_ms=int((time.monotonic() - started) * 1000),
            stdout_log=str(stdout_log),
            stderr_log=str(stderr_log),
        )
        self.report.commands.append(asdict(record))
        self.write_report()
        if check and proc.returncode != 0:
            stderr = stderr_log.read_text(encoding="utf-8", errors="replace")[-2000:]
            raise SmokeError(f"{name} failed with exit {proc.returncode}: {stderr}")
        return record

    def run_openssl(self, name: str, args: list[str]) -> None:
        self.run_cmd(
            "openssl " + name,
            ["openssl", *args],
            self.run_root,
            timeout=self.args.startup_timeout,
        )

    def setup_dirs(self) -> None:
        for path in (
            self.run_root,
            self.logs,
            self.artifacts,
            self.certs,
            self.origin,
            self.cache_root,
            self.client_cache,
        ):
            path.mkdir(parents=True, exist_ok=True)
        self.write_report()

    def preflight_tools(self) -> None:
        build_hint = (
            "run: cargo build -p crab --bin crab && "
            "cargo build -p crab-cache-server --bin crab-cache-server"
        )
        self.check("openssl-available", shutil.which("openssl") is not None)
        self.check(
            "crab-available",
            resolve_executable(self.crab_bin) is not None,
            {"crab_bin": self.args.crab_bin, "hint": build_hint},
        )
        self.check(
            "crab-cache-server-available",
            resolve_executable(self.cache_server_bin) is not None,
            {"cache_server_bin": self.args.cache_server_bin, "hint": build_hint},
        )

    def write_cert_configs(self) -> None:
        ca_config = """
[req]
distinguished_name = dn
x509_extensions = v3_ca
prompt = no

[dn]
CN = Crab Cache Smoke CA

[v3_ca]
basicConstraints = critical, CA:TRUE, pathlen:0
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid:always,issuer
""".lstrip()
        server_ext = """
[v3_req]
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = @alt_names

[alt_names]
IP.1 = 127.0.0.1
DNS.1 = localhost
""".lstrip()
        client_ext = """
[v3_req]
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = clientAuth
""".lstrip()
        (self.certs / "ca.cnf").write_text(ca_config)
        (self.certs / "server-ext.cnf").write_text(server_ext)
        (self.certs / "client-ext.cnf").write_text(client_ext)

    def generate_certs(self) -> None:
        self.write_cert_configs()
        ca_key = self.certs / "ca-key.pem"
        ca = self.certs / "ca.pem"
        server_key = self.certs / "server-key.pem"
        server_csr = self.certs / "server.csr"
        server_cert = self.certs / "server.pem"
        client_key = self.certs / "client-key.pem"
        client_csr = self.certs / "client.csr"
        client_cert = self.certs / "client.pem"
        denied_key = self.certs / "denied-client-key.pem"
        denied_csr = self.certs / "denied-client.csr"
        denied_cert = self.certs / "denied-client.pem"

        self.run_openssl(
            "ca",
            [
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-config",
                str(self.certs / "ca.cnf"),
                "-extensions",
                "v3_ca",
                "-keyout",
                str(ca_key),
                "-out",
                str(ca),
                "-sha256",
            ],
        )
        self.run_openssl(
            "server csr",
            [
                "req",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                str(server_key),
                "-out",
                str(server_csr),
                "-subj",
                "/CN=localhost",
                "-sha256",
            ],
        )
        self.run_openssl(
            "server cert",
            [
                "x509",
                "-req",
                "-in",
                str(server_csr),
                "-CA",
                str(ca),
                "-CAkey",
                str(ca_key),
                "-CAcreateserial",
                "-out",
                str(server_cert),
                "-days",
                "1",
                "-sha256",
                "-extfile",
                str(self.certs / "server-ext.cnf"),
                "-extensions",
                "v3_req",
            ],
        )
        self.run_openssl(
            "client csr",
            [
                "req",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                str(client_key),
                "-out",
                str(client_csr),
                "-subj",
                "/CN=crab-cache-smoke-client",
                "-sha256",
            ],
        )
        self.run_openssl(
            "client cert",
            [
                "x509",
                "-req",
                "-in",
                str(client_csr),
                "-CA",
                str(ca),
                "-CAkey",
                str(ca_key),
                "-CAcreateserial",
                "-out",
                str(client_cert),
                "-days",
                "1",
                "-sha256",
                "-extfile",
                str(self.certs / "client-ext.cnf"),
                "-extensions",
                "v3_req",
            ],
        )
        self.run_openssl(
            "denied client csr",
            [
                "req",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                str(denied_key),
                "-out",
                str(denied_csr),
                "-subj",
                "/CN=crab-cache-smoke-denied-client",
                "-sha256",
            ],
        )
        self.run_openssl(
            "denied client cert",
            [
                "x509",
                "-req",
                "-in",
                str(denied_csr),
                "-CA",
                str(ca),
                "-CAkey",
                str(ca_key),
                "-CAcreateserial",
                "-out",
                str(denied_cert),
                "-days",
                "1",
                "-sha256",
                "-extfile",
                str(self.certs / "client-ext.cnf"),
                "-extensions",
                "v3_req",
            ],
        )
        self.allowed_principal = self.certificate_principal(client_cert)
        self.denied_principal = self.certificate_principal(denied_cert)
        self.report.artifacts.update(
            {
                "ca_cert": str(ca),
                "server_cert": str(server_cert),
                "client_cert": str(client_cert),
                "client_key": str(client_key),
                "denied_client_cert": str(denied_cert),
                "denied_client_key": str(denied_key),
            }
        )
        self.write_report()
        self.check("certificates-generated", True)
        self.check(
            "client-principal-derived-from-cert",
            self.allowed_principal.startswith("mtls-sha256:")
            and self.denied_principal.startswith("mtls-sha256:")
            and self.allowed_principal != self.denied_principal,
        )

    @staticmethod
    def certificate_principal(cert_path: Path) -> str:
        pem = cert_path.read_text()
        der = ssl.PEM_cert_to_DER_cert(pem)
        return "mtls-sha256:" + hashlib.sha256(der).hexdigest()

    def write_policy(self) -> Path:
        if not self.allowed_principal:
            raise SmokeError("client certificate principal is not available")
        policy_path = self.artifacts / "policy.yaml"
        policy = "\n".join(
            [
                "rules:",
                f"  - principal: {json.dumps(self.allowed_principal)}",
                '    repos: [".crab", "org/allowed/*"]',
                '    actions: ["read", "write", "dedup", "admin"]',
                "",
            ]
        )
        policy_path.write_text(policy)
        self.report.artifacts["policy"] = str(policy_path)
        self.write_report()
        return policy_path

    def write_cache_server_config(self, listen_port: int) -> Path:
        config_path = self.artifacts / "cache-server.toml"
        policy_path = self.write_policy()
        config = "\n".join(
            [
                "[server]",
                f'listen_addr = "127.0.0.1:{listen_port}"',
                'mutable_path_mode = "strict"',
                f"policy_path = {json.dumps(str(policy_path))}",
                "drain_timeout_secs = 1",
                "",
                "[tls]",
                f"cert_path = {json.dumps(str(self.certs / 'server.pem'))}",
                f"key_path = {json.dumps(str(self.certs / 'server-key.pem'))}",
                f"client_ca_path = {json.dumps(str(self.certs / 'ca.pem'))}",
                "",
                "[auth]",
                'mechanism = "mtls"',
                "",
                "[origin]",
                f"url = {json.dumps(file_url(self.origin))}",
                "",
                "[cache]",
                f"root = {json.dumps(str(self.cache_root))}",
                f"max_bytes = {self.args.max_cache_bytes}",
                "",
                "[dedup]",
                'scope = "all"',
                "",
            ]
        )
        config_path.write_text(config)
        self.report.artifacts["cache_server_config"] = str(config_path)
        self.write_report()
        return config_path

    def start_cache_server(self) -> None:
        listen_port = find_free_port()
        config_path = self.write_cache_server_config(listen_port)
        cache_bin = resolve_executable(self.cache_server_bin)
        if cache_bin is None:
            raise SmokeError(f"cache server binary not found: {self.args.cache_server_bin}")

        record = self.run_cmd(
            "crab-cache-server preflight",
            [
                cache_bin,
                "--config",
                str(config_path),
                "check",
                "--json",
                "--profile",
                "enterprise",
            ],
            self.run_root,
            timeout=self.args.startup_timeout,
        )
        payload = json.loads(Path(record.stdout_log).read_text())
        checks = {str(check.get("name")): check for check in payload.get("checks", [])}
        tls_check = checks.get("tls")
        auth_check = checks.get("auth")
        policy_check = checks.get("authorization policy")
        enterprise_check = checks.get("enterprise profile")
        startup_check = checks.get("startup components")
        issue_codes = {
            str(check.get("code"))
            for check in payload.get("checks", [])
            if check.get("code")
        }
        policy_diagnostics = (payload.get("summary") or {}).get("policy_diagnostics") or {}
        self.check(
            "server-preflight-status",
            payload.get("status") in ("ok", "warn"),
            {"status": payload.get("status"), "codes": sorted(issue_codes)},
        )
        self.check(
            "server-preflight-native-mtls",
            tls_check is not None
            and tls_check.get("status") == "ok"
            and "native mTLS" in str(tls_check.get("detail", "")),
            {"detail": tls_check},
        )
        self.check(
            "server-preflight-auth-native-mtls",
            auth_check is not None
            and auth_check.get("status") == "ok"
            and "mtls-sha256" in str(auth_check.get("detail", "")),
            {"detail": auth_check},
        )
        self.check(
            "server-preflight-policy-loaded",
            policy_check is not None and policy_check.get("status") == "ok",
            {"detail": policy_check},
        )
        self.check(
            "server-preflight-enterprise-profile-ok",
            enterprise_check is not None and enterprise_check.get("status") == "ok",
            {"detail": enterprise_check},
        )
        self.check(
            "server-preflight-no-enterprise-profile-failures",
            not any(code.startswith("enterprise_") for code in issue_codes),
            {"codes": sorted(issue_codes)},
        )
        self.check(
            "server-preflight-policy-diagnostics",
            policy_diagnostics
            == {
                "rule_count": 1,
                "repo_pattern_count": 2,
                "actions": ["read", "write", "dedup", "admin"],
            }
            and self.allowed_principal not in json.dumps(policy_diagnostics),
            {"policy_diagnostics": policy_diagnostics},
        )
        self.check(
            "server-preflight-startup-ok",
            startup_check is not None and startup_check.get("status") == "ok",
            {"detail": startup_check},
        )

        stdout_log = self.logs / "cache-server.out.log"
        stderr_log = self.logs / "cache-server.err.log"
        self.report.artifacts["cache_server_stdout"] = str(stdout_log)
        self.report.artifacts["cache_server_stderr"] = str(stderr_log)
        self.write_report()
        env = os.environ.copy()
        env["RUST_LOG"] = self.args.cache_server_log
        stdout = stdout_log.open("wb")
        stderr = stderr_log.open("wb")
        proc = subprocess.Popen(
            [cache_bin, "--config", str(config_path)],
            cwd=self.run_root,
            env=env,
            stdout=stdout,
            stderr=stderr,
        )
        stdout.close()
        stderr.close()
        self.cache_proc = proc
        self.report.cache_service_url = f"https://127.0.0.1:{listen_port}"
        self.write_report()
        self.wait_for_health()

    def tls_context(self, *, client_cert: bool, identity: str = "client") -> ssl.SSLContext:
        context = ssl.create_default_context(cafile=str(self.certs / "ca.pem"))
        if client_cert:
            if identity == "client":
                cert_path = self.certs / "client.pem"
                key_path = self.certs / "client-key.pem"
            elif identity == "denied":
                cert_path = self.certs / "denied-client.pem"
                key_path = self.certs / "denied-client-key.pem"
            else:
                raise SmokeError(f"unknown client identity: {identity}")
            context.load_cert_chain(
                certfile=str(cert_path),
                keyfile=str(key_path),
            )
        return context

    def https_request(
        self,
        method: str,
        path: str,
        *,
        body: bytes | None = None,
        client_cert: bool = True,
        identity: str = "client",
        headers: dict[str, str] | None = None,
    ) -> tuple[int, dict[str, str], bytes]:
        url = f"{self.report.cache_service_url}{path}"
        request = urllib.request.Request(url, data=body, method=method, headers=headers or {})
        context = self.tls_context(client_cert=client_cert, identity=identity)
        try:
            with urllib.request.urlopen(
                request,
                timeout=self.args.timeout,
                context=context,
            ) as response:
                return (
                    response.status,
                    {k.lower(): v for k, v in response.headers.items()},
                    response.read(),
                )
        except urllib.error.HTTPError as exc:
            return (
                exc.code,
                {k.lower(): v for k, v in exc.headers.items()},
                exc.read(),
            )

    def wait_for_health(self) -> None:
        if self.cache_proc is None:
            raise SmokeError("cache server is not started")
        deadline = time.monotonic() + self.args.startup_timeout
        last_error = ""
        while time.monotonic() < deadline:
            if self.cache_proc.poll() is not None:
                last_error = f"cache server exited with {self.cache_proc.returncode}"
                break
            try:
                status, _, body = self.https_request("GET", "/v1/health")
                if status == 200 and body == b"ok":
                    self.check("cache-server-health-with-client-cert", True)
                    return
            except OSError as exc:
                last_error = str(exc)
            time.sleep(0.1)
        self.check("cache-server-health-with-client-cert", False, {"last_error": last_error})

    def verify_no_client_cert_rejected(self) -> None:
        try:
            status, _, _ = self.https_request(
                "GET",
                "/v1/health",
                client_cert=False,
            )
        except (OSError, ssl.SSLError, urllib.error.URLError) as exc:
            self.check(
                "health-without-client-cert-rejected",
                True,
                {"error_type": type(exc).__name__},
            )
            return
        self.check(
            "health-without-client-cert-rejected",
            False,
            {"status": status},
        )

    def verify_object_route(self) -> None:
        body = b"crab cache native mtls pack smoke\n"
        path = "/v1/org/allowed/repo/packs/cache-service-smoke.pack"
        put_status, _, _ = self.https_request("PUT", path, body=body)
        self.check("client-cert-object-put-created", put_status == 201, {"status": put_status})
        get_status, headers, get_body = self.https_request("GET", path)
        self.check("client-cert-object-get-ok", get_status == 200, {"status": get_status})
        self.check("client-cert-object-get-body", get_body == body, {"body_len": len(get_body)})
        self.check(
            "client-cert-object-get-cache-hit",
            headers.get("x-cache") == "HIT",
            {"x-cache": headers.get("x-cache", "")},
        )

    def verify_policy_enforcement(self) -> None:
        allowed = json.dumps(
            {"repo_path": "org/allowed/repo", "chunk_hashes": []}
        ).encode()
        denied = json.dumps(
            {"repo_path": "org/denied/repo", "chunk_hashes": []}
        ).encode()
        json_headers = {"Content-Type": "application/json"}

        status, _, body = self.https_request(
            "POST",
            "/v1/dedup/query",
            body=allowed,
            headers=json_headers,
        )
        self.check(
            "policy-allows-repo-dedup",
            status == 200 and json.loads(body.decode()) == {"known": [], "unknown": []},
            {"status": status, "body": body.decode(errors="replace")},
        )

        status, _, _ = self.https_request(
            "POST",
            "/v1/dedup/query",
            body=denied,
            headers=json_headers,
        )
        self.check("policy-denies-repo-dedup", status == 403, {"status": status})

        status, _, _ = self.https_request(
            "PUT",
            "/v1/org/denied/repo/packs/policy-denied.pack",
            body=b"denied pack body",
        )
        self.check("policy-denies-repo-write", status == 403, {"status": status})

        status, _, _ = self.https_request(
            "GET",
            "/v1/admin/stats",
            identity="denied",
        )
        self.check(
            "policy-denies-unknown-client-admin",
            status == 403,
            {"status": status, "denied_principal": self.denied_principal},
        )

    def client_env(self) -> dict[str, str]:
        env = os.environ.copy()
        env["CRAB_CACHE_DIR"] = str(self.client_cache)
        env["CRAB_CACHE_SERVICE_URL"] = self.report.cache_service_url
        return env

    def configure_client_repo(self) -> Path:
        repo = self.run_root / "client-repo"
        repo.mkdir(parents=True, exist_ok=True)
        env = self.client_env()
        self.run_cmd("git init client repo", ["git", "init", "-b", "main"], repo, env=env)
        self.run_cmd(
            "crab config cache service mode",
            [self.crab_bin, "config", "set", "cache.service_mode", "cache+dedup"],
            repo,
            env=env,
        )
        self.run_cmd(
            "crab config cache service auth",
            [self.crab_bin, "config", "set", "cache.service_auth", "mtls"],
            repo,
            env=env,
        )
        self.run_cmd(
            "crab config cache service ca",
            [
                self.crab_bin,
                "config",
                "set",
                "cache.service_ca_cert",
                str(self.certs / "ca.pem"),
            ],
            repo,
            env=env,
        )
        self.run_cmd(
            "crab config cache service client cert",
            [
                self.crab_bin,
                "config",
                "set",
                "cache.service_client_cert",
                str(self.certs / "client.pem"),
            ],
            repo,
            env=env,
        )
        self.run_cmd(
            "crab config cache service client key",
            [
                self.crab_bin,
                "config",
                "set",
                "cache.service_client_key",
                str(self.certs / "client-key.pem"),
            ],
            repo,
            env=env,
        )
        self.report.artifacts["client_config"] = str(repo / ".crab" / "config.toml")
        self.write_report()
        config = (repo / ".crab" / "config.toml").read_text()
        self.check(
            "client-cache-service-url-not-written-to-config",
            self.report.cache_service_url not in config,
        )
        return repo

    def verify_config_get_service_url(self, repo: Path) -> None:
        record = self.run_cmd(
            "crab config get cache service url",
            [self.crab_bin, "config", "get", "cache.service_url", "--json"],
            repo,
            env=self.client_env(),
        )
        payload = json.loads(Path(record.stdout_log).read_text())
        data = payload.get("data", payload)
        self.check(
            "config-get-cache-service-url-env",
            data.get("value") == self.report.cache_service_url
            and data.get("source") == "env",
            {"data": data},
        )

    def verify_doctor(self, repo: Path) -> None:
        record = self.run_cmd(
            "crab doctor mtls",
            [self.crab_bin, "doctor", "--json"],
            repo,
            env=self.client_env(),
        )
        payload = json.loads(Path(record.stdout_log).read_text())
        checks = payload.get("data", {}).get("checks", [])
        by_name = {str(check.get("name")): check for check in checks}
        cache_check = by_name.get("cache service")
        auth_check = by_name.get("cache service auth")
        admin_check = by_name.get("cache service admin")
        self.check(
            "doctor-cache-service-checks-present",
            cache_check is not None and auth_check is not None and admin_check is not None,
            {"checks": [check.get("name") for check in checks]},
        )
        self.check(
            "doctor-cache-service-health-ok",
            cache_check is not None and cache_check.get("status") == "ok",
            {"detail": cache_check},
        )
        self.check(
            "doctor-cache-service-auth-ok",
            auth_check is not None and auth_check.get("status") == "ok",
            {"detail": auth_check},
        )
        self.check(
            "doctor-cache-service-admin-ok",
            admin_check is not None and admin_check.get("status") == "ok",
            {"detail": admin_check},
        )
        detail = " ".join(
            str(check.get("detail", ""))
            for check in (cache_check, auth_check, admin_check)
            if check
        )
        self.check(
            "doctor-cache-service-mtls-labels",
            "mtls client cert configured" in detail
            and "custom CA" in detail
            and "client cert configured" in detail,
            {"detail": detail},
        )

    def stop_cache_server(self) -> None:
        proc = self.cache_proc
        if proc is None or proc.poll() is not None:
            return
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)

    def run(self) -> None:
        try:
            self.setup_dirs()
            self.preflight_tools()
            self.generate_certs()
            self.start_cache_server()
            self.verify_no_client_cert_rejected()
            self.verify_object_route()
            self.verify_policy_enforcement()
            repo = self.configure_client_repo()
            self.verify_config_get_service_url(repo)
            self.verify_doctor(repo)
            self.report.status = "passed"
            self.write_report()
        finally:
            self.stop_cache_server()


def parse_args() -> argparse.Namespace:
    def positive_int(value: str) -> int:
        try:
            parsed = int(value)
        except ValueError as exc:
            raise argparse.ArgumentTypeError(f"{value!r} is not an integer") from exc
        if parsed <= 0:
            raise argparse.ArgumentTypeError(f"{value!r} must be greater than zero")
        return parsed

    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--root", type=Path, default=DEFAULT_ROOT)
    parser.add_argument("--run-id")
    parser.add_argument("--crab-bin", default=str(DEFAULT_CRAB_BIN))
    parser.add_argument("--cache-server-bin", default=str(DEFAULT_CACHE_SERVER_BIN))
    parser.add_argument("--cache-server-log", default="info")
    parser.add_argument("--max-cache-bytes", type=positive_int, default=10 * 1024 * 1024)
    parser.add_argument("--timeout", type=positive_int, default=30)
    parser.add_argument("--startup-timeout", type=positive_int, default=15)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    smoke = CacheServiceMtlsSmoke(args)
    try:
        smoke.run()
    except SmokeError as exc:
        smoke.report.status = "failed"
        smoke.write_report()
        print(f"FAILED: {exc}", file=os.sys.stderr)
        print(f"report: {smoke.report.artifacts.get('report', '')}", file=os.sys.stderr)
        return 1
    print("PASS cache-service native mTLS smoke")
    print(f"report: {smoke.report.artifacts.get('report', '')}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
