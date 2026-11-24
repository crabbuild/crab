"""Exercise desktop qualification framing and binding to measured fixture data."""

import argparse
import base64
import hashlib
import json
import sys
import unittest
from contextlib import redirect_stderr
from io import BytesIO, StringIO, TextIOWrapper
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
from unittest.mock import patch

import run_cdn_desktop_workload as desktop
import collect_cdn_viewer_requests as viewer
import run_cdn_sdk_workload as sdk_workload
from cdn_qualification import CLI_BUILD_FIELDS, PROVIDER_FOR_EDGE, validate_desktop_git_read
from run_cdn_git_workload import read_target_fingerprints
from test_cdn_qualification import COMMIT, cli_candidate, json_file
from test_cdn_transport_qualification import fixture


class DesktopQualificationTests(unittest.TestCase):
    def setUp(self) -> None:
        directory = TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name).resolve()
        self.config = self.root / ".config/crab/config.toml"
        self.config.parent.mkdir(parents=True)
        self.body = bytes(range(256)) * (desktop.GIT_PAYLOAD_SIZE // 256)
        self.approval = "e" * 64
        self.issuer = "https://issuer.example.com/v1/credentials"
        reader = self.root / "reader.json"
        json_file(reader, {"oidc_issuer": "https://login.example.com", "oidc_client_id": "crab-test",
                           "token_cache_directory": str(self.root / "tokens")})
        sidecar = self.root / "sidecar"
        sidecar.write_bytes(b"registered executable")
        self.args = argparse.Namespace(arm="edge", fixture=self.root / "fixture.json", receipt=self.root / "receipt.json",
            sidecar=sidecar, sidecar_sha256=hashlib.sha256(sidecar.read_bytes()).hexdigest(), scratch=self.root,
            source_sha=COMMIT, run_id="123", run_attempt="2", private_reader=reader, export_approval=None,
            warm_local_output=None, transition_object=None)
        self.oid = "a" * 40
        self.stamp = {key: value for key, value in cli_candidate("cloudfront/private/desktop/git/direct",
                      desktop.runner_platform()).items() if key in CLI_BUILD_FIELDS}
        self.inspection = self.enterContext(patch.object(desktop.subprocess, "run", return_value=self.build_info(self.stamp)))
        home = patch.object(Path, "home", return_value=self.root)
        home.start()
        self.addCleanup(home.stop)

    @staticmethod
    def build_info(stamp: dict) -> desktop.subprocess.CompletedProcess:
        return desktop.subprocess.CompletedProcess([], 0, json.dumps(stamp).encode(), b"")

    def configure(self, edge: str, arm: str) -> dict:
        self.args.arm = arm
        scope = f"{edge}/private/desktop/git/direct"
        config = fixture(edge, "private") | {"scope": scope, "viewer_issuer": {
            "endpoint": self.issuer, "audience": "https://issuer.example.com", "approval_sha256": self.approval,
        }}
        self.args.configuration_sha256 = json_file(self.args.fixture, config)
        json_file(self.args.receipt, {"schema": "crab.cdn-git-publication/v1", "scope": scope,
            "configuration_sha256": self.args.configuration_sha256, "source_sha": COMMIT,
            "workflow_run_id": "123", "workflow_run_attempt": "2", "published": True, "visible_at_unix_ms": 1,
            "ref": "refs/heads/crab-cdn-qualification-123-2-0123456789ab", "candidate": cli_candidate(scope, desktop.runner_platform()),
            "oid": self.oid, "sha256": hashlib.sha256(self.body).hexdigest(), "payload_bytes": len(self.body)})
        self.config.write_text(f'''[auth]
provider = "crab-auth"
auth_endpoint = "{self.issuer}"
issuer_url = "https://login.example.com"
client_id = "crab-test"
token_cache_path = {json.dumps(str(self.root / "tokens"))}
[[cdn]]
origin = "{config['native_origin']}"
repo_prefix = "{config['fixture_prefix']}"
base_url = "{config['base_url']}"
''' + (f'approval_sha256 = "{self.approval}"\n' if arm == "edge" else ""), encoding="utf-8")
        return config

    def emulate(self, config: dict, edge: str, arm: str, *, reported_bytes: bytes | None = None,
                transfer: bool = True, capsule: bool = False, repeat_source: str | None = None,
                transition_approval: str = "", transition_key: str | None = None):
        route = {"provider": PROVIDER_FOR_EDGE[edge], "access": config["scope"].split("/")[1], "consumer": "desktop",
                 "family": "git", "profile": "direct"}
        events = [{"target": desktop.READ_TARGET, "fields": read_target_fingerprints(config, edge=arm != "origin",
                                                                                     export_sha256=transition_approval)
                   | {"message": "SDK captured CDN read target"}},
                  {"target": desktop.NATIVE_TARGET, "fields": {"message": "Native object store read", "provider": route["provider"],
                   "method": "get", "status": 200, "bytes": 8}}]
        oid = hashlib.sha1(f"blob {len(self.body)}\0".encode() + self.body, usedforsecurity=False).digest()
        verified = route | {"message": "CDN verified Git payload", "git_blob_oid_sha256": hashlib.sha256(oid).hexdigest()}
        response = route | {"message": "CDN immutable GET", "status": 206, "bytes": 64, "elapsed_ms": 4,
                            "range_start": 0, "object_bytes": 1024, "requested_bytes": 64,
                            "cache": "hit", "range": True, "complete": True, "request_sha256": "d" * 64}
        if arm == "edge" and transfer:
            if capsule:
                events.append({"target": desktop.TARGET, "fields": response | {"family": "capsule", "request_sha256": "c" * 64}})
            events += [{"target": desktop.TARGET, "fields": response},
                       {"target": desktop.TARGET, "fields": verified | {"source": "edge"}}]
        if transition_key is not None:
            fields = route | {"message": "CDN public export observation", "key_sha256": transition_key,
                              "approval_sha256": transition_approval}
            listed, source = ("listed", "edge") if arm == "edge" else ("unlisted", "native")
            events += [{"target": desktop.PUBLIC_TRANSITION_TARGET, "fields": fields | {"observation": listed}},
                       {"target": desktop.PUBLIC_TRANSITION_TARGET, "fields": fields | {"observation": f"verified_{source}",
                                                                                       "git_blob_oid_sha256": verified["git_blob_oid_sha256"]}}]
        repeat_events = []
        if repeat_source:
            repeat_events.append({"target": desktop.TARGET, "fields": verified | {"source": repeat_source}})
            if repeat_source == "edge":
                repeat_events.append({"target": desktop.TARGET, "fields": response})
        payload = base64.b64encode(self.body if reported_bytes is None else reported_bytes).decode()
        request_body = self.root / "payload.b64"
        request_body.write_text(payload, encoding="ascii")
        fake = self.root / "sidecar.py"
        fake.write_text(f'''import os, json, sys
from pathlib import Path
def expect(sequence, method):
    req = json.loads(sys.stdin.readline())
    assert req['id'] == sequence and req['method'] == method and req['jsonrpc'] == '2.0'
    assert req['params']['repo_id'] == 'cdn-qualification'
    return req['params']
def reply(sequence, result):
    print(json.dumps({{'jsonrpc': '2.0', 'id': sequence, 'result': result}}), flush=True)
assert not Path(os.environ['CRAB_CACHE_DIR']).exists()
location = expect(1, 'open_url')
assert location['url'] == {desktop.desktop_url(config)!r}
reply(1, location | {{'is_remote': True}})
read = expect(2, 'read_bytes')
assert read['rev'] == {self.oid!r} and read['path'] == 'cdn-qualification.bin'
for event in {events!r}:
    print(json.dumps(event), file=sys.stderr, flush=True)
payload = {{'size': {len(self.body)}, 'truncated': False, 'bytes_b64': Path({str(request_body)!r}).read_text()}}
reply(2, payload)
close = 3
if {bool(repeat_events)!r}:
    assert read == expect(3, 'read_bytes')
    for event in {repeat_events!r}:
        print(json.dumps(event), file=sys.stderr, flush=True)
    reply(3, payload)
    close = 4
expect(close, 'close')
print(json.dumps({{'target': {desktop.TARGET!r}, 'fields': {{'message': 'post-close diagnostic'}}}}), file=sys.stderr, flush=True)
reply(close, {{'closed': 'cdn-qualification'}})
assert not sys.stdin.read()
''', encoding="utf-8")
        run = desktop.run_sidecar

        async def launch(command, *params, **options):
            self.assertEqual(command, [str(self.args.sidecar), "--log-json"])
            return await run([sys.executable, str(fake)], *params, **options)

        return patch.object(desktop, "run_sidecar", launch)

    def test_measures_verified_private_edge_and_origin_reads_on_all_providers(self) -> None:
        for edge in PROVIDER_FOR_EDGE:
            with self.subTest(edge=edge):
                records = {}
                for arm in ("edge", "origin"):
                    config = self.configure(edge, arm)
                    with self.emulate(config, edge, arm, capsule=True):
                        records[arm], local = desktop.fetch(self.args)
                        self.assertIsNone(local)
                routed, native = records["edge"], records["origin"]
                self.assertEqual(routed["sha256"], native["sha256"])
                self.assertEqual(routed["candidate"], self.stamp | {"binary_sha256": self.args.sidecar_sha256})
                self.assertNotEqual(routed["cache_root_sha256"], native["cache_root_sha256"])
                self.assertEqual((routed["observed_edge_gets"], set(routed["edge_responses"])), (1, {"c" * 64, "d" * 64}))
                self.assertEqual(native["edge_responses"], {})
                self.assertNotIn(self.issuer, json.dumps(routed))
                with self.assertRaisesRegex(ValueError, "fingerprint for every viewer attempt"):
                    validate_desktop_git_read(routed | {"edge_responses": {}}, config["scope"], "edge")

    def test_edge_rejects_native_fallback_or_corrupt_rpc_payload(self) -> None:
        config = self.configure("cloudfront", "edge")
        with self.emulate(config, "cloudfront", "edge", transfer=False):
            with self.assertRaisesRegex(ValueError, "requires verified Git delivery"):
                desktop.fetch(self.args)
        with self.emulate(config, "cloudfront", "edge", reported_bytes=b"different"):
            with self.assertRaisesRegex(ValueError, "different bytes"):
                desktop.fetch(self.args)

    def test_repeat_proves_the_same_verified_blob_in_the_primed_desktop_session(self) -> None:
        config = self.configure("cloudfront", "edge")
        self.args.warm_local_output = self.root / "repeat.json"
        with self.emulate(config, "cloudfront", "edge", repeat_source="local"):
            edge, local = desktop.fetch(self.args)
        self.assertEqual((local["primer_sha256"], local["cache_root_sha256"], local["git_blob_oid_sha256"]), (
            hashlib.sha256(desktop.json_record(edge).encode()).hexdigest(), edge["cache_root_sha256"], edge["git_blob_oid_sha256"],
        ))
        self.assertEqual((local["local_payload_hits"], local["all_edge_attempts"]), (1, 0))
        with self.emulate(config, "cloudfront", "edge", repeat_source="edge"):
            with self.assertRaisesRegex(ValueError, "verified local Git blob without edge traffic"):
                desktop.fetch(self.args)

    def test_stdout_preserves_the_primer_digest_under_crlf_text_output(self) -> None:
        primary = {"arm": "edge"}
        expected = hashlib.sha256(desktop.json_record(primary).encode()).hexdigest()
        local = {"arm": "warm_local", "primer_sha256": expected}
        destination = self.root / "repeat.json"
        with TextIOWrapper(BytesIO(), newline="\r\n") as stdout:
            arguments = argparse.Namespace(warm_local_output=destination)
            with (patch.object(desktop.argparse.ArgumentParser, "parse_args", return_value=arguments),
                  patch.object(desktop, "fetch", return_value=(primary, local)), patch.object(sys, "stdout", stdout)):
                self.assertEqual(desktop.main(), 0)
            stdout.flush()
            self.assertEqual(hashlib.sha256(stdout.buffer.getvalue()).hexdigest(), expected)
        self.assertEqual(json.loads(destination.read_bytes()), local)

    def test_preregistered_binary_and_withdrawn_binding_are_checked_before_rpc(self) -> None:
        self.configure("cloudfront", "edge")
        self.args.arm = "origin"
        with patch.object(desktop, "run_sidecar") as launch:
            with self.assertRaisesRegex(ValueError, "differs from the approved CDN binding"):
                desktop.fetch(self.args)
            self.args.sidecar.write_bytes(b"different executable")
            with self.assertRaisesRegex(ValueError, "preregistered build digest"):
                desktop.fetch(self.args)
            launch.assert_not_called()

    def test_mismatched_build_metadata_prevents_a_remote_read(self) -> None:
        self.configure("cloudfront", "edge")
        with patch.object(desktop, "run_sidecar") as launch:
            self.inspection.return_value = self.build_info(self.stamp | {"source_sha": "f" * 40})
            with self.assertRaisesRegex(ValueError, "differs from the approved source"):
                desktop.fetch(self.args)
            self.inspection.assert_called_once_with([str(self.args.sidecar), "--cdn-build-info"],
                stdin=desktop.subprocess.DEVNULL, capture_output=True, timeout=15, check=False)
            launch.assert_not_called()

    def test_public_edge_requires_the_pinned_receipt_and_origin_requires_its_withdrawal(self) -> None:
        config = fixture("cloudfront", "public")
        publication = {"path": str(self.root / "publication.json"), "sha256": self.approval}
        entry = f'''[[cdn]]
origin = "{config['native_origin']}"
repo_prefix = "{config['fixture_prefix']}"
base_url = "{config['base_url']}"
access = "public"
export_origin = "{config['export_origin']}"
'''
        active = f'''[cdn.publication]
path = {json.dumps(publication['path'])}
sha256 = "{self.approval}"
'''
        edge_binding = {"access": "public", "export_origin": config["export_origin"], "publication": publication}
        origin_binding = edge_binding | {"publication": None}
        self.config.write_text(entry + active, encoding="utf-8")
        desktop.read_configuration(config, edge_binding)
        with self.assertRaisesRegex(ValueError, "differs from the approved CDN binding"):
            desktop.read_configuration(config, origin_binding)
        self.config.write_text(entry, encoding="utf-8")
        desktop.read_configuration(config, origin_binding)
        with self.assertRaisesRegex(ValueError, "differs from the approved CDN binding"):
            desktop.read_configuration(config, edge_binding)

    def test_public_transition_observes_the_same_pack_layer_with_both_receipts(self) -> None:
        self.configure("cloudfront", "edge")
        config = fixture("cloudfront", "public") | {"scope": "cloudfront/public/desktop/git/direct"}
        self.args.configuration_sha256 = json_file(self.args.fixture, config)
        publication = json.loads(self.args.receipt.read_bytes())
        json_file(self.args.receipt, publication | {"scope": config["scope"], "configuration_sha256": self.args.configuration_sha256})
        self.args.private_reader = None
        self.args.export_approval = self.root
        self.args.transition_object = config["object_path"]
        key = hashlib.sha256(f"{config['fixture_prefix']}/{config['object_path']}".encode()).hexdigest()
        for arm, approval in (("prior_export", "6" * 64), ("edge", "7" * 64)):
            with self.subTest(arm=arm):
                self.args.arm = arm
                self.config.write_text(f'''[[cdn]]
origin = "{config['native_origin']}"
repo_prefix = "{config['fixture_prefix']}"
base_url = "{config['base_url']}"
access = "public"
export_origin = "{config['export_origin']}"
[cdn.publication]
path = {json.dumps(str(self.root / "publication.json"))}
sha256 = "{approval}"
''', encoding="utf-8")
                objects = [SimpleNamespace(path=config["object_path"])] if arm == "edge" else []
                with patch.object(sdk_workload, "approved_public_export", return_value=(approval, objects, 1)):
                    with self.emulate(config, "cloudfront", arm, transition_approval=approval, transition_key=key):
                        read, _ = desktop.fetch(self.args)
                    self.assertEqual((read["transition_key_sha256"], read["approval_sha256"], read["all_edge_attempts"]),
                                     (key, approval, int(arm == "edge")))
                    with self.emulate(config, "cloudfront", arm, transition_approval=approval, transition_key="0" * 64):
                        with self.assertRaisesRegex(ValueError, "did not prove the selected object's public export transition"):
                            desktop.fetch(self.args)

    def test_joins_an_edge_read_to_the_matching_cloudfront_request(self) -> None:
        config = self.configure("cloudfront", "edge")
        with self.emulate(config, "cloudfront", "edge"):
            record, _ = desktop.fetch(self.args)
        client, output = self.root / "client.json", self.root / "cloudfront-requests.json"
        logs, origin_logs = self.root / "viewer.w3c", self.root / "origin.log"
        origin_logs.touch()
        request = "desktop-viewer-request"
        request_sha256 = hashlib.sha256(request.encode()).hexdigest()
        record["edge_responses"] = {request_sha256: record["edge_responses"]["d" * 64]}
        size = record["edge_responses"][request_sha256]["bytes"]
        digest = json_file(client, record)
        logs.write_text("#Fields: x-edge-request-id x-host-header cs-method cs-uri-stem cs-protocol sc-status "
                        "sc-bytes x-edge-response-result-type sc-content-len sc-range-start sc-range-end\n"
                        f"{request}\tfixture.example.com\tGET\t/crab/{config['fixture_prefix']}/{config['object_path']}"
                        f"\thttps\t206\t128\tHit\t{size}\t0\t{size - 1}\n", encoding="utf-8")
        command = ["collect", "--fixture", str(self.args.fixture), "--configuration-sha256", self.args.configuration_sha256,
                   "--client", str(client), "--log", str(logs), "--s3-origin-log", str(origin_logs), "--output", str(output)]
        with patch.object(sys, "argv", command):
            self.assertEqual(viewer.main(), 0)
            proof = json.loads(output.read_bytes())
            self.assertEqual((proof["client_report_sha256"], set(proof["records"])), (digest, {request_sha256}))
            output.unlink()
            json_file(client, record | {"approval_sha256": "f" * 64})
            with redirect_stderr(StringIO()):
                self.assertEqual(viewer.main(), 1)
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
