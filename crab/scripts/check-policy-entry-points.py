#!/usr/bin/env python3
"""Check that Cell runtime policy entry points are wired or explicitly deferred.

A policy seam that only tests reach is invisible in review: the tests stay
green while the behaviour it implements never runs, and the code reads as if the
policy were active. Three such seams have already existed in this tree
(`observe_pressure`, `evict_idle`, `takeover_unpublished`), so this check keeps
an explicit inventory of every policy entry point in the Cell stack.

Rules:
  1. Every public function in the Cell stack whose name starts with a policy
     prefix is a policy entry point.
  2. A `wired` entry point must have at least one call site outside tests.
  3. A `deferred` entry point must have no call site outside tests, and must
     record why it is not wired yet.
  4. An entry point with no inventory entry and no production caller fails the
     check, so a new seam cannot be added unnoticed.

The scan is name-based and crate-scoped on purpose: it is a review gate, not a
compiler, and the inventory carries the judgment.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# Cell stack only. Unrelated crates use the same verbs for their own types
# (`CatalogWriter::sweep_unreferenced`, cache evictors), which would otherwise
# read as production callers of a runtime seam.
CRATES = (
    "crates/crab-cell-runtime",
    "crates/crab-cell-app",
    "crates/crab-cell-host",
    "crates/crab-http-server",
)

# Bound policy surface: lifecycle and fleet seams that decide what the runtime
# does with ownership, residency, or pressure. Ordinary accessors stay out.
POLICY_PREFIXES = (
    "acquire_idle",
    "activate_",
    "evict",
    "observe_pressure",
    "quiesce",
    "rebalance",
    "release_idle",
    "shed",
    "sweep",
    "takeover_",
)

# Only the crate-external surface counts: a `pub(crate)` helper cannot be a
# policy entry point for another crate.
DEFINITION = re.compile(r"^\s*pub\s+(?:async\s+)?fn\s+(\w+)", re.M)
CFG_TEST = "#[cfg(test)]"

# (crate-relative path, symbol): (status, reason)
INVENTORY = {
    (
        "crates/crab-cell-runtime/src/cell/actor.rs",
        "observe_pressure",
    ): (
        "deferred",
        "no production sampler feeds the classifier, so soft and hard pressure "
        "never reach the actor; wiring waits on the pressure-seam decision",
    ),
    (
        "crates/crab-cell-runtime/src/cell/actor.rs",
        "evict_idle",
    ): (
        "deferred",
        "the idle sweep has no production caller; shedding waits on the same "
        "pressure-seam decision",
    ),
    (
        "crates/crab-cell-runtime/src/cell/actor.rs",
        "takeover_unpublished",
    ): (
        "deferred",
        "the router fails closed on a rootless control record instead of "
        "entering it, so only tests exercise this takeover path",
    ),
    (
        "crates/crab-cell-runtime/src/primitives/blob.rs",
        "sweep_unreferenced",
    ): (
        "deferred",
        "no product collector exists yet; abandoned uploads follow the provider "
        "lifecycle contract",
    ),
}


def strip_test_items(text: str) -> str:
    """Removes `#[cfg(test)]` items so in-src tests do not count as callers."""
    while True:
        index = text.find(CFG_TEST)
        if index < 0:
            return text
        start = text.find("{", index)
        if start < 0:
            return text[:index]
        depth = 0
        cursor = start
        while cursor < len(text):
            if text[cursor] == "{":
                depth += 1
            elif text[cursor] == "}":
                depth -= 1
                if depth == 0:
                    break
            cursor += 1
        text = text[:index] + text[cursor + 1 :]


def is_test_path(path: Path) -> bool:
    parts = path.parts
    return (
        any(part == "tests" or part.endswith("_tests") for part in parts)
        or path.name == "tests.rs"
        or path.name.endswith("_tests.rs")
    )


def production_sources(crate: Path) -> list[Path]:
    return sorted(
        path
        for path in (crate / "src").rglob("*.rs")
        if not is_test_path(path.relative_to(crate))
    )


def definitions(crates: tuple[Path, ...]) -> dict[tuple[str, str], Path]:
    found: dict[tuple[str, str], Path] = {}
    for crate in crates:
        for path in production_sources(crate):
            text = strip_test_items(path.read_text())
            for name in DEFINITION.findall(text):
                if name.startswith(POLICY_PREFIXES):
                    found[(str(path.relative_to(ROOT)), name)] = path
    return found


def call_sites(crates: tuple[Path, ...], name: str) -> list[str]:
    pattern = re.compile(r"\b" + re.escape(name) + r"\s*\(")
    definition = re.compile(r"fn\s+" + re.escape(name) + r"\s*\(")
    hits = []
    for crate in crates:
        for path in production_sources(crate):
            text = strip_test_items(path.read_text())
            for number, line in enumerate(text.splitlines(), 1):
                if line.lstrip().startswith("//"):
                    continue
                if pattern.search(line) and not definition.search(line):
                    hits.append(f"{path.relative_to(ROOT)}:{number}")
    return hits


def check() -> list[str]:
    crates = tuple(ROOT / crate for crate in CRATES)
    found = definitions(crates)
    problems: list[str] = []
    for entry, (status, reason) in sorted(INVENTORY.items()):
        if status not in ("wired", "deferred"):
            problems.append(f"{entry[0]}: {entry[1]} has unknown status {status!r}")
            continue
        if not reason:
            problems.append(f"{entry[0]}: {entry[1]} records no reason")
        if entry not in found:
            problems.append(
                f"{entry[0]}: inventory lists {entry[1]}, which is not a public policy entry point"
            )
            continue
        hits = call_sites(crates, entry[1])
        if status == "wired" and not hits:
            problems.append(
                f"{entry[0]}: {entry[1]} is inventoried as wired but no caller outside tests reaches it"
            )
        if status == "deferred" and hits:
            problems.append(
                f"{entry[0]}: {entry[1]} is inventoried as deferred but {hits[0]} calls it"
            )
    for entry, path in sorted(found.items()):
        if entry in INVENTORY:
            continue
        hits = call_sites(crates, entry[1])
        if not hits:
            problems.append(
                f"{path.relative_to(ROOT)}: {entry[1]} is an unwired policy entry point; "
                "wire it or add it to INVENTORY with a status and reason"
            )
    return problems


def main() -> int:
    problems = check()
    if problems:
        for problem in problems:
            print(f"error: {problem}", file=sys.stderr)
        return 1
    wired = sum(1 for status, _ in INVENTORY.values() if status == "wired")
    deferred = len(INVENTORY) - wired
    print(
        f"ok: every Cell runtime policy entry point is wired or deferred "
        f"({wired} wired, {deferred} deferred)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
