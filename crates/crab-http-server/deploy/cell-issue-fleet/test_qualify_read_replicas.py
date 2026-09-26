"""Reject incomplete or reset telemetry instead of reporting fabricated costs."""

import unittest

from qualify_read_replicas import COST_METRICS, cost_delta, parse_cost_metrics


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


if __name__ == "__main__":
    unittest.main()
