"""Negative checks for SDK dependency-boundary enforcement."""

import importlib.util
from pathlib import Path
import unittest


spec = importlib.util.spec_from_file_location(
    "sdk_features", Path(__file__).resolve().parents[1] / "check-sdk-features.py"
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class FeatureBoundaryTests(unittest.TestCase):
    def test_metadata_capture_is_available_without_hydration(self):
        self.assertEqual(
            module.violations("remote", {"remote"},
                              {"crab-sdk", "crab-read", "crab-storage", "crab-remote-git"}),
            ([], []),
        )

    def test_forbidden_dependencies_and_missing_owners_are_detected(self):
        for profile, extra in [("default", "tokio"), ("default", "crab-storage"),
                               ("remote", "crab-cache"), ("remote-files", "crab-vfs"),
                               ("remote-files", "crab-server"), ("remote", "crab"),
                               ("publish", "crab"), ("publish", "crab-auth-server"),
                               ("results", "crab-cache"), ("remote", "crab-workflow")]:
            with self.subTest(profile=profile, extra=extra):
                forbidden, _ = module.violations(
                    profile, set(module.PROFILES[profile]), {"crab-sdk", extra}
                )
                self.assertIn(extra, forbidden)
        self.assertEqual(
            module.violations(
                "remote-files", set(module.PROFILES["remote-files"]),
                {"crab-sdk", "crab-storage", "crab-remote-git"},
            ),
            ([], ["crab-lfs", "crab-read"]),
        )
        self.assertEqual(
            module.violations(
                "publish", set(module.PROFILES["publish"]),
                {"crab-sdk", "crab-storage", "crab-remote-git", "crab-read", "crab-lfs"},
            ),
            ([], ["crab-coordination", "crab-remote", "crab-write"]),
        )

    def test_results_requires_its_workflow_owner_without_enabling_hydration(self):
        packages = {"crab-sdk", "crab-read", "crab-storage", "crab-remote-git"}
        self.assertEqual(
            module.violations("results", {"results"}, packages),
            ([], ["crab-workflow"]),
        )
        self.assertEqual(
            module.violations("results", {"results"}, packages | {"crab-workflow"}),
            ([], []),
        )


if __name__ == "__main__":
    unittest.main()
