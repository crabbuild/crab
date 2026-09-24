# crab-ltx

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read `README.md` and
`UPSTREAM.md` before changing adapted code.

## Purpose and ownership

Managed SQLite WAL capture and exact, checksum-verified LTX recovery. The
optional `replica` feature adds Cell-root publication, bundles, compaction, and
sparse paged SQL. Cell authority, leases, retention policy, and HTTP stay
outside this crate.

## Read first

1. `src/lib.rs` — public surface and feature gating.
2. `src/db.rs` — managed connections, transactions, and capture boundary.
3. `src/capture/` and `src/ltx.rs` — WAL capture and LTX encoding.
4. `src/recovery.rs` — verified plans and exact restore.
5. `src/replica.rs` and `src/replica/` — Cell roots, directories, uploads,
   compaction.
6. `UPSTREAM.md` — Celld lineage, licenses, and review rules for imports.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Change WAL capture | `src/capture/wal.rs` | `src/db.rs`, `tests/ltx/crash.rs` |
| Change LTX encoding | `src/ltx.rs`, `src/codec.rs` | `src/format_tests.rs`, `tests/cell/` |
| Change restore or compaction | `src/recovery.rs`, `src/replica/compaction.rs` | `tests/cell/restore.rs`, `tests/ltx/properties.rs` |
| Change the paged VFS | `src/writable_vfs.rs`, `src/paged_io.rs` | `tests/cell/roots.rs` |
| Change host hooks | `src/environment/` | `tests/host/hooks.rs` |

## Layout and tests

- `src/` is production code; the environment splits into `host`, `resources`,
  `telemetry`, `directory_cache`, and `executor`, with replica-only modules
  gated at the declaration.
- Integration suites: `tests/cell.rs`, `tests/ltx.rs`, `tests/host.rs`, each
  with modules in the matching directory. No `#[path]` attributes.
- Unit tests for codec, page, VFS, and replica mechanics stay in `src/` and are
  listed in `tests-allow-list.txt`; they assert crate-private state that the
  public API intentionally does not expose.

## Invariants

- Checksum-bearing LTX only: readers reject checksum-disabled files and
  zero-checksum continuation markers.
- Capture records the committed WAL boundary; a valid prefix cannot hide a
  corrupt later committed frame.
- Restore and compaction install only fresh destinations and verify the exact
  requested endpoint.
- Cancellation never pretends to roll back dispatched work: admission and
  scratch stay owned until that work finishes.

## Features and platform

- `replica` (off by default) adds object-store transport, bundles, compaction,
  and sparse paged SQL. `tests/cell.rs` and `tests/host.rs` are gated on it.
- Local capture needs no network; provider examples are documented in
  `examples/README.md`.

## Verification

```sh
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/<checkout> \
  cargo test -p crab-ltx --features replica --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/<checkout> \
  cargo test -p crab-ltx --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/<checkout> \
  cargo clippy -p crab-ltx --all-targets --features replica --locked -- -D warnings
python3 crab/scripts/check-cell-ltx-layout.py
```

## Related documentation

`README.md`, `UPSTREAM.md`, `examples/README.md`, `perf/README.md`, and
`../crab-cell-runtime/docs/canonical-ltx-scaling.md`.
