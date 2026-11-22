"""Bind SDK hydration evidence to the child, protected view, xorb, and file."""

import argparse
import json
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

import run_cdn_sdk_hydration as HYDRATE
from cdn_qualification import CLI_BUILD_FIELDS
from run_cdn_sdk_workload import ARM_BOUNDARY
from test_cdn_qualification import COMMIT, json_file, sdk_candidate
from test_cdn_transport_qualification import fixture


class SdkHydrationQualificationTests(unittest.TestCase):
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
                                      source_sha=COMMIT, run_id="123", run_attempt="2", arm="edge")
            candidate = sdk_candidate(scope, HYDRATE.runner_platform())
            report = {"build": {name: candidate[name] for name in CLI_BUILD_FIELDS}, "payload": payload,
                      "started_at_unix_ms": 1001, "finished_at_unix_ms": 1005, "elapsed_ms": 4}
            route = {"provider": "s3", "consumer": "sdk", "access": "private", "family": "xorb", "profile": "direct"}

            def invoke(_command, *, input, env, **_):
                edge = args.arm == "edge"
                request = json.loads(input)
                self.assertEqual(request["worktree"]["hydration"]["pointer"], pointer.decode())
                self.assertEqual(request["binding"]["approval_sha256"], approval if edge else None)
                observations = [("crab_storage::native_read", {"message": "Native object store read",
                                 "provider": "s3", "method": "get", "status": 200, "bytes": 8})]
                if edge or fault == "origin-edge":
                    observations.extend([
                        ("crab_storage::cdn::transfer", route | {"message": "CDN immutable GET", "status": 206,
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
                return subprocess.CompletedProcess([], 0, json.dumps(value).encode(),
                                                   "\n".join(json.dumps(line) for line in lines).encode())

            for arm, fault, reason in (
                ("edge", "", ""), ("origin", "", ""),
                ("edge", "wrong-file", "did not verify the approved file"),
                ("edge", "wrong-key", "did not accept verified chunks"),
                ("edge", "wrong-cache", "did not confirm the approved projected view"),
                ("origin", "origin-edge", "zero edge attempts"),
            ):
                with self.subTest(arm=arm, fault=fault), patch.object(HYDRATE.subprocess, "run", side_effect=invoke), \
                        patch.object(HYDRATE.time, "time_ns", side_effect=(1_000_000_000, 1_010_000_000)):
                    args.arm = arm
                    if not reason:
                        result = HYDRATE.hydrate(args)
                        self.assertEqual((result["payload"], result["verified_edge_payloads"]),
                                         (payload, int(arm == "edge")))
                    else:
                        with self.assertRaisesRegex(ValueError, reason):
                            HYDRATE.hydrate(args)
