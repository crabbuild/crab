"""Validate the retention receipt's structured pass extraction."""

import unittest

from qualify_reader_retention import pass_counts


class RetentionReceiptTests(unittest.TestCase):
    def test_ansi_structured_counts_preserve_incomplete_result(self):
        sample = ("INFO completed offline Cell retention pass "
                  "\x1b[3mdeleted_objects\x1b[0m\x1b[2m=\x1b[0m1 "
                  "eligible_objects=24 complete=false")
        self.assertEqual(pass_counts(sample), {
            "deleted_objects": 1, "eligible_objects": 24, "complete": False,
        })

    def test_missing_or_multiple_passes_reject_ambiguous_evidence(self):
        line = "INFO completed offline Cell retention pass complete=true\n"
        for sample in ("no pass", line * 2):
            with self.subTest(sample=sample), self.assertRaises(RuntimeError):
                pass_counts(sample)
