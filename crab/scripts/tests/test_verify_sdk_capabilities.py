"""Regression checks for the SDK delivery inventory gate."""

import copy
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "verify_sdk_capabilities.py"
SPEC = importlib.util.spec_from_file_location("sdk_matrix", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class MatrixTests(unittest.TestCase):
    def setUp(self):
        self.matrix = json.loads((MODULE.DIRECTORY / "sdk-capabilities.json").read_text())

    def test_current_plan_has_complete_coverage(self):
        MODULE.validate(self.matrix)

    def test_incomplete_or_unsubstantiated_contracts_fail(self):
        mutations = {
            "missing cell": lambda m: m["cells"].pop(),
            "duplicate cell": lambda m: m["cells"].append(copy.deepcopy(m["cells"][0])),
            "unknown status": lambda m: m["cells"][0].update(status="supported"),
            "missing tests": lambda m: m["cells"][0].update(required_tests=[]),
            "invalid reference": lambda m: m["cells"][0].update(required_tests=["../fake"]),
            "missing phase": lambda m: m["cells"][0].update(phase=None),
            "relaxed contract": lambda m: m["cells"][0].update(contract="Outside 1.0"),
            "removed operation": lambda m: m["operations"].pop("remote_read"),
            "regressed implementation": lambda m: m["cells"][0].update(status="planned"),
            "missing phase test": lambda m: m["phase_tests"]["2"].pop(),
        }
        for name, mutate in mutations.items():
            with self.subTest(name=name):
                matrix = copy.deepcopy(self.matrix)
                mutate(matrix)
                with self.assertRaises(ValueError):
                    MODULE.validate(matrix)

    def test_activation_requires_resolved_tests(self):
        self.matrix["activated_phase"] = 1
        for cell in self.matrix["cells"]:
            if cell["contract"] != "Outside 1.0":
                cell["status"] = "implemented" if cell["phase"] == 1 else "planned"
        self.matrix["cells"][0]["required_tests"] = [
            "crates/crab-sdk/tests/remote_read.rs::missing_contract"
        ]
        with self.assertRaisesRegex(ValueError, "unresolved test"):
            MODULE.validate(self.matrix)

    def test_markdown_matches_inventory(self):
        self.assertEqual(
            (MODULE.DIRECTORY / "sdk-capabilities.md").read_text(),
            MODULE.render(self.matrix),
        )

    def test_reference_requires_a_test_attribute(self):
        cases = [
            ("fn contract() {}", False),
            ("#[test]\nfn contract() {}", True),
            ('#[tokio::test(flavor = "multi_thread")]\nasync fn contract() {}', True),
            ("#[test]\nfn other_contract() {}", False),
        ]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for source, expected in cases:
                with self.subTest(source=source):
                    (root / "case.rs").write_text(source)
                    self.assertEqual(MODULE.test_exists(root, "case.rs::contract"), expected)


if __name__ == "__main__":
    unittest.main()
