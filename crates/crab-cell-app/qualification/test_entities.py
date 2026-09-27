"""Reject false capacity and durability claims in the entity workload receipt."""

import csv
from pathlib import Path
import tempfile
import unittest

from entities import verify_window


class EntityWindowEvidence(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.label = "entities-3-uniform-1"
        (self.root / f"{self.label}-window.tsv").write_text(
            "window_id\tnodes\tshape\trate_per_node\tconcurrency\tseconds\tstarted_ms\tended_ms\telapsed_us\n"
            "0\t3\tuniform\t1\t4\t10\t100000\t110000\t10000000\n")
        counts = [0] * 12
        lines = ["arrival\tscheduled_us\tstarted_us\telapsed_us\tentity\tkind\toutcome\tsequence\tread_sequence\tcount"]
        for arrival in range(30):
            entity = arrival % 12
            counts[entity] += 1
            sequence = counts[entity] * 2 + 1
            scheduled = arrival * 1_000_000 // 3
            lines.append(f"{arrival}\t{scheduled}\t{scheduled + 10}\t1000\t{entity}\twrite\tok\t{sequence}\t{sequence}\t{counts[entity]}")
        (self.root / f"{self.label}.tsv").write_text("\n".join(lines) + "\n")
        (self.root / f"{self.label}-readback.tsv").write_text(
            "entity\texpected\tactual\tsequence\n" + "".join(
                f"{entity}\t{count}\t{count}\t{count * 2 + 1}\n" for entity, count in enumerate(counts)))

    def change(self, suffix, update):
        path = self.root / f"{self.label}{suffix}.tsv"
        with path.open(newline="") as source:
            rows = list(csv.DictReader(source, delimiter="\t"))
        update(rows)
        with path.open("w", newline="") as target:
            writer = csv.DictWriter(target, fieldnames=rows[0], delimiter="\t")
            writer.writeheader()
            writer.writerows(rows)

    def verify(self):
        return verify_window(self.root, 3, "uniform", 1, 4, 0, {})

    def test_complete_window_has_independent_owner_work(self):
        result = self.verify()
        self.assertEqual((result["fully_served_arrivals"], result["acknowledged_writes_by_node"]), (True, [12, 10, 8]))

    def test_value_at_wrong_receipt_is_rejected(self):
        self.change("", lambda rows: rows[0].update(count="2"))
        with self.assertRaisesRegex(AssertionError, "read value disagrees"):
            self.verify()

    def test_missing_arrival_is_rejected(self):
        self.change("", lambda rows: rows.pop())
        with self.assertRaisesRegex(AssertionError, "missing or duplicate arrival"):
            self.verify()

    def test_missing_persisted_write_is_rejected(self):
        self.change("-readback", lambda rows: rows[0].update(actual="2", expected="2"))
        with self.assertRaisesRegex(AssertionError, "lost or duplicated write"):
            self.verify()

    def test_missed_arrival_is_not_fully_served(self):
        self.change("", lambda rows: rows[-1].update(started_us="10000000", elapsed_us="0",
                    outcome="scheduler_late", sequence="0", read_sequence="0", count="0"))
        self.change("-readback", lambda rows: rows[5].update(actual="2", expected="2"))
        result = self.verify()
        self.assertEqual((result["fully_served_arrivals"], result["completed_actions"]), (False, 29))


if __name__ == "__main__":
    unittest.main()
