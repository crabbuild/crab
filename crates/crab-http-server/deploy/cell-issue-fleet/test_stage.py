"""Keep fixture provisioning ahead of every node's initial catalog snapshot."""

import json
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
        expected = {index: qualify.initial_issue(index) for index in range(1, cells + 1)}

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
                    qualify.run_stage(path, (), previous, 3, 18080, 18100, expected)
                self.assertFalse(started)
            else:
                with self.assertRaises(NodesStarted):
                    qualify.run_stage(path, (), previous, 5 if previous else 3, 18080, 18100, expected)
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

    def test_scale_out_reads_the_acknowledged_update_and_rejects_old_body(self):
        expected = {index: qualify.initial_issue(index) for index in (1, 2)}
        expected[1]["body"] = "acknowledged before scale-out"
        for stale in (False, True):
            with self.subTest(stale=stale):
                def request(_method, url):
                    if url.endswith("/livez"):
                        return {}
                    if url.endswith("/labels"):
                        return {"items": [{"name": "distributed"}]}
                    index = int(url.split("work-")[1][:2])
                    return qualify.initial_issue(index) if stale else expected[index]

                def compose(_path, _profiles, *args):
                    if args[0] == "up":
                        return ""
                    self.assertEqual(args[-2], "--name")
                    index = int(args[-1].split("-")[1])
                    return json.dumps({"state": "serving", "owner": {"session": f"session{index}"},
                                       "root": {"commit_sequence": 7}})

                with patch.object(qualify, "compose", side_effect=compose), \
                        patch.object(qualify, "request_json", side_effect=request), \
                        patch.object(qualify, "prove_node", side_effect=lambda _p, _f, i:
                                     (f"session{i}", {"admission": {"active_cells": 8}}, f"container{i}")), \
                        patch.object(qualify, "object_count", return_value=1), \
                        patch.object(qualify, "command", return_value=""):
                    if stale:
                        with self.assertRaisesRegex(RuntimeError, "did not read Cell 1"):
                            qualify.run_stage(Path("fixture"), ("five",), 3, 5, 18080, 18100, expected)
                    else:
                        stage = qualify.run_stage(Path("fixture"), ("five",), 3, 5, 18080, 18100, expected)
                        self.assertEqual(stage["owners"], {"work-01": "node-01", "work-02": "node-02"})


if __name__ == "__main__":
    unittest.main()
