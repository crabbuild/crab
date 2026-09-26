"""Recovery evidence must follow the process doing work, even after owner election."""

import copy
import json
import pathlib
import subprocess
import sys
import unittest

from recovery_work import KINDS, PHASES, PHASE_PREFIX, WORK_PREFIX, recovery_work


def snapshot(process, count, seconds="0"):
    samples = [f'{WORK_PREFIX}{{kind="{kind}"}} {count}' for kind in KINDS]
    for phase in PHASES:
        samples.extend([
            f'{PHASE_PREFIX}_count{{phase="{phase}"}} {count}',
            f'{PHASE_PREFIX}_sum{{phase="{phase}"}} {seconds}',
        ])
    return {"process": process, "metrics": "\n".join(samples)}


class RecoveryWorkTests(unittest.TestCase):
    def test_recoverer_can_differ_from_serving_owner_and_have_prior_work(self):
        before = {"owner": snapshot("owner-boot", 0), "recoverer": snapshot("other-boot", 7)}
        after = {"recoverer": snapshot("other-boot", 8, "0.020"), "owner": before["owner"]}
        work = recovery_work(before, after)
        self.assertEqual({key: work[key] for key in KINDS}, dict.fromkeys(KINDS, 1))
        self.assertEqual(work["phases"], dict.fromkeys(PHASES, {"count": 1, "duration_ms": 20}))

    def test_deltas_merge_before_duration_rounding(self):
        before = {node: snapshot(node, 2, "0.5") for node in ("a", "b")}
        after = {node: snapshot(node, 3, "0.5004") for node in ("a", "b")}
        work = recovery_work(before, after)
        self.assertEqual(work["candidate_count"], 2)
        self.assertEqual(work["phases"]["seal"], {"count": 2, "duration_ms": 1})

    def test_one_process_reset_cannot_hide_behind_another_process_increase(self):
        before = {"a": snapshot("a", 5), "b": snapshot("b", 0)}
        after = {"a": snapshot("a", 0), "b": snapshot("b", 6)}
        with self.assertRaisesRegex(ValueError, "reset"):
            recovery_work(before, after)

    def test_missing_or_restarted_process_and_incomplete_metrics_fail(self):
        before = {"a": snapshot("a", 1), "b": snapshot("b", 1)}
        for damage in ("missing", "restarted", "empty", "counter_missing", "duplicate"):
            after = copy.deepcopy(before)
            if damage == "missing":
                del after["b"]
            elif damage == "restarted":
                after["b"]["process"] = "new-boot"
            elif damage == "empty":
                after["b"]["metrics"] = ""
            elif damage == "counter_missing":
                after["b"]["metrics"] = after["b"]["metrics"].split("\n", 1)[1]
            else:
                after["b"]["metrics"] += "\n" + after["b"]["metrics"].splitlines()[0]
            with self.subTest(damage=damage), self.assertRaises(ValueError):
                recovery_work(before, after)

    def test_invalid_counters_fail_closed(self):
        for value in ("NaN", "Infinity", "-1", "1.5"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                recovery_work({"a": snapshot("a", 0)}, {"a": snapshot("a", value)})

    def test_cli_preserves_zero_work_for_an_inactive_object_covered_log(self):
        snapshots = {"a": snapshot("a", 0)}
        result = subprocess.run(
            [sys.executable, str(pathlib.Path(__file__).with_name("recovery_work.py"))],
            input=json.dumps({"before": snapshots, "after": snapshots}),
            capture_output=True, text=True, check=True,
        )
        self.assertEqual(json.loads(result.stdout)["candidate_count"], 0)


if __name__ == "__main__":
    unittest.main()
