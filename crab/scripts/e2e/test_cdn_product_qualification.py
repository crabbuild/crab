"""CDN product metrics consume only the route-specific redacted events."""

import json
import unittest

import collect_cdn_product_metrics as PRODUCT


SCOPE = "cloudfront/private/cli/git/direct"


def event(**fields):
    payload = {
        "message": PRODUCT.EVENT, "provider": "s3", "access": "private", "consumer": "cli",
        "family": "git", "profile": "direct", "status": 200, "cache": "hit",
        "range": False, "bytes": 8, "complete": True, "elapsed_ms": 10, "request_sha256": "",
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
            event(range=True, status=206, bytes=2),
            event(cache="unknown", bytes=1),
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
        metrics = PRODUCT.measure([event(status=0, bytes=0, complete=False), event(bytes=0)], SCOPE)
        self.assertEqual(metrics["edge_attempts"], 2)
        self.assertEqual(metrics["observed_edge_gets"], 0)

    def test_rejects_an_inconsistent_completed_range_or_raw_event_field(self):
        with self.assertRaisesRegex(ValueError, "inconsistent completed CDN response"):
            PRODUCT.measure([event(range=True)], SCOPE)
        with self.assertRaisesRegex(ValueError, "invalid fields in CDN transfer event"):
            PRODUCT.measure([event(headers={"cookie": "fake"})], SCOPE)

    def test_cloudfront_request_fingerprints_bind_all_families_without_entering_metrics(self):
        responses = {}
        observed = PRODUCT.measure([event(request_sha256="a" * 64), event(family="capsule", request_sha256="b" * 64)],
                                   SCOPE, edge_responses=responses)
        self.assertEqual(set(responses), {"a" * 64, "b" * 64})
        self.assertEqual(observed["observed_edge_gets"], 1)
        with self.assertRaisesRegex(ValueError, "duplicate CDN viewer request fingerprint"):
            PRODUCT.measure([event(request_sha256="a" * 64)], SCOPE, edge_responses=responses)
