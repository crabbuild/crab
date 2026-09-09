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
    def test_forbidden_dependencies_and_missing_owners_are_detected(self):
        for profile, extra in [("default", "tokio"), ("default", "crab-storage"),
                               ("remote", "crab-read"), ("remote-content", "crab-vfs"),
                               ("remote-content", "crab-http-server"), ("remote", "crab"),
                               ("write", "crab"), ("write", "crab-auth-server")]:
            with self.subTest(profile=profile, extra=extra):
                forbidden, _ = module.violations(
                    profile, set(module.PROFILES[profile]), {"crab-sdk", extra}
                )
                self.assertIn(extra, forbidden)
        self.assertEqual(
            module.violations(
                "remote-content", set(module.PROFILES["remote-content"]),
                {"crab-sdk", "crab-storage", "crab-remote-git"},
            ),
            ([], ["crab-lfs", "crab-read"]),
        )
        self.assertEqual(
            module.violations(
                "write", set(module.PROFILES["write"]),
                {"crab-sdk", "crab-storage", "crab-remote-git", "crab-read", "crab-lfs"},
            ),
            ([], ["crab-coordination", "crab-remote", "crab-write"]),
        )


if __name__ == "__main__":
    unittest.main()
