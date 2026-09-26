"""Verify replica fixture identity, image provenance, and measured costs."""

import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import qualify
import qualify_mode_rollout
import qualify_read_replicas
from qualify import initial_issue
from qualify_mode_rollout import verify_values
from qualify_read_replicas import COST_METRICS, cost_delta, parse_cost_metrics, replica_issue


def counters(control=0):
    return {name + '{result="ok"}': float(control if index == 0 else 0)
            for index, name in enumerate(COST_METRICS)}


def snapshot(control=0, started=1, finished=2):
    return {"node-01": {"started_seconds": started, "finished_seconds": finished,
                        "counters": counters(control)}}


class CostEvidenceTests(unittest.TestCase):
    def test_preserves_labeled_series_and_zero_values(self):
        series = counters(7)
        series[COST_METRICS[0] + '{result="failed"}'] = 2.0
        text = "# HELP ignored\n" + "\n".join(f"{key} {value}" for key, value in series.items())
        self.assertEqual(parse_cost_metrics(text), series)

    def test_refuses_missing_nonfinite_negative_or_duplicate_samples(self):
        valid = "\n".join(f"{key} {value}" for key, value in counters().items())
        for bad in ["", valid.replace("0.0", "NaN", 1), valid.replace("0.0", "-1", 1),
                    valid + "\n" + valid.splitlines()[0]]:
            with self.subTest(bad=bad), self.assertRaises(RuntimeError):
                parse_cost_metrics(bad)

    def test_retains_raw_samples_and_aggregates_node_windows(self):
        before = snapshot(10)
        after = snapshot(14, 5, 6)
        measured = cost_delta(before, after)
        self.assertEqual(measured["totals"][COST_METRICS[0]], 4)
        self.assertEqual(measured["nodes"]["node-01"]["window_seconds"], 5)
        self.assertEqual((measured["before"], measured["after"]), (before, after))

    def test_refuses_node_changes_counter_resets_and_disappearing_series(self):
        missing_series = snapshot(2)
        del missing_series["node-01"]["counters"][next(iter(counters()))]
        for after in [{}, snapshot(0), missing_series]:
            with self.subTest(after=after), self.assertRaises(RuntimeError):
                cost_delta(snapshot(1), after)


class FixtureReadbackTests(unittest.TestCase):
    def test_readers_and_rollout_reject_wrong_source_before_starting_nodes(self):
        source = "a" * 40
        wrong_image = {"Id": "sha256:" + "1" * 64, "Os": "linux", "Architecture": "arm64",
                       "Config": {"Labels": {"org.opencontainers.image.revision": "b" * 40}}}
        for module in (qualify_read_replicas, qualify_mode_rollout):
            with self.subTest(module=module.__name__), tempfile.TemporaryDirectory() as state:
                args = [module.__name__, "--state", state, "--project", "crab-cell-issue-reader-test", "--skip-build"]
                if module is qualify_mode_rollout:
                    args.extend(["--runtime-source", source])
                with patch("sys.argv", args), \
                        patch.object(module, "command", side_effect=lambda *args:
                                     source if "rev-parse" in args else ""), \
                        patch.object(module, "compose", return_value=""), \
                        patch.object(qualify, "command", return_value=json.dumps([wrong_image])), \
                        patch.object(module, "run_stage") as start:
                    with self.assertRaisesRegex(RuntimeError, "source revision"):
                        module.main()
                    start.assert_not_called()
                self.assertFalse((Path(state) / "read-replica-report.json").exists())

    def test_reader_accepts_the_current_fixture_after_body_refresh(self):
        response = io.BytesIO(json.dumps({**initial_issue(1), "body": "acknowledged update"}).encode())
        response.headers = {"x-crab-cell-reader": "a" * 32,
                            "x-crab-cell-incarnation": "b" * 32, "x-crab-cell-sequence": "7"}
        with response, patch("urllib.request.urlopen", return_value=response):
            self.assertEqual(replica_issue("http://fixture/issue", 1), ("a" * 32, 7, "b" * 32))

    def test_rollout_checks_the_complete_initial_issue(self):
        for corruption in (None, "title", "body", "number"):
            with self.subTest(corruption=corruption):
                def request(_method, url):
                    index = int(url.split("work-")[1][:2])
                    if url.endswith("/labels"):
                        return {"items": [{"name": "distributed"}]}
                    if "/comments?" in url:
                        return {"items": [{"body": "acknowledged comment"}]}
                    issue = initial_issue(index)
                    if corruption:
                        issue[corruption] = "wrong"
                    return issue

                with patch("qualify_mode_rollout.request_json", side_effect=request):
                    comments = {index: {"acknowledged comment"} for index in range(1, 4)}
                    if corruption:
                        with self.assertRaisesRegex(RuntimeError, "acknowledged issue"):
                            verify_values(18100, comments)
                    else:
                        verify_values(18100, comments)


if __name__ == "__main__":
    unittest.main()
