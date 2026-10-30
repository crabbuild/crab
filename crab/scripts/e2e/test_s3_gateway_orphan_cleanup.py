"""Keep command failure evidence useful without exposing signing material."""
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run_s3_gateway_orphan_cleanup as cleanup


class CommandFailureEvidenceTests(unittest.TestCase):
    def test_aws_error_code_is_preserved_and_credentials_are_redacted(self):
        stderr = (
            "An error occurred (SlowDown) when calling UploadPart: retry. "
            "secret=fixture-secret access=fixture-access token=fixture-token\n"
            "Authorization: AWS4-HMAC-SHA256 Credential=private-value\n"
            "https://example.invalid/object?X-Amz-Signature=private-signature\n"
            "X-Amz-Signature=another-signature"
        )
        environment = {"AWS_SECRET_ACCESS_KEY": "fixture-secret",
                       "AWS_ACCESS_KEY_ID": "fixture-access",
                       "AWS_SESSION_TOKEN": "fixture-token"}
        with patch.object(cleanup.subprocess, "run", return_value=
                          subprocess.CompletedProcess([], 254, "", stderr)):
            with self.assertRaises(cleanup.QualificationError) as raised:
                cleanup._run("aws", "--endpoint-url", "https://example.invalid",
                             "s3api", "upload-part", environment=environment)
        message = str(raised.exception)
        self.assertIn("aws upload-part", message)
        self.assertIn("SlowDown", message)
        for secret in (*environment.values(), "private-value", "private-signature",
                       "another-signature", "https://example.invalid"):
            self.assertNotIn(secret, message)

    def test_expected_nonzero_result_remains_available_to_the_caller(self):
        result = subprocess.CompletedProcess([], 254, "", "NoSuchKey")
        with patch.object(cleanup.subprocess, "run", return_value=result):
            self.assertIs(cleanup._run("aws", "s3api", "head-object", check=False), result)


if __name__ == "__main__":
    unittest.main()
