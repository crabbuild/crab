# Plan 005: Store and read immutable recipe trees by byte range

> **Executor instructions**: Add one immutable recipe-tree contract, bridge it
> from bounded local pages, and make reads range-aware. Do not retain a fallback
> that scans every manifest shard for partitioned-layout files.
>
> **Drift check (run first)**:
> `git diff --stat 1f9dae74..HEAD -- crates/crab-metadata/src crates/crab-storage/src crates/crab-read/src crab/src/git crab/src/metadata`

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/003-partitioned-metadata-and-receipts.md`, `plans/004-bounded-local-recipes.md`
- **Category**: perf
- **Planned at**: commit `1f9dae74`, 2026-08-19

## Why this matters

Current file lookup points to a shard and reconstruction downloads the whole
shard. For a TB-scale file, a flat recipe can itself be gigabytes. Immutable
leaves and bounded-fanout branches let readers fetch only metadata covering the
requested byte interval while keeping content identity verifiable.

## Current state

- `crates/crab-metadata/src/value_codec.rs:10-68` stores a committed file
  record containing recipe hash, shard hash, generation, and shard-index hash.
- `crab/src/metadata/metadb/stores/file_index.rs:102-145` checks each file and
  scans a generation prefix when building committed records.
- `crates/crab-metadata/src/file_index_lookup.rs:249-375` can fall back to
  scanning all manifest shards and building a complete map.
- `crates/crab-read/src/store_client.rs:93-169` resolves file → shard;
  `:220-240` loads the whole shard; `:391-478` reconstructs/coalesces xorb
  reads after metadata is loaded.
- Immutable content paths live under `.crab`; `StoreLayout` owns typed routing.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Metadata/read tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-metadata -p crab-storage -p crab-read --locked` | all pass |
| CLI read tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked` | all library tests pass |

## Scope

**In scope**:

- `crates/crab-metadata/src/recipe_tree.rs` (create)
- `crates/crab-metadata/src/value_codec.rs`
- `crates/crab-metadata/src/file_index_lookup.rs`
- `crates/crab-storage/src/layout.rs`
- `crates/crab-read/src/store_client.rs`
- `crab/src/metadata/metadb/stores/file_index.rs`
- `crab/src/cmd/hydrate.rs`
- `crab/src/git/smudge.rs`
- `crab/src/git/store_client.rs`
- focused tests in the listed modules

**Out of scope**:

- Changing the CDC algorithm or chunk profile.
- Making the pointer blob carry authoritative recipe data.
- Full push integration (Plan 006).
- Recursive full-tree materialization for convenience.

## Git workflow

- Branch: `advisor/005-remote-recipe-tree`
- Separate commits for format/codec, builder, reader/tests.
- Do not push without instruction.

## Steps

### Step 1: Define one immutable tree format

Define strict leaf and branch records with schema version, file hash, start
chunk, chunk count, start byte, covered bytes, policy ID, ordered terms/children,
and content digest. Leaves and branches are bounded by encoded bytes and
entries. Children include their byte and chunk ranges so traversal does not
fetch siblings. Use deterministic canonical encoding and domain-separated
hashes. Add
`{global_prefix}/partitioned1/recipes/{first_hash_byte}/{second_hash_byte}/{hash}`
path routing; recipe objects are global immutable content, while file-index
heads remain repo-local.

**Verify**: golden encoding and validation tests reject gaps, overlaps,
reordering, excessive fanout, wrong file hash, and digest mismatch.

### Step 2: Build trees from local pages with bounded state

Consume Plan 004 page iterators, write immutable leaves, and roll up bounded
branch levels. Retain only one fanout window per level. Return a root descriptor
with counts/bytes/hash; do not return every node. Create-only writes are
idempotent and conflicting bytes at a content path are corruption.

**Verify**: property tests compare random small trees to the source sequence;
an instrumented 10 TB cardinality simulation asserts builder memory is bounded
by page size × tree height, not chunk count.

### Step 3: Point the partitioned file index at roots

Add a distinct Partitioned1 file-record codec containing file size, chunk
count, policy ID, recipe root hash/kind, and committed generation/root identity.
Do not overload the existing unified record decoder. Writes are point updates;
remove any partitioned path that scans generation prefixes or all shards.

**Verify**: codec tests reject wrong layout/schema/length and lookup integration
returns the exact root for batched file hashes in caller order.

### Step 4: Traverse only the requested byte range

Add a reader that validates the file-index root, descends children overlapping
the requested range, validates each fetched node, and yields chunk terms in
order through a bounded stream. Feed those terms into the existing grouped and
coalesced xorb range-read path. A full-file hydrate is still streaming; a small
range must not fetch unrelated leaves.

**Verify**: a request spanning two leaves fetches only the root/path/two leaves;
random range property tests produce byte-identical slices; corruption returns
an error before unverified bytes are exposed.

## Test plan

- Canonical encoding and hash fixtures.
- Coverage/fanout/overflow corruption cases.
- Random sequence tree build + random byte-range reconstruction.
- Object GET-count assertion for narrow ranges.
- Bounded-memory instrumentation for build and traversal.
- Unified layout read behavior remains unchanged.

## Done criteria

- [ ] Partitioned file heads point directly to validated recipe roots.
- [ ] No partitioned read falls back to all-shard scans.
- [ ] Narrow ranges fetch only intersecting recipe nodes.
- [ ] Full reconstruction is byte-identical and bounded-memory.
- [ ] Existing xorb range coalescing is reused.
- [ ] Scoped tests/lint/format pass.

## STOP conditions

- Tree identity requires the whole sequence in memory.
- File-index validation depends on listing recipe objects.
- The read path would expose bytes before recipe/xorb validation.
- A second, incompatible recipe model is added instead of sharing the contract.

## Maintenance notes

Recipe node schemas are immutable cross-version contracts. Future fanout or
compression changes need a new recipe schema/kind, not silent reinterpretation.
