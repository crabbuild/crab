"""Bind ordinary service responses to the approved payload and fresh storage observations."""

import argparse
import hashlib
import io
import json
import subprocess
import sys
import unittest
from contextlib import contextmanager
from email.message import Message
from pathlib import Path
from tempfile import TemporaryDirectory
from types import SimpleNamespace
from unittest.mock import patch

import collect_cdn_viewer_requests as COLLECT
import run_cdn_service_read as SERVICE
import verify_cdn_service_lifecycle as LIFECYCLE
from cdn_qualification import CDN_PROVENANCE_FIELDS, CLEANUP_INVENTORIES, PROVIDER_FOR_EDGE, lfs_object_path
from test_cdn_qualification import COMMIT, cli_candidate, json_file, service_candidate
from test_cdn_transport_qualification import fixture


BODY = b"approved service bytes"
SHA256 = hashlib.sha256(BODY).hexdigest()
GIT_OID = hashlib.sha1(f"blob {len(BODY)}\0".encode() + BODY, usedforsecurity=False).digest()
GIT_SHA256 = hashlib.sha256(GIT_OID).hexdigest()
ETAG = '"approved-validator"'


def events(provider, consumer, family, access):
    route = {"provider": provider, "access": access, "consumer": consumer, "family": family, "profile": "direct"}
    transfer = {"message": "CDN immutable GET", "status": 206, "bytes": 64, "cache": "hit", "range": True,
                "range_start": 0, "object_bytes": 1024, "requested_bytes": 64,
                "complete": True, "elapsed_ms": 1, "request_sha256": "d" * 64}
    payload = {"message": f"CDN verified {'Git' if family == 'git' else 'LFS'} payload", "source": "edge"}
    if family == "git":
        payload["git_blob_oid_sha256"] = GIT_SHA256
    return [route | transfer, route | payload]


