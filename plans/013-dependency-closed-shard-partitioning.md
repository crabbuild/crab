# Plan 013: Partition shards as dependency-closed file bundles

> **Executor instructions**: Replace entry-by-entry shard splitting with one
> canonical file-bundle API used by every current producer. Run each gate and
> stop if an exact-recipe consumer would need cross-shard joins. Update
> `plans/README.md` when complete unless a reviewer owns the index.
>
> **Drift check (run first)**:
> `git diff --stat 86139f55..HEAD -- crates/crab-xet/src/shard.rs crates/crab-xet/src/shard_parse.rs crates/crab-auth-server/src/view/objects.rs crab/src/git/push.rs crab/docs/design/xet.md crab/tests`
> If the shard writer, parser, push builder, or protected-view builder changed,
> re-read every producer and exact-recipe consumer before proceeding.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: none
- **Category**: bug, perf
- **Planned at**: commit `86139f55`, 2026-08-29
- **Delivery status**: DONE — focused, property, protected-view, full-workspace,
  and RustFS acceptance proof passes in the combined v1 hardening change set.

## Why this matters

Current producers add every xorb-info entry before every file entry, while
`PushShardSession` rotates after any entry exceeds its 100 MiB soft cap. At
scale this can produce xorb-only shards followed by file-only shards. Exact
recipe extraction, protected view publication, and push remote-file proof
require each file's referenced xorb metadata in the same shard. Partitioning
must therefore operate on dependency-closed file bundles, not raw entries.

## Current state

- `crab/src/git/push.rs:12217-12247` groups placements by xorb, adds all xorb
  info, then starts adding files. Its source `HashMap` also makes multi-shard
  boundaries nondeterministic.
- `crates/crab-xet/src/shard.rs:400-543` stores completed writers and calls
  `maybe_rotate` after both `add_xorb` and `add_file`; the documented
  “file-boundary” split is not enforced.
- `crates/crab-xet/src/shard_parse.rs:265-283` rejects a file term when its
  xorb info is absent from the same shard.
- `crab/src/git/push.rs:1008-1048` imposes the same co-location requirement for
  remote-file proof.
- `crates/crab-auth-server/src/view/objects.rs:84-152` is a sibling producer
  with the same all-xorbs-then-all-files ordering, then calls exact extraction
  at `:167-180`. Both producers must move together.
- The current split test at `crates/crab-xet/src/shard.rs:716-727` proves only
  that xorb entries rotate; it does not prove dependency closure.
- Pinned `xet-core-structures` 1.6.0 accepts xorb/file entries independently.
  Crab's stricter same-shard requirement is therefore Crab's producer
  contract, not an upstream guarantee.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Xet tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-xet --locked` | all pass |
| Push tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked build_shard` | all matching tests pass |
| Protected view tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-auth-server --locked view` | all matching tests pass |

## Scope

**In scope**:

- `crates/crab-xet/src/shard.rs`
- `crates/crab-xet/src/shard_parse.rs` for invariant validation/tests, not a
  permissive cross-shard fallback
- `crab/src/git/push.rs`
- `crates/crab-auth-server/src/view/objects.rs`
- adjacent tests in those modules
- `crab/docs/design/xet.md`

**Out of scope**:

- Changing immutable xorb payload format, MDB entry semantics, or the 100 MiB
  default beyond collapsing Crab's shard envelope to canonical v1.
- Teaching consumers to scan arbitrary other shards.
- A user-facing shard-size config or environment variable.
- Remote recipe trees or partitioned-layout plans.
- Shard upload receipts and cache warming; those remain follow-up performance work.

Version rule: there is one canonical shard contract named v1. Because there
is no compatibility obligation, fold dependency-closed partitioning and the
desired bloom behavior into v1, delete any v2 reader/writer branch touched by
this work, and regenerate fixtures. Do not dual-read v1/v2.

## Git workflow

- Branch: `advisor/013-dependency-closed-shards`
- Suggested commits: `refactor: partition shards by file closure`, then
  `test: force dependency-closed shard splits`.
- Read `crates/AGENTS.md` before shared-crate edits.

## Steps

### Step 1: Define and test the bundle invariant in `crab-xet`

Introduce one narrow shard-session operation that accepts an `MDBFileInfo` and
the complete `MDBXorbInfo` set referenced by its terms. The session must:

- validate before mutation that every file term has exactly one supplied full
  xorb-info block and every range fits that block;
- add missing dependencies and the file to one current writer without rotating
  inside the bundle;
- avoid duplicating an xorb-info block within one shard, but allow the same
  block in different shards when files in both partitions reference it;
- rotate only after the complete bundle is present;
- return the exact shard index containing the file;
- keep zero-byte files valid with no xorb dependencies.

Keep the default cap internal. Give tests an internal policy/cap parameter so
they can force rotation without adding a product config surface.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-xet --locked file_bundle`
→ tests pass for one bundle, forced multi-shard split, shared xorb across
partitions, zero-byte file, missing dependency rejection, and invalid range.

