# Gitoxide Adoption — Architecture One-Pager

This doc captures the architectural shape of crab's `gix-*` adoption.
For the per-site decisions and LOC targets see
[`.kiro/specs/crab-gitoxide-adoption/`](../../.kiro/specs/crab-gitoxide-adoption/).
This file focuses on the cross-cutting design decisions that span
multiple requirements — the ones worth reading once rather than
rediscovering inside a task thread.

## Scope

Two user-visible surfaces to git: the remote helper
(`git-remote-crab`) and the filter process (`filter=crab`).
Everything between crab's unique value (chunking, dedup, xorb format,
shard metadata, VFS, LFS) and those two surfaces is **git plumbing**
— ref I/O, rev-parse, pack gen, worktree materialization, attributes,
ignore, config, credentials. Gitoxide exposes most of that as a
library; we adopt where it makes the call site smaller and more
correct.

## Two surfaces, one spine

```
  git client  ──┬───► remote helper (fetch via gix-protocol, push hand-rolled)
                │
                └───► filter process (gix-packetline framing, crab dispatch)
                                    │
                                    ▼
                    shared spine: gix::Repository, gix-ref, CrabOdb,
                                  gix-pack, gix-worktree-state,
                                  gix-config, gix-credentials,
                                  gix-attributes + gix-pathspec +
                                  gix-ignore, gix-status + gix-dir,
                                  gix-traverse + gix-revwalk + gix-fsck
                                    │
                                    ▼
                    crab unique plane: engine/, storage/xorb/,
                                         metadata/, vfs/,
                                         lfs/, workflow/, cmd/
```

Arrows go one way. Gitoxide never calls into crab's unique plane
directly. Crab exposes a `gix_object::Find + FindExt` adapter
(`CrabOdb`, `crates/crab-git/src/odb_adapter.rs`) so every gitoxide primitive
that wants blob bytes transparently pulls xorb-backed content through
shard reconstruction.

## § Streaming strategy for pointer-backed blobs

**Problem.** `gix_object::Find::try_find(&oid, &mut Vec<u8>)` materializes
the entire blob into the caller's buffer. Pointer-backed files in a
crab repo are 10 GiB and up; forcing that content through a
`Vec<u8>` OOMs the process.

**Decision: Option A — threshold-based bypass.**

The ODB adapter (`CrabOdb::try_find`) returns `None` (or a
zero-length mode-only sentinel) for blobs whose declared size exceeds
a configurable threshold (default **256 MiB**). `gix_worktree_state::
checkout` then performs a **mode-only pass** — it creates the file
with the correct mode, writes zero content, and returns success. A
second pass, driven by crab's existing
`vfs::engine::FuseEngine::promote_pointer_streaming`, streams the
real content chunk-by-chunk into the already-created file.

### Why Option A over Option B

| Option | Summary | Verdict |
|--------|---------|---------|
| **A** | Threshold-based bypass. ODB returns `None` above the threshold; crab streams the content separately. | **Chosen.** Zero changes to gitoxide. Preserves crab's existing 10 GiB+ streaming path. The contract between ODB and streamer is a size-threshold compare, not a reader API. |
| B | Extend `CrabOdb` with a streaming-reader surface (e.g. `try_find_streaming(&oid) -> Box<dyn Read>`). `gix_worktree_state::checkout` consumes chunk-by-chunk. | Rejected for now. Requires an upstream change — `gix_object::Find::try_find` takes `&mut Vec<u8>` by design, and forking the trait surface against gitoxide breaks the "no permanent local patches" policy in `requirements.md`. Revisit when gitoxide upstreams a streaming variant. |

### Contract between `CrabOdb::try_find` and the second-pass streamer

```text
                                   try_find(oid, &mut buf)
                                   │
                                   ├─ blob size known from pointer index
                                   │
                                   ▼
                   size <= threshold?
                   │                    │
                  yes                  no
                   │                    │
                   ▼                    ▼
       pull full bytes into       return None
       buf; gix-worktree-state    gix-worktree-state
       materializes content       creates file with mode
                                  only (empty content)
                                   │
                                   ▼
                   second pass:
                   FuseEngine::promote_pointer_streaming
                   (or cmd::hydrate::SmudgeSessionHydrator) streams
                   chunks from the hydration service into the
                   already-created file, then atomic-renames into
                   place
```

**Invariants:**

1. The threshold is **advisory for gitoxide** — the streaming pass is
   authoritative. If the threshold is misconfigured high, gitoxide
   materializes a large blob and OOMs; if misconfigured low, mode-only
   pass always runs and nothing breaks.
