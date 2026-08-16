from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).with_name("run_add_commit_push_rustfs_smoke.py")
SPEC = importlib.util.spec_from_file_location("run_add_commit_push_rustfs_smoke", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot import {SCRIPT}")
SMOKE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = SMOKE
SPEC.loader.exec_module(SMOKE)


class CredentialSelectionTests(unittest.TestCase):
    def test_runtime_credential_precedes_development_default(self) -> None:
        selected = SMOKE.credential_default(
            "AWS_ACCESS_KEY_ID",
            "development-access",
            {"AWS_ACCESS_KEY_ID": "runtime-access"},
        )

        self.assertEqual(selected, "runtime-access")

    def test_blank_runtime_credential_uses_development_default(self) -> None:
        selected = SMOKE.credential_default(
            "AWS_SECRET_ACCESS_KEY",
            "development-secret",
            {"AWS_SECRET_ACCESS_KEY": "  "},
        )

        self.assertEqual(selected, "development-secret")

    def test_default_region_uses_aws_default_region(self) -> None:
        environment = {
            "AWS_DEFAULT_REGION": "ca-central-1",
            "PATH": sys.path[0],
        }
        with mock.patch.dict(SMOKE.os.environ, environment, clear=True), mock.patch.object(
            sys, "argv", [str(SCRIPT)]
        ):
            args = SMOKE.parse_args()

        self.assertEqual(args.region, "ca-central-1")


class RedactionTests(unittest.TestCase):
    def test_environment_redacts_credentials_but_keeps_endpoint(self) -> None:
        redacted = SMOKE.redact_env(
            {
                "AWS_ACCESS_KEY_ID": "access-value",
                "AWS_SECRET_ACCESS_KEY": "secret-value",
                "AWS_SESSION_TOKEN": "token-value",
                "AWS_ENDPOINT_URL": "http://127.0.0.1:9000",
                "PATH": "/bin",
            }
        )

        self.assertEqual(redacted["AWS_ACCESS_KEY_ID"], "<redacted>")
        self.assertEqual(redacted["AWS_SECRET_ACCESS_KEY"], "<redacted>")
        self.assertEqual(redacted["AWS_SESSION_TOKEN"], "<redacted>")
        self.assertEqual(redacted["AWS_ENDPOINT_URL"], "http://127.0.0.1:9000")
        self.assertNotIn("PATH", redacted)

    def test_command_arguments_redact_split_and_equals_forms(self) -> None:
        redacted = SMOKE.redact_command_args(
            [
                "runner",
                "--access-key",
                "access-value",
                "--secret-key=secret-value",
                "--session-token",
                "token-value",
                "--region",
                "us-east-1",
            ]
        )

        self.assertEqual(
            redacted,
            [
                "runner",
                "--access-key",
                "<redacted>",
                "--secret-key=<redacted>",
                "--session-token",
                "<redacted>",
                "--region",
                "us-east-1",
            ],
        )

    def test_credential_leak_scan_reports_labels_without_secret_values(self) -> None:
        leaks = SMOKE.find_credential_leaks(
            {"report": "safe", "stderr.log": "failure for runtime-secret-value"},
            {
                "access_key": "runtime-access-value",
                "secret_key": "runtime-secret-value",
                "session_token": "",
            },
        )

        self.assertEqual(
            leaks, [{"credential": "secret_key", "source": "stderr.log"}]
        )
        self.assertNotIn("runtime-secret-value", repr(leaks))

    def test_command_output_and_nested_details_redact_runtime_credentials(self) -> None:
        credentials = {
            "access_key": "runtime-access-value",
            "secret_key": "runtime-secret-value",
            "session_token": "runtime-token-value",
        }

        text = SMOKE.redact_credential_text(
            "runtime-access-value runtime-secret-value runtime-token-value",
            credentials,
        )
        detail = SMOKE.redact_credential_value(
            {"error": ["runtime-secret-value", {"token": "runtime-token-value"}]},
            credentials,
        )

        self.assertEqual(text, "<redacted> <redacted> <redacted>")
        self.assertEqual(
            detail, {"error": ["<redacted>", {"token": "<redacted>"}]}
        )


if __name__ == "__main__":
    unittest.main()
