"""Exercise the Git qualification phases with a local remote-helper fixture."""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import sys
import time
import unittest
from pathlib import Path
from shutil import copytree
from tempfile import TemporaryDirectory

import run_cdn_git_workload as WORKLOAD
from cdn_qualification import CLI_BUILD_FIELDS, GIT_FETCH_FIELDS, collect_git_product_action, lfs_object_path, verify_public_transition
from public_inventory import Object, SCHEMA, publication_receipt, read_publication
from test_cdn_transport_qualification import PACK, fixture
from test_cdn_qualification import cli_candidate, write_product_viewer_proof


@unittest.skipUnless(os.name == "posix", "the local fixture helper uses a Python shebang")
class CdnGitWorkloadQualificationTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = TemporaryDirectory(prefix="crab-cdn-git-tests-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.origin = self.root / "origin.git"
        subprocess.run(["git", "init", "--bare", "--template=", "--object-format=sha1", str(self.origin)],
                       check=True, capture_output=True)
        self.receipt = self.root / "receipt.json"
        self.fixture = self.root / "fixture.json"
        approved = fixture(access="private")
        raw = json.dumps(approved).encode()
        self.fixture.write_bytes(raw)
        self.digest = hashlib.sha256(raw).hexdigest()
        self.helper = self.root / "git-remote-crab"
        self.build_event = self.root / "build-event.json"
        build = cli_candidate("cloudfront/private/cli/git/direct", WORKLOAD.runner_platform())
        self.build_event.write_text(json.dumps({
            "target": WORKLOAD.QUALIFICATION_LOG, "fields": {key: build[key] for key in CLI_BUILD_FIELDS}
            | {
                "message": WORKLOAD.QUALIFICATION_MESSAGE,
                "origin_sha256": hashlib.sha256(approved["native_origin"].encode()).hexdigest(),
                "prefix_sha256": hashlib.sha256((approved["fixture_prefix"] + "/").encode()).hexdigest(),
            },
        }))
        # Git still creates real packed objects and ref leases. The shim directs
        # its wire protocol to a disposable local origin and supplies trace input.
        self.helper.write_text(
            f"#!{sys.executable}\n"
            "import hashlib, json, os, subprocess, sys\n"
            "from pathlib import Path\n"
            "origin = Path(__file__).parent / 'origin.git'\n"
            "approved = json.loads((origin.parent / 'fixture.json').read_bytes())\n"
            "target = json.loads(os.environ['CRAB_INTERNAL_CDN_QUALIFICATION_TARGET'])\n"
            "target.pop('read', None)\n"
            "managed_url = target.pop('managed_url', None)\n"
            "assert target == {'origin':approved['native_origin'], 'repository_prefix':approved['fixture_prefix']}\n"
            "if managed_url is not None:\n"
            "    assert sys.argv[2] == managed_url == approved['managed_url']\n"
            "evidence = origin.parent / 'public-export-events.json'\n"
            "def emit_read():\n"
            "    binding = approved['scope'].split('/')[1] + '\\n' + approved['base_url'].rstrip('/')\n"
            "    binding = hashlib.sha256(binding.encode()).hexdigest()\n"
            "    if (origin.parent / 'binding-off').exists():\n"
            "        binding = ''\n"
            "    cache = hashlib.sha256(os.environ['CRAB_CACHE_DIR'].encode()).hexdigest()\n"
            "    approved_export = origin.parent / 'active-export-sha256'\n"
            "    export = approved_export.read_text() if binding and approved_export.exists() else ''\n"
            "    print(json.dumps({'target':'crab::cdn::qualification_read', 'fields':{\n"
            "        'message':'CDN qualification verified read configuration',\n"
            "        'binding_sha256':binding, 'cache_root_sha256':cache, 'public_export_sha256':export}}),\n"
            "        file=sys.stderr, flush=True)\n"
            "def emit_native():\n"
            "    print(json.dumps({'target':'crab_storage::native_read', 'fields':{\n"
            "        'message':'Native object store read', 'provider':'s3', 'method':'get',\n"
            "        'status':200, 'bytes':6}}), file=sys.stderr, flush=True)\n"
            "if Path(sys.argv[0]).name == 'crab':\n"
            "    if sys.argv[1:3] == ['lfs', 'clean']:\n"
            "        data = sys.stdin.buffer.read()\n"
            "        digest = hashlib.sha256(data).hexdigest()\n"
            "        (origin / digest).write_bytes(data)\n"
            "        print(f'version https://git-lfs.github.com/spec/v1\\noid sha256:{digest}\\nsize {len(data)}')\n"
            "        sys.exit(0)\n"
            "    assert sys.argv[1:4] == ['lfs', 'fetch', 'origin']\n"
            "    pointer = subprocess.check_output(['git', 'cat-file', 'blob', sys.argv[4] + ':cdn-qualification.bin'])\n"
            "    digest = pointer.splitlines()[1].split(b':')[1].decode()\n"
            "    payload = Path('.git/lfs/objects') / digest[:2] / digest[2:4] / digest\n"
            "    key = approved['fixture_prefix'] + '/' + payload.relative_to('.git').as_posix()\n"
            "    key_digest = hashlib.sha256(key.encode()).hexdigest()\n"
            "    print((origin.parent / 'build-event.json').read_text(), file=sys.stderr)\n"
            "    emit_read()\n"
            "    emit_native()\n"
            "    policy = {'provider':'s3', 'access': approved['scope'].split('/')[1],\n"
            "              'consumer':'cli', 'family':'lfs', 'profile':'direct'}\n"
            "    if payload.exists():\n"
            "        assert hashlib.sha256(payload.read_bytes()).hexdigest() == digest\n"
            "        if not (origin.parent / 'unverified-local').exists():\n"
            "            print(json.dumps({'target':'crab_storage::cdn::transfer', 'fields': policy | {\n"
            "                'message':'CDN verified LFS payload', 'source':'local', 'lfs_key_sha256':key_digest}}), file=sys.stderr)\n"
            "        sys.exit(0)\n"
            "    payload.parent.mkdir(parents=True)\n"
            "    payload.write_bytes((origin / digest).read_bytes())\n"
            "    if evidence.exists():\n"
            "        for event in json.loads(evidence.read_bytes()):\n"
            "            print(json.dumps(event), file=sys.stderr)\n"
            "    if not (origin.parent / 'origin-only').exists():\n"
            "        print(json.dumps({'target':'crab_storage::cdn::transfer', 'fields': policy | {\n"
            "            'message':'CDN immutable GET', 'status':206, 'cache':'hit', 'range':True,\n"
            "            'bytes':payload.stat().st_size, 'complete':True, 'elapsed_ms':10, 'request_sha256':'e' * 64}}), file=sys.stderr)\n"
            "        if not (origin.parent / 'rejected-edge').exists():\n"
            "            print(json.dumps({'target':'crab_storage::cdn::transfer', 'fields': policy | {\n"
            "                'message':'CDN verified LFS payload', 'source':'edge', 'lfs_key_sha256':key_digest}}), file=sys.stderr)\n"
            "    sys.exit(0)\n"
            "for line in sys.stdin.buffer:\n"
            "    if line == b'capabilities\\n':\n"
            "        print('connect\\n', flush=True)\n"
            "    elif line.startswith(b'connect '):\n"
            "        command = line.split()[1].decode()\n"
            "        if managed_url is not None:\n"
            "            assert command == 'git-upload-pack'\n"
            "            managed = json.loads((origin.parent / 'managed-events.json').read_bytes())\n"
            "            for event in managed:\n"
            "                print(json.dumps(event), file=sys.stderr, flush=True)\n"
            "            if not managed[-1]['fields']['accepted']:\n"
            "                sys.exit(1)\n"
            "        print(flush=True)\n"
            "        if not (origin.parent / 'legacy-helper').exists():\n"
            "            print((origin.parent / 'build-event.json').read_text(), file=sys.stderr, flush=True)\n"
            "        if command == 'git-upload-pack':\n"
            "            emit_read()\n"
            "            emit_native()\n"
            "        if command == 'git-upload-pack' and evidence.exists():\n"
            "            for event in json.loads(evidence.read_bytes()):\n"
            "                print(json.dumps(event), file=sys.stderr, flush=True)\n"
            "        if command == 'git-upload-pack' and not (origin.parent / 'origin-only').exists():\n"
            "            cache = Path(os.environ['CRAB_CACHE_DIR'])\n"
            "            family = 'capsule' if (origin.parent / 'foreign-edge').exists() else 'git'\n"
            "            local = (origin.parent / 'warm-cache').exists() and (cache / 'seed').exists()\n"
            "            policy = {'provider':'s3', 'access':'public' if evidence.exists() else 'private', 'consumer':'cli',\n"
            "                      'family':family, 'profile':'direct' if managed_url is None else 'managed'}\n"
            "            if not local:\n"
            "                print(json.dumps({'target':'crab_storage::cdn::transfer', 'fields': policy | {\n"
            "                    'message':'CDN immutable GET', 'status':206, 'cache':'hit', 'range':True,\n"
            "                    'bytes':1024, 'complete':True, 'elapsed_ms':10, 'request_sha256':'e' * 64}}), file=sys.stderr, flush=True)\n"
            "            if not (origin.parent / 'rejected-edge').exists():\n"
            "                print(json.dumps({'target':'crab_storage::cdn::transfer', 'fields': policy | {\n"
            "                    'message':'CDN verified Git payload', 'source':'local' if local else 'edge'}}),\n"
            "                    file=sys.stderr, flush=True)\n"
            "                cache.mkdir(parents=True, exist_ok=True)\n"
            "                (cache / 'seed').touch()\n"
            "        os.execlp(command, command, str(origin))\n",
            encoding="utf-8",
        )
        self.helper.chmod(0o700)
        (self.root / "crab").symlink_to(self.helper.name)

    def call(self, action: str, *arguments: str, digest: str | None = None) -> subprocess.CompletedProcess[str]:
        receipt = [] if action == "inspect" else ["--receipt", str(self.receipt)]
        return subprocess.run([
            sys.executable, "-B", str(Path(WORKLOAD.__file__)), action,
            "--fixture", str(self.fixture), "--configuration-sha256", digest or self.digest,
            *receipt, "--scratch", str(self.root), "--helper", str(self.helper),
            "--source-sha", "a" * 40, "--run-id", "123", "--run-attempt", "2",
            *arguments,
        ], capture_output=True, text=True, check=False)

    def run_phase(self, action: str, *arguments: str) -> dict:
        result = self.call(action, *arguments)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def test_publishes_fetches_and_retires_the_dedicated_ref(self) -> None:
        inspected = self.run_phase("inspect")
        self.assertFalse(self.receipt.exists())
        published = self.run_phase("publish")
        self.assertTrue(published["visible_new_ref"])
        self.assertEqual(published["candidate"], inspected["candidate"])
        self.assertFalse(published["candidate"]["qualification_feature"])
        self.assertEqual(published["candidate"]["binary_sha256"], hashlib.sha256(self.helper.read_bytes()).hexdigest())
        fetched = self.run_phase("fetch")
        self.assertEqual(fetched.keys(), GIT_FETCH_FIELDS)
        self.assertEqual(fetched["observed_sha256"], fetched["expected_sha256"])
        self.assertEqual(fetched["publication_oid"], json.loads(self.receipt.read_bytes())["oid"])
        self.assertLessEqual(fetched["started_at_unix_ms"], fetched["finished_at_unix_ms"])
        self.assertEqual(fetched["observed_bytes"], WORKLOAD.GIT_PAYLOAD_SIZE)
        self.assertEqual(fetched["observed_edge_gets"], 1)
        self.assertEqual(fetched["verified_edge_payloads"], 1)
        self.assertNotIn("fixture_cleanup_verified", fetched)
        (self.root / "binding-off").touch()
        (self.root / "origin-only").touch()
        rollback = self.run_phase("fetch-origin")
        retired = self.run_phase("retire")
        self.assertTrue(retired["ref_retired"])
        records = self.root / "git-product"
        records.mkdir()
        (records / "publication.json").write_bytes(self.receipt.read_bytes())
        fetch_path = records / "fetch.json"
        fetch_path.write_text(json.dumps(fetched), encoding="utf-8")
        write_product_viewer_proof(fetch_path)
        (records / "retirement.json").write_text(json.dumps(retired), encoding="utf-8")
        viewer = WORKLOAD.viewer_binding_sha256(json.loads(self.fixture.read_bytes()))
        rollback_raw = json.dumps(rollback).encode()
        measured = collect_git_product_action(records, fetched | {"viewer_binding_sha256": viewer}, rollback_raw)
        with self.assertRaisesRegex(ValueError, "product fetch did not use the registered viewer binding"):
            collect_git_product_action(records, fetched | {"viewer_binding_sha256": "0" * 64}, rollback_raw)
        self.assertEqual(measured["observed_sha256"], fetched["expected_sha256"])
        self.assertEqual(measured["verified_edge_payloads"], 1)

    def test_managed_fetch_observes_the_grant_for_each_product_arm(self) -> None:
        config = json.loads(self.fixture.read_bytes())
        config.update(scope="cloudfront/private/cli/git/managed", managed_url="crab://issuer.example/org/models")
        raw = json.dumps(config).encode()
        self.fixture.write_bytes(raw)
        self.digest = hashlib.sha256(raw).hexdigest()
        self.assertTrue(self.run_phase("publish")["visible_new_ref"])
        grant_events = self.root / "managed-events.json"
        identity = {"issuer_identity_sha256": "a" * 64, "issuer_version": "1.2.3", "repository_sha256": "c" * 64}
        origin, prefix, viewer = (hashlib.sha256(config[key].encode()).hexdigest()
                                  for key in ("native_origin", "fixture_prefix", "base_url"))
        approval = "b" * 64
        for role, action, status, version, accepted, grant_approval in (
            ("approved_v2", "fetch", 201, 2, True, approval),
            ("v1_peer", "fetch-origin", 201, 1, True, ""),
            ("denied_v2", "fetch-rejected", 403, 2, False, ""),
            ("mismatched_v2", "fetch-rejected", 201, 2, False, "d" * 64),
        ):
            with self.subTest(role=role):
                (self.root / "origin-only").unlink(missing_ok=True)
                (self.root / "binding-off").unlink(missing_ok=True)
                if version == 1:
                    (self.root / "origin-only").touch()
                    (self.root / "binding-off").touch()
                grant_events.write_text(json.dumps([
                    {"target": WORKLOAD.MANAGED_TARGET, "fields": identity | {
                        "message": "Managed CDN repository resolution", "binding_approval_sha256": approval,
                        "binding_origin_sha256": origin, "binding_prefix_sha256": prefix, "binding_viewer_sha256": viewer,
                    }},
                    {"target": WORKLOAD.MANAGED_TARGET, "fields": identity | {
                        "message": "Managed CDN read grant exchange", "operation": "Fetch", "request_version": version,
                        "status": status, "grant_version": version if status == 201 else 0, "accepted": accepted,
                        "request_approval_sha256": approval if version == 2 else "", "grant_approval_sha256": grant_approval,
                        "grant_origin_sha256": origin if accepted else "", "grant_prefix_sha256": prefix if accepted else "",
                    }},
                ]))
                directory = self.root / role
                result = self.run_phase(action, "--managed-issuer-capture", str(directory))
                self.assertEqual((directory / "payload").exists(), accepted)
                if accepted:
                    self.assertEqual(hashlib.sha256((directory / "payload").read_bytes()).hexdigest(), result["observed_sha256"])
                else:
                    self.assertTrue(result["rejected"])
                self.assertEqual(json.loads((directory / "client.json").read_bytes()), result)
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_managed_workload_rejects_a_locator_with_an_unapproved_selector(self) -> None:
        config = json.loads(self.fixture.read_bytes())
        config.update(scope="cloudfront/private/cli/git/managed", managed_url="crab://issuer.example/org/models?view=other")
        raw = json.dumps(config).encode()
        self.fixture.write_bytes(raw)
        self.assertIn("exact repository-local logical URL", self.call("publish", digest=hashlib.sha256(raw).hexdigest()).stderr)
        self.assertFalse(self.receipt.exists())

    def test_rejects_a_different_compiled_source_before_writing(self) -> None:
        event = json.loads(self.build_event.read_bytes())
        event["fields"]["source_sha"] = "f" * 40
        self.build_event.write_text(json.dumps(event))
        rejected = self.call("publish")
        self.assertEqual(rejected.returncode, 1, rejected.stdout)
        self.assertIn("differs from the approved source", rejected.stderr)
        self.assertFalse(self.receipt.exists())

    def test_sdk_hydration_publishes_the_exact_approved_pointer(self) -> None:
        config = json.loads(self.fixture.read_bytes())
        config.update(scope="cloudfront/private/sdk/git/direct")
        raw = json.dumps(config).encode()
        self.fixture.write_bytes(raw)
        self.digest = hashlib.sha256(raw).hexdigest()
        pointer = self.root / "pointer"
        content = f"version https://crab.build/spec/v1\nfile-hash {'a' * 64}\nsize 1024\n".encode()
        pointer.write_bytes(content)
        args = ("--hydration-pointer", str(pointer), "--hydration-pointer-sha256", hashlib.sha256(content).hexdigest())
        pointer.write_bytes(content + b"changed")
        self.assertIn("pointer differs", self.call("publish", *args).stderr)
        self.assertFalse(self.receipt.exists())
        pointer.write_bytes(content)
        self.assertTrue(self.run_phase("publish", *args)["visible_new_ref"])
        published = json.loads(self.receipt.read_bytes())
        observed = subprocess.check_output(["git", "--git-dir", str(self.origin), "cat-file", "blob",
                                            published["oid"] + ":cdn-qualification.bin"])
        self.assertEqual(observed, content)
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_rejects_the_same_origin_with_a_neighbor_prefix_before_writing(self) -> None:
        event = json.loads(self.build_event.read_bytes())
        event["fields"]["prefix_sha256"] = hashlib.sha256(b"crab-cdn-qualification/neighbor/").hexdigest()
        self.build_event.write_text(json.dumps(event))
        rejected = self.call("publish")
        self.assertEqual(rejected.returncode, 1, rejected.stdout)
        self.assertIn("did not confirm the approved native origin and repository prefix", rejected.stderr)
        self.assertFalse(self.receipt.exists())

    def test_retirement_rejects_a_replaced_helper(self) -> None:
        self.run_phase("publish")
        raw = self.helper.read_bytes()
        try:
            self.helper.write_bytes(raw + b"\n")
            rejected = self.call("retire")
            self.assertEqual(rejected.returncode, 1, rejected.stdout)
            self.assertIn("differs from the native publication binary", rejected.stderr)
        finally:
            self.helper.write_bytes(raw)
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_native_collector_observes_only_the_approved_provider_and_ref(self) -> None:
        self.verify_native_ref_observation()

    def verify_native_ref_observation(self) -> None:
        self.assertTrue(self.run_phase("publish")["visible_new_ref"])
        publication = json.loads(self.receipt.read_bytes())
        configuration = json.loads(self.fixture.read_bytes())
        self.assertEqual(publication["scope"], configuration["scope"])
        if "/sdk/" in publication["scope"]:
            self.assertNotEqual(self.call("fetch").returncode, 0)

        def digest(value: str) -> str:
            return hashlib.sha256(value.encode()).hexdigest()

        body = {
            "origin_sha256": digest(configuration["native_origin"]),
            "prefix_sha256": digest(configuration["fixture_prefix"] + "/"),
            "ref_sha256": digest(publication["ref"]), "probe_source_sha": publication["source_sha"],
            "oid": publication["oid"], "repository_sha256": "1" * 64, "epoch_sha256": "2" * 64,
            "root_digest": "3" * 64, "head_sha256": "4" * 64, "revision_sha256": "5" * 64, "transaction_id": "6" * 64,
        }
        probe = self.root / "native-probe"
        output = self.root / "observation.json"
        executor = Path(WORKLOAD.__file__).with_name("collect_cdn_git_native_ref.py")
        command = [sys.executable, "-B", str(executor), "published", "--fixture", str(self.fixture),
                   "--configuration-sha256", self.digest, "--publication", str(self.receipt),
                   "--output", str(output), "--probe", str(probe), "--source-sha", publication["source_sha"],
                   "--run-id", "123", "--run-attempt", "2"]
        for identity in ("f" * 64, body["origin_sha256"]):
            probe.write_text(f"#!{sys.executable}\nimport sys\n"
                             f"assert sys.argv[1:] == {[configuration['native_origin'], configuration['fixture_prefix'], publication['ref']]!r}\n"
                             f"print({json.dumps(body | {'origin_sha256': identity})!r})\n")
            probe.chmod(0o700)
            result = subprocess.run(command, capture_output=True, text=True, check=False)
            if identity != body["origin_sha256"]:
                self.assertEqual(result.returncode, 1)
                self.assertFalse(output.exists())
            else:
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(json.loads(output.read_bytes())["native_ref"], body)
        self.assertTrue(self.run_phase("retire")["ref_retired"])
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_a_byte_identical_fetch_from_origin_only_does_not_qualify(self) -> None:
        self.run_phase("publish")
        (self.root / "origin-only").touch()
        fetched = self.call("fetch")
        self.assertEqual(fetched.returncode, 1, fetched.stdout)
        self.assertIn("no completed edge GET", fetched.stderr)
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_public_transition_checks_the_approval_and_exact_key_in_both_git_actions(self) -> None:
        self.verify_public_transition("git")

    def test_public_lfs_transition_requires_verified_bytes_from_the_exact_export(self) -> None:
        self.verify_public_transition("lfs")

    def verify_public_transition(self, family: str) -> None:
        sample = fixture(access="public") | {"base_url": "https://fixture.example.com"}
        prior = Object("v2/pack-layers/bb/" + "b" * 64, 6, "c" * 64)
        if family == "lfs":
            prior = Object(lfs_object_path("b" * 64), 6, "b" * 64)
            sample |= {"scope": "cloudfront/public/cli/lfs/direct", "object_path": prior.path, "sha256": prior.sha256}
        raw = json.dumps(sample).encode()
        self.fixture.write_bytes(raw)
        self.digest = hashlib.sha256(raw).hexdigest()
        event_path = self.root / "public-export-events.json"

        def seal(directory: Path, objects: list[Object]) -> str:
            directory.mkdir()
            inventory = json.dumps({"schema": SCHEMA, "source_origin": sample["native_origin"],
                                    "source_placement": sample["source_placement"], "repository_prefix": sample["fixture_prefix"],
                                    "objects": [vars(obj) for obj in objects]}).encode()
            (directory / "inventory.json").write_bytes(inventory)
            receipt = publication_receipt(
                sample["native_origin"], sample["export_origin"], sample["source_placement"],
                {"resource": "accounts/123456789012/buckets/export-bucket", "location": "us-east-1"},
                sample["fixture_prefix"], sample["base_url"], "fixture-deployment", hashlib.sha256(inventory).hexdigest(), objects,
            ) | {"sealed_at_unix_ms": time.time_ns() // 1_000_000}
            document = json.dumps(receipt).encode()
            (directory / "publication.json").write_bytes(document)
            return hashlib.sha256(document).hexdigest()

        def observe(approval: str, *kinds: str) -> None:
            event_path.write_text(json.dumps([
                {"target": WORKLOAD.PUBLIC_TRANSITION_TARGET, "fields": {
                    "message": "CDN public export observation", "provider": "s3", "access": "public",
                    "consumer": "cli", "family": family, "profile": "direct", "key_sha256": key,
                    "approval_sha256": approval, "observation": kind,
                }} for kind in kinds
            ]))

        old, new = self.root / "previous", self.root / "current"
        previous = seal(old, [prior])
        active_export = self.root / "active-export-sha256"
        active_export.write_text(previous)
        self.run_phase("publish")
        payload = json.loads(self.receipt.read_bytes())
        if family == "lfs":
            added = Object(lfs_object_path(payload["sha256"]), WORKLOAD.GIT_PAYLOAD_SIZE, payload["sha256"])
            selector = ()
        else:
            added = Object(PACK, 6, "d" * 64)
            selector = ("--transition-object", PACK)
        key = hashlib.sha256(f"{sample['fixture_prefix']}/{added.path}".encode()).hexdigest()
        before, after = self.root / "before.json", self.root / "after.json"
        (self.root / "origin-only").touch()
        observe("0" * 64, "unlisted")
        flags = (*selector, "--export-approval", str(old), "--transition-output", str(before))
        rejected = self.call("fetch-origin", *flags)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("different public export receipt", rejected.stderr)
        self.assertFalse(before.exists())
        observe(previous, "unlisted")
        result = self.call("fetch-origin", *flags)
        self.assertEqual(result.returncode, 0, result.stderr)

        (self.root / "origin-only").unlink()
        current = seal(new, [prior, added])
        active_export.write_text(current)
        observe(current, "listed", "verified_edge")
        flags = (*selector, "--export-approval", str(new), "--transition-output", str(after), "--before-transition", str(before))
        if family == "lfs":
            (self.root / "rejected-edge").touch()
            rejected = self.call("fetch", *flags)
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("did not accept a verified edge payload", rejected.stderr)
            self.assertFalse(after.exists())
            (self.root / "rejected-edge").unlink()
        result = self.call("fetch", *flags)
        self.assertEqual(result.returncode, 0, result.stderr)
        fetched = json.loads(result.stdout)
        self.assertEqual(set(fetched), GIT_FETCH_FIELDS | {"transition_key_sha256"})
        self.assertEqual(fetched["transition_key_sha256"], key)
        self.assertEqual(fetched["name"], f"{family}-fetch")
        retained = json.loads(after.read_bytes())
        self.assertEqual(retained["before"]["receipt_sha256"], previous)
        self.assertEqual(retained["after"]["receipt_sha256"], current)
        self.assertEqual(retained["after"]["verified_edge_key_sha256"], key)
        self.assertEqual(retained["before"]["result_sha256"], fetched["observed_sha256"])
        self.assertEqual(retained["before"]["edge_attempts"], 0)
        if family == "git":
            missing = self.call("fetch")
            self.assertIn("an export approval is required", missing.stderr)
            active_export.write_text(previous)
            stale = self.call("fetch", "--export-approval", str(new))
            self.assertIn("different CDN public export approval", stale.stderr)
            active_export.write_text(current)
            edge = self.run_phase("fetch", "--export-approval", str(new))
            self.assertEqual(edge["reader"]["public_export_sha256"], current)
            (self.root / "warm-cache").touch()
            warm = self.run_phase("fetch-warm-local", "--export-approval", str(new))
            self.assertEqual(warm["reader"]["public_export_sha256"], current)
        (self.root / "binding-off").touch()
        (self.root / "origin-only").touch()
        rollback = self.run_phase("fetch-origin")
        retired = self.run_phase("retire")
        self.assertTrue(retired["ref_retired"])
        if family == "lfs":
            records = self.root / "git-product"
            records.mkdir()
            (records / "publication.json").write_bytes(self.receipt.read_bytes())
            fetch_path = records / "fetch.json"
            fetch_path.write_text(json.dumps(fetched), encoding="utf-8")
            write_product_viewer_proof(fetch_path, key)
            (records / "retirement.json").write_text(json.dumps(retired), encoding="utf-8")
            viewer = WORKLOAD.viewer_binding_sha256(sample)
            product = collect_git_product_action(records, fetched | {"viewer_binding_sha256": viewer},
                                                 json.dumps(rollback).encode(), public_export_sha256=current)
            self.assertEqual(product["name"], "lfs-fetch")
            export = self.root / "public-export"
            copytree(new, export)
            copytree(old, export / "before")
            registration = {"previous_publication_sha256": previous, "publication_sha256": current,
                            "viewer_binding_sha256": viewer}
            binding, entries, sealed = read_publication(export, current)
            row = fetched | {"product_action": product}
            verify_public_transition(retained, self.root, row, registration, binding, entries, sealed)
            with self.assertRaisesRegex(ValueError, "must verify the exported LFS object itself"):
                verify_public_transition(
                    retained, self.root, row | {"product_action": product | {"observed_sha256": prior.sha256}},
                    registration, binding, entries, sealed,
                )

    def test_a_fetch_with_only_rejected_edge_payloads_does_not_qualify(self) -> None:
        self.run_phase("publish")
        (self.root / "rejected-edge").touch()
        fetched = self.call("fetch")
        self.assertEqual(fetched.returncode, 1, fetched.stdout)
        self.assertIn("did not accept a verified edge payload", fetched.stderr)
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_origin_control_rejects_edge_traffic_even_for_another_family(self) -> None:
        self.run_phase("publish")
        (self.root / "binding-off").touch()
        (self.root / "foreign-edge").touch()
        result = self.call("fetch-origin")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("origin control observed edge traffic", result.stderr)
        (self.root / "origin-only").touch()
        origin = self.run_phase("fetch-origin")
        self.assertEqual(origin["arm"], "origin")
        self.assertEqual(origin["all_edge_attempts"], 0)
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_origin_control_rejects_an_active_binding_with_zero_edge_requests(self) -> None:
        self.run_phase("publish")
        (self.root / "origin-only").touch()
        result = self.call("fetch-origin")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("a different CDN binding or local cache root", result.stderr)
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_warm_local_requires_a_verified_cache_hit_in_the_second_client(self) -> None:
        self.run_phase("publish")
        missed = self.call("fetch-warm-local")
        self.assertEqual(missed.returncode, 1, missed.stdout)
        self.assertIn("warm-local fetch must use a verified local payload", missed.stderr)
        (self.root / "warm-cache").touch()
        warm = self.run_phase("fetch-warm-local")
        self.assertEqual(warm["arm"], "warm_local")
        self.assertEqual(warm["local_payload_hits"], 1)
        self.assertEqual(warm["edge_attempts"], 0)
        self.assertEqual(warm["observed_sha256"], warm["expected_sha256"])
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_warm_local_lfs_reruns_fetch_against_the_verified_primed_worktree(self) -> None:
        config = fixture(access="private")
        config |= {"scope": "cloudfront/private/cli/lfs/direct", "object_path": lfs_object_path(config["sha256"])}
        raw = json.dumps(config).encode()
        self.fixture.write_bytes(raw)
        self.digest = hashlib.sha256(raw).hexdigest()
        self.run_phase("publish")
        warm = self.run_phase("fetch-warm-local")
        self.assertEqual((warm["name"], warm["arm"], warm["local_payload_hits"], warm["all_edge_attempts"]),
                         ("lfs-fetch", "warm_local", 1, 0))
        (self.root / "unverified-local").touch()
        missed = self.call("fetch-warm-local")
        self.assertNotEqual(missed.returncode, 0)
        self.assertIn("warm-local fetch must use a verified local payload", missed.stderr)
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_native_writer_and_independent_probe_support_an_sdk_fixture(self) -> None:
        config = fixture(access="private") | {"scope": "cloudfront/private/sdk/git/direct"}
        raw = json.dumps(config).encode()
        self.fixture.write_bytes(raw)
        self.digest = hashlib.sha256(raw).hexdigest()
        self.verify_native_ref_observation()

    def test_native_service_lfs_writer_publishes_the_approved_fixture_payload(self) -> None:
        content = b"approved native LFS content for the service"
        digest = hashlib.sha256(content).hexdigest()
        config = fixture(access="private") | {
            "scope": "cloudfront/private/s3-gateway/lfs/direct", "object_path": lfs_object_path(digest),
            "sha256": digest, "size": len(content),
        }
        self.fixture.write_text(json.dumps(config), encoding="utf-8")
        self.digest = hashlib.sha256(self.fixture.read_bytes()).hexdigest()
        payload = self.root / "payload.bin"
        arguments = ("--service-lfs-payload", str(payload))
        self.assertIn("requires the exact approved fixture payload", self.call("publish").stderr)
        payload.write_bytes(b"x" * len(content))
        self.assertIn("payload differs from the approved fixture", self.call("publish", *arguments).stderr)
        self.assertFalse(self.receipt.exists())
        payload.write_bytes(content)
        self.assertTrue(self.run_phase("publish", *arguments)["visible_new_ref"])
        publication = json.loads(self.receipt.read_bytes())
        pointer = subprocess.check_output(["git", "--git-dir", str(self.origin), "cat-file", "blob",
                                           publication["oid"] + ":" + WORKLOAD.PAYLOAD])
        self.assertEqual(publication["sha256"], digest)
        self.assertEqual(pointer, WORKLOAD.lfs_pointer(digest, len(content)))
        self.assertEqual((self.origin / digest).read_bytes(), content)
        self.assertNotEqual(self.call("fetch").returncode, 0)
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_native_writer_and_independent_probe_support_a_server_fixture(self) -> None:
        config = fixture(access="private") | {"scope": "cloudfront/private/server/git/direct"}
        self.fixture.write_text(json.dumps(config), encoding="utf-8")
        self.digest = hashlib.sha256(self.fixture.read_bytes()).hexdigest()
        self.verify_native_ref_observation()

    def test_rejects_unapproved_fixture_or_another_runs_receipt(self) -> None:
        rejected = self.call("publish", digest="f" * 64)
        self.assertEqual(rejected.returncode, 1, rejected.stdout)
        self.assertFalse(self.receipt.exists())
        self.run_phase("publish")
        saved = self.receipt.read_bytes()
        record = json.loads(saved)
        record["ref"] = record["ref"].replace("-123-2-", "-124-2-")
        self.receipt.write_text(json.dumps(record), encoding="utf-8")
        rejected = self.call("retire")
        self.assertEqual(rejected.returncode, 1, rejected.stdout)
        self.assertIn("does not match this qualification run", rejected.stderr)
        self.receipt.write_bytes(saved)
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_publish_refuses_a_helper_that_does_not_attest_the_origin(self) -> None:
        (self.root / "legacy-helper").touch()
        result = self.call("publish")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("did not confirm the approved native origin", result.stderr)
        self.assertFalse(self.receipt.exists())
        refs = subprocess.check_output(["git", "for-each-ref", "refs/heads"], cwd=self.origin)
        self.assertEqual(refs, b"")

    def test_retirement_preserves_a_ref_changed_after_publication(self) -> None:
        self.run_phase("publish")
        record = json.loads(self.receipt.read_bytes())

        def git(*args: str, input: bytes | None = None) -> bytes:
            return subprocess.run(["git", *args], cwd=self.origin, input=input, check=True,
                                  capture_output=True).stdout.strip()

        tree = git("rev-parse", f"{record['oid']}^{{tree}}").decode("ascii")
        moved = git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                    "commit-tree", tree, "-p", record["oid"], input=b"changed after publication\n").decode("ascii")
        git("update-ref", record["ref"], moved, record["oid"])
        rejected = self.call("retire")
        self.assertEqual(rejected.returncode, 1, rejected.stdout)
        self.assertIn("ref changed after publication", rejected.stderr)
        self.assertEqual(git("rev-parse", record["ref"]), moved.encode("ascii"))


if __name__ == "__main__":
    unittest.main()
