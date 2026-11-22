import hashlib
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from google.api_core.exceptions import PreconditionFailed

import publish_export as publisher
from public_inventory import SCHEMA


class Blob:
    def __init__(self, bucket):
        self.bucket = bucket

    def reload(self):
        self.size = len(self.bucket.body)
        self.generation = 42
        self.metageneration = 1
        self.content_type = publisher.CONTENT_TYPE
        self.cache_control = publisher.CACHE_CONTROL
        self.metadata = self.bucket.metadata

    def download_to_filename(self, filename, *, raw_download, if_generation_match,
                             if_metageneration_match):
        if (raw_download, if_generation_match, if_metageneration_match) != (True, 42, 1):
            raise AssertionError("publisher must pin native bytes and representation")
        Path(filename).write_bytes(self.bucket.body)

    def upload_from_filename(self, filename, *, content_type, if_generation_match):
        if (content_type, if_generation_match) != (publisher.CONTENT_TYPE, 0):
            raise AssertionError("publisher must use create-only conditional upload")
        if self.cache_control != publisher.CACHE_CONTROL:
            raise AssertionError("publisher must set approved response metadata")
        if self.bucket.body is not None:
            raise PreconditionFailed("existing destination")
        self.bucket.body = Path(filename).read_bytes()
        self.bucket.writes += 1


class Bucket:
    def __init__(self, key, body=None):
        self.key = key
        self.body = body
        self.writes = 0
        self.metadata = None

    def blob(self, key):
        if key != self.key:
            raise AssertionError("publisher must keep the same physical object key")
        return Blob(self)


