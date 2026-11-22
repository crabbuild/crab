"""Exercise the CloudFront standard log join on redacted timed fetch evidence."""

import hashlib
import json
import subprocess
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import collect_cloudfront_viewer_requests as COLLECT
import test_cdn_qualification as examples
from test_cdn_transport_qualification import PACK, fixture


class CloudFrontViewerRequestQualificationTests(unittest.TestCase):
    def test_canonical_cli_matches_only_the_timed_request_and_redacts_the_raw_export(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            config = fixture(access="private")
            fixture_path = root / "fixture.json"
            configuration = examples.json_file(fixture_path, config)
            registration = {"scope": config["scope"], "source_sha": examples.COMMIT, "configuration_sha256": configuration,
                            "regions": {"near": "near"}, "samples": 1, "viewer_binding_sha256": "e" * 64,
                            "candidate": examples.cli_candidate(config["scope"], "linux-x86_64")}
            examples.write_git_measurements(root, registration, "linux-x86_64")
            client = root / "near/0001/warm_edge/client.json"
            report = json.loads(client.read_bytes())
            viewer_id = "fixture-request_V1=="
            fingerprint = hashlib.sha256(viewer_id.encode()).hexdigest()
            response = next(iter(report["edge_responses"].values()))
            digest = examples.json_file(client, report | {"edge_responses": {fingerprint: response}})
            log = root / "export.w3c"
            prefix = f"/crab/{config['fixture_prefix']}/"
            header = ("#Version: 1.0\n#Fields: date x-edge-request-id x-host-header cs-method cs-uri-stem cs-protocol "
                      "sc-status sc-bytes x-edge-response-result-type c-ip\n")
            actual = (f"2026-11-22\t{viewer_id}\tfixture.example.com\tGET\t{prefix}{PACK}\thttps\t206\t620\tHit\t192.0.2.1\n")
            unrelated = actual.replace(viewer_id, "unrelated-request").replace("fixture.example.com", "other.example.com")
            output = root / "cloudfront-requests.json"
            command = [sys.executable, "-B", str(Path(COLLECT.__file__)), "--fixture", str(fixture_path), "--log", str(log),
                       "--client", str(client), "--configuration-sha256", configuration, "--output", str(output)]
            for records, reason in (
                (unrelated, "must cover each measured viewer attempt"),
                (actual.replace(prefix, "/other/"), "outside the approved viewer host or repository prefix"),
                (actual + actual.replace("620", "619"), "exports disagree about one viewer request"),
                (unrelated + actual + actual, None),
            ):
                with self.subTest(reason=reason):
                    log.write_text(header + records, encoding="utf-8")
                    result = subprocess.run(command, capture_output=True, text=True, check=False)
                    if reason is not None:
                        self.assertEqual(result.returncode, 1)
                        self.assertIn(reason, result.stderr)
                        self.assertFalse(output.exists())
                    else:
                        self.assertEqual(result.returncode, 0, result.stderr)
                        retained = output.read_text(encoding="utf-8")
                        self.assertEqual(json.loads(retained), {
                            "schema": "crab.cloudfront-viewer-requests/v1", "client_report_sha256": digest,
                            "records": {fingerprint: {"status": 206, "response_bytes": 620, "cache": "hit"}},
                        })
                        for private in (viewer_id, "fixture.example.com", prefix, "192.0.2.1"):
                            self.assertNotIn(private, retained)
