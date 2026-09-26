"""Signed placement must converge with fresh observations of the same process."""

import copy
import contextlib
import io
import unittest

from placement import collect_placement


CAPACITY = {"resources": {"memory_bytes": 1024}, "admission": {"local_disk_bytes": 4096, "active_cells": 8}}


def metrics(active):
    return (f"crab_http_server_cell_runtime_active_cells {active}\n"
            "crab_http_server_cell_runtime_active_cell_capacity 8\n"
            "crab_http_server_cell_runtime_local_disk_capacity_bytes 4096\n")


def observation(generation, active, advertised, after=None, process="same-boot"):
    return {
        "process_before": process, "process": process,
        "metrics_before": metrics(active), "metrics": metrics(active if after is None else after),
        "node": {"session": "session", "live": True, "advertisement": {
            "generation": generation,
            "placement": {"memory_capacity_bytes": 1024, "disk_capacity_bytes": 4096,
                          "active_cells": advertised, "max_active_cells": 8},
        }},
    }


class PlacementTests(unittest.TestCase):
    def collect(self, frames, observation_seconds=0):
        calls = 0
        now = [0.0]

        def observe():
            nonlocal calls
            value = frames[min(calls, len(frames) - 1)]
            calls += 1
            now[0] += observation_seconds
            return copy.deepcopy(value)

        def pause(seconds):
            now[0] += seconds

        with contextlib.redirect_stderr(io.StringIO()):
            result = collect_placement(CAPACITY, "session", observe, lambda: now[0], pause)
        return result, calls

    def test_delayed_advertisement_converges_without_reusing_old_metrics(self):
        frames = [observation(1, 1, 0), observation(2, 1, 1), observation(3, 1, 1)]
        result, calls = self.collect(frames)
        self.assertEqual(calls, 3)
        self.assertEqual(result["node"]["advertisement"]["generation"], 3)
        self.assertEqual(result["metrics"], metrics(1))

    def test_changing_count_during_observation_must_settle(self):
        frames = [observation(1, 0, 0, after=1), observation(2, 1, 1), observation(3, 1, 1)]
        result, calls = self.collect(frames)
        self.assertEqual(calls, 3)
        self.assertEqual(result["metrics_before"], metrics(1))

    def test_stale_advertisement_or_persistent_mismatch_cannot_pass(self):
        for frame in (observation(1, 1, 1), observation(1, 1, 0)):
            with self.subTest(frame=frame), self.assertRaisesRegex(ValueError, "did not converge"):
                self.collect([frame])

    def test_matching_observation_after_deadline_cannot_pass(self):
        with self.assertRaisesRegex(ValueError, "did not converge"):
            self.collect([observation(1, 1, 1), observation(2, 1, 1)], observation_seconds=16)

    def test_regressing_or_invalid_generation_cannot_pass(self):
        for generation in (0, -1, True, "2", 1.5):
            with self.subTest(generation=generation), self.assertRaisesRegex(ValueError, "invalid"):
                self.collect([observation(generation, 1, 1)])
        with self.assertRaisesRegex(ValueError, "regressed"):
            self.collect([observation(2, 1, 1), observation(1, 1, 1)])

    def test_process_change_inside_or_between_samples_cannot_pass(self):
        during = observation(1, 1, 1)
        during["process"] = "new-boot"
        for frames in ([during], [observation(1, 1, 1), observation(2, 1, 1, process="new-boot")]):
            with self.subTest(frames=frames), self.assertRaisesRegex(ValueError, "restarted"):
                self.collect(frames)

    def test_capacity_identity_and_metric_contracts_still_fail(self):
        for corruption in ("memory", "disk", "limit", "live", "session", "missing", "duplicate", "nan"):
            frame = observation(1, 1, 1)
            placement = frame["node"]["advertisement"]["placement"]
            if corruption in ("memory", "disk", "limit"):
                field = {"memory": "memory_capacity_bytes", "disk": "disk_capacity_bytes", "limit": "max_active_cells"}[corruption]
                placement[field] += 1
            elif corruption in ("live", "session"):
                frame["node"][corruption] = False if corruption == "live" else "wrong-session"
            elif corruption == "missing":
                frame["metrics"] = ""
            elif corruption == "duplicate":
                frame["metrics"] += metrics(1)
            else:
                frame["metrics"] = frame["metrics"].replace("active_cells 1", "active_cells NaN")
            with self.subTest(corruption=corruption), self.assertRaises(ValueError):
                self.collect([frame])


if __name__ == "__main__":
    unittest.main()