class ExportTest(unittest.TestCase):
    def setUp(self):
        scratch = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.data = b"approved bytes"
        digest = hashlib.sha256(self.data).hexdigest()
        path = f"lfs/objects/{digest[:2]}/{digest[2:4]}/{digest}"
        inventory = {
            "schema": SCHEMA,
            "source_origin": "gs://crab-live-models",
            "source_placement": {"resource": "projects/23456789/buckets/crab-live-models", "location": "NAM4"},
            "repository_prefix": "org/repo",
            "objects": [{"path": path, "size": len(self.data), "sha256": digest}],
        }
        raw = json.dumps(inventory).encode()
        self.inventory_hash = hashlib.sha256(raw).hexdigest()
        self.options = SimpleNamespace(
            inventory=scratch / "inventory.json",
            deployment=scratch / "deployment.json",
            operator_configuration="operator",
            source_adc=scratch / "source.json",
            writer_adc=scratch / "writer.json",
            scratch_directory=scratch,
        )
        self.options.inventory.write_bytes(raw)
        self.options.deployment.write_text(json.dumps({
            key: {"value": value} for key, value in {
                "source_origin": inventory["source_origin"],
                "repository_prefix": inventory["repository_prefix"],
                "approved_inventory_sha256": self.inventory_hash,
                "export_origin": "gs://crab-public-export",
                "public_delivery_enabled": False,
                "backend_bucket": "fixture-backend",
                "base_url": "https://export.example.com",
                "project_id": "crab-cdn-test",
            }.items()
        }))
        key = f"org/repo/{path}"
        self.source = Bucket(key, self.data)
        self.export = Bucket(key)
        self.live_backend_bucket = "crab-public-export"
        self.viewer_host = "export.example.com"
        self.staged_access = "enforced"
        self.source_project = "23456789"
        self.destination_project = "12345678"
        self.enterContext(patch.object(publisher, "gcloud", self.gcloud))
        self.enterContext(patch.object(publisher, "storage_client", self.storage_client))

    def gcloud(self, configuration, arguments):
        self.assertEqual(configuration, "operator")
        if arguments == ["projects", "describe", "crab-cdn-test"]:
            return {"projectId": "crab-cdn-test", "projectNumber": "12345678"}
        if arguments[0] == "compute":
            if arguments[1] == "security-policies":
                self.assertEqual(arguments[3], "fixture-viewer")
                return {"type": "CLOUD_ARMOR_EDGE", "rules": [{"action": "deny(403)", "match": {"expr": {
                    "expression": f"!has(request.headers['host']) || request.headers['host'] != '{self.viewer_host}'",
                }}}]}
            self.assertEqual(arguments[3], "fixture-backend")
            return {"name": "fixture-backend", "bucketName": self.live_backend_bucket, "enableCdn": True,
                    "edgeSecurityPolicy": "projects/crab-cdn-test/global/securityPolicies/fixture-viewer"}
        if arguments == ["storage", "buckets", "describe", "gs://crab-live-models", "--raw"]:
            return {"name": "crab-live-models", "projectNumber": self.source_project, "location": "NAM4"}
        self.assertEqual(arguments, [
            "storage", "buckets", "describe", "gs://crab-public-export", "--raw",
        ])
        return {
            "name": "crab-public-export",
            "projectNumber": self.destination_project,
            "location": "US",
            "labels": {
                "crab_public_export": "true",
                "crab_inventory_0": self.inventory_hash[:32],
                "crab_inventory_1": self.inventory_hash[32:],
            },
            "iamConfiguration": {
                "publicAccessPrevention": self.staged_access,
                "uniformBucketLevelAccess": {"enabled": True},
            },
            "softDeletePolicy": {"retentionDurationSeconds": "0"},
        }

    def storage_client(self, credentials, project):
        self.assertEqual(project, "crab-cdn-test")
        choices = {
            self.options.source_adc: ("crab-live-models", self.source),
            self.options.writer_adc: ("crab-public-export", self.export),
        }
        expected, store = choices[credentials]

        def bucket(name):
            self.assertEqual(name, expected)
            return store

        return SimpleNamespace(bucket=bucket)

    def test_publish_rechecks_existing_generation_and_approved_metadata(self):
        receipt = publisher.publish(self.options)
        self.assertEqual(publisher.publish(self.options), receipt)
        self.assertEqual(receipt["inventory_sha256"], self.inventory_hash)
        self.assertEqual(receipt["base_url"], "https://export.example.com")
        self.assertEqual(receipt["deployment"], "projects/crab-cdn-test/global/backendBuckets/fixture-backend")
        self.assertEqual(receipt["source_placement"]["location"], "NAM4")
        self.assertEqual(receipt["export_placement"]["resource"], "projects/12345678/buckets/crab-public-export")

        self.assertEqual(self.export.body, self.data)
        self.assertEqual((self.source.writes, self.export.writes), (0, 1))
        self.export.metadata = {"unapproved": "fixture"}
        with self.assertRaisesRegex(publisher.ExportError, "metadata differs"):
            publisher.publish(self.options)
        self.assertEqual(self.export.writes, 1)

    def test_stage_guard_must_approve_viewer_hostname(self):
        self.viewer_host = "other.example.com"
        with self.assertRaisesRegex(publisher.ExportError, "staged hostname"):
            publisher.publish(self.options)
        self.assertEqual(self.export.writes, 0)

    def test_staged_inventory_keeps_bad_source_bytes_out_of_export(self):
        self.source.body = b"x" * len(self.data)
        with self.assertRaisesRegex(publisher.ExportError, "digest mismatch"):
            publisher.publish(self.options)
        self.assertEqual(self.export.writes, 0)

    def test_different_source_or_export_project_fails_before_copy(self):
        self.source_project = "22222222"
        with self.assertRaisesRegex(publisher.ExportError, "approved GCS placement"):
            publisher.publish(self.options)
        self.destination_project = "22222222"
        with self.assertRaisesRegex(publisher.ExportError, "outside the approved Cloud CDN project"):
            publisher.publish(self.options)
        self.assertEqual(self.export.writes, 0)

    def test_control_plane_mismatch_stops_before_any_object_access(self):
        with patch.object(publisher, "storage_client") as store:
            self.live_backend_bucket = "crab-live-models"
            with self.assertRaisesRegex(publisher.ExportError, "backend does not serve"):
                publisher.publish(self.options)
            self.staged_access = "inherited"
            with self.assertRaisesRegex(publisher.ExportError, "not staged"):
                publisher.publish(self.options)
            store.assert_not_called()


if __name__ == "__main__":
    unittest.main()
