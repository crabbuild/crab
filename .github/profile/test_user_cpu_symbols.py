"""Check real ELF boundaries and adversarial profiler records before collection."""

from pathlib import Path
import sys
import unittest

from user_cpu_symbols import Symbols, classify, decode


ARTIFACT = Path(sys.argv.pop(1)) if len(sys.argv) > 1 else Path(__file__).parent.parent / "safe-icf-component-42676127980"


class SymbolRecords(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.symbols = Symbols(ARTIFACT / "normal/crab", ARTIFACT / "safe.map")
        start = next(row for row in cls.symbols.rows if row[2] == "_start")
        cls.offset = cls.symbols.file_offset + start[0] - cls.symbols.start + 1

    def records(self, offset=None, mmap_kind="inode"):
        symbols = self.symbols
        offset = self.offset if offset is None else offset
        mapping = 0x700000000000
        page_offset = symbols.file_offset // 4096 * 4096
        ip = mapping + offset - page_offset
        filename = str(symbols.binary)
        length = symbols.size + 8192
        if mmap_kind == "legacy":
            metadata = f"PERF_RECORD_MMAP 71/71: [{mapping:#x}({length:#x}) @ {page_offset:#x}]: x {filename}"
        else:
            extra = "<abcdef1234>" if mmap_kind == "buildid" else "00:00 123 0"
            metadata = f"PERF_RECORD_MMAP2 71/71: [{mapping:#x}({length:#x}) @ {page_offset:#x} {extra}]: r-xp {filename}"
        return metadata + f"\ncrab 71/73 [002] 42.123456789: 1001001 cpu-clock:u:\n  {ip:x} [unknown] ({filename}+0x{offset:x})\n\n"

    def test_actual_text_and_start_boundaries(self):
        symbols = self.symbols
        start = next(row for row in symbols.rows if row[2] == "_start")
        offset = symbols.file_offset + start[0] - symbols.start
        self.assertIn("_start", symbols.lookup(offset))
        self.assertIn("_start", symbols.lookup(offset + start[1] - start[0] - 1))
        self.assertNotIn("_start", symbols.lookup(offset + start[1] - start[0]))
        self.assertEqual(symbols.lookup(symbols.file_offset - 1), [])
        self.assertEqual(symbols.lookup(symbols.file_offset + symbols.size), [])

    def test_overlapping_aliases_preserved(self):
        symbols = object.__new__(Symbols)
        symbols.start, symbols.file_offset, symbols.size = 100, 1000, 100
        symbols.rows = [(120, 180, "outer"), (140, 150, "inner"), (140, 160, "alias")]
        symbols.starts = [120, 140, 140]
        symbols.maximum_end = [180, 180, 180]
        for offset, names in [(1045, ["alias", "inner", "outer"]),
                              (1055, ["alias", "outer"]), (1075, ["outer"]), (1080, [])]:
            with self.subTest(offset=offset):
                self.assertEqual(symbols.lookup(offset), names)

    def test_all_mapping_record_contracts_crosscheck(self):
        for kind in ["inode", "buildid", "legacy"]:
            with self.subTest(kind=kind):
                samples, metadata = decode(self.records(mmap_kind=kind))
                proof = classify(samples, metadata, self.symbols)
                self.assertEqual(proof["normal_mmap_crosschecked_samples"], 1)
                self.assertEqual(proof["normal_mapped_samples"], 1)
                self.assertEqual(proof["total_period_weight"], 1001001)

    def test_observer_unknown_and_missing_weights_retained(self):
        script = self.records() + (
            "perf-exec 81/81 [001] 42.2: 2002002 cpu-clock:u:\n"
            "  1234 [unknown] (/usr/bin/perf+0x20)\n\n"
            "crab 71/73 [002] 42.3: 3003003 cpu-clock:u:\n\n"
        )
        samples, metadata = decode(script)
        proof = classify(samples, metadata, self.symbols)
        self.assertEqual(proof["samples"], 3)
        self.assertEqual(proof["total_period_weight"], 6006006)
        self.assertEqual(proof["leaf_categories"], {
            "normal-resolved": 1001001, "other-dso": 2002002, "missing-frame": 3003003,
        })

    def test_real_alignment_gap_stays_unresolved(self):
        end = self.symbols.start
        for begin, stop, _ in self.symbols.rows:
            if begin > end:
                break
            end = max(end, stop)
        self.assertLess(end, begin)
        offset = self.symbols.file_offset + end - self.symbols.start
        samples, metadata = decode(self.records(offset))
        proof = classify(samples, metadata, self.symbols)
        self.assertEqual(proof["leaf_categories"], {"normal-unresolved": 1001001})
        self.assertEqual(proof["normal_function_weights"], {})

    def test_wrong_perf_offset_rejected(self):
        samples, metadata = decode(self.records())
        samples[0]["frames"][0]["file_offset"] += 1
        with self.assertRaises(AssertionError):
            classify(samples, metadata, self.symbols)

    def test_unknown_mapping_format_for_normal_rejected(self):
        samples, metadata = decode(self.records())
        metadata.append(f"PERF_RECORD_MMAP2 unsupported {self.symbols.binary}")
        with self.assertRaises(ValueError):
            classify(samples, metadata, self.symbols)

    def test_inline_or_zero_period_sample_rejected(self):
        for script in ["crab 71/71 [000] 1.0: 100 cpu-clock:u: 1234 [unknown] ([unknown])",
                       "crab 71/71 [000] 1.0: 0 cpu-clock:u:\n"]:
            with self.subTest(script=script), self.assertRaises((ValueError, AssertionError)):
                decode(script)


if __name__ == "__main__":
    unittest.main()
