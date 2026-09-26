"""Evidence and cleanup boundaries for unpublished-tail loss during traffic."""

import copy
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import fault


class TailFaultTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.output = Path(self.directory.name)
        self.project = "crab-cell-issue-fault-test"
        self.path = self.output / "compose.yaml"
        self.path.write_text(json.dumps({"name": self.project}))
        self.control = {"cell": "a" * 64, "incarnation": "b" * 32, "owner": {"session": "old"},
                        "epoch": 3, "state": "serving", "root": {"commit_sequence": 8}}
        self.node = {"live": True, "session": "old", "advertisement": {
            "node": "owner", "log": {"state": "open", "active": True, "epoch": 7,
                                      "member_nodes": ["follower1", "follower2"]}}}
        self.action = {"cell": f"CellId({'a' * 64})", "incarnation": f"IncarnationId({'b' * 32})",
                       "owner_session": "SessionId(old)", "owner": "node-03",
                       "proof": "fleet", "commit_sequence": 9}
        self.driver = fault.TailFault(self.path, (), 3, "http://fixture", 1, "node-03", self.control, self.output)
        self.sample = {"cell": 1, "acknowledged": {"number": 1, "title": "received"}, "operations": []}

    def test_failed_placement_is_reported_before_any_fault_mutation(self):
        image = "sha256:" + "1" * 64
        nodes = {f"node-{index:02d}" for index in range(1, 4)}
        self.path.write_text(json.dumps({"name": self.project, "services": {name: {"image": image} for name in nodes}}))
        output = self.output / "failed-run"

        def failed_placement(_path, _profiles, _nodes, _cells, receipt):
            receipt.update({"passed": False, "samples": [{"owner_counts": {"node-01": 20}}]})
            raise RuntimeError("ownership did not converge")

        with patch.object(sys, "argv", ["fault.py", "--state", str(self.output), "--nodes", "3", "--output", str(output)]), \
                patch.object(fault.load, "running_nodes", return_value=nodes), \
                patch.object(fault, "image_provenance", return_value={"image": image, "source": "a" * 40}), \
                patch.object(fault, "command", return_value="a" * 40), \
                patch.object(fault.load, "wait_for_placement", side_effect=failed_placement), \
                patch.object(fault, "TailFault") as driver, \
                self.assertRaisesRegex(RuntimeError, "ownership did not converge"):
            fault.main()
        driver.assert_not_called()
        report = json.loads((output / "report.json").read_text())
        self.assertFalse(report["passed"])
        self.assertEqual(report["placement"]["samples"][0]["owner_counts"], {"node-01": 20})
        self.assertEqual(report["error"], "RuntimeError: ownership did not converge")

    def test_acknowledgement_must_be_unpublished_and_bound_to_an_active_cohort(self):
        fault.unpublished_acknowledgement(self.action, self.control, "node-03", self.node)
        cases = [("action", "proof", "object"), ("action", "owner", "node-01"),
                 ("action", "commit_sequence", 8), ("action", "owner_session", "SessionId(other)"),
                 ("action", "cell", "CellId(other)"), ("action", "incarnation", "IncarnationId(other)"),
                 ("node", "live", False), ("node", "session", "other"),
                 ("log", "active", False), ("log", "state", "sealed"),
                 ("log", "epoch", 0), ("log", "epoch", True),
                 ("log", "member_nodes", []), ("log", "member_nodes", ["owner"])]
        for surface, field, value in cases:
            action, node = copy.deepcopy(self.action), copy.deepcopy(self.node)
            target = {"action": action, "node": node, "log": node["advertisement"]["log"]}[surface]
            target[field] = value
            with self.subTest(surface=surface, field=field, value=value), self.assertRaises(RuntimeError):
                fault.unpublished_acknowledgement(action, self.control, "node-03", node)

    def test_callback_retains_the_received_write_before_the_pair_changes(self):
        self.driver.on_acknowledged({**self.sample, "cell": 2})
        self.driver.on_acknowledged(self.sample)
        self.sample["operations"].append({"operation": "read"})
        self.driver.on_acknowledged(self.sample)
        self.assertEqual(self.driver.acknowledgements.get_nowait()["operations"], [])
        self.assertTrue(self.driver.acknowledgements.empty())

    def test_post_recovery_trace_join_retains_the_removed_owners_events(self):
        self.driver.receipt["owner_disk_removed"] = True
        (self.output / "failed-owner.log").write_text('event="cell_command_response" commit_sequence=9\n')
        with patch.object(fault, "compose", return_value='event="application_submission" submission_id="received"\n') as compose:
            events = self.driver.trace_events("after-recovery")
        self.assertEqual({event["node"] for event in events}, {"node-01", "node-02", "node-03"})
        self.assertEqual(compose.call_count, 2)
        self.assertTrue(all(call.args[-1] != "node-03" for call in compose.call_args_list))
        self.assertEqual(events[-1]["commit_sequence"], 9)

    def test_policy_cleanup_retains_both_primary_and_cleanup_failures(self):
        for primary in (False, True):
            with self.subTest(primary=primary), \
                    patch.object(self.driver, "install_policy"), \
                    patch.object(self.driver, "clear_policy", side_effect=RuntimeError("policy cleanup failed")), \
                    self.assertRaisesRegex(RuntimeError, "lost acknowledgement" if primary else "policy cleanup failed"):
                with self.driver.publication_denied():
                    if primary:
                        raise RuntimeError("lost acknowledgement")
            self.assertEqual(self.driver.receipt["policy_cleanup"], {"passed": False, "error": "RuntimeError: policy cleanup failed"})

    def test_uncertain_policy_install_still_attempts_cleanup(self):
        with patch.object(self.driver, "install_policy", side_effect=RuntimeError("install response lost")), \
                patch.object(self.driver, "clear_policy") as clear, \
                self.assertRaisesRegex(RuntimeError, "install response lost"):
            with self.driver.publication_denied():
                self.fail("uncertain installation must not start the workload")
        clear.assert_called_once_with()

    def test_transport_failures_cannot_prove_an_intentional_policy_denial(self):
        for detail in ("Could not connect to the endpoint URL", "An error occurred (NoSuchBucket) when calling the PutObject operation"):
            result = subprocess.CompletedProcess(["aws"], 255, "", detail)
            with self.subTest(detail=detail), patch.object(fault.subprocess, "run", return_value=result), \
                    self.assertRaises(subprocess.CalledProcessError):
                self.driver.aws("put-object", missing="AccessDenied")
        result.stderr = "An error occurred (AccessDenied) when calling the PutObject operation: Access Denied."
        with patch.object(fault.subprocess, "run", return_value=result):
            self.assertIsNone(self.driver.aws("put-object", missing="AccessDenied"))

    def test_kill_requires_the_same_cohort_and_a_project_owned_disposable_volume(self):
        for case in ("cohort_changed", "other_project", "bind_mount", "success"):
            with self.subTest(case=case):
                self.driver.receipt.clear()
                self.driver.on_acknowledged(self.sample)
                last_node = copy.deepcopy(self.node)
                if case == "cohort_changed":
                    last_node["advertisement"]["log"]["epoch"] += 1
                after = {**self.control, "epoch": 4, "owner": {"session": "new"}, "root": {"commit_sequence": 9}}
                commands = []

                def command(*args):
                    commands.append(args)
                    if args[:2] == ("docker", "ps"):
                        return "c" * 12
                    if args[:2] == ("docker", "inspect"):
                        return json.dumps([{"Mounts": [{"Destination": "/var/lib/crab/cells", "Name": "fixture-data",
                                                        "Type": "bind" if case == "bind_mount" else "volume"}]}])
                    if args[:3] == ("docker", "volume", "inspect"):
                        return json.dumps([{"Labels": {"com.docker.compose.project": "unrelated" if case == "other_project" else self.project}}])
                    return ""

                def compose(*args):
                    if args[-1] == "metrics":
                        return "crab_cell_node_log_uncovered_bytes 512\n"
                    return {"node-01": "follower1", "node-02": "follower2", "node-03": "owner"}[args[4]]

                with patch.object(self.driver, "trace_events", return_value=[]), \
                        patch.object(fault.action_traces, "join", return_value=[self.action]), \
                        patch.object(self.driver, "status", side_effect=[self.control, self.control, after]), \
                        patch.object(self.driver, "cli", side_effect=[json.dumps(self.node), json.dumps(last_node)]), \
                        patch.object(fault, "compose", side_effect=compose), \
                        patch.object(fault, "command", side_effect=command), \
                        patch.object(fault.subprocess, "run"), \
                        patch.object(self.driver, "clear_policy"), \
                        patch.object(fault.load, "request", return_value={"body": self.sample["acknowledged"]}):
                    if case == "success":
                        self.driver.run()
                    else:
                        with self.assertRaises(RuntimeError):
                            self.driver.run()
                effects = [args for args in commands if args[1] in ("kill", "rm") or args[1:3] == ("volume", "rm")]
                self.assertEqual(effects, [
                    ("docker", "kill", "--signal", "KILL", "c" * 12),
                    ("docker", "rm", "c" * 12), ("docker", "volume", "rm", "fixture-data"),
                ] if case == "success" else [])
                if case == "success":
                    self.assertTrue(self.driver.receipt["owner_disk_removed"])
                    self.assertEqual(self.driver.receipt["control_after"]["owner"]["session"], "new")