### Step 2: Make serialization validate dependency closure

Before finalizing each writer, run the same exact-recipe validation used by
`assemble_file_recipes`, or factor a bounded validation helper shared by the
writer and parser. Serialization must fail before any shard upload if a file
references absent xorb info. Do not relax `assemble_file_recipes` and do not
add cross-shard lookup.

Collapse the shard serializer/parser onto canonical v1 in the same cutover.
Delete v2 branching and old fixtures rather than teaching the reader both
formats. If an upstream type forces an internal encoded version, keep Crab's
product contract v1 and do not expose a Crab v2.

The forced-cap test must finalize every shard and call
`extract_file_recipes` on each body independently. Every file returned from a
shard must reconstruct its exact ordered chunk sequence using only that body.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-xet --locked dependency_closed`
→ all closure and corruption tests pass.

### Step 3: Convert direct push to deterministic file bundles

In `crab/src/git/push.rs`, build the complete xorb-info map once, then process
unique files in a stable order (file hash, with xorb dependencies sorted by
xorb hash). For each file:

1. build and validate complete reconstruction terms;
2. collect every referenced full xorb-info block;
3. call the bundle API;
4. record its returned file→shard index.

Remove the all-xorbs-first loop and the unused `xorb_data_map`. Do not change
the merged placement or verified-existing proof rules. Stable ordering must
make identical input produce identical shard byte/hash partitions.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked build_shard`
→ existing coverage tests pass and new forced-partition tests prove exact
per-shard extraction plus deterministic hashes across repeated builds.

### Step 4: Convert the protected-view sibling producer

Change `crates/crab-auth-server/src/view/objects.rs:84-152` to build the same
stable file bundles. Do not keep an independent ordering or dependency policy.
If producer-specific conversion is needed, factor only the generic bundle
mechanics into `crab-xet`; auth error mapping stays in auth-server.

Add a protected-view test that forces at least two partitions and then runs the
real `commit_view_metadb` exact extraction. It must succeed with every file
assigned to a containing dependency-closed shard.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-auth-server --locked view_shard`
→ all view shard and MetaDb commit tests pass.

### Step 5: Update the design contract

Update `crab/docs/design/xet.md` to state:

- the cap is soft at a complete file bundle boundary;
- a shard containing a file contains every referenced full xorb-info block;
- shared xorb metadata may repeat across shard bodies;
- producers use deterministic file/xorb ordering;
- readers may optimize with fallback metadata GETs, but producers cannot rely
  on that to emit incomplete shards.

**Verify**: `rg -n "bundle|same shard|dependency" crab/docs/design/xet.md`
→ the contract is present and consistent with code terminology.

## Test plan

- Unit: bundle validation, rotation only after file, duplicate suppression
  within a shard, permitted duplication across shards, zero-byte files.
- Property: for generated files/xorbs and small caps, every finalized body
  independently passes exact extraction and term coverage.
- Determinism: same shuffled placement input yields the same ordered shard hash
  vector and file→shard mapping.
- Integration: direct push shard builder and protected view MetaDb builder each
  force multiple shards and resolve exact recipes.

## Done criteria

- [x] No current producer adds all xorbs before all files.
- [x] Rotation cannot occur inside one file/dependency bundle.
- [x] Every finalized shard independently satisfies exact recipe extraction.
- [x] Identical inputs yield deterministic shard partitions and hashes.
- [x] Direct push and protected view use the same bundle contract.
- [x] Shards have one canonical v1 reader/writer and no v2 compatibility branch.
- [x] Focused xet, push, auth, and format commands pass.
- [x] The combined v1 hardening change set is committed locally; both direct
  and protected producers share the canonical bundle boundary.

## STOP conditions

Stop and report if:

- Upstream `ShardWriter` cannot safely contain repeated xorb info across
  different shard bodies.
- One file's complete dependency closure cannot fit representable MDB limits;
  a new wire-format design is required rather than splitting the file.
- Any exact-recipe consumer intentionally depends on cross-shard metadata.
- Protected-view and direct-push files use incompatible term semantics.
- The proposed API requires a public runtime size knob solely for testing.
- A shipped or user-owned shard corpus requiring v2 compatibility is
  discovered; the stated no-user hard-cutover premise would be false.

## Maintenance notes

Future shard producers must use the bundle API; raw entry methods should be
private or clearly restricted to tests/low-level construction. Reviewers should
inspect forced-cap tests, shared-xorb duplication, deterministic order, and
file→shard index accuracy. Streaming/spilling serialized shards is a separate
optimization once this partition contract is stable.
