import base64
import hashlib
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import publish_export as publisher
from public_inventory import PUBLICATION_SCHEMA, SCHEMA


class ExportTest(unittest.TestCase):
    def setUp(self):
        self.scratch = self.enterContext(tempfile.TemporaryDirectory())
        self.data = b"approved bytes"
        digest = hashlib.sha256(self.data).hexdigest()
        self.path = f"lfs/objects/{digest[:2]}/{digest[2:4]}/{digest}"
        self.inventory = {
            "schema": SCHEMA,
            "source_origin": "s3://source-models",
            "source_placement": {"resource": "accounts/210987654321/buckets/source-models", "location": "us-west-2"},
            "repository_prefix": "org/repo",
            "objects": [{"path": self.path, "size": len(self.data), "sha256": digest}],
        }
        self.options = SimpleNamespace(
            inventory=Path(self.scratch) / "inventory.json",
            scratch_directory=Path(self.scratch),
            operator_profile="operator",
            writer_profile="export-writer",
            source_profile="source-reader",
            stack="test-export",
            region="us-east-1",
        )
        self.write_inventory()
        self.export_data = None
        self.source_data = self.data
        self.enabled = "false"
        self.physical_bucket = "export-models"
        self.viewer_host = "export.cloudfront.net"
        self.destination_host = "export-models.s3.us-east-1.amazonaws.com"
        self.source_region = "us-west-2"
        self.source_owner = "210987654321"
        self.corrupt_destination_write = False
        self.enable_during_readback = False
        self.writes = 0
        self.parts = None
        self.aborts = 0
        self.complete_attempts = 0
        self.failed_part = None
        self.bad_part_checksum = False
        self.complete_conflict = None
        self.mutate_spool = False

    def write_inventory(self):
        raw = json.dumps(self.inventory).encode()
        self.options.inventory.write_bytes(raw)
        self.approval = hashlib.sha256(raw).hexdigest()

    def aws(self, profile, region, service, operation, *args):
        self.assertEqual(region, "us-west-2" if profile == "source-reader" else "us-east-1")
        if service == "cloudformation":
            self.assertEqual(profile, "operator")
            if operation == "describe-stacks":
                outputs = {
                    "SourceOrigin": "s3://source-models",
                    "ExportOrigin": "s3://export-models",
                    "RepositoryPrefix": "org/repo",
                    "DistributionId": "E123ABC",
                    "BaseUrl": "https://export.cloudfront.net",
                    "ApprovedInventorySha256": self.approval,
                    "PublicDeliveryEnabled": self.enabled,
                }
                return {"Stacks": [{
                    "StackStatus": "CREATE_COMPLETE",
                    "StackId": "arn:aws:cloudformation:us-east-1:123456789012:stack/export/id",
                    "Outputs": [{"OutputKey": key, "OutputValue": value}
                                for key, value in outputs.items()],
                }]}
            self.assertEqual(operation, "describe-stack-resource")
            self.assertEqual(args[-2:], ("--logical-resource-id", "ExportBucket"))
            return {"StackResourceDetail": {
                "ResourceType": "AWS::S3::Bucket",
                "PhysicalResourceId": self.physical_bucket,
            }}

        if service == "cloudfront":
            self.assertEqual((profile, operation, args), ("operator", "get-distribution", ("--id", "E123ABC")))
            return {"Distribution": {
                "Id": "E123ABC", "ARN": "arn:aws:cloudfront::123456789012:distribution/E123ABC",
                "DomainName": self.viewer_host, "Status": "Deployed",
                "DistributionConfig": {"Enabled": False, "Origins": {"Items": [{"DomainName": self.destination_host}]}},
            }}
        self.assertEqual(service, "s3api")
        target = args[args.index("--bucket") + 1]
        owner = args[args.index("--expected-bucket-owner") + 1]
        if operation == "get-bucket-location":
            if target == "source-models":
                self.assertEqual(profile, "source-reader")
                if owner != self.source_owner:
                    raise publisher.AwsError(operation, "An error occurred (AccessDenied)")
                return {"LocationConstraint": self.source_region}
            self.assertEqual((target, profile, owner), ("export-models", "operator", "123456789012"))
            return {"LocationConstraint": None}
        self.assertEqual(args[args.index("--key") + 1], f"org/repo/{self.path}")
        if target == "export-models":
            self.assertEqual(profile, "export-writer")
            self.assertEqual(owner, "123456789012")
            data = self.export_data
        else:
            self.assertEqual(target, "source-models")
            self.assertEqual(profile, "source-reader")
            self.assertEqual(owner, "210987654321")
            data = self.source_data
        if operation == "head-object":
            return {"ContentLength": len(data), "ETag": '"fixture"',
                    "ContentType": publisher.CONTENT_TYPE, "CacheControl": publisher.CACHE_CONTROL}
        if operation == "get-object":
            self.assertEqual(args[-3:-1], ("--if-match", '"fixture"'))
            Path(args[-1]).write_bytes(data)
            if target == "source-models":
                self.source_spool = Path(args[-1])
            if target == "export-models" and self.enable_during_readback:
                self.enabled = "true"
            return {"ContentType": publisher.CONTENT_TYPE, "CacheControl": publisher.CACHE_CONTROL}
        if operation in ("create-multipart-upload", "upload-part", "complete-multipart-upload", "abort-multipart-upload"):
            self.assertEqual(target, "export-models")
            return self.multipart(operation, args)
        self.assertEqual(operation, "put-object")
        self.assertEqual(target, "export-models")
        self.assertEqual(args[args.index("--if-none-match") + 1], "*")
        if self.export_data is not None:
            raise publisher.AwsError(operation, "An error occurred (PreconditionFailed)")
        self.export_data = Path(args[args.index("--body") + 1]).read_bytes()
        if self.corrupt_destination_write:
            self.export_data = b"x" * len(self.export_data)
        self.writes += 1
        return {"ChecksumSHA256": args[args.index("--checksum-sha256") + 1]}

    def multipart(self, operation, args):
        def argument(flag):
            return args[args.index(flag) + 1]

        if operation == "create-multipart-upload":
            self.assertIsNone(self.parts)
            self.assertEqual(argument("--checksum-algorithm"), "SHA256")
            self.assertEqual(argument("--checksum-type"), "COMPOSITE")
            self.assertEqual(argument("--content-type"), publisher.CONTENT_TYPE)
            self.assertEqual(argument("--cache-control"), publisher.CACHE_CONTROL)
            self.assertNotIn("--if-none-match", args)
            self.parts = []
            if self.mutate_spool:
                with self.source_spool.open("r+b") as source:
                    source.seek(5)
                    source.write(b"X")
            return {"UploadId": "fixture-upload"}
        self.assertEqual(argument("--upload-id"), "fixture-upload")
        self.assertIsNotNone(self.parts)
        if operation == "abort-multipart-upload":
            self.aborts += 1
            self.parts = None
            return {}
        if operation == "upload-part":
            number = int(argument("--part-number"))
            self.assertEqual(number, len(self.parts) + 1)
            if number == self.failed_part:
                raise publisher.AwsError(operation, "An error occurred (ServiceUnavailable)")
            body = Path(argument("--body")).read_bytes()
            checksum = base64.b64encode(hashlib.sha256(body).digest()).decode("ascii")
            self.assertEqual(argument("--checksum-sha256"), checksum)
            tag = f'"part-{number}"'
            self.parts.append((body, {"PartNumber": number, "ETag": tag, "ChecksumSHA256": checksum}))
            return {"ETag": tag, "ChecksumSHA256": "" if self.bad_part_checksum else checksum}
        self.complete_attempts += 1
        self.assertEqual(argument("--if-none-match"), "*")
        self.assertEqual(argument("--checksum-type"), "COMPOSITE")
        self.assertEqual(int(argument("--mpu-object-size")), len(self.data))
        manifest = json.loads(Path(argument("--multipart-upload").removeprefix("file://")).read_bytes())
        self.assertEqual(manifest, {"Parts": [part for _, part in self.parts]})
        if self.complete_conflict:
            raise publisher.AwsError(operation, f"An error occurred ({self.complete_conflict})")
        if self.export_data is not None:
            raise publisher.AwsError(operation, "An error occurred (PreconditionFailed)")
        self.assertEqual([body for body, _ in self.parts], [self.data[:5], self.data[5:10], self.data[10:]])
        self.export_data = b"".join(body for body, _ in self.parts)
        self.parts = None
        self.writes += 1
        return {"ETag": '"complete"'}

    def enable_multipart_fixture(self):
        self.enterContext(patch.object(publisher, "aws", self.aws))
        self.enterContext(patch.multiple(publisher, SINGLE_PUT_LIMIT=8, DEFAULT_PART_BYTES=5))

    def test_multipart_creates_once_and_rechecks_existing_destination_on_retry(self):
        self.enable_multipart_fixture()
        receipt = publisher.publish(self.options)
        self.assertEqual(publisher.publish(self.options), receipt)
        self.assertEqual(self.export_data, self.data)
        self.assertEqual((self.writes, self.complete_attempts, self.aborts), (1, 2, 1))
        self.export_data = b"d" * len(self.data)
        with self.assertRaisesRegex(publisher.ExportError, "inventory digest mismatch"):
            publisher.publish(self.options)
        self.assertEqual(self.export_data, b"d" * len(self.data))

    def test_multipart_aborts_before_publication_on_failed_part_or_changed_bytes(self):
        self.enable_multipart_fixture()
        for settings, message in (
            ({"failed_part": 2}, "ServiceUnavailable"),
            ({"bad_part_checksum": True}, "different part identity"),
            ({"mutate_spool": True}, "source spool differs"),
        ):
            with self.subTest(settings=settings), patch.multiple(self, **settings):
                with self.assertRaisesRegex(publisher.ExportError, message):
                    publisher.publish(self.options)
                self.assertIsNone(self.parts)
                self.assertIsNone(self.export_data)
        self.assertEqual((self.complete_attempts, self.aborts), (0, 3))

    def test_multipart_completion_conflict_aborts_and_leaves_no_receipt(self):
        self.enable_multipart_fixture()
        self.complete_conflict = "ConditionalRequestConflict"
        path = Path(self.scratch) / "publication.json"
        with self.assertRaisesRegex(publisher.ExportError, "ConditionalRequestConflict"):
            publisher.write_publication(path, lambda: publisher.publish(self.options))
        self.assertFalse(path.exists())
        self.assertEqual((self.complete_attempts, self.aborts), (1, 1))
        self.assertIsNone(self.parts)

    def test_create_and_retry_preserve_verified_bytes(self):
        with patch.object(publisher, "aws", self.aws):
            receipt = publisher.publish(self.options)
            self.assertEqual(publisher.publish(self.options), receipt)
        self.assertEqual(receipt["inventory_sha256"], self.approval)
        self.assertEqual(receipt["schema"], PUBLICATION_SCHEMA)
        self.assertEqual(receipt["base_url"], "https://export.cloudfront.net")
        self.assertEqual(receipt["source_placement"], self.inventory["source_placement"])
        self.assertEqual(receipt["export_placement"],
                         {"resource": "accounts/123456789012/buckets/export-models", "location": "us-east-1"})
        self.assertEqual(receipt["objects"], self.inventory["objects"])
        self.assertEqual(self.export_data, self.data)
        self.assertEqual(self.writes, 1)
        self.assertEqual(list(Path(self.scratch).iterdir()), [self.options.inventory])

    def test_bad_source_or_existing_bytes_cannot_be_published(self):
        replacement = b"x" * len(self.data)
        with patch.object(publisher, "aws", self.aws):
            self.source_data = replacement
            with self.assertRaisesRegex(publisher.ExportError, "digest mismatch"):
                publisher.publish(self.options)
            self.assertIsNone(self.export_data)
            self.source_data = self.data
            self.export_data = replacement
            with self.assertRaisesRegex(publisher.ExportError, "digest mismatch"):
                publisher.publish(self.options)
        self.assertEqual(self.export_data, replacement)
        self.assertEqual(self.writes, 0)

    def test_same_named_source_in_wrong_account_or_region_fails_before_copy(self):
        with patch.object(publisher, "aws", self.aws):
            self.source_region = "eu-west-1"
            with self.assertRaisesRegex(publisher.ExportError, "region differs"):
                publisher.publish(self.options)
            self.source_owner = "999999999999"
            with self.assertRaisesRegex(publisher.ExportError, "AccessDenied"):
                publisher.publish(self.options)
        self.assertEqual(self.writes, 0)

    def test_successful_put_with_corrupt_destination_cannot_seal_receipt(self):
        receipt_path = Path(self.scratch) / "publication.json"
        self.corrupt_destination_write = True
        with (patch.object(publisher, "aws", self.aws),
              self.assertRaisesRegex(publisher.ExportError, "inventory digest mismatch")):
            publisher.write_publication(receipt_path, lambda: publisher.publish(self.options))
        self.assertEqual(self.writes, 1)
        self.assertFalse(receipt_path.exists())

    def test_sealed_receipt_is_content_addressed_and_cannot_be_replaced(self):
        receipt_path = Path(self.scratch) / "publication.json"
        with patch.object(publisher, "aws", self.aws):
            receipt = publisher.write_publication(receipt_path, lambda: publisher.publish(self.options))
        raw = receipt_path.read_bytes()
        self.assertEqual(receipt["publication_sha256"], hashlib.sha256(raw).hexdigest())
        self.assertEqual(json.loads(raw)["inventory_sha256"], self.approval)
        with self.assertRaisesRegex(publisher.ExportError, "path must be new"):
            publisher.write_publication(receipt_path, lambda: self.fail("publisher ran on an existing receipt"))
        self.assertEqual(receipt_path.read_bytes(), raw)

    def test_early_activation_during_copy_cannot_seal_receipt(self):
        receipt_path = Path(self.scratch) / "publication.json"
        self.enable_during_readback = True
        with (patch.object(publisher, "aws", self.aws),
              self.assertRaisesRegex(publisher.ExportError, "public delivery disabled")):
            publisher.write_publication(receipt_path, lambda: publisher.publish(self.options))
        self.assertEqual(self.export_data, self.data)
        self.assertFalse(receipt_path.exists())

    def test_viewer_hostname_or_origin_mismatch_denies_publication(self):
        for name, wrong in (("viewer_host", "other.cloudfront.net"),
                            ("destination_host", "other.s3.us-east-1.amazonaws.com")):
            with self.subTest(name=name):
                current = getattr(self, name)
                setattr(self, name, wrong)
                with (patch.object(publisher, "aws", self.aws),
                      self.assertRaisesRegex(publisher.ExportError, "staged export bucket and viewer hostname")):
                    publisher.publish(self.options)
                setattr(self, name, current)
        self.assertEqual(self.writes, 0)

    def test_export_stack_must_approve_inventory_and_own_bucket(self):
        with patch.object(publisher, "aws", wraps=self.aws) as aws:
            self.enabled = "true"
            with self.assertRaisesRegex(publisher.ExportError, "public delivery disabled"):
                publisher.publish(self.options)
            self.enabled = "false"
            self.physical_bucket = "other-bucket"
            with self.assertRaisesRegex(publisher.ExportError, "dedicated bucket"):
                publisher.publish(self.options)
            self.physical_bucket = "export-models"
            self.approval = "0" * 64
            with self.assertRaisesRegex(publisher.ExportError, "approve this inventory"):
                publisher.publish(self.options)
        self.assertTrue(all(call.args[2] == "cloudformation" for call in aws.call_args_list))

    def test_mutable_invalid_or_duplicate_keys_fail_before_aws_calls(self):
        original = self.inventory["objects"][0]
        for objects in ([original, original], [
            {**original, "path": "v2/root"},
        ], [
            {**original, "path": self.path.replace("/objects/", "/objects/%2e/")},
        ], [
            {**original, "sha256": "0" * 64},
        ]):
            with self.subTest(objects=objects):
                self.inventory["objects"] = objects
                self.write_inventory()
                with patch.object(publisher, "aws") as aws:
                    with self.assertRaises(publisher.ExportError):
                        publisher.publish(self.options)
                    aws.assert_not_called()


if __name__ == "__main__":
    unittest.main()
