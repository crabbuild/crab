"""Reject incomplete or false mixed-workload receipts before qualification."""

import csv
from pathlib import Path
import tempfile
import unittest

from scale import verify_mixed_load


class MixedLoadEvidence(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        writes = ["arrival\tscheduled_us\tstarted_us\telapsed_us\toutcome\tsequence\tcount",
                  "baseline\t0\t0\t0\tbaseline\t100\t10"]
        for index in range(300):
            writes.append(f"{index}\t{index * 200_000}\t{index * 200_000 + 100}\t1000\tcommitted\t{102 + index * 2}\t{11 + index}")
        (self.root / "mixed-3-writes.tsv").write_text("\n".join(writes) + "\n")
        for lane in range(8):
            strict = lane % 2 == 0
            lines = ["started_us\telapsed_us\tminimum_sequence\tlatest_count\toutcome\tsequence\tcount"]
            if strict:
                lines.append("59000000\t1000\t700\t310\tbehind\t0\t0")
            lines.append(f"59999000\t3000\t{700 if strict else 0}\t310\tok\t{700 if strict else 699}\t{310 if strict else 309}")
            (self.root / f"mixed-3-reader-{lane}.tsv").write_text("\n".join(lines) + "\n")

    def change(self, name, update):
        path = self.root / name
        with path.open(newline="") as source:
            rows = list(csv.DictReader(source, delimiter="\t"))
        update(rows)
        with path.open("w", newline="") as target:
            writer = csv.DictWriter(target, fieldnames=rows[0], delimiter="\t")
            writer.writeheader()
            writer.writerows(rows)

    def test_observed_lag_and_typed_behind_remain_visible(self):
        result = verify_mixed_load(self.root, 3)
        self.assertEqual((result["acknowledged_writes"], result["successful_reads"],
                          result["behind_responses"], result["max_acknowledged_count_lag"]),
                         (300, 8, 4, 1))

    def test_stale_value_at_covering_receipt_is_rejected(self):
        self.change("mixed-3-reader-0.tsv", lambda rows: rows[-1].update(count="309"))
        with self.assertRaisesRegex(AssertionError, "snapshot value"):
            verify_mixed_load(self.root, 3)

    def test_result_below_requested_minimum_is_rejected(self):
        self.change("mixed-3-reader-0.tsv", lambda rows: rows[-1].update(sequence="699", count="309"))
        with self.assertRaises(AssertionError):
            verify_mixed_load(self.root, 3)

    def test_missing_scheduled_arrival_is_rejected(self):
        self.change("mixed-3-writes.tsv", lambda rows: rows.pop())
        with self.assertRaises(AssertionError):
            verify_mixed_load(self.root, 3)

    def test_duplicate_effect_is_rejected(self):
        self.change("mixed-3-writes.tsv", lambda rows: rows[-1].update(count="311"))
        with self.assertRaises(AssertionError):
            verify_mixed_load(self.root, 3)

    def test_missed_arrival_cannot_be_reported_as_fully_served(self):
        self.change("mixed-3-writes.tsv", lambda rows: rows[-1].update(
            started_us="60000000", elapsed_us="0", outcome="scheduler_late", sequence="0", count="0"))
        for lane in range(8):
            def update(rows):
                for row in rows:
                    row["latest_count"] = "309"
                    if int(row["minimum_sequence"]):
                        row["minimum_sequence"] = "698"
                    if row["outcome"] == "ok":
                        row["sequence"] = str(int(row["sequence"]) - 2)
                        row["count"] = str(int(row["count"]) - 1)
            self.change(f"mixed-3-reader-{lane}.tsv", update)
        result = verify_mixed_load(self.root, 3)
        self.assertEqual((result["acknowledged_writes"], result["missed_writes"], result["fully_served_writes"]),
                         (299, 1, False))


if __name__ == "__main__":
    unittest.main()
