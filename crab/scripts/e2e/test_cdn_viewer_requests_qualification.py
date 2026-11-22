"""Exercise provider log joins for timed fetch evidence."""

import hashlib
import json
import subprocess
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import collect_cdn_viewer_requests as COLLECT
import test_cdn_qualification as examples
from test_cdn_transport_qualification import PACK, fixture


class CdnViewerRequestQualificationTests(unittest.TestCase):
    def test_cloudfront_matches_only_the_timed_request_and_redacts_the_raw_export(self):
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

    def test_cloud_cdn_joins_range_fills_and_validations_to_the_tagged_viewer(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            host = "fixture.example.com"
            project, rule = "fixture-project", "crab-https"
            config = fixture("cloud-cdn", "private") | {
                "base_url": f"https://{host}", "deployment": {"project": project, "forwarding_rule": rule},
            }
            fixture_path = root / "fixture.json"
            configuration = examples.json_file(fixture_path, config)
            registration = {"scope": config["scope"], "source_sha": examples.COMMIT, "configuration_sha256": configuration,
                            "regions": {"near": "near"}, "samples": 1, "viewer_binding_sha256": "e" * 64,
                            "candidate": examples.cli_candidate(config["scope"], "linux-x86_64")}
            examples.write_git_measurements(root, registration, "linux-x86_64")
            client = root / "near/0001/cold_edge/client.json"
            report = json.loads(client.read_bytes())
            tag = f"crab-cdn-qualification/{'0a' * 16}"
            fingerprint = hashlib.sha256(tag.encode()).hexdigest()
            response = next(iter(report["edge_responses"].values()))
            digest = examples.json_file(client, report | {"edge_responses": {fingerprint: response}})
            request = {"requestMethod": "GET", "requestUrl": f"https://{host}/{config['fixture_prefix']}/{PACK}",
                       "responseSize": "620", "status": 206, "userAgent": tag, "remoteIp": "192.0.2.1",
                       "cacheFillBytes": "4096", "cacheLookup": True}
            resource = {"type": "http_load_balancer", "labels": {"project_id": project, "forwarding_rule_name": rule}}
            viewer = {"httpRequest": request, "jsonPayload": {"statusDetails": "response_sent_by_backend"},
                      "logName": f"projects/{project}/logs/requests", "insertId": "viewer", "resource": resource}
            origin = viewer | {"jsonPayload": {"parentInsertId": "viewer"}, "insertId": "range-fill",
                               "httpRequest": request | {"userAgent": "Cloud-CDN-Google", "cacheFillBytes": "2048"}}
            validation = origin | {"insertId": "validation",
                                   "httpRequest": origin["httpRequest"] | {"status": 304, "cacheFillBytes": "0"}}
            unrelated = origin | {"jsonPayload": {"parentInsertId": "another-viewer"}, "insertId": "unrelated-origin"}
            log, output = root / "gcloud.json", root / "cloud-cdn-requests.json"
            command = [sys.executable, "-B", str(Path(COLLECT.__file__)), "--fixture", str(fixture_path), "--log", str(log),
                       "--client", str(client), "--configuration-sha256", configuration, "--output", str(output)]
            for entries, error in (
                ([origin], "must cover each measured viewer attempt"),
                ([viewer | {"resource": resource | {"labels": {"project_id": project, "forwarding_rule_name": "wrong"}}}],
                 "outside the approved forwarding rule"),
                ([viewer, viewer | {"insertId": "a-different-viewer"}], "exports disagree about one viewer request"),
                ([viewer | {"httpRequest": request | {"cacheHit": True, "cacheValidatedWithOriginServer": True}}],
                 "cache result disagrees"),
                ([viewer, origin | {"resource": resource | {"type": "another-resource"}}],
                 "origin request was logged outside the approved forwarding rule"),
                ([viewer, origin, origin | {"httpRequest": origin["httpRequest"] | {"cacheFillBytes": "1999"}}],
                 "exports disagree about one origin request"),
                ([origin, unrelated, viewer, viewer, validation, origin], None),
            ):
                with self.subTest(error=error):
                    examples.json_file(log, entries)
                    result = subprocess.run(command, capture_output=True, text=True, check=False)
                    if error is not None:
                        self.assertEqual(result.returncode, 1)
                        self.assertIn(error, result.stderr)
                        self.assertFalse(output.exists())
                    else:
                        self.assertEqual(result.returncode, 0, result.stderr)
                        retained = output.read_text(encoding="utf-8")
                        self.assertEqual(json.loads(retained), {
                            "schema": "crab.cloud-cdn-viewer-requests/v1", "client_report_sha256": digest,
                            "records": {fingerprint: {"status": 206, "response_bytes": 620, "cache": "miss",
                                                     "cache_fill_bytes": 4096, "origin_requests": {
                                                         hashlib.sha256(b"range-fill").hexdigest():
                                                         {"method": "GET", "status": 206, "cache_fill_bytes": 2048},
                                                         hashlib.sha256(b"validation").hexdigest():
                                                         {"method": "GET", "status": 304, "cache_fill_bytes": 0},
                                                     }}},
                        })
                        for private in (tag, host, project, rule, "192.0.2.1", "range-fill", "validation", "unrelated-origin"):
                            self.assertNotIn(private, retained)

    def test_front_door_matches_the_viewer_row_and_excludes_its_origin_shield_row(self):
        for access in ("private", "public"):
            with self.subTest(access=access), TemporaryDirectory() as temporary:
                root = Path(temporary)
                host = "fixture.z01.azurefd.net"
                deployment = {"subscription": "12345678-1234-1234-1234-123456789012", "resource_group": "test", "profile": "test-cdn"}
                config = fixture("front-door", access) | {"deployment": deployment,
                    "base_url": "https://test-gateway.azure-api.net/crab" if access == "private" else f"https://{host}"}
                fixture_path = root / "fixture.json"
                configuration = examples.json_file(fixture_path, config)
                registration = {"scope": config["scope"], "source_sha": examples.COMMIT, "configuration_sha256": configuration,
                                "regions": {"near": "near"}, "samples": 1, "viewer_binding_sha256": "e" * 64,
                                "candidate": examples.cli_candidate(config["scope"], "linux-x86_64")}
                examples.write_git_measurements(root, registration, "linux-x86_64")
                client = root / "near/0001/warm_edge/client.json"
                report = json.loads(client.read_bytes())
                reference = "0uniqueFrontDoorViewerReference=="
                request_id = hashlib.sha256(reference.encode()).hexdigest()
                response = next(iter(report["edge_responses"].values()))
                digest = examples.json_file(client, report | {"edge_responses": {request_id: response}})
                resource = (f"/SUBSCRIPTIONS/{deployment['subscription']}/RESOURCEGROUPS/TEST/PROVIDERS/MICROSOFT.CDN/"
                            "PROFILES/TEST-CDN")
                properties = {"trackingReference": reference, "requestUri": f"https://{host}:443/{config['fixture_prefix']}/{PACK}",
                              "endpoint": host, "isReceivedFromClient": True, "httpStatusCode": "206", "responseBytes": "620",
                              "httpMethod": "GET", "cacheStatus": "REMOTE_HIT", "clientIp": "192.0.2.1"}
                viewer = {"category": "FrontdoorAccessLog", "resourceId": resource, "properties": properties}
                shield = viewer | {"properties": properties | {"isReceivedFromClient": False, "cacheStatus": "MISS", "responseBytes": 900}}
                archive, output = root / "PT1H.json", root / "front-door-requests.json"
                command = [sys.executable, "-B", str(Path(COLLECT.__file__)), "--fixture", str(fixture_path), "--log", str(archive),
                           "--client", str(client), "--configuration-sha256", configuration, "--output", str(output)]
                for entries, error in (
                    ([shield], "must cover each measured viewer attempt"),
                    ([viewer | {"resourceId": resource + "-other"}], "outside the approved profile or repository prefix"),
                    ([viewer | {"properties": properties | {"cacheStatus": "PARTIAL_HIT"}}], "cache result disagrees"),
                    ([shield, viewer], None),
                ):
                    with self.subTest(error=error):
                        examples.json_file(archive, {"records": entries})
                        result = subprocess.run(command, capture_output=True, text=True, check=False)
                        if error is not None:
                            self.assertEqual(result.returncode, 1)
                            self.assertIn(error, result.stderr)
                            self.assertFalse(output.exists())
                        else:
                            self.assertEqual(result.returncode, 0, result.stderr)
                            retained = output.read_text(encoding="utf-8")
                            self.assertEqual(json.loads(retained), {
                                "schema": "crab.front-door-viewer-requests/v1", "client_report_sha256": digest,
                                "records": {request_id: {"status": 206, "response_bytes": 620, "cache": "hit"}},
                            })
                            for private in (reference, resource, host, "192.0.2.1"):
                                self.assertNotIn(private, retained)
