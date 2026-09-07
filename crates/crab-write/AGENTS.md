# crab-write

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-write/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns shared initialization, journal publication/compaction, and catalog generation mechanics. Authorization, ref leases, dependency proof, GC fencing, and generation-owner election belong to callers.

## Read first

1. `crates/crab-write/src/lib.rs` — shared API and source-preserving errors.
2. `crates/crab-write/src/journal.rs` — `commit_edits / compact_for_owner`: commit preconditions and uncertain outcomes.
3. `crates/crab-write/src/namespace.rs` — `with_ref_namespace`: namespace lease around ref-set changes.
4. `crates/crab-write/src/generation.rs` — `maintain_catalog / make_readable`: catalog lifecycle and make_readable.
5. `crates/crab-write/src/catalog.rs` — `publish_inventory / LocatorPackEvidence`: immutable pack evidence and locator writes.

Trace one path: `crab/src/git/push.rs` → `commit_edits` in
`crates/crab-write/src/journal.rs` → namespace protection in
`crates/crab-write/src/namespace.rs` and journal commit in
`crates/crab-metadata/src/ref_journal.rs`.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Ref publication | `crates/crab-write/src/journal.rs` | `crab/src/git/push.rs` |
| Read readiness | `crates/crab-write/src/generation.rs` | `crates/crab-http-server/src/maintenance.rs` |
| Repository initialization | `crates/crab-write/src/initialize.rs` | `crab/src/cmd/init.rs` |

## Invariants

- commit_edits requires a snapshot captured under every edited ref lease; callers retain authorization, dependency checks, uploads, and lease renewal.
  Source: `crates/crab-write/src/journal.rs`.
- A marker storage error can leave an uncertain commit. Await the commit future and resolve outcome rather than treating every error as rejection.
  Source: `crates/crab-write/src/journal.rs`.
- Journal publication does not make the catalog readable. Trace maintain_catalog and make_readable, including explicit writer close, before acknowledging broader readiness.
  Source: `crates/crab-write/src/generation.rs`.
- Namespace changes need a fresh final-ref-set check under the namespace lease; individual ref locks do not prevent parent/child name conflicts.
  Source: `crates/crab-write/src/namespace.rs`.

## Features and platform

No declared Cargo features. Dependencies enable storage/index/lease capabilities. Tests use local fixtures; live service publication and restart qualification belong in dedicated CI.

## Verification

Integration tests `crates/crab-write/tests/journal.rs`, `crates/crab-write/tests/generation.rs`, and `crates/crab-write/tests/catalog.rs` exercise publication and maintenance boundaries.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-write --locked --test journal
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-write --locked --test generation
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-write/README.md` — usage and detailed contracts.
- `crates/crab-write/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
