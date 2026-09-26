"""Keep overloaded measurements distinct from failed integrity qualification."""

import contextlib
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import load


class CurvePointTests(unittest.TestCase):
    def test_only_fully_verified_points_can_return_the_overload_status(self):
        for failure in (None, "overload", "contract", "drain", "trace", "readback", "recovery", "resources"):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                state = Path(directory)
                nodes = ["node-01", "node-02", "node-03"]
                services = {name: {"image": "sha256:fixture", "cpus": 1, "environment": {}}
                            for name in nodes}
                services["rustfs"] = {"image": "rustfs-fixture"}
                (state / "compose.yaml").write_text(json.dumps({"name": "fixture", "services": services}))
                output = state / "load.json"
                args = ["load.py", "--state", directory, "--nodes", "3", "--cells", "3", "--output", str(output)]
                samples = [{
                    "cell": index, "dispatch_delay_ms": 0, "acknowledged": {"number": 1},
                    "operations": [{"operation": "write", "entry": name, "outcome": "success",
                                    "retries": 0, "retry_reasons": [], "latency_ms": 1}],
                } for index, name in enumerate(nodes, 1)]
                summary = {"elapsed_seconds": 1, "planned_pairs": 3 if failure is None else 6,
                           "outcomes": {"success": 3}, "stopped_on_invariant": failure == "contract"}
                controls = {index: {"root": {"commit_sequence": 1}} for index in range(1, 4)}
                actions = [{"proof": "object", "owner": name, "entry": name, "http_latency_ms": 1,
                            "phases": {"http_response_ready_us": 900, "client_invocation_us": 600},
                            "captures": [{"capture_ns": 1000}]} for name in nodes]
                responses = {
                    "running_nodes": set(nodes), "image_provenance": {"image": "sha256:fixture"},
                    "owner_map": ({index: name for index, name in enumerate(nodes, 1)}, controls),
                    "cover_routes": ({}, []), "command": "fixture", "compose": "",
                    "observe_nodes": {"errors": ["missing observation"] if failure == "resources" else []},
                    "scheduled_load": (summary, samples), "drain_publication": {"drained": failure != "drain"},
                    "verify_acknowledged": {"verified": 3}, "verify_roots": controls, "recover_owner": None,
                }
                with contextlib.ExitStack() as stack:
                    stack.enter_context(patch.object(load.sys, "argv", args))
                    mocks = {name: stack.enter_context(patch.object(load, name, return_value=value))
                             for name, value in responses.items()}
                    stack.enter_context(patch.object(load.action_traces, "parse_log", return_value=[]))
                    join = stack.enter_context(patch.object(load.action_traces, "join", return_value=actions))
                    if failure == "trace":
                        join.side_effect = ValueError("missing acknowledgement trace")
                    if failure == "readback":
                        mocks["verify_acknowledged"].side_effect = RuntimeError("acknowledged value missing")
                    if failure == "recovery":
                        mocks["recover_owner"].side_effect = RuntimeError("owner restart failed")
                    if failure in (None, "overload"):
                        self.assertEqual(load.main(), 0 if failure is None else 2)
                    else:
                        with self.assertRaises((RuntimeError, ValueError)):
                            load.main()
                receipt = json.loads(output.read_text())
                self.assertEqual(receipt["integrity_verified"], failure in (None, "overload"))
                self.assertEqual(receipt["passed"], failure is None)
                if failure:
                    self.assertIn("error", receipt)
                if failure in (None, "overload"):
                    self.assertEqual(receipt["action_traces"]["latency"]["local"]["http"]["count"], 3)
                    mocks["verify_acknowledged"].assert_called_once()
                    mocks["recover_owner"].assert_called_once()


if __name__ == "__main__":
    unittest.main()
