# crab-write

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-write/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns shared Capsule initialization/publication and captured browse-index mechanics; retained journal primitives are separate retirement work. Authorization, ref leases, dependency proof, GC fencing, and generation-owner election belong to callers.

## Read first

1. `crates/crab-write/src/lib.rs` — shared API and source-preserving errors.
2. `crates/crab-write/src/journal.rs` — `commit_edits / compact_for_owner`: commit preconditions and uncertain outcomes.
3. `crates/crab-write/src/namespace.rs` — `with_ref_namespace`: namespace lease around ref-set changes.
4. `crates/crab-write/src/generation.rs` and `src/generation/browse.rs` — fenced ownership and caller-captured browse-index construction.
5. `crates/crab-write/src/capsule_protocol.rs` — `initialize / prepare_publication / publish_prepared`: native Capsule authority.

Trace one path: `crates/crab-remote/src/publication.rs` → prepared Capsule
publication in `crates/crab-write/src/capsule_protocol.rs` → conditional ref-head
and transaction metadata in `crates/crab-metadata/src/capsule_protocol/`.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Ref publication | `crates/crab-write/src/journal.rs` | `crab/src/git/push.rs` |
| Read readiness | `crates/crab-write/src/generation/browse.rs` | `crates/crab-remote/src/browse_indexes.rs` |
| Repository initialization | `crates/crab-write/src/capsule_protocol.rs::initialize` | `crab/src/cmd/init.rs`, SDK, gateway and view-service callers |

## Invariants

- commit_edits requires a snapshot captured under every edited ref lease; callers retain authorization, dependency checks, uploads, and lease renewal.
  Source: `crates/crab-write/src/journal.rs`.
- Cancellation-sensitive capsule callers use `prepare_publication`, check their
  lease token, then await `publish_prepared` through commit resolution. Do not
  drop publication on renewal loss or reinterpret committed success as cleanup
  failure. Source: `crates/crab-write/src/capsule_protocol.rs`.
- Foreground compaction preserves large historical runs, bounds ordinary input
  bytes, and respects the persisted 64-segment/512-capsule ceilings. Selected
  windows may precede the new leaf; load each selected source by identity.
  Policy, I/O and tests: `crates/crab-write/src/capsule_protocol/compaction.rs`.
- A marker storage error can leave an uncertain commit. Await the commit future and resolve outcome rather than treating every error as rejection.
  Source: `crates/crab-write/src/journal.rs`.
- Ref-head retries require readback of the exact original state and retain its
  conditional token. Matching candidate bytes prove commit; unreadable state
  stays uncertain. Bound readback for both original and candidate head sizes.
  Source: `crates/crab-write/src/capsule_protocol.rs`.
- Maintenance combines operation and cleanup errors through `finish_after_cleanup`
  in `crates/crab-write/src/lib.rs`. Namespace publication preserves a known
  committed result even after lease failure; keep that distinct outcome policy.
- Browse construction consumes a caller-pinned Capsule snapshot and origin-backed
  Git reader. The orchestrator rechecks root/ref-head authority before publishing
  readiness. Generation maintenance has no live v1 manifest/journal path.
  Sources: `crates/crab-write/src/generation/browse.rs`,
  `crates/crab-remote/src/browse_indexes.rs`.
- Namespace changes need a fresh final-ref-set check under the namespace lease; individual ref locks do not prevent parent/child name conflicts.
  Source: `crates/crab-write/src/namespace.rs`.

## Features and platform

No declared Cargo features. Dependencies enable storage/index/lease capabilities. Tests use local fixtures; live service publication and restart qualification belong in dedicated CI.

## Verification

Integration tests `crates/crab-write/tests/journal.rs` and `crates/crab-write/tests/generation.rs` exercise retained journal and ownership boundaries. Inline Capsule protocol tests exercise native initialization, publication, checkpointing and recovery. The unused mutable inventory publisher and initial-manifest publisher are removed.

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
