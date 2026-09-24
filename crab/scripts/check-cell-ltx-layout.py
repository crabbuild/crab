#!/usr/bin/env python3
"""Check the Cell and LTX crate layout rules from advisor plan 033.

Rules:
  1. No `#[path]` attributes in the four crates.
  2. Every `#[cfg(test)]`/`#[test]` location under `src/` is listed in the
     crate's `tests-allow-list.txt`.
  3. Every `tests-allow-list.txt` entry names an existing `src/` file, carries
     a reason, and still holds tests or test modules, so a moved or emptied
     test location cannot leave a stale entry behind.
  4. No `tests/<name>.rs` that shadows `src/<name>.rs`.
  5. Every test suite root has a matching module directory and at least one
     test.
  6. Every module file inside a suite directory is declared by its parent module
     file, so a split cannot leave a test file that the compiler never builds.
  7. Every `src/...` or `tests/...` path named by a crate guide exists, so the
     guides keep owning the layout rules they describe.
  8. The runtime root surface equals `api-prelude.txt`.
  9. The runtime coordination kernel stays sans-I/O: no async, clock, or
     storage, so the simulator and the model can replay the same transitions.
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
MODULE_DECL = re.compile(r"^\s*(?:pub(?:\([^)]*\))? )?mod ([a-z_][a-z_0-9]*)\s*;", re.M)
LINE_COMMENT = re.compile(r"//[^\n]*")
ROOT_RE_EXPORT = re.compile(r"^pub use ([^;]+);", re.M)
ROOT_CONST = re.compile(r"^\s*pub (?:const|struct|enum|trait|fn|type) ([A-Za-z_][A-Za-z0-9_]*)", re.M)

# A pure coordination kernel is what lets `coordination/sim.rs` and the TLA+
# model replay production transitions; an I/O call here would silently move the
# decision out of the replayable surface.
SANS_IO_PATTERNS = (
    ("async", re.compile(r"\basync\b")),
    ("await", re.compile(r"\.await\b")),
    ("tokio", re.compile(r"\btokio::")),
    ("storage", re.compile(r"\brusqlite\b|\bobject_store\b|\bstd::fs\b")),
    ("clock", re.compile(r"\bSystemTime\b|\bInstant\b|\bstd::time\b")),
)


def sans_io_paths(crate_path: Path) -> list[Path]:
    paths = []
    root = crate_path / "src" / "coordination.rs"
    if root.is_file():
        paths.append(root)
    directory = crate_path / "src" / "coordination"
    if directory.is_dir():
        paths.extend(sorted(directory.rglob("*.rs")))
    return paths


def allow_list(crate_path: Path) -> dict[str, str]:
    path = crate_path / "tests-allow-list.txt"
    if not path.is_file():
        return {}
    entries: dict[str, str] = {}
    for line in path.read_text().splitlines():
        entry, _, reason = line.partition("#")
        entry = entry.strip()
        if entry:
            entries[entry] = reason.strip()
    return entries


def check_allow_list_entries(crate_path: Path, entries: dict[str, str]) -> list[str]:
    problems: list[str] = []
    allow_path = (crate_path / "tests-allow-list.txt").relative_to(ROOT)
    for entry, reason in sorted(entries.items()):
        target = crate_path / "src" / entry
        if not target.is_file():
            problems.append(f"{allow_path}: {entry} does not name an existing src file")
            continue
        if not reason:
            problems.append(f"{allow_path}: {entry} needs a reason comment")
        text = LINE_COMMENT.sub("", target.read_text())
        if (
            TEST_ATTR.search(text) is None
            and CFG_TEST.search(text) is None
            and MODULE_DECL.search(text) is None
        ):
            problems.append(f"{allow_path}: {entry} no longer holds tests or test modules")
    return problems



def check_suite_module_declarations(crate: str, crate_path: Path) -> list[str]:
    """Every suite module file must be declared by the module that owns it."""
    problems: list[str] = []
    tests = crate_path / "tests"
    for suite in SUITES.get(crate, ()):
        suite_dir = tests / suite
        suite_root = tests / f"{suite}.rs"
        if not suite_dir.is_dir() or not suite_root.is_file():
            continue
        for path in sorted(suite_dir.rglob("*.rs")):
            if path.name == "mod.rs":
                continue
            if path.parent == suite_dir:
                declaring = suite_root
            else:
                declaring = path.parent.with_suffix(".rs")
                if not declaring.is_file():
                    declaring = path.parent / "mod.rs"
            if not declaring.is_file():
                problems.append(
                    f"{path.relative_to(ROOT)}: no module file declares it"
                )
                continue
            text = LINE_COMMENT.sub("", declaring.read_text())
            if path.stem not in MODULE_DECL.findall(text):
                problems.append(
                    f"{path.relative_to(ROOT)}: {declaring.relative_to(ROOT)} does "
                    f"not declare `mod {path.stem};`"
                )
    return problems



GUIDE_PATH = re.compile(r"`((?:src|tests)/[^`]+)`")


def check_guide_paths(crate_path: Path) -> list[str]:
    """Every crate-relative path a crate guide names must exist."""
    guide = crate_path / "AGENTS.md"
    if not guide.is_file():
        return []
    problems: list[str] = []
    for token in GUIDE_PATH.findall(guide.read_text()):
        token = token.strip()
        if "<" in token or "{" in token:
            continue
        if not (crate_path / token).exists():
            problems.append(
                f"{guide.relative_to(ROOT)}: {token} is not present in the crate"
            )
    return problems


def check(crate: str) -> list[str]:
    crate_path = ROOT / crate
    problems: list[str] = []
    entries = allow_list(crate_path)
    allowed = set(entries)
    problems.extend(check_allow_list_entries(crate_path, entries))
    problems.extend(check_suite_module_declarations(crate, crate_path))
    problems.extend(check_guide_paths(crate_path))
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
    if crate == "crates/crab-cell-runtime":
        for path in sans_io_paths(crate_path):
            text = LINE_COMMENT.sub("", path.read_text())
            for label, pattern in SANS_IO_PATTERNS:
                found = pattern.search(text)
                if found is None:
                    continue
                line = text[: found.start()].count("\n") + 1
                problems.append(
                    f"{path.relative_to(ROOT)}:{line}: coordination kernel must stay "
                    f"sans-I/O (found {label})"
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
