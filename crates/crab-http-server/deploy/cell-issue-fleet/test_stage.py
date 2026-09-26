"""Keep fixture provisioning ahead of every node's initial catalog snapshot."""

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import qualify


class NodesStarted(Exception):
    pass


class StageProvisioningTests(unittest.TestCase):
    def run_startup(self, previous=0, failure=None):
        cells = 20
        catalog = set(range(1, cells + 1)) if previous else set()
        initialized = bool(previous)
        started = False
        created = []

        def compose(_path, _profiles, *args):
            nonlocal initialized, started
            if args == ("run", "--rm", "repository-init"):
                if failure == "release":
                    raise RuntimeError("release bootstrap failed")
                initialized = True
            elif args[0] == "up":
                # A serving peer authorizes against its startup snapshot. A
                # later create cannot make that peer ready before its poll.
                self.assertEqual(catalog, set(range(1, cells + 1)))
                started = True
            else:
                self.fail(f"unexpected Compose operation: {args}")

        def create(_path, _profiles, index):
            self.assertTrue(initialized)
            self.assertFalse(started)
            if failure == "repository" and index == cells:
                raise RuntimeError("repository provisioning failed")
            catalog.add(index)
            created.append(index)

        with tempfile.TemporaryDirectory() as state, \
                patch.object(qualify, "compose", side_effect=compose), \
                patch.object(qualify, "create_repository", side_effect=create), \
                patch.object(qualify, "request_json", return_value={}), \
                patch.object(qualify, "prove_node", side_effect=NodesStarted):
            path = Path(state) / "compose.yaml"
            if failure:
                with self.assertRaisesRegex(RuntimeError, "failed"):
                    qualify.run_stage(path, (), previous, 3, 18080, 18100, cells)
                self.assertFalse(started)
            else:
                with self.assertRaises(NodesStarted):
                    qualify.run_stage(path, (), previous, 5 if previous else 3, 18080, 18100, cells)
                self.assertTrue(started)
        return created

    def test_initial_nodes_load_the_complete_fixture_catalog(self):
        self.assertEqual(self.run_startup(), list(range(1, 21)))

    def test_scale_out_preserves_the_original_repositories(self):
        self.assertEqual(self.run_startup(previous=3), [])

    def test_failed_provisioning_never_starts_traffic_nodes(self):
        for failure in ("release", "repository"):
            with self.subTest(failure=failure):
                self.run_startup(failure=failure)


if __name__ == "__main__":
    unittest.main()
