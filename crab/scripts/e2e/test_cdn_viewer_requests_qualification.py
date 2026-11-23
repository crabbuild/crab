"""Exercise provider log joins for timed fetch evidence."""

import gzip
import hashlib
import json
import subprocess
import sys
import unittest
from contextlib import contextmanager, redirect_stderr
from datetime import datetime, timedelta, timezone
from io import BytesIO, StringIO
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
from unittest.mock import patch

import collect_cdn_viewer_requests as COLLECT
import collect_cloudfront_access_logs as AWS_LOGS
import collect_azure_access_logs as AZURE_LOGS
import run_cdn_mount_workload as MOUNT
import test_cdn_qualification as examples
from test_cdn_transport_qualification import PACK, fixture


class CdnViewerRequestQualificationTests(unittest.TestCase):
    def test_mount_sparse_reader_verifies_exact_offset_and_total_size(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            payload = b"before" + b"approved slice" + b"after"
            path = root / "mounted-file"
            path.write_bytes(payload)
            window = {"offset": 6, "bytes": 14, "sha256": hashlib.sha256(b"approved slice").hexdigest()}
            spec = {"payload_bytes": len(payload), "payload_sha256": hashlib.sha256(payload).hexdigest(),
                    "read_range": window}
            MOUNT.read_mounted_file(path, spec, {}, root)
            with self.assertRaisesRegex(ValueError, "different payload bytes"):
                MOUNT.read_mounted_file(path, spec | {"read_range": window | {"offset": 7}}, {}, root)
            with self.assertRaisesRegex(ValueError, "different payload bytes"):
                MOUNT.read_mounted_file(path, spec | {"payload_bytes": len(payload) + 1}, {}, root)

    def test_sparse_mount_does_not_treat_adjacent_metadata_as_xorb_fill(self):
        cdn = examples.CDN
        xorb_key = "a" * 64
        shard_key = "b" * 64
        xorb_response = {"family": "xorb", "status": 206, "range": True, "complete": True, "cache": "hit",
                         "elapsed_ms": 1, "object_bytes": 1024, "bytes": 64, "requested_bytes": 64}
        shard_response = xorb_response | {"family": "shard", "status": 200, "range": False, "cache": "miss",
                                         "object_bytes": 64}
        read = dict.fromkeys(cdn.PRODUCT_COUNTERS, 0) | {
            "schema": cdn.MOUNT_XORB_RANGE_READ, "xorb_key_sha256": xorb_key, "started_at_unix_ms": 1000,
            "finished_at_unix_ms": 1001, "all_edge_attempts": 2, "edge_attempts": 1, "observed_edge_gets": 1,
            "verified_edge_payloads": 1, "edge_hits": 1, "edge_bytes": 64,
            "edge_responses": {"c" * 64: xorb_response, "d" * 64: shard_response},
        }
        proof = {"schema": "crab.cloudfront-viewer-requests/v1", "client_report_sha256": "e" * 64,
                 "records": {"c" * 64: {"status": 206, "cache": "hit", "response_bytes": 64, "key_sha256": xorb_key},
                             "d" * 64: {"status": 200, "cache": "miss", "response_bytes": 64, "key_sha256": shard_key}},
                 "origin_requests": {"f" * 64: {"key_sha256": shard_key, "at_unix_s": 1, "status": 200,
                                                 "response_body_bytes": 64}}}
        cdn.verify_sparse_xorb_traffic(read, 1024)
        observed = cdn.verify_viewer_requests(proof, read, "e" * 64, "cloudfront")
        self.assertEqual((observed.fill_bytes, observed.successful_fill, observed.completed_cache), (0, False, {"c" * 64: "hit"}))
        proof["origin_requests"]["1" * 64] = {"key_sha256": xorb_key, "at_unix_s": 1, "status": 206, "response_body_bytes": 512}
        observed = cdn.verify_viewer_requests(proof, read, "e" * 64, "cloudfront")
        self.assertEqual((observed.fill_bytes, observed.completed_cache), (512, {}))
        with self.assertRaisesRegex(ValueError, "ranges totaling less"):
            cdn.verify_sparse_xorb_traffic(read | {"edge_bytes": 1024}, 1024)
        with self.assertRaisesRegex(ValueError, "approved hydration xorb"):
            proof["records"]["c" * 64]["key_sha256"] = shard_key
            cdn.verify_viewer_requests(proof, read, "e" * 64, "cloudfront")

    def test_mount_record_joins_the_exact_projected_xorb_request(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            example = examples.CdnQualificationTests()
            example.setUp()
            self.addCleanup(example.doCleanups)
            example.configure("s3", "private", "direct", consumer="mount", family="xorb")
            config = fixture(access="private") | {
                "scope": example.scope, "view_suffix": f"/acl-views/v2/{'b' * 64}/7-{'c' * 64}",
                "object_path": ".crab/xorbs/aa/" + "a" * 64, "size": 1024,
            }
            key = config["fixture_prefix"] + config["view_suffix"] + "/" + config["object_path"]
            fixture_path = root / "fixture.json"
            configuration = examples.json_file(fixture_path, config)
            record = json.loads((example.directory / examples.CDN.XORB_PRODUCT_DIR / "mount.json").read_bytes())
            request_id = "mount-viewer-request"
            response = next(iter(record["edge_responses"].values()))
            record.update(configuration_sha256=configuration, xorb_key_sha256=hashlib.sha256(key.encode()).hexdigest(),
                          edge_responses={hashlib.sha256(request_id.encode()).hexdigest(): response})
            client, logs, origins, output = (root / name for name in ("mount.json", "viewer.log", "s3.log", "proof.json"))
            examples.json_file(client, record)
            origins.touch()
            arguments = ["collect", "--fixture", str(fixture_path), "--client", str(client),
                         "--configuration-sha256", configuration, "--output", str(output), "--log", str(logs),
                         "--s3-origin-log", str(origins)]
            header = ("#Fields: x-edge-request-id x-host-header cs-method cs-uri-stem cs-protocol "
                      "sc-status sc-bytes x-edge-response-result-type\n")
            wrong_key = key.rsplit("/xorbs/", 1)[0] + "/xorbs/bb/" + "b" * 64
            stderr = StringIO()
            with patch.object(sys, "argv", arguments), redirect_stderr(stderr):
                for observed, expected in ((wrong_key, 1), (key, 0)):
                    logs.write_text(header + f"{request_id}\tfixture.example.com\tGET\t/crab/{observed}\thttps\t206\t128\tHit\n")
                    self.assertEqual(COLLECT.main(), expected)
                proof = json.loads(output.read_bytes())
                self.assertEqual(next(iter(proof["records"].values()))["key_sha256"], record["xorb_key_sha256"])
                output.unlink()
                response.update(family="xorb", object_bytes=config["size"])
                sparse = record | {"schema": examples.CDN.MOUNT_XORB_RANGE_READ, "range_offset": 512,
                                   "payload": {"sha256": "a" * 64, "bytes": 64}}
                examples.json_file(client, sparse)
                self.assertEqual(COLLECT.main(), 0, stderr.getvalue())

    def test_cloudfront_companion_waits_for_timed_viewer_and_origin_logs(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            config = fixture(access="private") | {"deployment": {"distribution_id": "EDIST12345", "region": "us-east-1"}}
            fixture_path, output = root / "fixture.json", root / "cloudfront-requests.json"
            configuration = examples.json_file(fixture_path, config)
            registration = {"scope": config["scope"], "source_sha": examples.COMMIT, "configuration_sha256": configuration,
                            "regions": {"near": "near"}, "samples": 1, "viewer_binding_sha256": "e" * 64,
                            "candidate": examples.cli_candidate(config["scope"], "linux-x86_64")}
            examples.write_git_measurements(root, registration, "linux-x86_64")
            client = root / "near/0001/cold_edge/client.json"
            report = json.loads(client.read_bytes())
            viewer_id = "raw-viewer-id"
            viewer_digest = hashlib.sha256(viewer_id.encode()).hexdigest()
            response = next(iter(report["edge_responses"].values()))
            examples.json_file(client, report | {"edge_responses": {viewer_digest: response}})
            started = datetime.fromtimestamp(report["started_at_unix_ms"] // 1000, timezone.utc)
            timestamp = started.strftime("%d/%b/%Y:%H:%M:%S +0000")
            key = f"{config['fixture_prefix']}/{PACK}"
            viewer_prefix = "AWSLogs/123456789012/CloudFront/"
            viewer_key, origin_key = viewer_prefix + "viewer.gz", "origin/origin-log"
            viewer = gzip.compress(
                ("#Fields: x-edge-request-id x-host-header cs-method cs-uri-stem cs-protocol sc-status sc-bytes "
                 "x-edge-response-result-type\n"
                 f"{viewer_id}\tfixture.example.com\tGET\t/crab/{key}\thttps\t206\t620\tMiss\n").encode(),
            )
            origin = (f'owner fixture-bucket [{timestamp}] 192.0.2.1 fixture-principal raw-origin-id '
                      f'REST.GET.OBJECT {key} "GET /{key} HTTP/1.1" 206 - 4096 5000 4 1 "-" "Amazon CloudFront" '
                      '- extended-id SigV4 cipher AuthHeader fixture-bucket.s3.us-east-1.amazonaws.com TLSv1.3 - - us-east-1\n').encode()
            objects = {viewer_key: viewer, origin_key: origin}
            delivered = set(objects)
            values = {"LogBucket": "fixture-logs", "ViewerLogPrefix": viewer_prefix, "OriginLogPrefix": "origin/",
                      "OriginBucket": "fixture-bucket", "DistributionId": "EDIST12345"}
            etag = '"snapshot"'
            downloaded = []
            arguments = ["collect", "--fixture", str(fixture_path), "--client", str(client),
                         "--configuration-sha256", configuration, "--output", str(output),
                         "--aws-log-stack", "crab-logs", "--aws-profile", "crab", "--require-origin-fill"]

            def aws(profile, region, service, operation, *args):
                self.assertEqual((profile, region), ("crab", "us-east-1"))
                if service == "s3api":
                    self.assertEqual(args[args.index("--expected-bucket-owner") + 1], "123456789012")
                if operation == "describe-stacks":
                    self.assertEqual(args, ("--stack-name", "crab-logs"))
                    return {"Stacks": [{"StackName": "crab-logs", "StackStatus": "CREATE_COMPLETE",
                                        "StackId": "arn:aws:cloudformation:us-east-1:123456789012:stack/crab-logs/a-b",
                                        "Outputs": [{"OutputKey": k, "OutputValue": v} for k, v in values.items()]}]}
                if operation == "get-bucket-logging":
                    self.assertIn("fixture-bucket", args)
                    return {"LoggingEnabled": {"TargetPrefix": "origin/", "TargetBucket": "fixture-logs"}}
                self.assertIn("fixture-logs", args)
                if operation == "list-objects-v2":
                    prefix = args[args.index("--prefix") + 1]
                    log = viewer_key if prefix == viewer_prefix else origin_key
                    contents = [{"Key": prefix + "older", "Size": 100, "ETag": etag, "LastModified": "1970-01-01T00:00:01Z"}]
                    if log in delivered:
                        contents.append({"Key": log, "Size": len(objects[log]), "ETag": etag, "LastModified": started.isoformat()})
                    return {"IsTruncated": False, "Contents": contents}
                self.assertEqual(operation, "get-object")
                self.assertEqual(args[args.index("--if-match") + 1], etag)
                key = args[args.index("--key") + 1]
                raw = Path(args[-1])
                raw.write_bytes(objects[key])
                downloaded.append(raw)
                return {"ContentLength": len(objects[key]), "ETag": etag}

            with patch.object(AWS_LOGS, "aws", aws), patch.object(sys, "argv", arguments):
                with patch.dict(values, {"DistributionId": "OTHER1234"}):
                    self.assertEqual(COLLECT.main(), 1)
                    self.assertFalse(output.exists())
                self.assertFalse(downloaded)
                with patch.object(AWS_LOGS, "LOG_BYTES", len(viewer) + len(origin)):
                    self.assertEqual(COLLECT.main(), 1)
                    self.assertFalse(output.exists())
                    self.assertTrue(all(not raw.parent.exists() for raw in downloaded))
                downloaded.clear()
                delivered.remove(origin_key)
                self.assertEqual(COLLECT.main(), 1)
                self.assertFalse(output.exists())
                downloaded.clear()
                delivered.clear()
                with (patch.object(sys, "argv", arguments + ["--aws-log-wait-seconds", "1"]),
                      patch.object(COLLECT.time, "monotonic", side_effect=[0, 1]),
                      patch.object(COLLECT.time, "sleep") as sleep):
                    self.assertEqual(COLLECT.main(), 1)
                    self.assertFalse(output.exists())
                    sleep.assert_not_called()
                pending = iter((viewer_key, origin_key))

                def receive_log(_seconds):
                    self.assertFalse(output.exists())
                    self.assertTrue(all(not raw.parent.exists() for raw in downloaded))
                    delivered.add(next(pending))

                with (patch.object(sys, "argv", arguments + ["--aws-log-wait-seconds", "180"]),
                      patch.object(COLLECT.time, "monotonic", side_effect=[0, 0, 60]),
                      patch.object(COLLECT.time, "sleep", side_effect=receive_log) as sleep):
                    self.assertEqual(COLLECT.main(), 0)
                    self.assertEqual(sleep.call_count, 2)
            proof = json.loads(output.read_bytes())
            self.assertEqual(set(proof["records"]), {viewer_digest})
            self.assertEqual(next(iter(proof["origin_requests"].values()))["response_body_bytes"], 4096)
            self.assertEqual(len(downloaded), 3)
            self.assertTrue(all(not raw.parent.exists() for raw in downloaded))
            self.assertNotIn("raw-origin-id", output.read_text())

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
            client = root / "near/0001/cold_edge/client.json"
            report = json.loads(client.read_bytes())
            viewer_id = "fixture-request_V1=="
            fingerprint = hashlib.sha256(viewer_id.encode()).hexdigest()
            response = next(iter(report["edge_responses"].values()))
            digest = examples.json_file(client, report | {"edge_responses": {fingerprint: response}})
            log = root / "export.w3c"
            prefix = f"/crab/{config['fixture_prefix']}/"
            header = ("#Version: 1.0\n#Fields: date x-edge-request-id x-host-header cs-method cs-uri-stem cs-protocol "
                      "sc-status sc-bytes x-edge-response-result-type c-ip\n")
            actual = (f"1970-01-01\t{viewer_id}\tfixture.example.com\tGET\t{prefix}{PACK}\thttps\t206\t620\tMiss\t192.0.2.1\n")
            unrelated = actual.replace(viewer_id, "unrelated-request").replace("fixture.example.com", "other.example.com")
            key = f"{config['fixture_prefix']}/{PACK}"
            at = report["started_at_unix_ms"] // 1000
            timestamp = datetime.fromtimestamp(at, timezone.utc).strftime("%d/%b/%Y:%H:%M:%S %z")
            origin_line = (
                f'fixture-owner fixture-bucket [{timestamp}] 192.0.2.2 fixture-principal s3-origin-id '
                f'REST.GET.OBJECT {key} "GET /{key} HTTP/1.1" 206 - 4096 5000 4 1 "-" "Amazon CloudFront" '
                '- extended-id SigV4 cipher AuthHeader fixture-bucket.s3.us-east-1.amazonaws.com TLSv1.3 - - us-east-1\n'
            )
            origins = root / "s3-access.log"
            output = root / "cloudfront-requests.json"
            command = [sys.executable, "-B", str(Path(COLLECT.__file__)), "--fixture", str(fixture_path), "--log", str(log),
                       "--client", str(client), "--configuration-sha256", configuration, "--output", str(output),
                       "--s3-origin-log", str(origins), "--require-origin-fill"]
            for records, s3, reason in (
                (unrelated, origin_line, "must cover each measured viewer attempt"),
                (actual, "", "no logged fill from the approved CDN origin"),
                (actual.replace(prefix, "/other/"), origin_line, "outside the approved viewer host or repository prefix"),
                (actual + actual.replace("620", "619"), origin_line, "exports disagree about one viewer request"),
                (actual, origin_line + origin_line.replace("4096", "4100"), "exports disagree about one origin request"),
                (actual, origin_line.replace("fixture-bucket.s3.", "wrong-bucket.s3."), "outside the approved S3 endpoint"),
                (unrelated + actual + actual, origin_line * 2
                 + origin_line.replace("s3-origin-id", "direct-id").replace("Amazon CloudFront", "crab")
                 + origin_line.replace("s3-origin-id", "wrong-bucket-id").replace("fixture-bucket", "another-bucket")
                 + origin_line.replace("s3-origin-id", "wrong-object-id").replace(key, "other/key")
                 + origin_line.replace("s3-origin-id", "older-id").replace(timestamp, "01/Jan/1970:00:00:00 +0000"), None),
            ):
                with self.subTest(reason=reason):
                    log.write_text(header + records, encoding="utf-8")
                    origins.write_text(s3, encoding="utf-8")
                    result = subprocess.run(command, capture_output=True, text=True, check=False)
                    if reason is not None:
                        self.assertEqual(result.returncode, 1)
                        self.assertIn(reason, result.stderr)
                        self.assertFalse(output.exists())
                    else:
                        self.assertEqual(result.returncode, 0, result.stderr)
                        retained = output.read_text(encoding="utf-8")
                        key_hash = hashlib.sha256(key.encode()).hexdigest()
                        self.assertEqual(json.loads(retained), {
                            "schema": "crab.cloudfront-viewer-requests/v1", "client_report_sha256": digest,
                            "records": {fingerprint: {"status": 206, "response_bytes": 620, "cache": "miss", "key_sha256": key_hash}},
                            "origin_requests": {hashlib.sha256(b"s3-origin-id").hexdigest(): {
                                "key_sha256": key_hash, "at_unix_s": at, "status": 206, "response_body_bytes": 4096,
                            }},
                        })
                        for private in (viewer_id, "fixture.example.com", prefix, "192.0.2.1", "192.0.2.2",
                                        "s3-origin-id", "fixture-principal", key, "fixture-bucket"):
                            self.assertNotIn(private, retained)

            config["scope"] = "cloudfront/private/sdk/git/direct"
            sdk_configuration = examples.json_file(fixture_path, config)
            sdk_registration = registration | {
                "scope": config["scope"], "configuration_sha256": sdk_configuration,
                "sdk_candidate": examples.sdk_candidate(config["scope"], "linux-x86_64"),
            }
            examples.write_git_measurements(root, sdk_registration, "linux-x86_64")
            snapshot = json.loads(client.read_bytes()) | {"edge_responses": {fingerprint: response}}
            pack = "8" * 64
            worktree = {"crab_sha256": "9" * 64, "installed_packs": [pack], "verified_edge_packs": [pack]}
            sdk_records = {
                "read": snapshot,
                "clone": snapshot | worktree,
                "fetch": snapshot | worktree | {"previous_head": "1" * 40, "head_after_fetch": "1" * 40},
            }
            command[command.index(configuration)] = sdk_configuration
            for action, record in sdk_records.items():
                with self.subTest(action=action):
                    sdk_digest = examples.json_file(client, record | {"schema": f"crab.cdn-sdk-git-{action}/v1"})
                    sdk_output = root / f"sdk-{action}-requests.json"
                    args = [str(sdk_output) if arg == str(output) else arg for arg in command]
                    result = subprocess.run(args, capture_output=True, text=True, check=False)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(json.loads(sdk_output.read_bytes())["client_report_sha256"], sdk_digest)

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
                       "--client", str(client), "--configuration-sha256", configuration, "--output", str(output),
                       "--require-origin-fill"]
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
                ([viewer | {"httpRequest": request | {"cacheFillBytes": "0"}},
                  origin | {"httpRequest": origin["httpRequest"] | {"status": 404}}],
                 "no logged fill from the approved CDN origin"),
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

            expected = output.read_bytes()
            output.unlink()
            log_arg = command.index("--log")
            api_command = [*command[2:log_arg], *command[log_arg + 2:], "--gcp-logging"]
            pages = iter((
                {"nextPageToken": "empty-page"},
                {"entries": [origin, validation], "nextPageToken": "origin-page"},
                {"entries": [viewer]},
            ))
            tokens = []

            @contextmanager
            def logging(base, method, target, headers, label, *, body):
                self.assertEqual((base.netloc, method, target, label),
                                 ("logging.googleapis.com", "POST", "/v2/entries:list", "Cloud Logging"))
                self.assertEqual(headers["Authorization"], "Bearer fixture-cloud-identity")
                query = json.loads(body)
                self.assertEqual(query["resourceNames"], [f"projects/{project}"])
                self.assertEqual(query["orderBy"], "timestamp desc")
                self.assertEqual(query["pageSize"], COLLECT.GCP_LOG_PAGE_SIZE)
                self.assertIn('resource.type="http_load_balancer"', query["filter"])
                self.assertIn(f'logName="projects/{project}/logs/requests"', query["filter"])
                self.assertIn(f'resource.labels.forwarding_rule_name="{rule}"', query["filter"])
                start = datetime.fromtimestamp(report["started_at_unix_ms"] / 1000, timezone.utc) - timedelta(minutes=1)
                finish = datetime.fromtimestamp(report["finished_at_unix_ms"] / 1000, timezone.utc) + timedelta(minutes=1)
                self.assertIn(f'timestamp >= "{start.isoformat(timespec="milliseconds")}"', query["filter"])
                self.assertIn(f'timestamp <= "{finish.isoformat(timespec="milliseconds")}"', query["filter"])
                tokens.append(query.get("pageToken"))
                yield SimpleNamespace(status=200, read=BytesIO(json.dumps(next(pages)).encode()).read)

            with (patch.object(COLLECT, "bearer", return_value="Bearer fixture-cloud-identity") as bearer,
                  patch.object(COLLECT, "https_response", logging), patch.object(sys, "argv", api_command)):
                self.assertEqual(COLLECT.main(), 0)
                bearer.assert_called_once_with("gcloud", "auth", "print-access-token", "--quiet")
                self.assertEqual(tokens, [None, "empty-page", "origin-page"])
                self.assertEqual(output.read_bytes(), expected)
                output.unlink()
                pages = iter(({"nextPageToken": "repeated-token"}, {"nextPageToken": "repeated-token"}))
                with redirect_stderr(StringIO()) as errors:
                    self.assertEqual(COLLECT.main(), 1)
                self.assertIn("repeated a pagination token", errors.getvalue())
                self.assertNotIn("repeated-token", errors.getvalue())
                self.assertFalse(output.exists())
                for limit, value, reason in (
                    ("GCP_LOG_PAGES", 1, "exceeds the page limit"),
                    ("JSON_LOG_LIMIT", 1, "exceed the JSON size limit"),
                ):
                    with self.subTest(limit=limit), patch.object(COLLECT, limit, value), redirect_stderr(StringIO()) as errors:
                        pages = iter(({"nextPageToken": "pending-page"},))
                        self.assertEqual(COLLECT.main(), 1)
                        self.assertIn(reason, errors.getvalue())
                        self.assertFalse(output.exists())

            config |= {"scope": "cloud-cdn/private/sdk/xorb/direct", "base_url": f"https://{host}/crab",
                       "view_suffix": "/acl-views/v2/" + "2" * 64 + "/1-" + "3" * 64,
                       "object_path": ".crab/xorbs/aa/" + "a" * 64}
            hydration_config = examples.json_file(fixture_path, config)
            key = COLLECT.fixture_object_key(config, "private", "xorb")
            key_sha256 = hashlib.sha256(key.encode()).hexdigest()
            hydration = {key: report[key] for key in examples.CDN.SDK_HYDRATION_READ_FIELDS & report.keys()} | {
                "schema": "crab.cdn-sdk-hydration/v1", "scope": config["scope"], "xorb_key_sha256": key_sha256,
                "configuration_sha256": hydration_config, "edge_responses": {fingerprint: response | {"family": "xorb"}},
                "workload_sha256": "1" * 64, "crab_sha256": "2" * 64, "pointer_sha256": "3" * 64,
                "payload": {"sha256": "4" * 64, "bytes": 8},
            }
            hydration_digest = examples.json_file(client, hydration)
            command[command.index(configuration)] = hydration_config
            for observed_key in (key[:-64] + "b" * 64, key):
                request_uri = f"{config['base_url']}/{observed_key}"
                examples.json_file(log, [viewer | {"httpRequest": request | {"requestUrl": request_uri}}])
                result = subprocess.run(command, capture_output=True, text=True, check=False)
                if observed_key != key:
                    self.assertEqual(result.returncode, 1)
                    self.assertIn("did not read the approved hydration xorb", result.stderr)
                    self.assertFalse(output.exists())
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    proof = json.loads(output.read_bytes())
                    self.assertEqual(proof["client_report_sha256"], hydration_digest)
                    self.assertEqual(proof["records"][fingerprint]["key_sha256"], key_sha256)

    def test_public_sdk_snapshot_requires_cloud_cdn_delivery_of_the_approved_pack(self):
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            project, rule = "fixture-project", "crab-https"
            config = fixture("cloud-cdn", "public") | {
                "scope": "cloud-cdn/public/sdk/git/direct", "deployment": {"project": project, "forwarding_rule": rule},
            }
            fixture_path = root / "fixture.json"
            configuration = examples.json_file(fixture_path, config)
            registration = {"scope": config["scope"], "source_sha": examples.COMMIT, "configuration_sha256": configuration,
                            "regions": {"near": "near"}, "samples": 1, "viewer_binding_sha256": "e" * 64,
                            "sdk_candidate": examples.sdk_candidate(config["scope"], "linux-x86_64")}
            examples.write_git_measurements(root, registration, "linux-x86_64")
            client = root / "near/0001/cold_edge/client.json"
            report = json.loads(client.read_bytes())
            tag = f"crab-cdn-qualification/{'0a' * 16}"
            fingerprint = hashlib.sha256(tag.encode()).hexdigest()
            response = next(iter(report["edge_responses"].values()))
            key = f"{config['fixture_prefix']}/{PACK}"
            digest = hashlib.sha256(key.encode()).hexdigest()
            snapshot = report | {"transition_key_sha256": digest, "edge_responses": {fingerprint: response}}
            log, output = root / "gcloud.json", root / "viewer-requests.json"
            command = [sys.executable, "-B", str(Path(COLLECT.__file__)), "--fixture", str(fixture_path), "--log", str(log),
                       "--client", str(client), "--configuration-sha256", configuration, "--output", str(output)]
            for observed_key, status, error in (
                (key[:-64] + "b" * 64, 206, "did not confirm the new export object"),
                (key, 403, "did not confirm the new export object"),
                (key, 206, None),
            ):
                with self.subTest(observed_key=observed_key, status=status):
                    snapshot_digest = examples.json_file(client, snapshot | {"edge_responses": {
                        fingerprint: response if status == 206 else response | {"status": status, "complete": False, "bytes": 0},
                    }})
                    request = {"requestMethod": "GET", "requestUrl": f"{config['base_url']}/{observed_key}",
                               "responseSize": "620", "status": status, "userAgent": tag, "cacheLookup": True}
                    resource = {"type": "http_load_balancer", "labels": {"project_id": project, "forwarding_rule_name": rule}}
                    examples.json_file(log, [{"httpRequest": request, "jsonPayload": {}, "insertId": "viewer",
                                              "logName": f"projects/{project}/logs/requests", "resource": resource}])
                    result = subprocess.run(command, capture_output=True, text=True, check=False)
                    if error:
                        self.assertEqual(result.returncode, 1)
                        self.assertIn(error, result.stderr)
                        self.assertFalse(output.exists())
                    else:
                        self.assertEqual(result.returncode, 0, result.stderr)
                        proof = json.loads(output.read_bytes())
                        self.assertEqual(proof["client_report_sha256"], snapshot_digest)
                        self.assertEqual(proof["records"][fingerprint]["key_sha256"], digest)

    def test_azure_archive_reads_only_the_hours_around_the_timed_action(self):
        subscription = "a0a0a0a0-bbbb-cccc-dddd-e1e1e1e1e1e1"
        resource = f"/subscriptions/{subscription}/resourcegroups/test/providers/microsoft.cdn/profiles/profile"
        client = {"started_at_unix_ms": 3_600_001, "finished_at_unix_ms": 3_600_010}
        archive = {"records": []}
        paths = []

        @contextmanager
        def storage(_base, _method, path, _headers, _label):
            paths.append(path)
            if "/h=00/" in path:
                yield SimpleNamespace(status=404, getheader=lambda _name: "BlobNotFound")
            else:
                yield SimpleNamespace(status=200, read=BytesIO(json.dumps(archive).encode()).read)

        with (patch.object(AZURE_LOGS, "archive_account", return_value="viewerlogs"),
              patch.object(AZURE_LOGS, "https_response", storage)):
            records = list(AZURE_LOGS.azure_log_archives(resource, "FrontDoorAccessLog", client, "arm", "storage", 4096))
            self.assertEqual(records, [archive])
            prefix = (f"/insights-logs-frontdooraccesslog/resourceId=/SUBSCRIPTIONS/{subscription}"
                      "/RESOURCEGROUPS/TEST/PROVIDERS/MICROSOFT.CDN/PROFILES/PROFILE/y=1970/m=01/d=01")
            self.assertEqual(paths, [f"{prefix}/h={hour:02}/m=00/PT1H.json" for hour in (0, 1)])
            with self.assertRaisesRegex(ValueError, "exceed the JSON size limit"):
                list(AZURE_LOGS.azure_log_archives(resource, "FrontDoorAccessLog", client, "arm", "storage", 1))

    def test_front_door_matches_blob_reads_and_excludes_origin_shield_and_direct_reads(self):
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
                if access == "public":
                    registration["publication_sha256"] = "6" * 64
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
                selected_origin = config["native_origin" if access == "private" else "export_origin"]
                account = selected_origin.split("//", 1)[1].split(".", 1)[0]
                blob_resource = (f"/subscriptions/{deployment['subscription']}/resourceGroups/test/providers/Microsoft.Storage/"
                                 f"storageAccounts/{account}/blobServices/default")
                principal, correlation = "7abb6400-a5b4-4fed-a638-fcd011a29292", "77777777-0000-1111-2222-333333333333"
                key = f"{config['fixture_prefix']}/{PACK}"
                at = report["started_at_unix_ms"] + 1
                timestamp = datetime.fromtimestamp(at / 1000, timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "1234Z")
                blob = {"category": "StorageRead", "resourceId": blob_resource, "operationName": "GetBlob",
                        "uri": f"{selected_origin}/{key}", "statusCode": 206, "correlationId": correlation, "time": timestamp,
                        "properties": {"responseBodySize": 1024}, "callerIpAddress": "[2001:db8::1]:12345"}
                if access == "private":
                    authority = {"id": resource, "identity": {"type": "SystemAssigned", "principalId": principal}}
                    blob["identity"] = {"type": "OAuth", "requester": {"objectId": principal}}
                    direct = blob | {"identity": {"type": "OAuth", "requester": {"objectId": "other"}}}
                    bad_authority = authority | {"id": resource + "-other"}
                    authority_error = "needs the approved Front Door profile's system-assigned identity"
                else:
                    authority = {"cloud": "Public", "values": [{"name": "AzureFrontDoor.Backend",
                                                                 "properties": {"addressPrefixes": ["2001:db8::/64"]}}]}
                    blob["identity"] = {"type": "Anonymous"}
                    direct = blob | {"callerIpAddress": "[2001:db9::1]:12345"}
                    bad_authority = authority | {"cloud": "AzureGovernment"}
                    authority_error = "expected Azure Public service tags"
                archive, output = root / "PT1H.json", root / "front-door-requests.json"
                origin_log, origin_authority = root / "storage-read.json", root / "origin-authority.json"
                command = [sys.executable, "-B", str(Path(COLLECT.__file__)), "--fixture", str(fixture_path), "--log", str(archive),
                           "--client", str(client), "--configuration-sha256", configuration, "--output", str(output),
                           "--azure-blob-log", str(origin_log), "--azure-origin-authority", str(origin_authority)]
                for entries, blobs, selected, error in (
                    ([shield], [blob], authority, "must cover each measured viewer attempt"),
                    ([viewer | {"resourceId": resource + "-other"}], [blob], authority, "outside the approved profile or repository prefix"),
                    ([viewer | {"properties": properties | {"cacheStatus": "PARTIAL_HIT"}}], [blob], authority, "cache result disagrees"),
                    ([viewer], [blob], bad_authority, authority_error),
                    ([viewer], [blob, blob | {"properties": {"responseBodySize": 999}}], authority,
                     "Azure Blob log exports disagree about one origin request"),
                    ([shield, viewer], [blob, direct, blob | {"resourceId": blob_resource + "2"},
                                       blob | {"time": "1970-01-01T00:00:01.0000000Z"}, blob], authority, None),
                ):
                    with self.subTest(error=error):
                        examples.json_file(archive, {"records": entries})
                        examples.json_file(origin_log, {"records": blobs})
                        examples.json_file(origin_authority, selected)
                        result = subprocess.run(command, capture_output=True, text=True, check=False)
                        if error is not None:
                            self.assertEqual(result.returncode, 1)
                            self.assertIn(error, result.stderr)
                            self.assertFalse(output.exists())
                        else:
                            self.assertEqual(result.returncode, 0, result.stderr)
                            retained = output.read_text(encoding="utf-8")
                            key_hash = hashlib.sha256(key.encode()).hexdigest()
                            self.assertEqual(json.loads(retained), {
                                "schema": "crab.front-door-viewer-requests/v1", "client_report_sha256": digest,
                                "records": {request_id: {"status": 206, "response_bytes": 620, "cache": "hit", "key_sha256": key_hash}},
                                "origin_requests": {hashlib.sha256(correlation.encode()).hexdigest(): {
                                    "key_sha256": key_hash, "at_unix_ms": at, "status": 206, "response_body_bytes": 1024,
                                }},
                            })
                            for private in (reference, resource, host, "192.0.2.1", "2001:db8::1", principal,
                                            blob_resource, key, correlation):
                                self.assertNotIn(private, retained)

                expected = output.read_bytes()
                output.unlink()
                group = f"/subscriptions/{deployment['subscription']}/resourceGroups/test"
                diagnostics = {
                    resource.lower(): ("FrontDoorAccessLog", "viewerlogs", archive),
                    blob_resource.lower(): ("StorageRead", "originlogs", origin_log),
                }
                destinations = {
                    source: [{"properties": {
                        "storageAccountId": f"{group}/providers/Microsoft.Storage/storageAccounts/{account}",
                        "logs": [{"category": category, "enabled": True}],
                    }}]
                    for source, (category, account, _) in diagnostics.items()
                }

                def arm(path, version, token):
                    self.assertEqual(token, "Bearer arm-test")
                    if path == resource.lower():
                        self.assertEqual(version, COLLECT.FRONT_DOOR)
                        self.assertEqual(access, "private")
                        return authority
                    self.assertEqual(version, AZURE_LOGS.DIAGNOSTIC_SETTINGS)
                    suffix = "/providers/Microsoft.Insights/diagnosticSettings"
                    self.assertTrue(path.endswith(suffix))
                    return {"value": destinations[path.removesuffix(suffix)]}

                @contextmanager
                def storage(base, method, target, headers, label):
                    self.assertEqual((method, label, headers["Authorization"]), ("GET", "Azure Monitor archive", "Bearer storage-test"))
                    account = base.netloc.removesuffix(".blob.core.windows.net")
                    source, category, path = next((source, category, path) for source, (category, storage, path) in diagnostics.items()
                                                  if account == storage)
                    expected_path = (f"/insights-logs-{category.lower()}/resourceId={source.upper()}"
                                     "/y=1970/m=01/d=01/h=00/m=00/PT1H.json")
                    self.assertEqual(target, expected_path)
                    yield SimpleNamespace(status=200, read=BytesIO(path.read_bytes()).read)

                def auth(*args):
                    self.assertIn(deployment["subscription"], args)
                    return {"https://management.azure.com/": "Bearer arm-test",
                            "https://storage.azure.com/": "Bearer storage-test"}[args[-1]]

                api_command = ["collect", "--fixture", str(fixture_path), "--configuration-sha256", configuration,
                               "--client", str(client), "--output", str(output), "--azure-monitor", "--require-origin-fill"]
                if access == "public":
                    api_command += ["--azure-origin-authority", str(origin_authority)]
                with (patch.object(sys, "argv", api_command), patch.object(COLLECT, "bearer", auth),
                      patch.object(COLLECT, "arm_document", arm), patch.object(AZURE_LOGS, "arm_document", arm),
                      patch.object(AZURE_LOGS, "https_response", storage)):
                    self.assertEqual(COLLECT.main(), 0)
                    self.assertEqual(output.read_bytes(), expected)
                    output.unlink()
                    destinations[resource.lower()].append({"properties": {
                        "storageAccountId": f"{group}/providers/Microsoft.Storage/storageAccounts/otherlogs",
                        "logs": [{"categoryGroup": "allLogs", "enabled": True}],
                    }})
                    with redirect_stderr(StringIO()) as errors:
                        self.assertEqual(COLLECT.main(), 1)
                    self.assertIn("expected one Azure storage destination", errors.getvalue())
                    self.assertNotIn("otherlogs", errors.getvalue())
                    self.assertFalse(output.exists())
