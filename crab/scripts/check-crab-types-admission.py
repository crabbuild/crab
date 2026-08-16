#!/usr/bin/env python3
"""Validate the crab-types admission ledger and dependency budget."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path


ALLOWED_NORMAL_DEPS = {"schemars", "serde"}
FORBIDDEN_SOURCE_RE = re.compile(r"\b(CrabError|progress|output)\b|crate::core")
PUBLIC_ITEM_RE = re.compile(
    r"^pub\s+(?:const|enum|struct|trait|type|fn)\s+([A-Za-z_][A-Za-z0-9_]*)\b",
    re.MULTILINE,
)
CODE_SPAN_RE = re.compile(r"`([^`]+)`")
GROUP_SPAN_RE = re.compile(
    r"(?P<module>[a-z_][A-Za-z0-9_]*(::[a-z_][A-Za-z0-9_]*)*)::\{(?P<items>[^}]+)\}"
)
DIRECT_SPAN_RE = re.compile(
    r"[a-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)+"
)


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def module_path(source_root: Path, source: Path) -> str:
    relative = source.relative_to(source_root).with_suffix("")
    if relative.name == "mod":
        relative = relative.parent
    return "::".join(relative.parts)


def public_items(source_root: Path) -> set[str]:
    items: set[str] = set()
    for source in sorted(source_root.rglob("*.rs")):
        if source.name == "lib.rs":
            continue
        module = module_path(source_root, source)
        text = source.read_text(encoding="utf-8")
        for match in PUBLIC_ITEM_RE.finditer(text):
            items.add(f"{module}::{match.group(1)}")
    return items


def ledger_items(ledger_path: Path) -> set[str]:
    text = ledger_path.read_text(encoding="utf-8")
    _, marker, current_surface = text.partition("## Current Public Surface")
    if not marker:
        raise ValueError(f"{ledger_path} is missing '## Current Public Surface'")

    items: set[str] = set()
    for span in CODE_SPAN_RE.findall(current_surface):
        group_match = GROUP_SPAN_RE.fullmatch(span)
        if group_match:
            module = group_match.group("module")
            for item in group_match.group("items").split(","):
                item = item.strip()
                if item:
                    items.add(f"{module}::{item}")
            continue

        if DIRECT_SPAN_RE.fullmatch(span):
            items.add(span)
    return items


def check_admission_ledger(source_root: Path, ledger_path: Path) -> bool:
    public = public_items(source_root)
    admitted = ledger_items(ledger_path)

    missing = sorted(public - admitted)
    stale = sorted(admitted - public)
    if not missing and not stale:
        print(f"ok: {len(public)} crab-types public items are admitted")
        return True

    if missing:
        print("error: public crab-types items missing from ADMISSION.md:", file=sys.stderr)
        for item in missing:
            print(f"  {item}", file=sys.stderr)
    if stale:
        print("error: ADMISSION.md lists stale crab-types items:", file=sys.stderr)
        for item in stale:
            print(f"  {item}", file=sys.stderr)
    return False


def check_forbidden_source(source_root: Path) -> bool:
    violations: list[str] = []
    for source in sorted(source_root.rglob("*.rs")):
        for index, line in enumerate(source.read_text(encoding="utf-8").splitlines(), start=1):
            if FORBIDDEN_SOURCE_RE.search(line):
                violations.append(f"{source}:{index}: {line.strip()}")

    if not violations:
        print("ok: crab-types source has no CLI/output policy imports")
        return True

    print("error: crab-types source contains forbidden owner-policy terms:", file=sys.stderr)
    for violation in violations:
        print(f"  {violation}", file=sys.stderr)
    return False


def cargo_tree_direct_deps(root: Path, cargo: str) -> set[str]:
    result = subprocess.run(
        [
            cargo,
            "tree",
            "-p",
            "crab-types",
            "--edges",
            "normal",
            "--depth",
            "1",
        ],
        cwd=root,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(result.stdout)

    deps: set[str] = set()
    for line in result.stdout.splitlines():
        if "── " not in line:
            continue
        package = line.split("── ", 1)[1].split(maxsplit=1)[0]
        deps.add(package)
    return deps


def check_dependency_budget(root: Path, cargo: str) -> bool:
    try:
        deps = cargo_tree_direct_deps(root, cargo)
    except RuntimeError as error:
        print("error: cargo tree failed:", file=sys.stderr)
        print(error, file=sys.stderr)
        return False

    extra = sorted(deps - ALLOWED_NORMAL_DEPS)
    if not extra:
        shown = ", ".join(sorted(deps)) or "(none)"
        print(f"ok: crab-types normal deps stay within budget: {shown}")
        return True

    print("error: crab-types normal deps exceed the admission budget:", file=sys.stderr)
    for dep in extra:
        print(f"  {dep}", file=sys.stderr)
    return False


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Validate crab-types public-surface admission and dependency budget.",
    )
    parser.add_argument(
        "--skip-cargo-tree",
        action="store_true",
        help="Skip the dependency-budget cargo tree check.",
    )
    parser.add_argument(
        "--cargo",
        default="cargo",
        help="Cargo executable to use for dependency checks.",
    )
    args = parser.parse_args()

    root = repo_root()
    source_root = root / "crates" / "crab-types" / "src"
    ledger_path = root / "crates" / "crab-types" / "ADMISSION.md"

    checks = [
        check_admission_ledger(source_root, ledger_path),
        check_forbidden_source(source_root),
    ]
    if not args.skip_cargo_tree:
        checks.append(check_dependency_budget(root, args.cargo))

    return 0 if all(checks) else 1


if __name__ == "__main__":
    raise SystemExit(main())
