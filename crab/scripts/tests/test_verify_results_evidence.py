import importlib.util
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from copy import deepcopy
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "verify_results_evidence.py"
SPEC = importlib.util.spec_from_file_location("results_evidence", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ResultsEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.proof = {"schema": 1, "passed": True, "source_sha": "a"*40,
                      "source_tree": "b"*64, "runtime_sha256": "c"*64,
                      "checks": dict.fromkeys(MODULE.CORE | MODULE.FAULTS | MODULE.DESKTOP | MODULE.HTTP | MODULE.BROWSER, True),
                      "tests": {"core": 1, "checkpoint": 1, "inputs": 5, "settings": 1, "workspace": 8, "migration": 3, "restore": 2, "charts":12, "templates":9, "authored_plots":2, "partial_templates":1, "matrices":5, "matrix_cli":1, "faults": 2, "transfers": 1, "transport": 1, "sdk":1, "discovery":1, "agent": 1, "agent_errors": 3, "desktop": 5, "http": 2, "browser":4},
                      "binaries": dict.fromkeys(["cli", "packaged_cli", "agent", "application"], "d"*64),
                      "provider": "native-rustfs-s3", "platform": "Darwin-arm64",
                      "workflow_run": "qualification", "workflow_attempt": "1"}
        self.proof["resources"] = {"desktop": {
            "schema": 1, "report": "e" * 64, "cycles": 20, "warmup_cycles": 5,
            "samples": [{"cycle": index, "heap_bytes": 10000,
                         "fd_counts": {"main": 12, "agent": 12},
                         "readers": {"active": 0, "peak": 2, "started": 10 + index, "finished": 10 + index}}
                        for index in range(21)],
        }}
        self.proof["resources"]["desktop"]["after_cancellation"] = {
            "active": 0, "peak": 2, "started": 40, "finished": 40,
        }

    def verify(self, proof):
        binding = {"source_sha": "a"*40, "source_tree": "b"*64, "runtime_sha256": "c"*64,
                   "platform": "Darwin-arm64", "workflow_run": "qualification", "workflow_attempt": "1",
                   "binaries": dict.fromkeys(["cli", "packaged_cli", "agent", "application"], "d"*64)}
        MODULE.verify(proof, binding, True, True, True)

    def test_accepts_complete_bound_evidence(self):
        self.verify(self.proof)

    def test_rejects_missing_or_unexecuted_schema_discovery(self):
        for mutation in [lambda proof: proof["checks"].pop("schema_discovery"),
                         lambda proof: proof["tests"].update(discovery=0)]:
            with self.subTest(mutation=mutation):
                proof = deepcopy(self.proof)
                mutation(proof)
                with self.assertRaises(ValueError):
                    self.verify(proof)

    def test_rejects_missing_unfinished_and_growing_resource_measurements(self):
        mutations = [
            lambda p: p.pop("resources"),
            lambda p: p.update(resources=[]),
            lambda p: p["checks"].pop("desktop_resource_envelope"),
            lambda p: p["resources"]["desktop"]["samples"].pop(),
            lambda p: p["resources"]["desktop"].pop("after_cancellation"),
            lambda p: p["resources"]["desktop"]["after_cancellation"].update(active=1),
            lambda p: p["resources"]["desktop"]["samples"][20]["fd_counts"].update(agent=21),
            lambda p: p["resources"]["desktop"]["samples"][20]["fd_counts"].pop("agent"),
            lambda p: p["resources"]["desktop"]["samples"][20].update(heap_bytes=10001 + 16 * 1024 * 1024),
            lambda p: p["resources"]["desktop"]["samples"][0]["fd_counts"].update(main=True),
        ]
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                proof = deepcopy(self.proof)
                mutation(proof)
                with self.assertRaises(ValueError):
                    self.verify(proof)

    def test_cli_rejects_changed_package_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            arguments = [sys.executable, str(SCRIPT), str(root / "evidence.json"),
                         "--source", "a"*40, "--tree", "b"*64, "--runtime", "c"*64,
                         "--platform", "Darwin-arm64", "--workflow-run", "qualification",
                         "--workflow-attempt", "1", "--desktop", "--http", "--browser"]
            for name in ("cli", "packaged_cli", "agent", "application"):
                file = root / name
                file.write_bytes(name.encode())
                self.proof["binaries"][name] = hashlib.sha256(file.read_bytes()).hexdigest()
                arguments.extend(["--" + name.replace("_", "-"), str(file)])
            (root / "evidence.json").write_text(json.dumps(self.proof))
            first = subprocess.run(arguments, capture_output=True, text=True)
            self.assertEqual(first.returncode, 0, first.stderr)
            (root / "packaged_cli").write_bytes(b"replacement")
            second = subprocess.run(arguments, capture_output=True, text=True)
            self.assertNotEqual(second.returncode, 0)

    def test_fixture_setup_failure_invalidates_previous_success(self):
        producer_path = SCRIPT.with_name("qualify_results.py")
        specification = importlib.util.spec_from_file_location("results_producer", producer_path)
        producer = importlib.util.module_from_spec(specification)
        with patch.object(sys, "dont_write_bytecode", True):
            specification.loader.exec_module(producer)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report = root / "results-acceptance/evidence.json"
            report.parent.mkdir()
            report.write_text(json.dumps({"passed": True}))
            with patch.dict("os.environ", {"CARGO_TARGET_DIR": str(root)}), \
                 patch.object(producer.sys, "argv", [str(producer_path)]), \
                 patch.object(producer, "git", return_value=b"a"*40), \
                 patch.object(producer, "source_tree", return_value="b"*64), \
                 patch.object(producer.fixture, "install_rustfs", side_effect=RuntimeError("fixture unavailable")):
                with self.assertRaisesRegex(RuntimeError, "fixture unavailable"):
                    producer.main()
            self.assertIs(json.loads(report.read_text())["passed"], False)

    def test_rejects_missing_false_stale_and_zero_test_evidence(self):
        mutations = [lambda p:p["checks"].pop("partial_template_evidence"),
                     lambda p:p["checks"].pop("desktop_partial_template_evidence"),
                     lambda p:p["tests"].update(partial_templates=0),
                     lambda p:p["tests"].update(authored_plots=1),
                     lambda p:p["checks"].pop("desktop_authored_plot_failures"),
                     lambda p:p["tests"].update(desktop=3),
                     lambda p:p["checks"].pop("authored_plot_capture"),
                     lambda p:p["tests"].update(authored_plots=0),
                     lambda p:p["tests"].update(browser=6),
                     lambda p:p["checks"].pop("http_read_timeout_recovery"),
                     lambda p:p["tests"].update(http=1),
                     lambda p:p["checks"].pop("desktop_storage_disconnect"),
                     lambda p:p["checks"].pop("desktop_inflight_session_close"),
                     lambda p:p["tests"].update(migration=2),
                     lambda p:p["checks"].pop("metadata_migration"),
                     lambda p:p["checks"].pop("migration_source_validation"),
                     lambda p:p["checks"].pop("migration_preserves_capture_roots"),
                     lambda p:p["checks"].pop("desktop_metadata_migration"),
                     lambda p:p["checks"].pop("cold_remote_retrieval"),
                     lambda p:p["tests"].update(workspace=7),
                     lambda p:p["checks"].pop("private_template_boundary"),
                     lambda p:p["checks"].pop("saved_template_assets"),
                     lambda p:p["checks"].pop("custom_template_capture_failure"),
                     lambda p:p["checks"].pop("recorded_template_reproduction"),
                     lambda p:p["checks"].pop("desktop_saved_template"),
                     lambda p:p["tests"].update(restore=1),
                     lambda p:p["checks"].pop("saved_observations"),
                     lambda p:p["checks"].pop("workspace_action_integrity"),
                     lambda p:p["checks"].pop("workspace_target_boundaries"),
                     lambda p:p["checks"].pop("directory_output_restore"),
                     lambda p:p["checks"].pop("checkpoint_target_boundaries"),
                     lambda p:p["checks"].pop("streaming_restore"),
                     lambda p:p["checks"].pop("desktop_saved_observation"),
                     lambda p:p["checks"].pop("desktop_workspace_integrity"),
                     lambda p:p["checks"].update(offline_html=False),
                     lambda p:p.update(source_sha="e"*40),
                     lambda p:p.update(source_tree="e"*64),
                     lambda p:p.update(runtime_sha256="e"*64),
                     lambda p:p["tests"].update(desktop=0),
                     lambda p:p["tests"].update(agent_errors=2),
                     lambda p:p["checks"].pop("desktop_structured_cli_errors"),
                     lambda p:p["tests"].update(desktop=2),
                     lambda p:p["checks"].pop("desktop_reproduce"),
                     lambda p:p["checks"].pop("desktop_queue_reply_ordering"),
                     lambda p:p["checks"].pop("desktop_metadata_focus"),
                     lambda p:p["checks"].pop("offline_foreground_paint"),
                     lambda p:p["checks"].pop("chart_response_bounds"),
                     lambda p:p["checks"].pop("browser_maximum_overlay_budget"),
                     lambda p:p["checks"].pop("browser_missing_plot_gaps"),
                     lambda p:p["checks"].pop("browser_authored_plot_semantics"),
                     lambda p:p["checks"].pop("browser_builtin_run_isolation"),
                     lambda p:p["tests"].update(charts=0),
                     lambda p:p["tests"].update(charts=5),
                     lambda p:p["tests"].update(charts=6),
                     lambda p:p["tests"].update(charts=7),
                     lambda p:p["tests"].update(charts=8),
                     lambda p:p["tests"].update(charts=9),
                     lambda p:p["tests"].update(browser=2),
                     lambda p:p["tests"].update(browser=5),
                     lambda p:p["tests"].update(matrices=4),
                     lambda p:p["tests"].update(matrix_cli=0),
                     lambda p:p["checks"].pop("large_matrix_exact_counts"),
                     lambda p:p["checks"].pop("cli_matrix_exports"),
                     lambda p:p["checks"].pop("desktop_matrix_counts"),
                     lambda p:p["checks"].pop("desktop_corrupt_evidence"),
                     lambda p:p["checks"].pop("desktop_missing_evidence"),
                     lambda p:p["checks"].pop("desktop_closed_session"),
                     lambda p:p["checks"].pop("desktop_partial_comparison"),
                     lambda p:p["checks"].pop("desktop_failure_layout"),
                     lambda p:p["checks"].pop("desktop_cross_file_failure"),
                     lambda p:p["checks"].pop("desktop_bare_remote"),
                     lambda p:p["checks"].pop("partial_chart_evidence"),
                     lambda p:p["checks"].pop("results_read_admission"),
                     lambda p:p["tests"].update(sdk=0),
                     lambda p:p["tests"].update(charts=10),
                     lambda p:p["tests"].update(charts=11),
                     lambda p:p["checks"].pop("browser_large_matrix_counts"),
                     lambda p:p["tests"].update(templates=8),
                     lambda p:p["checks"].pop("dvc_template_parity"),
                     lambda p:p["checks"].pop("browser_dvc_template_parity"),
                     lambda p:p["tests"].update(settings=0),
                     lambda p:p["checks"].pop("empty_private_settings_reproduction"),
                     lambda p:p["checks"].pop("reproduction_lineage"),
                     lambda p:p["tests"].update(checkpoint=0),
                     lambda p:p["tests"].update(inputs=0),
                     lambda p:p["tests"].update(inputs=1),
                     lambda p:p["tests"].update(inputs=2),
                     lambda p:p["tests"].update(inputs=3),
                     lambda p:p["tests"].update(inputs=4),
                     lambda p:p["checks"].pop("directory_inputs_reproduced"),
                     lambda p:p["checks"].pop("materialized_inputs_reproduced"),
                     lambda p:p["checks"].pop("admitted_input_identities"),
                     lambda p:p["checks"].pop("produced_inputs_regenerated"),
                     lambda p:p["tests"].update(faults=0),
                     lambda p:p["tests"].update(transfers=0),
                     lambda p:p["tests"].update(transport=0),
                     lambda p:p["checks"].pop("managed_results_transfer_refusal"),
                     lambda p:p["tests"].update(agent=0),
                     lambda p:p["checks"].pop("interrupted_capture"),
                     lambda p:p["tests"].update(http=0),
                     lambda p:p["tests"].update(browser=0),
                     lambda p:p["binaries"].update(cli="e"*64),
                     lambda p:p["binaries"].update(application="e"*64),
                     lambda p:p.update(platform="Linux-x86_64"),
                     lambda p:p.update(workflow_run="different-run"),
                     lambda p:p.update(workflow_attempt="2"),
                     lambda p:p["binaries"].pop("application")]
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                proof = deepcopy(self.proof)
                mutation(proof)
                with self.assertRaises(ValueError): self.verify(proof)


if __name__ == "__main__":
    unittest.main()
