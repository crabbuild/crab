"""Regression tests for the capsule smoke's direct-proof oracle."""

import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from run_add_commit_push_rustfs_smoke import capsule_staged_proof_complete


class CapsuleProofTest(unittest.TestCase):
    def proof(self):
        return {
            "message": "revalidated staged remote placement proofs",
            "planned": 7, "indexed": 7, "matched": 7,
            "generation_verified": 0, "capsule_source_verified": 7,
            "payload_verified": 7, "placement_mismatches": 0,
            "origin_proof_mismatches": 0,
        }

    def adoption(self):
        return {
            "message": "capsule staged remote proof adoption complete",
            "planned_existing": 7, "stale_existing": 0, "global_existing": 0,
        }

    def log(self, *events):
        return "\n".join(json.dumps({"fields": e}, separators=(",", ":")) for e in events)

    def test_accepts_complete_capsule_evidence(self):
        self.assertTrue(capsule_staged_proof_complete(self.log(self.proof(), self.adoption()), 7))

    def test_rejects_incomplete_or_invalid_counters(self):
        for key in self.proof():
            if key == "message":
                continue
            for value in [None, -1, True, "7", 8]:
                with self.subTest(key=key, value=value):
                    event = {**self.proof(), key: value}
                    self.assertFalse(capsule_staged_proof_complete(self.log(event, self.adoption()), 7))
        for key in ["planned_existing", "stale_existing", "global_existing"]:
            with self.subTest(key=key):
                event = {**self.adoption(), key: 1}
                self.assertFalse(capsule_staged_proof_complete(self.log(self.proof(), event), 7))

    def test_rejects_counts_spread_across_events(self):
        first = {**self.proof(), "generation_verified": 7, "matched": 0}
        second = {**first, "matched": 7, "payload_verified": 0}
        self.assertFalse(capsule_staged_proof_complete(self.log(first, second, self.adoption()), 7))

    def test_rejects_missing_reordered_or_ambiguous_events(self):
        for events in [[], [self.proof()], [self.adoption()],
                       [self.adoption(), self.proof()],
                       [self.proof(), self.proof(), self.adoption()]]:
            with self.subTest(events=events):
                self.assertFalse(capsule_staged_proof_complete(self.log(*events), 7))

    def test_rejects_non_positive_expected_count(self):
        for count in [0, -1, True]:
            self.assertFalse(capsule_staged_proof_complete(self.log(self.proof(), self.adoption()), count))


if __name__ == "__main__":
    unittest.main()