2. The second-pass streamer **overwrites** the file content created by
   the mode-only pass. Atomicity is preserved via `tempfile + rename`
   inside `promote_pointer_streaming`.
3. Mode and symlink-target application happen in the mode-only pass.
   Gitoxide owns the Windows-developer-mode fallback, CRLF
   normalization policy, and exec-bit logic; crab owns the bytes.
4. For blobs below the threshold, the adapter behaves like a normal
   `Find` impl — gitoxide writes the entire content in one pass and
   the streamer is not invoked.

### Threshold configuration

Default: 256 MiB, chosen so that a 200 MiB blob (comfortably fits
in memory on CI runners) goes through the fast path, but a
1 GiB+ model weight triggers the bypass. Override via
`crab.worktree.streamingThreshold` config (integer bytes). Zero
disables the threshold — everything goes through streaming. Unset
uses the default.

### Regression guard: `hydrate_streaming_smoke`

The integration test `crab/tests/hydrate_streaming.rs` synthesizes a
10 GiB pointer-backed blob via a counting reader (no actual 10 GiB
allocation) and asserts the process peak RSS stays below ~2 GiB. The
test is `#[ignore]` by default so the full-matrix CI run does not
allocate the synthetic tree; operators opt in with
`cargo test -p crab --features gix-worktree --test hydrate_streaming
-- --ignored`.

## Module-by-module mapping (summary)

See `.kiro/specs/crab-gitoxide-adoption/design.md` §Module-by-module
plan for the full per-file table. The highlights relevant to this
one-pager:

- `cmd/hydrate.rs`, `cmd/dehydrate.rs`, `vfs/engine.rs` — worktree
  writes route through `gix_worktree_state::checkout` on the
  `gix-worktree` feature flag. Bespoke EOL / exec-bit / symlink /
  Windows-developer-mode handling moves to gitoxide; crab keeps the
  xorb streaming content path.
- `cmd/status.rs` — committed-pointer lookup uses the ODB adapter
  instead of `git show HEAD:<path>`, eliminating the per-file
  `fork+exec` on repos with thousands of tracked pointers.
- `cmd/dehydrate.rs` — dirty check uses `gix_status::index_as_worktree`
  + `gix_dir::walk` instead of `git status --porcelain`, delivering
  the ≥ 20× speedup called out in `requirements.md`.
- `lfs/status.rs` — `diff-index` / `diff-files` shellouts replaced
  with `gix-status` outputs shared with dehydrate.

## Feature flags

All new behavior lives behind per-requirement feature flags in
`crab/Cargo.toml`. Req 6 adoption (this document's scope) is gated
by `gix-worktree`. Flags default off; each flips to default-on after
one release cycle of green CI and E2E. Legacy paths stay in-tree for
one cycle after the flag flips, then delete.

## Cross-references

- Spec: `.kiro/specs/crab-gitoxide-adoption/{requirements,design,tasks}.md`
- Shellout baseline: [`shellout-baseline.md`](shellout-baseline.md)
- ODB adapter source: `crates/crab-git/src/odb_adapter.rs`
- Streaming hydration path: `crates/crab-vfs/src/engine.rs`
  (`promote_pointer_streaming`, `promote_from_blob_cache`)

## Req 6 LOC projection

Legacy shellout paths kept behind `#[cfg(not(feature = "gix-worktree"))]`
as of the worktree-adoption branch (per Task 7.11):

| File | Legacy lines queued for deletion |
|------|----------------------------------|
| `crab/src/cmd/status.rs` | ~40 (`read_committed_pointer` shellout + `use std::process::Command`) |
| `crab/src/cmd/dehydrate.rs` | ~65 (`query_dirty_files` shellout + `use std::process::Command`) |
| `crab/src/lfs/status.rs` | ~75 (`diff_index_head` / `diff_files` / `parse_diff_output` shellouts) |
| **Subtotal from feature-gated cleanup** | **~180** |
| `crab/src/cmd/hydrate.rs` routing (Task 7.2, queued) | projected ~200 |
| `crates/crab-vfs/src/engine.rs` bespoke worktree handling (Task 7.3, queued) | projected ~250 |
| **Projected total when tasks 7.2 + 7.3 land** | **≥ 600** |

Task 25 (flag flip sweep) in the adoption spec performs the actual
deletion after the `gix-worktree` flag has been default-on for one
release cycle with green CI. Until then the legacy code stays
reachable so the default build is unaffected by upstream gitoxide
changes and operators can roll back to the shellout path with a
single flag toggle.
