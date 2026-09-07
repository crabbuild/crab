# Crate quality implementation plans

Created 2026-09-06 with the improve skill; planned against `ebd0e40d14c`.
This directory separates the selected crate-guidance work from the existing
GC/product roadmap in `plans/`.

| Plan | Scope | Priority | Effort | Depends on | Status |
|---|---|---|---|---|---|
| [001: Per-crate agent guides](001-per-crate-agent-guides.md) | All 21 shared/server crates; AGENTS.md plus CLAUDE.md symlinks | P1 | M–L | None | DONE |

## Execution order

Execute plan 001 in its six batches. Complete source-backed navigation and
validation for every crate before marking it DONE. No dependency on GC plans.

## Source quality follow-up

[002 — Rust crate source quality](002-crate-source-quality.md) is in progress
across all 21 crates. It tracks source fixes, documentation corrections,
regression evidence, and the remaining qualification work.

## Agent-guide scope decisions

- Selected by the user: agent guides across all 21 crates.
- Deferred: README/rustdoc rewrites, executable examples and code decomposition.
- Rejected: copying root instructions into each crate; this adds duplicated policy.
- Existing split-crate CI already checks interfaces, behavior, Clippy and tests;
  new blanket quality gates are not part of this plan.

## Completion evidence — 2026-09-07

Implemented all six batches: 21 crate-local AGENTS.md guides and 21 relative
CLAUDE.md symlinks. Each guide includes named source entry points, a concrete
call path, common-change routes, local invariants, feature/platform notes and
focused verification recipes. Parent review checked the complete guides and
relevant source paths; revisions corrected close-test selection, staging
flush ownership, metadata minimal features and LFS sibling implementation scope.

Checks passed against the completed files:

- Plan membership and structural checks: 21/21 crates and valid relative aliases.
- 210 distinct repository path references exist; 146 navigation symbol tokens
  occur in their referenced source files.
- All declared crate feature names appear in the corresponding guides.
- 47 Cargo test recipes: shell syntax, package names, feature names and
  integration targets checked; 44 library filters map to source modules/functions.
- Exactly 42 guide/alias additions. No Rust source, manifests, tests, existing
  READMEs, inherited guides or CI changes. `git diff --check` passed.

These are static documentation checks and source review, not test execution.
Cargo recipes were not run: this is documentation-only work, and the required
workspace build volume is unavailable on this host. No runtime or provider
qualification is claimed. README rewrites, runnable examples and Rust refactors
remain separate work; this completion covers plan 001's full guide scope.
