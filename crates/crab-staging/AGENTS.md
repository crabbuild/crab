# crab-staging

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-staging/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns local segment bytes, SQLite locators, durable boundaries, recipes, prepared plans, and recovery. Remote publication and retirement decisions remain with push callers.

## Read first

1. `crates/crab-staging/src/lib.rs` — `StagingArea::open / flush_pending / close`: open/read-only admission and lock hierarchy.
2. `crates/crab-staging/src/index.rs` — `Index::chunks_for_file / chunks_for_file_with_sizes`: file-version membership and transactions.
3. `crates/crab-staging/src/segment.rs` — `SegmentWriter / decode_record`: record framing and durable writes.
4. `crates/crab-staging/src/recovery.rs` — `recover`: torn-tail and sealed-segment handling.
5. `crates/crab-staging/src/push_plan.rs` — `FilePushPlan / PreparedXorbCache`: prepared xorb identity and persistence.

Trace one path: `crab/src/cmd/add.rs` (`close_staging_before_indexing`) →
`StagingArea::flush_pending` in `crates/crab-staging/src/lib.rs` →
`Index::flush_pending` in `crates/crab-staging/src/index.rs`, after segment fsync.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| File chunk membership | `crates/crab-staging/src/index.rs` | `crab/src/cmd/add.rs` |
| Crash recovery or flush | `crates/crab-staging/src/recovery.rs` | `crates/crab-staging/src/segment.rs` |
| Prepared upload/resume | `crates/crab-staging/src/push_plan.rs` | `crates/crab-staging/src/multipart_resume.rs` |

## Invariants

- Keep process admission and in-process locks separate; synchronous index guards must end before await points.
  Source: `crates/crab-staging/src/lib.rs`.
- File-version reads must include all ordered chunk occurrences, even when chunk bytes deduplicate; inspect recipe consumers before changing index membership.
  Source: `crates/crab-staging/src/index.rs`.
  Prepared-only versions remain enumerable through open/delegated leases and
  push-snapshot pins. Chunk enumeration must not imply Git publication; push
  admission uses its separate published/delegated selection.

- Segment payload lifetime follows canonical inventory, including recording and
  sealed recipe owners. Compaction transfers inventory and occurrence locators
  together; reads capture handles before releasing locator ownership. Layout v3
  upgrades released v1 locators transactionally before admission.
  Sources: `crates/crab-staging/src/index.rs`, `crates/crab-staging/src/lib.rs`.
- Recovery distinguishes confirmed absence from filesystem lookup errors; only absence permits dropping pending locators. It rejects missing/undersized sealed segments; current-segment truncation and index cleanup must agree on the surviving durable data.
  Source: `crates/crab-staging/src/recovery.rs`.

- Flush staged data before publication. `StagingArea::close` flushes explicitly;
  Drop releases handles but does not provide the same flush barrier. Inspect
  success/error/cancellation ownership before relying on cleanup.
  Sources: `crates/crab-staging/src/lib.rs`,
  `crab/src/cmd/add.rs` (`close_staging_before_indexing`), and
  `crab/src/import/coordinator.rs` (flush before exposing import pointers).

- Multipart abandonment scans reject unrepresentable clock values; never convert a failed time conversion into an expired-lease cutoff. Provider cleanup still requires claiming the observed row revision.
  Source: `crates/crab-staging/src/multipart_resume.rs`.

- Completed Git-clean and native `--defer` batches delegate publication only after
  flush and verification. Delegation seals all batch paths and rejects further membership/recording edits.
  Delegation is not Git-index acceptance. Push may capture
  only selected pointer identities; retirement preserves uncaptured handoffs and
  active native owners. Layout v3 upgrades released v1 atomically and rejects the
  unreleased v2 prototype. Sources: `index.rs`, `lib.rs`, and crate README.

- `StagingPushSnapshot` owns reader fencing and exact snapshot cleanup. Product
  callers establish remote commit; the guard retains deferred committed journals
  and preserves unpublished leases on cancellation. Read `src/push_snapshot.rs`
  and the capsule consumer before changing this boundary.

## Features and platform

No declared Cargo features. Requires writable local filesystem/SQLite and platform lock semantics. Scale tests under `crates/crab-staging/tests` need dedicated disk/time budgets; do not run them as an incidental unit check.

## Verification

Inline recovery tests cover torn tails and sealed corruption. Property modules under `crates/crab-staging/tests/unit` are path-mounted by lib.rs; they are not standalone integration targets.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-staging --locked --lib recovery::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-staging --locked --lib prop_compaction_preserves_reads
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-staging/README.md` — usage and detailed contracts.
- `crates/crab-staging/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
