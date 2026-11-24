"""Exercise the native cleanup inventories for each supported cloud."""

from __future__ import annotations

import hashlib
import json
import sys
import unittest
from contextlib import nullcontext, redirect_stdout
from io import StringIO
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import Mock, patch
from urllib.parse import parse_qs, urlsplit

import collect_cdn_fixture_cleanup as cleanup
from cdn_qualification import CLEANUP_INVENTORIES
from test_cdn_transport_qualification import REPO, fixture


PREFIX = REPO + "/"


class CdnCleanupQualificationTests(unittest.TestCase):
    def test_s3_checks_pending_uploads_before_committed_versions_for_the_exact_prefix(self) -> None:
        base = urlsplit("https://s3.us-west-2.amazonaws.com/fixture-bucket")
        empty = {"Prefix": PREFIX, "IsTruncated": False}
        with (patch.object(cleanup, "native_base", return_value=base),
              patch.object(cleanup, "command_output",
                           side_effect=[json.dumps(page).encode() for page in (empty | {"Bucket": "fixture-bucket"}, empty)]) as run):
            result = cleanup.s3_inventory("s3://fixture-bucket", PREFIX)
        self.assertEqual(run.call_count, 2)
        for call, (operation, maximum) in zip(run.call_args_list, (
            ("list-multipart-uploads", "--max-uploads"), ("list-object-versions", "--max-keys"),
        )):
            args = call.args
            self.assertEqual(args[:3], ("aws", "s3api", operation))
            for option, expected in (
                ("--bucket", "fixture-bucket"), ("--prefix", PREFIX), (maximum, "1"),
                ("--region", "us-west-2"), ("--endpoint-url", "https://s3.us-west-2.amazonaws.com"),
            ):
                self.assertEqual(args[args.index(option) + 1], expected)
            self.assertIn("--no-paginate", args)
        self.assertIn(result, CLEANUP_INVENTORIES["cloudfront"])

    def test_s3_rejects_retained_versions_delete_markers_and_incomplete_inventory(self) -> None:
        base = urlsplit("https://s3.us-west-2.amazonaws.com/fixture-bucket")
        candidate = {"Prefix": PREFIX, "IsTruncated": False}
        uploads = candidate | {"Bucket": "fixture-bucket"}
        for changed in ({"Versions": [{}]}, {"DeleteMarkers": [{}]}, {"IsTruncated": True}, {"Prefix": REPO}):
            with (self.subTest(changed=changed), patch.object(cleanup, "native_base", return_value=base),
                  patch.object(cleanup, "command_output",
                               side_effect=[json.dumps(page).encode() for page in (uploads, candidate | changed)])):
                with self.assertRaisesRegex(ValueError, "inventory is incomplete"):
                    cleanup.s3_inventory("s3://fixture-bucket", PREFIX)

    def test_s3_cannot_claim_cleanup_with_an_upload_or_incomplete_upload_listing(self) -> None:
        base = urlsplit("https://s3.us-west-2.amazonaws.com/fixture-bucket")
        candidate = {"Bucket": "fixture-bucket", "Prefix": PREFIX, "IsTruncated": False}
        for changed in ({"Uploads": [{}]}, {"IsTruncated": True}, {"Bucket": "other"}, {"Prefix": REPO}):
            with (self.subTest(changed=changed), patch.object(cleanup, "native_base", return_value=base),
                  patch.object(cleanup, "command_output", return_value=json.dumps(candidate | changed).encode()) as run):
                with self.assertRaisesRegex(ValueError, "multipart uploads"):
                    cleanup.s3_inventory("s3://fixture-bucket", PREFIX)
                self.assertEqual(run.call_count, 1)

    def test_gcs_fetches_live_and_historical_inventories_based_on_the_bucket_policy(self) -> None:
        settings = {"name": "fixture-bucket", "softDeletePolicy": {"retentionDurationSeconds": "604800"}}
        empty = {"kind": "storage#objects"}
        for retention, softened in (("604800", "empty"), ("0", "disabled")):
            with self.subTest(retention=retention):
                settings["softDeletePolicy"]["retentionDurationSeconds"] = retention
                responses = [settings, empty, empty] if softened == "empty" else [settings, empty]
                with patch.object(cleanup, "api_body", side_effect=[json.dumps(item).encode() for item in responses]) as get:
                    result = cleanup.gcs_inventory("gs://fixture-bucket", PREFIX, "Bearer test-token")
                queries = [parse_qs(urlsplit(call.args[1]).query) for call in get.call_args_list]
                self.assertEqual(queries[1]["versions"], ["true"])
                self.assertEqual(queries[1]["prefix"], [PREFIX])
                if softened == "empty":
                    self.assertEqual(queries[2]["softDeleted"], ["true"])
                    self.assertEqual(queries[2]["prefix"], [PREFIX])
                self.assertEqual(result, {"generations": "empty", "soft_deleted": softened})

    def test_gcs_does_not_accept_an_empty_page_with_a_continuation(self) -> None:
        responses = [b'{"name":"fixture-bucket"}', b'{"kind":"storage#objects","nextPageToken":"more"}']
        with patch.object(cleanup, "api_body", side_effect=responses):
            with self.assertRaisesRegex(ValueError, "inventory is incomplete"):
                cleanup.gcs_inventory("gs://fixture-bucket", PREFIX, "Bearer test-token")

    def test_azure_inventories_handle_flat_and_hierarchical_namespaces(self) -> None:
        xml = f"<EnumerationResults><Prefix>{PREFIX}</Prefix><Blobs/><NextMarker/></EnumerationResults>".encode()
        origin = fixture("front-door")["native_origin"]
        for namespace in ("true", "false"):
            response = Mock(status=200)
            response.getheader.return_value = namespace
            with (self.subTest(namespace=namespace), patch.object(cleanup, "https_response", return_value=nullcontext(response)),
                  patch.object(cleanup, "api_body", return_value=xml) as get):
                result = cleanup.azure_inventory(origin, PREFIX, "Bearer test-token")
            queries = [parse_qs(urlsplit(call.args[1]).query) for call in get.call_args_list]
            self.assertTrue(all(query["prefix"] == [PREFIX] and query["maxresults"] == ["1"] for query in queries))
            if namespace == "true":
                self.assertEqual(queries[0]["include"], ["uncommittedblobs"])
                self.assertEqual(queries[1]["showonly"], ["deleted"])
                self.assertEqual(result, {"blobs": "empty", "soft_deleted": "empty", "uncommitted_blobs": "empty"})
            else:
                self.assertEqual(queries[0]["include"], ["uncommittedblobs,snapshots,versions,deleted,deletedwithversions"])
                self.assertEqual(result, {"versions_and_snapshots": "empty", "uncommitted_blobs": "empty"})

    def test_azure_cleanup_rejects_an_uncommitted_blob(self) -> None:
        xml = (f"<EnumerationResults><Prefix>{PREFIX}</Prefix><Blobs><Blob><Name>{PREFIX}pending</Name>"
               "</Blob></Blobs><NextMarker/></EnumerationResults>").encode()
        response = Mock(status=200)
        response.getheader.return_value = "true"
        with (patch.object(cleanup, "https_response", return_value=nullcontext(response)),
              patch.object(cleanup, "api_body", return_value=xml)):
            with self.assertRaisesRegex(ValueError, "inventory is incomplete"):
                cleanup.azure_inventory(fixture("front-door")["native_origin"], PREFIX, "Bearer test-token")

    def test_public_trace_records_both_origins_and_redacts_targets(self) -> None:
        sample = fixture()
        with TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, output = root / "fixture.json", root / "cleanup.json"
            source.write_text(json.dumps(sample), encoding="utf-8")
            digest = hashlib.sha256(source.read_bytes()).hexdigest()
            argv = ["cleanup", "--fixture", str(source), "--configuration-sha256", digest,
                    "--output", str(output), "--source-sha", "a" * 40, "--run-id", "123", "--run-attempt", "2"]
            with (patch.object(sys, "argv", argv), redirect_stdout(StringIO()),
                  patch.object(cleanup, "s3_inventory", return_value=CLEANUP_INVENTORIES["cloudfront"][0]) as inventory):
                self.assertEqual(cleanup.main(), 0)
            self.assertEqual([call.args for call in inventory.call_args_list],
                             [(sample["native_origin"], PREFIX), (sample["export_origin"], PREFIX)])
            trace = output.read_text(encoding="utf-8")
            self.assertNotIn(sample["native_origin"], trace)
            self.assertNotIn(sample["export_origin"], trace)
            self.assertNotIn(REPO, trace)
            self.assertEqual(set(json.loads(trace)["targets"]), {"native", "export"})


if __name__ == "__main__":
    unittest.main()
