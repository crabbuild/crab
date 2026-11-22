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
from tempfile import TemporaryDirectory

import run_cdn_git_workload as WORKLOAD
from cdn_qualification import GIT_FETCH_FIELDS, collect_git_product_action
from public_inventory import Object, SCHEMA, publication_receipt
from test_cdn_transport_qualification import PACK, fixture


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
        raw = json.dumps(fixture(access="private")).encode()
        self.fixture.write_bytes(raw)
        self.digest = hashlib.sha256(raw).hexdigest()
        self.helper = self.root / "git-remote-crab"
        # Git still creates real packed objects and ref leases. The shim directs
        # its wire protocol to a disposable local origin and supplies trace input.
        self.helper.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "origin = Path(__file__).parent / 'origin.git'\n"
            "for line in sys.stdin.buffer:\n"
            "    if line == b'capabilities\\n':\n"
            "        print('connect\\n', flush=True)\n"
            "    elif line.startswith(b'connect '):\n"
            "        command = line.split()[1].decode()\n"
            "        expected = os.environ['CRAB_INTERNAL_CDN_QUALIFICATION_ORIGIN']\n"
            "        if expected != 's3://fixture-bucket':\n"
            "            sys.exit(2)\n"
            "        print(flush=True)\n"
            "        if not (origin.parent / 'legacy-helper').exists():\n"
            "            print(json.dumps({'target':'crab::cdn::qualification', 'fields': {\n"
            "                'message':'CDN qualification verified native origin'}}), file=sys.stderr, flush=True)\n"
            "        evidence = origin.parent / 'public-export-events.json'\n"
            "        if command == 'git-upload-pack' and evidence.exists():\n"
            "            for event in json.loads(evidence.read_bytes()):\n"
            "                print(json.dumps(event), file=sys.stderr, flush=True)\n"
            "        if command == 'git-upload-pack' and not (origin.parent / 'origin-only').exists():\n"
            "            cache = Path(os.environ['CRAB_CACHE_DIR'])\n"
            "            family = 'capsule' if (origin.parent / 'foreign-edge').exists() else 'git'\n"
            "            local = (origin.parent / 'warm-cache').exists() and (cache / 'seed').exists()\n"
            "            policy = {'provider':'s3', 'access':'public' if evidence.exists() else 'private', 'consumer':'cli',\n"
            "                      'family':family, 'profile':'direct'}\n"
            "            if not local:\n"
            "                print(json.dumps({'target':'crab_storage::cdn::transfer', 'fields': policy | {\n"
            "                    'message':'CDN immutable GET', 'status':206, 'cache':'hit', 'range':True,\n"
            "                    'bytes':1024, 'complete':True, 'elapsed_ms':10}}), file=sys.stderr, flush=True)\n"
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

    def call(self, action: str, *arguments: str, digest: str | None = None) -> subprocess.CompletedProcess[str]:
        return subprocess.run([
            sys.executable, "-B", str(Path(WORKLOAD.__file__)), action,
            "--fixture", str(self.fixture), "--configuration-sha256", digest or self.digest,
            "--receipt", str(self.receipt), "--scratch", str(self.root), "--helper", str(self.helper),
            "--source-sha", "a" * 40, "--run-id", "123", "--run-attempt", "2",
            *arguments,
        ], capture_output=True, text=True, check=False)

    def run_phase(self, action: str) -> dict:
        result = self.call(action)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def test_publishes_fetches_and_retires_the_dedicated_ref(self) -> None:
        published = self.run_phase("publish")
        self.assertTrue(published["visible_new_ref"])
        fetched = self.run_phase("fetch")
        self.assertEqual(fetched.keys(), GIT_FETCH_FIELDS)
        self.assertEqual(fetched["observed_sha256"], fetched["expected_sha256"])
        self.assertEqual(fetched["publication_oid"], json.loads(self.receipt.read_bytes())["oid"])
        self.assertLessEqual(fetched["started_at_unix_ms"], fetched["finished_at_unix_ms"])
        self.assertEqual(fetched["observed_bytes"], WORKLOAD.GIT_PAYLOAD_SIZE)
        self.assertEqual(fetched["observed_edge_gets"], 1)
        self.assertEqual(fetched["verified_edge_payloads"], 1)
        self.assertNotIn("fixture_cleanup_verified", fetched)
        retired = self.run_phase("retire")
        self.assertTrue(retired["ref_retired"])
        records = self.root / "git-product"
        records.mkdir()
        (records / "publication.json").write_bytes(self.receipt.read_bytes())
        (records / "fetch.json").write_text(json.dumps(fetched), encoding="utf-8")
        (records / "retirement.json").write_text(json.dumps(retired), encoding="utf-8")
        measured = collect_git_product_action(records, fetched)
        self.assertEqual(measured["observed_sha256"], fetched["expected_sha256"])
        self.assertEqual(measured["verified_edge_payloads"], 1)
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_a_byte_identical_fetch_from_origin_only_does_not_qualify(self) -> None:
        self.run_phase("publish")
        (self.root / "origin-only").touch()
        fetched = self.call("fetch")
        self.assertEqual(fetched.returncode, 1, fetched.stdout)
        self.assertIn("no completed edge GET", fetched.stderr)
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_public_transition_checks_the_approval_and_exact_key_in_both_git_actions(self) -> None:
        sample = fixture(access="public") | {"base_url": "https://fixture.example.com"}
        raw = json.dumps(sample).encode()
        self.fixture.write_bytes(raw)
        self.digest = hashlib.sha256(raw).hexdigest()
        key = hashlib.sha256(f"{sample['fixture_prefix']}/{PACK}".encode()).hexdigest()
        event_path = self.root / "public-export-events.json"
        prior = Object("v2/pack-layers/bb/" + "b" * 64, 6, "c" * 64)
        added = Object(PACK, 6, "d" * 64)

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
                    "consumer": "cli", "family": "git", "profile": "direct", "key_sha256": key,
                    "approval_sha256": approval, "observation": kind,
                }} for kind in kinds
            ]))

        old, new = self.root / "previous", self.root / "current"
        previous = seal(old, [prior])
        self.run_phase("publish")
        before, after = self.root / "before.json", self.root / "after.json"
        (self.root / "origin-only").touch()
        observe("0" * 64, "unlisted")
        flags = ("--transition-object", PACK, "--export-approval", str(old), "--transition-output", str(before))
        rejected = self.call("fetch-origin", *flags)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn("different public export receipt", rejected.stderr)
        self.assertFalse(before.exists())
        observe(previous, "unlisted")
        result = self.call("fetch-origin", *flags)
        self.assertEqual(result.returncode, 0, result.stderr)

        (self.root / "origin-only").unlink()
        current = seal(new, [prior, added])
        observe(current, "listed", "verified_edge")
        result = self.call("fetch", "--transition-object", PACK, "--export-approval", str(new),
                           "--transition-output", str(after), "--before-transition", str(before))
        self.assertEqual(result.returncode, 0, result.stderr)
        fetched = json.loads(result.stdout)
        self.assertEqual(set(fetched), GIT_FETCH_FIELDS)
        retained = json.loads(after.read_bytes())
        self.assertEqual(retained["before"]["receipt_sha256"], previous)
        self.assertEqual(retained["after"]["receipt_sha256"], current)
        self.assertEqual(retained["after"]["verified_edge_key_sha256"], key)
        self.assertEqual(retained["before"]["result_sha256"], fetched["observed_sha256"])
        self.assertEqual(retained["before"]["edge_attempts"], 0)
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_a_fetch_with_only_rejected_edge_payloads_does_not_qualify(self) -> None:
        self.run_phase("publish")
        (self.root / "rejected-edge").touch()
        fetched = self.call("fetch")
        self.assertEqual(fetched.returncode, 1, fetched.stdout)
        self.assertIn("did not accept a verified edge payload", fetched.stderr)
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_origin_control_rejects_edge_traffic_even_for_another_family(self) -> None:
        self.run_phase("publish")
        (self.root / "foreign-edge").touch()
        result = self.call("fetch-origin")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("origin control observed edge traffic", result.stderr)
        (self.root / "origin-only").touch()
        origin = self.run_phase("fetch-origin")
        self.assertEqual(origin["arm"], "origin")
        self.assertEqual(origin["all_edge_attempts"], 0)
        self.assertTrue(self.run_phase("retire")["ref_retired"])

    def test_warm_local_requires_a_verified_cache_hit_in_the_second_client(self) -> None:
        self.run_phase("publish")
        missed = self.call("fetch-warm-local")
        self.assertEqual(missed.returncode, 1, missed.stdout)
        self.assertIn("warm-local fetch must use a verified local pack", missed.stderr)
        (self.root / "warm-cache").touch()
        warm = self.run_phase("fetch-warm-local")
        self.assertEqual(warm["arm"], "warm_local")
        self.assertEqual(warm["local_payload_hits"], 1)
        self.assertEqual(warm["edge_attempts"], 0)
        self.assertEqual(warm["observed_sha256"], warm["expected_sha256"])
        self.assertTrue(self.run_phase("retire")["ref_retired"])

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
