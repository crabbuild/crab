"""Protect native qualification's backend isolation and executable pinning."""

import importlib.util
import json
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
    def test_preflight_verifies_single_and_bulk_deletion(self):
        with tempfile.TemporaryDirectory() as directory:
            body = b"SDK storage contract probe\n"
            deleted = []

            def response(_environment, *arguments):
                if arguments[:2] == ("s3api", "put-object"):
                    body_path = Path(arguments[arguments.index("--body") + 1])
                    self.assertEqual(body_path.read_bytes(), body)
                if arguments[:2] == ("s3api", "get-object"):
                    Path(arguments[-1]).write_bytes(body)
                if arguments[:2] == ("s3api", "create-multipart-upload"):
                    return {"UploadId": "owned-upload"}
                if arguments[:2] == ("s3api", "upload-part"):
                    return {"ETag": '"owned-etag"'}
                if arguments[:2] == ("s3api", "delete-object"):
                    deleted.append(arguments[arguments.index("--key") + 1])
                if arguments[:2] == ("s3api", "delete-objects"):
                    payload = json.loads(arguments[arguments.index("--delete") + 1])
                    keys = [entry["Key"] for entry in payload["Objects"]]
                    deleted.extend(keys)
                    return {"Deleted": [{"Key": key} for key in keys]}
                return {}

            with patch.object(RUNNER, "aws", side_effect=response):
                RUNNER.preflight_backend({}, Path(directory), "owned-fixture")

            self.assertCountEqual(deleted, [
                "qualification/backend-probe/retained",
                "qualification/backend-probe/False/False",
                "qualification/backend-probe/False/True",
                "qualification/backend-probe/True/False",
                "qualification/backend-probe/True/True",
            ])

    def test_bulk_delete_per_key_errors_and_missing_acknowledgements_fail(self):
        for result in ({"Errors": [{"Code": "ServiceUnavailable"}]}, {}):
            with self.subTest(result=result), tempfile.TemporaryDirectory() as directory:
                payload = b""

                def response(_environment, *arguments):
                    nonlocal payload
                    if arguments[:2] == ("s3api", "put-object"):
                        payload = Path(arguments[-1]).read_bytes()
                    if arguments[:2] == ("s3api", "get-object"):
                        Path(arguments[-1]).write_bytes(payload)
                    if arguments[:2] == ("s3api", "delete-objects"):
                        return result
                    return {}

                with patch.object(RUNNER, "aws", side_effect=response):
                    with self.assertRaisesRegex(RuntimeError, "bulk deletion"):
                        RUNNER.preflight_backend({}, Path(directory), "owned-fixture")

    def test_backend_errors_retain_diagnostics_without_credentials(self):
        environment = {"AWS_ENDPOINT_URL": "http://127.0.0.1:12345",
                       "AWS_ACCESS_KEY_ID": "fixture-access", "AWS_SECRET_ACCESS_KEY": "fixture-secret"}
        result = RUNNER.subprocess.CompletedProcess([], 1, "", "InternalError fixture-access fixture-secret")
        with patch.object(RUNNER.subprocess, "run", return_value=result):
            with self.assertRaisesRegex(RuntimeError, "InternalError") as failure:
                RUNNER.aws(environment, "s3api", "delete-object")
        self.assertNotIn("fixture-access", str(failure.exception))
        self.assertNotIn("fixture-secret", str(failure.exception))

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
