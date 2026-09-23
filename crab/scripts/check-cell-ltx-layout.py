#!/usr/bin/env python3
"""Check the Cell and LTX crate layout rules from advisor plan 033.

Rules:
  1. No `#[path]` attributes in the four crates.
  2. Every `#[cfg(test)]`/`#[test]` location under `src/` is listed in the
     crate's `tests-allow-list.txt`.
  3. No `tests/<name>.rs` that shadows `src/<name>.rs`.
  4. Every test suite root has a matching module directory and at least one
     test.
  5. The runtime root surface equals `api-prelude.txt`.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CRATES = (
    "crates/crab-cell-runtime",
    "crates/crab-cell-app",
    "crates/crab-cell-host",
    "crates/crab-ltx",
)
SUITES = {
    "crates/crab-cell-runtime": (
        "runtime",
        "primitives",
        "protocol",
        "contracts",
        "fleet",
        "qualification",
    ),
    "crates/crab-cell-app": ("reference_application",),
    "crates/crab-cell-host": ("node",),
    "crates/crab-ltx": ("cell", "ltx", "host"),
}
TEST_ATTR = re.compile(r"^\s*#\[(?:tokio::)?test", re.M)
CFG_TEST = re.compile(r"^\s*#\[cfg\(test\)\]", re.M)
PATH_ATTR = re.compile(r"#\[path\s*=")
LINE_COMMENT = re.compile(r"//[^\n]*")
ROOT_RE_EXPORT = re.compile(r"^pub use ([^;]+);", re.M)
ROOT_CONST = re.compile(r"^\s*pub (?:const|struct|enum|trait|fn|type) ([A-Za-z_][A-Za-z0-9_]*)", re.M)


def allow_list(crate_path: Path) -> set[str]:
    path = crate_path / "tests-allow-list.txt"
    if not path.is_file():
        return set()
    entries = set()
    for line in path.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            entries.add(line)
    return entries


def check(crate: str) -> list[str]:
    crate_path = ROOT / crate
    problems: list[str] = []
    allowed = allow_list(crate_path)
    search_roots = [crate_path / "src"]
    if (crate_path / "tests").is_dir():
        search_roots.append(crate_path / "tests")
    for path in sorted({file for root in search_roots for file in root.rglob("*.rs")}):
        text = path.read_text()
        if PATH_ATTR.search(LINE_COMMENT.sub("", text)):
            problems.append(f"{path.relative_to(ROOT)}: #[path] attribute is not allowed")
        if "/src/" not in f"/{path.relative_to(ROOT)}":
            continue
        if TEST_ATTR.search(text) or CFG_TEST.search(text):
            relative = str(path.relative_to(crate_path / "src"))
            if relative not in allowed:
                problems.append(
                    f"{path.relative_to(ROOT)}: in-src tests are not in tests-allow-list.txt"
                )
    for suite in SUITES.get(crate, ()):
        root_file = crate_path / "tests" / f"{suite}.rs"
        if not root_file.is_file():
            problems.append(f"{root_file.relative_to(ROOT)}: missing suite root")
            continue
        if not TEST_ATTR.search(root_file.read_text()) and (crate_path / "tests" / suite).is_dir():
            module_dir = crate_path / "tests" / suite
            if not any(TEST_ATTR.search(child.read_text()) for child in module_dir.rglob("*.rs")):
                problems.append(f"{root_file.relative_to(ROOT)}: suite has no tests")
    tests_dir = crate_path / "tests"
    if tests_dir.is_dir():
        suite_names = set(SUITES.get(crate, ()))
        for path in tests_dir.glob("*.rs"):
            if path.stem in suite_names:
                continue
            if (crate_path / "src" / path.name).exists():
                problems.append(
                    f"{path.relative_to(ROOT)}: test file shadows a source module name"
                )
    prelude_path = crate_path / "api-prelude.txt"
    if prelude_path.is_file():
        expected = {line.strip() for line in prelude_path.read_text().splitlines() if line.strip()}
        actual: set[str] = set()
        lib = (crate_path / "src/lib.rs").read_text()
        for statement in ROOT_RE_EXPORT.findall(lib):
            statement = " ".join(statement.split())
            if "{" in statement:
                inner = statement[statement.index("{") + 1 : statement.rindex("}")]
                for part in inner.split(","):
                    part = part.strip()
                    if part:
                        actual.add(part.split(" as ")[-1].strip())
            else:
                actual.add(statement.split("::")[-1].strip())
        if actual != expected:
            problems.append(
                f"{prelude_path.relative_to(ROOT)}: root surface differs "
                f"(missing {sorted(expected - actual)}, extra {sorted(actual - expected)})"
            )
    return problems


def main() -> int:
    problems: list[str] = []
    for crate in CRATES:
        problems.extend(check(crate))
    if problems:
        for problem in problems:
            print(f"error: {problem}")
        return 1
    print("ok: Cell and LTX crate layout checks pass")
    return 0


if __name__ == "__main__":
    sys.exit(main())
