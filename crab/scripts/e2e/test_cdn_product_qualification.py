"""CDN product metrics consume only the route-specific redacted events."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import collect_cdn_product_metrics as PRODUCT


SCOPE = "cloudfront/private/cli/git/direct"


def event(**fields):
    payload = {
        "message": PRODUCT.EVENT, "provider": "s3", "access": "private", "consumer": "cli",
        "family": "git", "profile": "direct", "status": 200, "cache": "hit",
        "range": False, "object_bytes": 8, "requested_bytes": 8, "bytes": 8,
        "complete": True, "elapsed_ms": 10, "request_sha256": "",
    }
    return json.dumps({"target": PRODUCT.TARGET, "fields": payload | fields}).encode()

def verified(**fields):
    payload = {
        "message": PRODUCT.VERIFIED_GIT_EVENT, "provider": "s3", "access": "private",
        "consumer": "cli", "family": "git", "profile": "direct", "source": "edge",
    }
    return json.dumps({"target": PRODUCT.TARGET, "fields": payload | fields}).encode()


class CdnProductQualificationTests(unittest.TestCase):
    def test_counts_actual_attempts_and_completed_bodies_for_the_requested_row(self):
        messages = [
            b"remote: progress",
            event(cache="miss"),
            event(range=True, status=206, bytes=2, requested_bytes=2),
            event(cache="unknown", bytes=1, requested_bytes=1, object_bytes=1),
            event(status=403, bytes=0, complete=False),
            event(bytes=3, complete=False),
            event(family="capsule"),
            event(consumer="sdk"),
            event(access="public"),
            event(provider="gcs"),
            event(profile="managed"),
            verified(),
            verified(family="capsule"),
            verified(source="local"),
        ]
        self.assertEqual(PRODUCT.measure(messages, SCOPE), {
            "edge_attempts": 5, "observed_edge_gets": 3, "edge_bytes": 11, "edge_hits": 1,
            "edge_misses": 1, "edge_unknown": 1, "incomplete_edge_bytes": 3,
            "edge_elapsed_ms_total": 50,
            "verified_edge_payloads": 1,
            "all_edge_attempts": 10, "local_payload_hits": 1,
        })

    def test_transport_failure_or_empty_body_cannot_prove_payload_delivery(self):
        metrics = PRODUCT.measure([event(status=0, bytes=0, complete=False),
                                   event(bytes=0, requested_bytes=0, object_bytes=0)], SCOPE)
        self.assertEqual(metrics["edge_attempts"], 2)
        self.assertEqual(metrics["observed_edge_gets"], 0)

    def test_xorb_proof_requires_verified_chunks_from_the_selected_physical_key(self):
        scope = "cloudfront/private/sdk/xorb/direct"
        key = "a" * 64
        xorb = {"message": PRODUCT.VERIFIED_XORB_EVENT, "consumer": "sdk", "family": "xorb",
                "xorb_key_sha256": key}
        messages = [event(consumer="sdk", family="xorb"), verified(**(xorb | {"xorb_key_sha256": "b" * 64})),
                    verified(**(xorb | {"consumer": "cli"}))]
        self.assertEqual(PRODUCT.measure(messages, scope, expected_xorb_key=key)["verified_edge_payloads"], 0)
        messages.append(verified(**xorb))
        self.assertEqual(PRODUCT.measure(messages, scope, expected_xorb_key=key)["verified_edge_payloads"], 1)
        local = verified(**(xorb | {"source": "local"}))
        self.assertEqual(PRODUCT.measure([local], scope, expected_xorb_key=key, required_xorb_source="local")["local_payload_hits"], 1)
        self.assertEqual(PRODUCT.measure([local], scope, expected_xorb_key=key)["verified_edge_payloads"], 0)
        for overrides in ({"source": "unknown"}, {"xorb_key_sha256": "unredacted"}, {"family": "git"}):
            with self.assertRaisesRegex(ValueError, "invalid verified CDN xorb key"):
                PRODUCT.measure([verified(**(xorb | overrides))], scope, expected_xorb_key=key)
        for overrides in ({"source": "origin"}, {"consumer": "cli"}, {"xorb_key_sha256": "b" * 64}):
            foreign = verified(**(xorb | {"source": "local"} | overrides))
            with self.assertRaisesRegex(ValueError, "outside its approved local cache"):
                PRODUCT.measure([local, foreign], scope, expected_xorb_key=key, required_xorb_source="local")
        with self.assertRaisesRegex(ValueError, "outside its approved edge cache"):
            PRODUCT.measure([verified(**xorb), verified(**(xorb | {"source": "origin"}))], scope,
                            expected_xorb_key=key, required_xorb_source="edge")

    def test_xorb_collector_requires_a_preregistered_physical_key(self):
        scope = "cloudfront/private/cli/xorb/direct"
        key = "a" * 64
        with tempfile.TemporaryDirectory() as scratch:
            trace = Path(scratch) / "read.jsonl"
            trace.write_bytes(verified(message=PRODUCT.VERIFIED_XORB_EVENT, family="xorb", xorb_key_sha256=key) + b"\n")
            command = [sys.executable, "-B", PRODUCT.__file__, "--stderr", str(trace), "--scope", scope]
            missing = subprocess.run(command, capture_output=True, check=False)
            self.assertEqual(missing.returncode, 1)
            result = subprocess.run([*command, "--xorb-key-sha256", key], capture_output=True, check=True)
            self.assertEqual(json.loads(result.stdout)["verified_edge_payloads"], 1)

    def test_rejects_an_inconsistent_completed_range_or_raw_event_field(self):
        with self.assertRaisesRegex(ValueError, "inconsistent completed CDN response"):
            PRODUCT.measure([event(range=True)], SCOPE)
        with self.assertRaisesRegex(ValueError, "invalid fields in CDN transfer event"):
            PRODUCT.measure([event(headers={"cookie": "fake"})], SCOPE)

    def test_rejects_inconsistent_sizes_even_for_other_families(self):
        for change, error in (
            ({"object_bytes": 4}, "invalid CDN transfer observation"),
            ({"range": True, "requested_bytes": 0}, "invalid CDN transfer observation"),
            ({"range": True, "requested_bytes": 4, "status": 206}, "inconsistent completed CDN response"),
            ({"elapsed_ms": True}, "invalid CDN transfer observation"),
        ):
            with self.subTest(change=change), self.assertRaisesRegex(ValueError, error):
                PRODUCT.measure([event(family="capsule", **change)], SCOPE)

    def test_cloudfront_request_fingerprints_bind_all_families_without_entering_metrics(self):
        responses = {}
        observed = PRODUCT.measure([
            event(request_sha256="a" * 64),
            event(family="capsule", request_sha256="b" * 64, status=206, range=True, object_bytes=24),
        ], SCOPE, edge_responses=responses)
        self.assertEqual(responses["a" * 64], {
            "family": "git", "status": 200, "cache": "hit", "range": False, "object_bytes": 8,
            "requested_bytes": 8, "bytes": 8, "complete": True, "elapsed_ms": 10,
        })
        self.assertEqual(responses["b" * 64], responses["a" * 64] | {
            "family": "capsule", "status": 206, "range": True, "object_bytes": 24,
        })
        self.assertEqual(observed["observed_edge_gets"], 1)
        with self.assertRaisesRegex(ValueError, "duplicate CDN viewer request fingerprint"):
            PRODUCT.measure([event(request_sha256="a" * 64)], SCOPE, edge_responses=responses)
