"""Bind SDK hydration evidence to the child, protected view, xorb, and file."""

import argparse
import json
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

import run_cdn_sdk_hydration as HYDRATE
from cdn_qualification import CLI_BUILD_FIELDS, SDK_XORB_RANGE_READ, SDK_XORB_RANGE_WORKLOAD, validate_xorb_workload
from run_cdn_sdk_workload import ARM_BOUNDARY, READ_TARGET, json_record
from test_cdn_qualification import COMMIT, cli_candidate, json_file, sdk_candidate
from test_cdn_transport_qualification import fixture


class SdkHydrationQualificationTests(unittest.TestCase):
    def test_native_sdk_range_requires_the_approved_projection_and_sparse_edge_xorb(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            scope = "cloudfront/private/sdk/xorb/direct"
            approval = "6" * 64
            config = fixture(access="private") | {
                "scope": scope, "view_suffix": "/acl-views/v2/" + "2" * 64 + "/1-" + "3" * 64,
                "object_path": ".crab/xorbs/aa/" + "a" * 64, "size": 1024,
                "viewer_issuer": {"endpoint": "https://issuer.example.com/v1/credentials",
                                  "audience": "https://issuer.example.com", "approval_sha256": approval},
            }
            configuration = json_file(root / "fixture", config)
            key = HYDRATE.fixture_object_key(config, "private", "xorb")
            key_sha256 = HYDRATE.digest(key.encode())
            pointer, probe = b"approved pointer", b"approved native SDK probe"
            (root / "pointer").write_bytes(pointer)
            (root / "probe").write_bytes(probe)
            platform = HYDRATE.runner_platform()
            writer = cli_candidate(scope, platform)
            ref = "refs/heads/crab-cdn-qualification-123-2-0123456789ab"
            writer_scope = "cloudfront/private/sdk/git/direct"
            receipt = json_file(root / "receipt", {
                "schema": "crab.cdn-git-publication/v1", "scope": writer_scope, "configuration_sha256": "7" * 64,
                "source_sha": COMMIT, "workflow_run_id": "123", "workflow_run_attempt": "2", "ref": ref,
                "oid": "a" * 40, "sha256": HYDRATE.digest(pointer), "payload_bytes": len(pointer),
                "published": True, "visible_at_unix_ms": 900, "candidate": writer,
            })
            projected = "c" * 40
            payload = {"sha256": "b" * 64, "bytes": 17}
            window = payload | {"offset": 127}
            workload = json_file(root / "workload", {
                "schema": SDK_XORB_RANGE_WORKLOAD, "configuration_sha256": configuration, "projected_oid": projected,
                "pointer_sha256": HYDRATE.digest(pointer), "crab_sha256": writer["binary_sha256"],
                "sdk_sha256": HYDRATE.digest(probe), "global_prefix_sha256": HYDRATE.digest(key.rsplit("/xorbs/", 1)[0].encode()),
                "publication_sha256": receipt, "payload_sha256": "a" * 64, "payload_bytes": 2048, "read_range": window,
            })
            with self.assertRaisesRegex(ValueError, "invalid fields in xorb read workload"):
                validate_xorb_workload((root / "workload").read_bytes(), workload, configuration)
            json_file(root / "reader", {"oidc_issuer": "https://login.example.com",
                                       "oidc_client_id": "crab", "token_cache_directory": str(root / "login")})
            args = argparse.Namespace(fixture=root / "fixture", configuration_sha256=configuration,
                                      workload=root / "workload", workload_sha256=workload, pointer=root / "pointer",
                                      receipt=root / "receipt", git=None, crab=None, probe=root / "probe", private_reader=root / "reader",
                                      source_sha=COMMIT, run_id="123", run_attempt="2", arm="edge", warm_local_output=None)

            def invoke(_command, *, input, env, **_):
                edge = args.arm == "edge"
                request = json.loads(input)
                self.assertNotIn("worktree", request)
                self.assertEqual(request["branch"], ref.removeprefix("refs/heads/"))
                self.assertEqual((request["expected_oid"], request["expected_bytes"], request["expected_sha256"]),
                                 (projected, payload["bytes"], payload["sha256"]))
                self.assertEqual(request["binding"]["approval_sha256"], approval if edge else None)
                self.assertEqual(request["xorb_range"], {"offset": window["offset"], "file_bytes": 2048,
                                 "pointer_sha256": HYDRATE.digest(pointer), "cache_root": env["CRAB_CACHE_DIR"]})
                route = {"provider": "s3", "consumer": "sdk", "access": "private", "family": "xorb", "profile": "direct"}
                physical = config["fixture_prefix"] + config["view_suffix"] + "/"
                observations = [(READ_TARGET, {"message": "SDK captured CDN read target", "public_export_sha256": "",
                                 "origin_sha256": HYDRATE.digest(config["native_origin"].encode()),
                                 "prefix_sha256": HYDRATE.digest(physical.encode()),
                                 "binding_sha256": HYDRATE.viewer_binding_sha256(config) if edge else ""}),
                                ("crab_storage::native_read", {"message": "Native object store read", "provider": "s3",
                                 "method": "get", "status": 200, "bytes": 32})]
                if edge:
                    whole = fault == "whole"
                    observations.extend([
                        ("crab_storage::cdn::transfer", route | {"message": "CDN immutable GET", "status": 200 if whole else 206,
                         "range_start": 0, "object_bytes": 1024, "requested_bytes": 1024 if whole else 64,
                         "bytes": 1024 if whole else 64,
                         "cache": "hit", "range": not whole, "complete": True, "elapsed_ms": 1, "request_sha256": "d" * 64}),
                        ("crab_storage::cdn::transfer", route | {"message": "CDN verified xorb payload", "source": "edge",
                         "xorb_key_sha256": key_sha256}),
                    ])
                if fault == "restart":
                    observations.append(("crab_storage::cdn::transfer", route | {"message": "CDN verified xorb payload",
                                         "source": "origin", "xorb_key_sha256": key_sha256}))
                observations.append((ARM_BOUNDARY, {"phase": "primary_finish"}))
                build = sdk_candidate(scope, platform)
                report = {"build": {name: build[name] for name in CLI_BUILD_FIELDS}, "payload": payload,
                          "commit": "e" * 40 if fault == "commit" else projected,
                          "range_offset": 128 if fault == "offset" else window["offset"],
                          "cache_root_sha256": HYDRATE.digest(env["CRAB_CACHE_DIR"].encode()),
                          "started_at_unix_ms": 1001, "finished_at_unix_ms": 1005, "elapsed_ms": 4, "warm_local": None}
                stderr = b"\n".join(json.dumps({"target": target, "fields": data}).encode() for target, data in observations)
                return subprocess.CompletedProcess([], 0, json.dumps(report).encode(), stderr)

            for fault, reason in (("", ""), ("whole", "ranges totaling less"), ("restart", "outside its approved edge cache"),
                                  ("commit", "approved SDK executable and projected commit"), ("offset", "approved file offset")):
                with self.subTest(fault=fault), patch.object(HYDRATE.subprocess, "run", side_effect=invoke), \
                        patch.object(HYDRATE.time, "time_ns", side_effect=(1_000_000_000, 1_010_000_000)):
                    if reason:
                        with self.assertRaisesRegex(ValueError, reason):
                            HYDRATE.hydrate(args)
                    else:
                        record, local = HYDRATE.hydrate(args)
                        self.assertEqual((record["schema"], record["payload"], record["edge_bytes"]), (SDK_XORB_RANGE_READ, payload, 64))
                        self.assertIsNone(local)
            fault = ""
            args.arm = "origin"
            with patch.object(HYDRATE.subprocess, "run", side_effect=invoke), \
                    patch.object(HYDRATE.time, "time_ns", side_effect=(1_000_000_000, 1_010_000_000)):
                origin, _ = HYDRATE.hydrate(args)
            self.assertEqual((origin["edge_bytes"], origin["native_traffic"]["body_bytes"]), (0, 32))

    def test_ordinary_workload_requires_the_approved_child_and_hydrated_file(self):
        with TemporaryDirectory() as directory:
            root = Path(directory)
            scope = "cloudfront/private/sdk/xorb/direct"
            approval = "6" * 64
            config = fixture(access="private") | {
                "scope": scope, "view_suffix": "/acl-views/v2/" + "2" * 64 + "/1-" + "3" * 64,
                "object_path": ".crab/xorbs/aa/" + "a" * 64,
                "viewer_issuer": {"endpoint": "https://issuer.example.com/v1/credentials",
                                  "audience": "https://issuer.example.com", "approval_sha256": approval},
            }
            config_sha = json_file(root / "fixture.json", config)
            key = HYDRATE.fixture_object_key(config, "private", "xorb")
            payload = {"sha256": "b" * 64, "bytes": 100}
            pointer, crab = b"fixture pointer", b"approved Crab"
            (root / "pointer").write_bytes(pointer)
            (root / "crab").write_bytes(crab)
            (root / "probe").write_bytes(b"SDK probe")
            json_file(root / "reader", {"oidc_issuer": "https://login.example.com",
                                       "oidc_client_id": "crab", "token_cache_directory": str(root / "login")})
            workload = json_file(root / "workload.json", {
                "schema": "crab.cdn-sdk-hydration-workload/v1", "configuration_sha256": config_sha,
                "pointer_sha256": HYDRATE.digest(pointer), "crab_sha256": HYDRATE.digest(crab),
                "global_prefix_sha256": HYDRATE.digest(key.rsplit("/xorbs/", 1)[0].encode()),
                "publication_sha256": "7" * 64,
                "payload_sha256": payload["sha256"], "payload_bytes": payload["bytes"],
            })
            args = argparse.Namespace(fixture=root / "fixture.json", configuration_sha256=config_sha,
                                      workload=root / "workload.json", workload_sha256=workload, pointer=root / "pointer",
                                      git=root / "crab", crab=root / "crab", probe=root / "probe", private_reader=root / "reader",
                                      source_sha=COMMIT, run_id="123", run_attempt="2", arm="edge", warm_local_output=None,
                                      receipt=None)
            candidate = sdk_candidate(scope, HYDRATE.runner_platform())
            report = {"build": {name: candidate[name] for name in CLI_BUILD_FIELDS}, "payload": payload,
                      "started_at_unix_ms": 1001, "finished_at_unix_ms": 1005, "elapsed_ms": 4, "warm_local": None}
            route = {"provider": "s3", "consumer": "sdk", "access": "private", "family": "xorb", "profile": "direct"}

            def invoke(_command, *, input, env, **_):
                edge = args.arm == "edge"
                repeat = args.warm_local_output is not None
                request = json.loads(input)
                self.assertEqual(request["worktree"]["hydration"]["pointer"], pointer.decode())
                self.assertEqual(request["worktree"]["hydration"]["repeat"], repeat)
                self.assertEqual(request["binding"]["approval_sha256"], approval if edge else None)
                observations = [("crab_storage::native_read", {"message": "Native object store read",
                                 "provider": "s3", "method": "get", "status": 200, "bytes": 8})]
                if edge or fault == "origin-edge":
                    observations.extend([
                        ("crab_storage::cdn::transfer", route | {"message": "CDN immutable GET", "status": 206,
                         "range_start": 0, "object_bytes": 1024, "requested_bytes": 64,
                         "bytes": 64, "cache": "hit", "range": True, "complete": True, "elapsed_ms": 1,
                         "request_sha256": "d" * 64}),
                        ("crab_storage::cdn::transfer", route | {"message": "CDN verified xorb payload",
                         "source": "edge", "xorb_key_sha256": "d" * 64 if fault == "wrong-key" else HYDRATE.digest(key.encode())}),
                    ])
                view = config["fixture_prefix"] + config["view_suffix"]
                observations.append((HYDRATE.TARGET, {
                    "message": "CDN child captured read target", "origin_sha256": HYDRATE.digest(config["native_origin"].encode()),
                    "prefix_sha256": HYDRATE.digest((view + "/").encode()), "global_prefix_sha256": HYDRATE.digest((view + "/.crab").encode()),
                    "cache_root_sha256": "f" * 64 if fault == "wrong-cache" else HYDRATE.digest(env["CRAB_CACHE_DIR"].encode()),
                    "binding_sha256": HYDRATE.viewer_binding_sha256(config) if edge else "",
                    "approval_sha256": approval if edge else "", "local_cache_only": True,
                }))
                lines = [{"target": HYDRATE.CHILD, "fields": {"observation": json.dumps({"target": target, "fields": data})}}
                         for target, data in observations]
                lines.append({"target": ARM_BOUNDARY, "fields": {"phase": "primary_finish"}})
                value = report | ({"payload": payload | {"bytes": 99}} if fault == "wrong-file" else {})
                if repeat:
                    value["warm_local"] = {"started_at_unix_ms": 1006, "finished_at_unix_ms": 1009, "elapsed_ms": 3}
                    lines.append({"target": ARM_BOUNDARY, "fields": {"phase": "local_start"}})
                    lines.extend({"target": HYDRATE.CHILD, "fields": {"observation": json.dumps({"target": target, "fields": data})}}
                                 for target, data in [observations[0], observations[-1], ("crab_storage::cdn::transfer", route | {
                                     "message": "CDN verified xorb payload", "source": "origin" if fault == "local-origin" else "local",
                                     "xorb_key_sha256": HYDRATE.digest(key.encode()),
                                 })])
                    lines.append({"target": ARM_BOUNDARY, "fields": {"phase": "local_finish"}})
                return subprocess.CompletedProcess([], 0, json.dumps(value).encode(),
                                                   "\n".join(json.dumps(line) for line in lines).encode())

            for arm, fault, reason in (
                ("edge", "", ""), ("origin", "", ""),
                ("edge", "wrong-file", "did not verify the approved file"),
                ("edge", "wrong-key", "did not accept verified chunks"),
                ("edge", "wrong-cache", "did not confirm the approved projected view"),
                ("origin", "origin-edge", "zero edge attempts"),
                ("edge", "local", ""),
                ("edge", "local-origin", "outside its approved local cache"),
                ("origin", "local", "requires its own edge primer"),
            ):
                with self.subTest(arm=arm, fault=fault), patch.object(HYDRATE.subprocess, "run", side_effect=invoke), \
                        patch.object(HYDRATE.time, "time_ns", side_effect=(1_000_000_000, 1_010_000_000)):
                    args.arm = arm
                    args.warm_local_output = root / "warm-local.json" if fault.startswith("local") else None
                    if not reason:
                        result, local = HYDRATE.hydrate(args)
                        self.assertEqual((result["payload"], result["verified_edge_payloads"]),
                                         (payload, int(arm == "edge")))
                        if fault == "local":
                            self.assertEqual(local["primer_sha256"], HYDRATE.digest(json_record(result).encode()))
                            self.assertEqual(local["reader"], result["reader"])
                            self.assertEqual(local["local_payload_hits"], 1)
                        else:
                            self.assertIsNone(local)
                    else:
                        with self.assertRaisesRegex(ValueError, reason):
                            HYDRATE.hydrate(args)
