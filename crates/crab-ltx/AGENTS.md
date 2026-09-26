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
3. `src/capture.rs` with `src/capture/{wal,checkpoint,verify}.rs`, and
   `src/ltx.rs` — WAL capture and LTX encoding.
4. `src/recovery.rs` — verified plans and exact restore.
5. `src/replica.rs` and `src/replica/` — Cell roots, directories, uploads, and
   compaction (see the module map below).
6. `UPSTREAM.md` — Celld lineage, licenses, and review rules for imports.
7. `src/internal.rs` — the unstable inspection surface external fuzzers and
   auditors use; `tests/vectors/README.md` records the external fixture
   provenance.

## Module map

Subsystem roots keep the shared contract; the named child modules own one
concern each. Module files sit beside their root (`foo.rs` + `foo/`).

- Capture: `capture.rs` + `capture/{wal,checkpoint,verify}.rs`.
- Replica: `replica.rs` +
  `replica/{cache,compaction,directory,restore,root,upload,verify}.rs`,
  `replica/compaction/scratch.rs`, `replica/directory/{initial,update,relocate}.rs`.
- Environment: `environment.rs` +
  `environment/{directory_cache,executor,host,resources,telemetry,tests}.rs`.
- Storage and IO: `pages.rs`, `paged.rs`, `paged_io.rs`, `writable_vfs.rs`,
  `writable_vfs/hydration.rs` (asynchronous fetch and owner-thread installation),
  `wal.rs`, `bundle.rs`, `codec.rs`, `lz4_block.rs`, `node_frame.rs`,
  `cell_layout.rs`.
- Top level: `host.rs`, `types.rs`, `error.rs`, `commit.rs`, `format_tests.rs`.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Change WAL capture | `src/capture/wal.rs` | `src/db.rs`, `tests/ltx/crash.rs` |
| Change LTX encoding | `src/ltx.rs`, `src/codec.rs` | `src/format_tests.rs`, `tests/cell/` |
| Change restore or compaction | `src/recovery.rs`, `src/replica/compaction.rs` | `tests/cell/restore.rs`, `tests/ltx/properties.rs` |
| Change the paged VFS | `src/writable_vfs.rs`, `src/paged_io.rs` | `tests/cell/roots.rs` |
| Change host hooks | `src/environment/` | `tests/host/hooks.rs` |
| Change a decoder or the format | `src/codec.rs`, `src/ltx.rs`, `src/internal.rs` | `src/format_tests.rs`, `tests/ltx/vectors.rs`, `fuzz/` |
| Change a durability seam | `src/capture/`, `src/db.rs`, `src/recovery.rs` | `tests/host/hooks/matrix.rs` |

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
- A commit is never stranded by size: an incremental cut that cannot fit
  `max_capture_bytes` is captured as a full database image bounded by
  `max_file_bytes`, and only a failure after the cut writer starts fences the
  session.
- Restore and compaction install only fresh destinations and verify the exact
  requested endpoint.
- Cancellation never pretends to roll back dispatched work: admission and
  scratch stay owned until that work finishes.
- Callers branch on `CrabError::classify()`, never on error text.

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

External vectors and decoder fuzzing:

```sh
# Regenerate the celld-written fixtures (see tests/vectors/README.md).
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-ltx-vectors \
  cargo run --release --manifest-path \
  crates/crab-ltx/tests/vectors/generate/Cargo.toml -- crates/crab-ltx/tests/vectors

# Deep search needs nightly; tests/ltx/vectors.rs replays the same entry points
# on the stable toolchain during `cargo test -p crab-ltx --features replica`.
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-ltx-fuzz \
  cargo +nightly fuzz run ltx -- -max_total_time=600
```

`.github/workflows/crab-ltx-fuzz.yml` runs the same targets per pull request and
on a nightly schedule, seeded from `tests/vectors/`.

## Related documentation

`README.md`, `UPSTREAM.md`, `examples/README.md`, `perf/README.md`, and
`../crab-cell-runtime/docs/canonical-ltx-scaling.md`.
