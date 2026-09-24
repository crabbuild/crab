#!/usr/bin/env python3
"""Check that Rust fences in the Cell and LTX documentation parse.

A reader copies documentation examples, so a fence that is not valid Rust is a
defect even when no test compiles it: the docs are `rust,ignore` precisely
because they need a provider, not because they may be syntactically broken.

Rules:
  1. Every ```rust fence under the four crates parses after being wrapped in
     `fn main() { ... }`, so a statement snippet parses while a broken one fails.
  2. A deliberately incomplete snippet is listed in ALLOW with the reason and
     the first line of the fence, so the exception is reviewed, not assumed.
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CRATES = (
    "crates/crab-cell-runtime",
    "crates/crab-cell-app",
    "crates/crab-cell-host",
    "crates/crab-ltx",
)

# (crate-relative path, first non-empty line of the fence): reason.
ALLOW = {
    (
        "crates/crab-cell-runtime/docs/failover-and-followers.md",
        "let observed = /* latest VersionedControl loaded from authority */;",
    ): "the comment is the placeholder for a value only the caller can load",
}


def fences(text: str) -> list[tuple[str, int, str]]:
    """Returns one (language, first line number, body) entry per fenced block."""
    found: list[tuple[str, int, str]] = []
    body: list[str] | None = None
    language = ""
    start = 0
    for number, line in enumerate(text.splitlines(), 1):
        if line.startswith("```"):
            if body is None:
                body, language, start = [], line.strip("`").strip(), number + 1
            else:
                found.append((language, start, "\n".join(body)))
                body = None
        elif body is not None:
            body.append(line)
    return found


def parses(body: str) -> str | None:
    """Returns the first parse error, or None when the wrapped snippet parses."""
    wrapped = "fn main() {\n" + body + "\n}\n"
    with tempfile.NamedTemporaryFile("w", suffix=".rs") as file:
        file.write(wrapped)
        file.flush()
        result = subprocess.run(
            ["rustfmt", "--edition", "2024", "--emit", "stdout", file.name],
            capture_output=True,
            text=True,
        )
    if result.returncode == 0:
        return None
    return next(
        (line for line in result.stderr.splitlines() if line.startswith("error")),
        "rustfmt rejected the snippet",
    )


def main() -> int:
    problems: list[str] = []
    checked = 0
    for crate in CRATES:
        crate_path = ROOT / crate
        for path in sorted(crate_path.rglob("*.md")):
            if "target" in path.parts:
                continue
            relative = str(path.relative_to(ROOT))
            for language, start, body in fences(path.read_text()):
                if not language.startswith("rust"):
                    continue
                first = next((line for line in body.splitlines() if line.strip()), "")
                if (relative, first) in ALLOW:
                    continue
                checked += 1
                error = parses(body)
                if error is not None:
                    problems.append(f"{relative}:{start}: {error}")
    for problem in problems:
        print(f"error: {problem}", file=sys.stderr)
    if problems:
        return 1
    print(f"ok: {checked} documented Rust snippets parse")
    return 0


if __name__ == "__main__":
    sys.exit(main())