class CdnServiceReadTests(unittest.TestCase):
    def prepare(self, root, provider, consumer, family, access="private"):
        config = fixture(provider, access) | {"scope": f"{provider}/{access}/{consumer}/{family}/direct"}
        if provider == "cloud-cdn":
            config["deployment"] = {"project": "fixture-project", "forwarding_rule": "crab-https"}
        if family == "lfs":
            config |= {"object_path": lfs_object_path(SHA256), "sha256": SHA256, "size": len(BODY)}
        config_sha = json_file(root / "fixture.json", config)
        provenance = {"scope": config["scope"], "configuration_sha256": config_sha, "source_sha": COMMIT,
                      "workflow_run_id": "123", "workflow_run_attempt": "2"}
        platform = SERVICE.runner_platform()
        ref = "refs/heads/crab-cdn-qualification-123-2-abcdefabcdef"
        publication = provenance | {
            "schema": "crab.cdn-git-publication/v1", "ref": ref, "oid": "c" * 40, "sha256": SHA256,
            "payload_bytes": len(BODY),
            "published": True, "visible_at_unix_ms": 10, "candidate": cli_candidate(config["scope"], platform),
        }
        publication_sha = json_file(root / "publication.json", publication)
        native_ref = {"origin_sha256": hashlib.sha256(config["native_origin"].encode()).hexdigest(),
                      "prefix_sha256": hashlib.sha256(f"{config['fixture_prefix']}/".encode()).hexdigest(),
                      "ref_sha256": hashlib.sha256(ref.encode()).hexdigest(), "probe_source_sha": COMMIT,
                      "oid": publication["oid"], "repository_sha256": "1" * 64, "epoch_sha256": "2" * 64,
                      "root_digest": "3" * 64, "head_sha256": "4" * 64,
                      "revision_sha256": "5" * 64, "transaction_id": "6" * 64}
        json_file(root / "native-published.json", provenance | {
            "schema": "crab.cdn-native-ref/v1", "phase": "published", "publication_sha256": publication_sha,
            "platform": platform, "started_at_unix_ms": 20, "observed_at_unix_ms": 30, "native_ref": native_ref,
        })
        repo = "org/repo" if consumer == "server" else "fixture-bucket"
        workload = {"schema": "crab.cdn-service-workload/v1", "configuration_sha256": config_sha,
                    "publication_sha256": publication_sha, "endpoint": "https://service.example.com", "repository": repo,
                    "commit": "c" * 40, "payload_sha256": SHA256, "payload_bytes": len(BODY),
                    "etag_sha256": hashlib.sha256(ETAG.encode()).hexdigest(),
                    "public_export_sha256": "b" * 64 if access == "public" else ""}
        credentials = ({"access_key": "fixture-key", "secret_key": "fixture-secret", "region": "us-east-1"}
                       if consumer == "s3-gateway" else {"cookie": "__Host-crab_session=fixture-token"}
                       if family == "git" else {"authorization": "Basic Zml4dHVyZTp0b2tlbg=="})
        json_file(root / "credentials.json", credentials)
        return argparse.Namespace(fixture=root / "fixture.json", configuration_sha256=config_sha,
            workload=root / "workload.json", workload_sha256=json_file(root / "workload.json", workload),
            credentials=root / "credentials.json", service_log=root / "server.log",
            publication=root / "publication.json", native_published=root / "native-published.json",
            source_sha=COMMIT, run_id="123", run_attempt="2", arm="edge", export_approval=None, primer=None,
            transition_object=None, before_export=None, prior_workload=None, prior_workload_sha256=None)

    def request(self, args, provider, consumer, family, fault=""):
        config = json.loads(args.fixture.read_bytes())
        _, access, _, _, _ = SERVICE.scope_parts(config["scope"])
        edge = events(PROVIDER_FOR_EDGE[provider], consumer, family, access)
        if family == "lfs":
            key = SERVICE.fixture_object_key(config, access, family)
            edge[1]["lfs_key_sha256"] = hashlib.sha256(key.encode()).hexdigest()
        instance = "f" * 64 if args.arm == "origin" or fault == "another-process" else "a" * 64
        if args.arm == "prior_export":
            instance = "8" * 64
        candidate = service_candidate(config["scope"], SERVICE.runner_platform())
        if fault == "wrong-build-source":
            candidate["source_sha"] = "b" * 40
        build = {"target": SERVICE.SERVICE_BUILD_TARGET, "fields": candidate | {
            "consumer": consumer, "message": "CDN service build identity",
            "service_instance_sha256": "c" * 64 if fault == "wrong-build-instance" else instance,
        }}
        natives = {"target": "crab_storage::native_read", "fields": {"message": "Native object store read",
                   "provider": PROVIDER_FOR_EDGE[provider], "method": "get", "status": 200, "bytes": len(BODY)}}
        prior = {"target": "crab_storage::cdn::transfer", "fields": edge[0]}
        initial = [prior] if fault == "no-build" else [build, prior]
        args.service_log.write_text("".join(json.dumps(line) + "\n" for line in initial), encoding="utf-8")

        @contextmanager
        def exchange(base, method, path, headers, _label):
            self.assertEqual((base.netloc, method), ("service.example.com", "GET"))
            if consumer == "s3-gateway":
                self.assertEqual(path, f"/fixture-bucket/{'c' * 40}/{SERVICE.PAYLOAD}")
                self.assertTrue(headers["authorization"].startswith("AWS4-HMAC-SHA256 Credential=fixture-key/"))
            elif family == "git":
                self.assertEqual(path, f"/api/repos/org/repo/blob?rev={'c' * 40}&path={SERVICE.PAYLOAD}")
                self.assertEqual(headers["cookie"], "__Host-crab_session=fixture-token")
            else:
                self.assertEqual(path, f"/git/org/repo.git/info/lfs/objects/{SHA256}?size={len(BODY)}")
                self.assertTrue(headers["authorization"].startswith("Basic "))
            if fault == "rotate":
                args.service_log.rename(args.service_log.with_suffix(".old"))
            if fault == "wrong-blob-event":
                edge[1]["git_blob_oid_sha256"] = "e" * 64
            if fault == "wrong-lfs-event":
                edge[1]["lfs_key_sha256"] = "e" * 64
            if fault == "unkeyed-lfs-event":
                del edge[1]["lfs_key_sha256"]
            if fault == "unrelated-route":
                edge[0]["consumer"] = "sdk"
            if fault == "missing-request":
                edge[0]["request_sha256"] = ""
            reading_at_edge = args.arm != "origin" or fault == "rollback-binding"
            export = "b" * 64 if reading_at_edge and access == "public" else ""
            if args.arm == "prior_export":
                export = "9" * 64
            reader = SERVICE.read_target_fingerprints(config, edge=reading_at_edge, export_sha256=export)
            for different in reader:
                if fault == different:
                    reader[different] = "e" * 64
            target = {"target": SERVICE.READ_TARGET,
                      "fields": reader | {"consumer": consumer, "message": "CDN service read target",
                                          "service_instance_sha256": instance}}
            lines = [natives]
            if fault != "no-target":
                lines.append(target)
            if fault == "duplicate-target":
                lines.append(target)
            if args.arm == "edge" and fault != "no-edge" or fault == "origin-edge":
                lines += [{"target": "crab_storage::cdn::transfer", "fields": event} for event in edge]
            if args.arm == "warm_local" and fault != "no-local":
                local = edge[1] | {"source": "local"}
                lines.append({"target": "crab_storage::cdn::transfer", "fields": local})
            if family == "lfs" and args.arm in ("edge", "warm_local") and fault != "no-cache-root":
                lines.append({"target": SERVICE.LFS_CACHE_TARGET, "fields": {
                    "message": "CDN service LFS cache",
                    "service_instance_sha256": "b" * 64 if fault == "wrong-cache-process" else instance,
                    "lfs_key_sha256": hashlib.sha256(SERVICE.fixture_object_key(config, access, family).encode()).hexdigest(),
                    "cache_root_sha256": "5" * 64 if fault == "different-cache-root" else "4" * 64,
                }})
            if args.transition_object is not None and fault != "no-transition":
                before = args.arm == "prior_export"
                key = hashlib.sha256(f"{config['fixture_prefix']}/{args.transition_object}".encode()).hexdigest()
                route = {name: edge[0][name] for name in ("provider", "consumer", "access", "family", "profile")}
                event = route | {"message": "CDN public export observation", "approval_sha256": export,
                                 "key_sha256": key, "observation": "unlisted" if before else "listed"}
                target_name = "crab_storage::cdn::public_transition"
                lines.append({"target": target_name, "fields": event})
                if not before or family == "git":
                    verified = event | {"observation": "verified_native" if before else "verified_edge"}
                    if family == "git":
                        verified["git_blob_oid_sha256"] = GIT_SHA256
                    lines.append({"target": target_name, "fields": verified})
            with args.service_log.open("a", encoding="utf-8") as log:
                log.writelines(json.dumps(line) + "\n" for line in lines)
            output = Message()
            output["ETag"] = ETAG if fault != "wrong-etag" else '"another"'
            output["Content-Length"] = str(len(BODY))
            if consumer == "server" and family == "git":
                output["x-crab-blob-oid"] = GIT_OID.hex() if fault != "wrong-blob-header" else "f" * 40
            body = BODY if fault != "wrong-body" else b"z" * len(BODY)
            yield SimpleNamespace(status=206 if fault == "partial" else 200, headers=output, read=io.BytesIO(body).read)

        with patch.object(SERVICE, "https_response", exchange):
            return SERVICE.probe(args)

    def test_measures_each_consumer_and_rejects_stale_traffic(self):
        for provider, consumer, family in (("cloud-cdn", "server", "git"), ("front-door", "server", "lfs"),
                                           ("cloudfront", "s3-gateway", "git"), ("cloudfront", "s3-gateway", "lfs")):
            with self.subTest(provider=provider, consumer=consumer, family=family), TemporaryDirectory() as directory:
                args = self.prepare(Path(directory), provider, consumer, family)
                edge = self.request(args, provider, consumer, family)
                args.arm = "origin"
                origin = self.request(args, provider, consumer, family)
                self.assertEqual((edge["verified_edge_payloads"], origin["all_edge_attempts"]), (1, 0))
                self.assertEqual((edge["payload_sha256"], origin["payload_sha256"]), (SHA256, SHA256))
                self.assertEqual(edge["etag_sha256"], origin["etag_sha256"])
                self.assertEqual(edge["reader"]["origin_sha256"], origin["reader"]["origin_sha256"])
                self.assertTrue(edge["reader"]["binding_sha256"])
                self.assertEqual(origin["reader"]["binding_sha256"], "")
                self.assertEqual(set(edge["edge_responses"]), {"d" * 64})
                self.assertNotIn("fixture-token", json.dumps(edge))
                self.assertNotIn("fixture-secret", json.dumps(edge))

    def test_requires_the_exact_verified_lfs_key(self):
        with TemporaryDirectory() as directory:
            args = self.prepare(Path(directory), "cloudfront", "server", "lfs")
            for fault, reason in (("wrong-lfs-event", "requires verified edge delivery"),
                                  ("unkeyed-lfs-event", "CDN verified LFS event"),
                                  ("no-cache-root", "prove its selected disk cache"),
                                  ("wrong-cache-process", "selected process and object")):
                with self.subTest(fault=fault), self.assertRaisesRegex(ValueError, reason):
                    self.request(args, "cloudfront", "server", "lfs", fault)

    def test_requires_verified_http_content_and_exact_new_storage_observations(self):
        provider, consumer, family = "cloudfront", "server", "git"
        with TemporaryDirectory() as directory:
            args = self.prepare(Path(directory), provider, consumer, family)
            for fault, reason in (("wrong-etag", "approved strong ETag"), ("wrong-body", "different payload bytes"),
                                  ("wrong-blob-header", "blob ID did not match"), ("partial", "incomplete or transformed"),
                                  ("no-edge", "requires verified edge delivery"), ("missing-request", "every viewer attempt"),
                                  ("wrong-blob-event", "requires verified edge delivery"),
                                  ("unrelated-route", "requires verified edge delivery"), ("rotate", "log rotated"),
                                  ("wrong-build-source", "build identity is invalid"),
                                  ("wrong-build-instance", "different process than the startup build"),
                                  ("no-build", "exactly one startup build observation")):
                with self.subTest(fault=fault), self.assertRaisesRegex(ValueError, reason):
                    self.request(args, provider, consumer, family, fault)
            args.arm = "origin"
            with self.assertRaisesRegex(ValueError, "zero edge attempts"):
                self.request(args, provider, consumer, family, "origin-edge")

    def test_rejects_unproved_service_placement_and_viewer_binding(self):
        provider, consumer, family = "front-door", "s3-gateway", "git"
        with TemporaryDirectory() as directory:
            args = self.prepare(Path(directory), provider, consumer, family, access="public")
            args.export_approval = Path(directory)
            with patch.object(SERVICE, "approved_public_export", return_value=("c" * 64, [], 1)):
                with self.assertRaisesRegex(ValueError, "exact public export receipt"):
                    self.request(args, provider, consumer, family)
            with patch.object(SERVICE, "approved_public_export", return_value=("b" * 64, [], 1)):
                for fault in ("origin_sha256", "prefix_sha256", "binding_sha256", "public_export_sha256",
                              "no-target", "duplicate-target"):
                    with self.subTest(fault=fault), self.assertRaisesRegex(ValueError, "approved native target"):
                        self.request(args, provider, consumer, family, fault)
                edge = self.request(args, provider, consumer, family)
                self.assertEqual(edge["reader"]["public_export_sha256"], "b" * 64)
            args.arm, args.export_approval = "origin", None
            with self.assertRaisesRegex(ValueError, "approved native target"):
                self.request(args, provider, consumer, family, "rollback-binding")
            origin = self.request(args, provider, consumer, family)
            self.assertEqual(origin["reader"]["public_export_sha256"], "")

    def test_requires_the_same_published_ref_before_opening_the_service(self):
        with TemporaryDirectory() as directory:
            args = self.prepare(Path(directory), "cloudfront", "server", "git")
            observed = json.loads(args.native_published.read_bytes())
            cases = (
                ({"phase": "retired"}, "does not match the Git publication"),
                ({"native_ref": observed["native_ref"] | {"oid": "d" * 40}}, "unexpected committed tip"),
                ({"started_at_unix_ms": 1}, "started before publication"),
                ({"observed_at_unix_ms": 10**17}, "started before the independent publication observation"),
            )
            for change, message in cases:
                with self.subTest(change=change), patch.object(SERVICE, "https_response") as http:
                    json_file(args.native_published, observed | change)
                    args.service_log.touch()
                    with self.assertRaisesRegex(ValueError, message):
                        SERVICE.probe(args)
                    http.assert_not_called()

    def test_repeats_the_exact_payload_in_the_edge_primers_process(self):
        for consumer, family in (("server", "git"), ("s3-gateway", "git"),
                                 ("server", "lfs"), ("s3-gateway", "lfs")):
            with self.subTest(consumer=consumer, family=family), TemporaryDirectory() as directory:
                root = Path(directory)
                args = self.prepare(root, "cloudfront", consumer, family)
                edge = self.request(args, "cloudfront", consumer, family)
                args.arm, args.primer = "warm_local", root / "warm-edge.json"
                digest = json_file(args.primer, edge)
                local = self.request(args, "cloudfront", consumer, family)
                self.assertEqual((local["primer_sha256"], local["local_payload_hits"]), (digest, 1))
                self.assertEqual(local["all_edge_attempts"], 0)
                for fault, error in (("another-process", "same process"), ("no-local", "verified cached payload")):
                    with self.subTest(fault=fault), self.assertRaisesRegex(ValueError, error):
                        self.request(args, "cloudfront", consumer, family, fault)
                if family == "lfs":
                    with self.assertRaisesRegex(ValueError, "reuse its own edge primer's cache root"):
                        self.request(args, "cloudfront", consumer, family, "different-cache-root")
                    json_file(args.primer, edge | {"lfs_key_sha256": "e" * 64})
                    with self.assertRaisesRegex(ValueError, "same process"):
                        self.request(args, "cloudfront", consumer, family)

    def test_service_read_observes_the_exact_public_transition_key(self):
        for consumer, family in (("server", "git"), ("s3-gateway", "lfs")):
            with self.subTest(family=family), TemporaryDirectory() as directory:
                root = Path(directory)
                args = self.prepare(root, "cloud-cdn", consumer, family, access="public")
                args.export_approval = root
                args.transition_object = json.loads(args.fixture.read_bytes())["object_path"]
                object = SimpleNamespace(path=args.transition_object)
                latest = json.loads(args.workload.read_bytes())
                args.arm = "prior_export"
                args.workload_sha256 = json_file(args.workload, latest | {"public_export_sha256": "9" * 64})
                with patch.object(SERVICE, "approved_public_export", return_value=("9" * 64, [object], 1)):
                    with self.assertRaisesRegex(ValueError, "membership does not match"):
                        self.request(args, "cloud-cdn", consumer, family)
                with patch.object(SERVICE, "approved_public_export", return_value=("9" * 64, [], 1)):
                    prior = self.request(args, "cloud-cdn", consumer, family)
                    self.assertEqual(prior["all_edge_attempts"], 0)
                args.arm = "edge"
                args.workload_sha256 = json_file(args.workload, latest)
                with patch.object(SERVICE, "approved_public_export", return_value=("b" * 64, [object], 50)):
                    with self.assertRaisesRegex(ValueError, "selected object's public export transition"):
                        self.request(args, "cloud-cdn", consumer, family, "no-transition")
                    edge = self.request(args, "cloud-cdn", consumer, family)
                self.assertEqual(prior["transition_key_sha256"], edge["transition_key_sha256"])
                self.assertEqual(prior["payload_sha256"], edge["payload_sha256"])

    def test_service_lifecycle_requires_edge_rollback_distinct_tombstone_and_matching_cleanup(self):
        provider, consumer, family = "cloud-cdn", "s3-gateway", "lfs"
        with TemporaryDirectory() as directory:
            root = Path(directory)
            args = self.prepare(root, provider, consumer, family)
            edge = self.request(args, provider, consumer, family)
            args.arm = "origin"
            origin = self.request(args, provider, consumer, family)
            publication = json.loads(args.publication.read_bytes())
            published = json.loads(args.native_published.read_bytes())
            reader = edge["reader"]
            provenance = {key: edge[key] for key in CDN_PROVENANCE_FIELDS}
            for field in ("edge", "origin", "viewer_requests", "retirement", "native_retired", "cleanup"):
                setattr(args, field, root / f"{field}.json")
            edge_sha = json_file(args.edge, edge)
            json_file(args.origin, origin)
            viewer = {"schema": "crab.cloud-cdn-viewer-requests/v1", "client_report_sha256": edge_sha,
                      "records": {"d" * 64: {"status": 206, "cache": "hit", "response_bytes": 300, "cache_fill_bytes": 0,
                                              "origin_requests": {}, "key_sha256": edge["lfs_key_sha256"]}}}
            retirement_time = origin["finished_at_unix_ms"] + 10
            retirement = provenance | {
                "schema": "crab.cdn-git-retirement/v1", "publication_sha256": edge["publication_sha256"],
                "candidate": publication["candidate"], "ref_retired": True, "retired_at_unix_ms": retirement_time,
            }
            tombstone = published["native_ref"] | {"oid": None, "head_sha256": "7" * 64,
                                                   "revision_sha256": "8" * 64, "transaction_id": "9" * 64}
            retired = published | {"phase": "retired", "started_at_unix_ms": retirement_time + 10,
                                   "observed_at_unix_ms": retirement_time + 20, "native_ref": tombstone}
            cleanup = provenance | {
                "schema": "crab.cdn-cleanup-trace/v1", "check": "cleanup", "prefix_sha256": reader["prefix_sha256"],
                "observed_at_unix_ms": retirement_time + 30,
                "targets": {"native": {"origin_sha256": reader["origin_sha256"],
                                       "inventories": CLEANUP_INVENTORIES[provider][0]}},
            }
            files = [(args.origin, origin), (args.viewer_requests, viewer), (args.retirement, retirement),
                     (args.native_retired, retired), (args.cleanup, cleanup)]
            for path, record in files:
                json_file(path, record)
            evidence = LIFECYCLE.verify(args)
            self.assertEqual(evidence["records"]["edge"], edge_sha)
            for path, change, error in (
                (args.origin, {"native_published_sha256": "c" * 64}, "same approved workload and native publication"),
                (args.viewer_requests, {"client_report_sha256": "c" * 64}, "differs from the measured fetch"),
                (args.retirement, {"retired_at_unix_ms": 1}, "after rollback"),
                (args.native_retired, {"native_ref": tombstone | {"transaction_id": published["native_ref"]["transaction_id"]}},
                 "distinct tombstone"),
                (args.cleanup, {"observed_at_unix_ms": retirement_time}, "precede fixture cleanup"),
            ):
                with self.subTest(path=path.name), self.assertRaisesRegex(ValueError, error):
                    record = dict(files)[path]
                    json_file(path, record | change)
                    try:
                        LIFECYCLE.verify(args)
                    finally:
                        json_file(path, record)

    def test_cloud_cdn_viewer_logs_require_the_exact_service_lfs_key_and_traffic(self):
        self.check_viewer_key("lfs", "lfs_key_sha256", lfs_object_path("1" * 64), "approved service LFS object")

    def test_cloud_cdn_viewer_logs_join_the_exact_service_git_transition_key(self):
        self.check_viewer_key("git", "transition_key_sha256", "v2/pack-layers/ff/" + "f" * 64,
                              "did not confirm the new export object")

    def check_viewer_key(self, family, fingerprint_field, unrelated_key, error_message):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            access = "public" if family == "git" else "private"
            args = self.prepare(root, "cloud-cdn", "server", family, access=access)
            config = json.loads(args.fixture.read_bytes())
            if family == "git":
                args.export_approval, args.transition_object = root, config["object_path"]
                objects = [SimpleNamespace(path=args.transition_object)]
                with patch.object(SERVICE, "approved_public_export", return_value=("b" * 64, objects, 50)):
                    report = self.request(args, "cloud-cdn", "server", family)
            else:
                report = self.request(args, "cloud-cdn", "server", family)
            tag = f"crab-cdn-qualification/{'0a' * 16}"
            fingerprint = hashlib.sha256(tag.encode()).hexdigest()
            report["edge_responses"] = {fingerprint: report["edge_responses"]["d" * 64]}
            project, rule = "fixture-project", "crab-https"
            prefix = f"{config['base_url']}/{config['fixture_prefix']}"
            viewer = {"logName": f"projects/{project}/logs/requests", "insertId": "viewer",
                      "resource": {"type": "http_load_balancer",
                                   "labels": {"project_id": project, "forwarding_rule_name": rule}},
                      "jsonPayload": {"statusDetails": "response_from_cache"}}
            client, log, output = root / "client.json", root / "gcloud.json", root / "viewer.json"
            command = [sys.executable, "-B", str(Path(COLLECT.__file__)), "--fixture", str(args.fixture),
                       "--configuration-sha256", args.configuration_sha256, "--client", str(client),
                       "--log", str(log), "--output", str(output)]
            for key, extras, error in (
                (unrelated_key, {}, error_message),
                (config["object_path"], {"edge_bytes": report["edge_bytes"] + 1}, "transfer totals disagree with the logged responses"),
                (config["object_path"], {"reader": report["reader"] | {"binding_sha256": "e" * 64}}, "approved native target"),
                (config["object_path"], {}, None),
            ):
                with self.subTest(error=error):
                    request = {"requestMethod": "GET", "requestUrl": f"{prefix}/{key}",
                               "responseSize": "300", "status": 206, "userAgent": tag,
                               "cacheHit": True, "cacheLookup": True}
                    json_file(log, [viewer | {"httpRequest": request}])
                    json_file(client, report | extras)
                    result = subprocess.run(command, capture_output=True, text=True, check=False)
                    if error:
                        self.assertEqual(result.returncode, 1, result.stderr)
                        self.assertIn(error, result.stderr)
                    else:
                        self.assertEqual(result.returncode, 0, result.stderr)
                        proof = json.loads(output.read_bytes())
                        self.assertEqual(proof["records"][fingerprint]["key_sha256"], report[fingerprint_field])


if __name__ == "__main__":
    unittest.main()