class DuplicateResultTests(unittest.TestCase):
    def setUp(self):
        self.issue = {"title": "fleet-load-fixture-01-000000", "number": 2}
        self.sample = {"cell": 1, "acknowledged": self.issue}

    def test_filtered_empty_pages_continue_and_ambiguous_extra_results_are_allowed(self):
        pages = [{"items": [], "next": 20},
                 {"items": [{"title": "fleet-load-fixture-01-000001", "number": 3}, self.issue], "next": None}]
        with patch.object(fault.load, "load_request", side_effect=[{"outcome": "success", "body": page} for page in pages]) as request:
            proof = fault.verify_unique_results("gateway", 3, [self.sample], "fixture")
        self.assertIn("&before=20", request.call_args.args[-1])
        self.assertEqual(proof, {1: {"unique_results": 2, "acknowledged": 1}})

    def test_duplicate_effects_missing_results_and_looping_cursors_fail_closed(self):
        for pages in (
            [{"items": [self.issue, {**self.issue, "number": 3}], "next": None}],
            [{"items": [], "next": None}],
            [{"items": [], "next": 4}, {"items": [], "next": 4}],
        ):
            with self.subTest(pages=pages), \
                    patch.object(fault.load, "load_request", side_effect=[{"outcome": "success", "body": page} for page in pages]), \
                    self.assertRaises(RuntimeError):
                fault.verify_unique_results("gateway", 3, [self.sample], "fixture")


if __name__ == "__main__":
    unittest.main()
