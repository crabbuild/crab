"""Protect native qualification's backend isolation and executable pinning."""

import importlib.util
from contextlib import contextmanager
import json
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

SCRIPT = Path(__file__).resolve().parents[1] / "qualify_sdk_native_rustfs.py"
SPEC = importlib.util.spec_from_file_location("qualify_sdk_native_rustfs", SCRIPT)
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)
DESKTOP_SPEC = importlib.util.spec_from_file_location(
    "qualify_desktop_v2", SCRIPT.parents[2] / "packages/desktop/scripts/qualify-v2.py",
)
DESKTOP = importlib.util.module_from_spec(DESKTOP_SPEC)
DESKTOP_SPEC.loader.exec_module(DESKTOP)


class NativeRustfsTests(unittest.TestCase):
    def test_desktop_storage_smoke_uses_the_platform_bundle_and_owned_fixture(self):
        for platform, system, bundle, cli in (
            ("linux", "posix", "linux-unpacked/crab", "linux-unpacked/resources/agent/crab"),
            ("darwin", "posix", "mac-arm64/Crab.app/Contents/MacOS/Crab", "mac-arm64/Crab.app/Contents/Resources/agent/crab"),
            ("win32", "nt", "win-unpacked/Crab.exe", "win-unpacked/resources/agent/crab.exe"),
        ):
            with self.subTest(platform=platform), tempfile.TemporaryDirectory() as directory:
                target = Path(directory).resolve()
                executable = DESKTOP.DESKTOP / "release" / bundle
                crab = DESKTOP.DESKTOP / "release" / cli
                environment = {"AWS_ACCESS_KEY_ID": "fixture-access", "AWS_SECRET_ACCESS_KEY": "fixture-secret"}
                fixture = SimpleNamespace(
                    install_rustfs=Mock(return_value=(target / "rustfs", "asset", "digest")),
                    rustfs_work_location=Mock(return_value=(target, "storage-", "data")),
                    fixture_environment=Mock(return_value=environment),
                )
                descriptor = target / "fixture.json"

                @contextmanager
                def prepare(*arguments):
                    self.assertEqual(arguments, (target, crab, target / "rustfs"))
                    yield descriptor

                identity = SimpleNamespace(prepare=prepare)
                server = Mock()
                result = SimpleNamespace(returncode=0)
                loader = SimpleNamespace(loader=SimpleNamespace(exec_module=lambda _module: None))
                with patch.object(DESKTOP, "os", SimpleNamespace(name=system, environ={"CARGO_TARGET_DIR": str(target)})), \
                        patch.object(DESKTOP, "sys", SimpleNamespace(platform=platform), create=True), \
                        patch.object(DESKTOP.importlib.util, "spec_from_file_location", return_value=loader), \
                        patch.object(DESKTOP.importlib.util, "module_from_spec", side_effect=[fixture, identity]), \
                        patch.object(DESKTOP.shutil, "which", side_effect=lambda name: name), \
                        patch.object(DESKTOP.subprocess, "Popen", return_value=server), \
                        patch.object(DESKTOP.subprocess, "run", return_value=result) as run:
                    DESKTOP.main()
                self.assertEqual(fixture.fixture_environment.call_args.args[5], crab)
                smoke = run.call_args
                self.assertEqual(smoke.args[0], ["node", "scripts/smoke.mjs"])
                self.assertEqual(smoke.kwargs["env"]["CRAB_PACKAGED_APP"], str(executable))
                self.assertEqual(smoke.kwargs["env"]["CRAB_DESKTOP_STORAGE_IDENTITY_FIXTURE"], str(descriptor))
                server.terminate.assert_called_once_with()
                server.wait.assert_called_once_with(timeout=15)

    def test_windows_fixture_leaves_room_for_deep_object_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            runner_temp = Path(directory) / "runner-temp"
            runner_temp.mkdir()
            output = runner_temp / "output"
            output.mkdir()
            with patch.object(RUNNER, "os", SimpleNamespace(name="nt")):
                parent, prefix, data_name = RUNNER.rustfs_work_location(output)
            self.assertEqual(parent, Path(directory))
            self.assertEqual((prefix, data_name), ("r", "d"))
            self.assertLess(len(str(parent / f"{prefix}12345678" / data_name)),
                            len(str(runner_temp / "rfs-12345678" / "data")))

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
                             "CRAB_SDK_TEST_S3_BUCKET", "CRAB_SDK_TEST_S3_CLEANUP"}},
                {"AWS_ENDPOINT_URL_S3": "http://127.0.0.1:12345",
                 "CRAB_SDK_TEST_S3_BUCKET": "owned-fixture",
                 "CRAB_SDK_TEST_S3_CLEANUP": "fixture"},
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
