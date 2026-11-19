"""Exercise Results source binding and native package selection."""
import hashlib
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "qualify_results.py"
SPEC = importlib.util.spec_from_file_location("qualify_results", SCRIPT)
qualification = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(qualification)


class SourceBindingTests(unittest.TestCase):
    def test_results_workflow_changes_invalidate_the_source_binding(self):
        target = Path(os.environ["CARGO_TARGET_DIR"])
        target.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=target) as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "--initial-branch=main", str(root)],
                           check=True, capture_output=True)
            for name in ["desktop.yml", "workflow-native.yml", "sdk.yml"]:
                with self.subTest(workflow=name):
                    workflow = root / ".github/workflows" / name
                    workflow.parent.mkdir(parents=True, exist_ok=True)
                    workflow.write_text("jobs: {native: {runs-on: windows-2022}}\n")
                    # Match tracked producers without global ignore rules.
                    subprocess.run(["git", "-C", str(root), "add", "--force", str(workflow)],
                                   check=True, capture_output=True)
                    with patch.object(qualification, "ROOT", root):
                        before = qualification.source_tree()
                        workflow.write_text("jobs: {native: {runs-on: macos-15}}\n")
                        self.assertNotEqual(qualification.source_tree(), before)


class PackageBindingTests(unittest.TestCase):
    def test_native_package_binding_uses_the_current_platform_and_architecture(self):
        layouts = [
            ("Darwin", "arm64", "mac-arm64/Crab.app/Contents/MacOS/Crab", ""),
            ("Darwin", "x86_64", "mac/Crab.app/Contents/MacOS/Crab", ""),
            ("Linux", "x86_64", "linux-unpacked/crab", ""),
            ("Windows", "AMD64", "win-unpacked/Crab.exe", ".exe"),
        ]
        with tempfile.TemporaryDirectory(dir=os.environ["CARGO_TARGET_DIR"]) as directory:
            root = Path(directory)
            for system, machine, relative, suffix in layouts:
                app = root / "packages/desktop/release" / relative
                app.parent.mkdir(parents=True)
                app.touch()
                resources = app.parent.parent / "Resources" if system == "Darwin" else app.parent / "resources"
                (resources / "agent").mkdir(parents=True)
                for name in ("crab", "crab-desktop-agent"):
                    (resources / "agent" / (name + suffix)).write_bytes(f"{machine}-{system}-{name}".encode())
                (resources / "app.asar").write_bytes(f"{machine}-{system}-application".encode())
            for system, machine, _, _ in layouts:
                with self.subTest(system=system, machine=machine), \
                     patch.object(qualification, "ROOT", root), \
                     patch.object(qualification.platform, "system", return_value=system), \
                     patch.object(qualification.platform, "machine", return_value=machine), \
                     patch.dict("os.environ", {"CRAB_PACKAGED_APP": ""}):
                    proof = {"binaries": {}}
                    qualification.desktop_proof(proof)
                    expected = {key: hashlib.sha256(f"{machine}-{system}-{name}".encode()).hexdigest()
                                for key, name in [("packaged_cli", "crab"), ("agent", "crab-desktop-agent"),
                                                  ("application", "application")]}
                    self.assertEqual(proof["binaries"], expected)

    def test_a_foreign_package_cannot_replace_a_missing_native_package(self):
        with tempfile.TemporaryDirectory(dir=os.environ["CARGO_TARGET_DIR"]) as directory:
            root = Path(directory)
            app = root / "packages/desktop/release/mac-arm64/Crab.app/Contents/MacOS/Crab"
            app.parent.mkdir(parents=True)
            app.touch()
            with patch.object(qualification, "ROOT", root), \
                 patch.object(qualification.platform, "system", return_value="Windows"), \
                 patch.dict("os.environ", {"CRAB_PACKAGED_APP": ""}):
                with self.assertRaisesRegex(RuntimeError, "Build the unpacked desktop"):
                    qualification.desktop_proof({"binaries": {}})


if __name__ == "__main__":
    unittest.main()
