"""Protect native qualification's backend isolation and executable pinning."""

import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "qualify_sdk_native_rustfs.py"
SPEC = importlib.util.spec_from_file_location("qualify_sdk_native_rustfs", SCRIPT)
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)


class NativeRustfsTests(unittest.TestCase):
    def test_paginated_empty_listing_allows_bucket_removal(self):
        with patch.object(RUNNER, "aws", return_value={}) as aws:
            RUNNER.cleanup_bucket({}, "owned-fixture")
            self.assertEqual(aws.call_args.args[1:],
                             ("s3api", "delete-bucket", "--bucket", "owned-fixture"))

    def test_remaining_objects_prevent_bucket_removal(self):
        def response(_environment, *arguments):
            if arguments[:2] == ("s3api", "list-objects-v2"):
                return {"Contents": [{"Key": "retained-object"}]}
            if arguments[:2] == ("s3api", "delete-bucket"):
                self.fail("bucket deletion must not run with retained objects")
            return {}

        with patch.object(RUNNER, "aws", side_effect=response):
            with self.assertRaisesRegex(RuntimeError, "objects remain"):
                RUNNER.cleanup_bucket({}, "owned-fixture")

    def test_parent_provider_selectors_cannot_retarget_fixture(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {
            "AWS_PROFILE": "production", "AWS_SESSION_TOKEN": "old-session",
            "AWS_WEB_IDENTITY_TOKEN_FILE": "old-token", "AWS_ROLE_ARN": "old-role",
            "AWS_ENDPOINT_URL_S3": "https://production.invalid",
            "RUSTFS_ACCESS_KEY": "old-server-key", "CRAB_SDK_TEST_S3_BUCKET": "production",
        }):
            root = Path(directory)
            environment = RUNNER.fixture_environment(
                root, root, "http://127.0.0.1:12345", "owned-fixture",
                root / "git", root / "crab", root,
            )
            self.assertEqual(
                {name: value for name, value in environment.items()
                 if name in {"AWS_PROFILE", "AWS_SESSION_TOKEN", "AWS_WEB_IDENTITY_TOKEN_FILE",
                             "AWS_ROLE_ARN", "RUSTFS_ACCESS_KEY", "AWS_ENDPOINT_URL_S3",
                             "CRAB_SDK_TEST_S3_BUCKET"}},
                {"AWS_ENDPOINT_URL_S3": "http://127.0.0.1:12345",
                 "CRAB_SDK_TEST_S3_BUCKET": "owned-fixture"},
            )

    def test_corrupt_cached_release_is_rejected_before_extraction(self):
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(RUNNER.platform, "system", return_value="Darwin"), \
                patch.object(RUNNER.platform, "machine", return_value="arm64"), \
                patch.object(RUNNER.zipfile, "ZipFile") as extract:
            root = Path(directory)
            asset, _digest = RUNNER.ASSETS[("Darwin", "arm64")]
            (root / asset).write_bytes(b"not the pinned executable")
            with self.assertRaisesRegex(RuntimeError, "checksum mismatch"):
                RUNNER.install_rustfs(root)
            extract.assert_not_called()


if __name__ == "__main__":
    unittest.main()
