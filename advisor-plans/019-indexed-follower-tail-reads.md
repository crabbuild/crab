# Crash-rebuildable indexed follower tail reads

Status: IMPLEMENTED — protected large-tail evidence pending
Priority: P0
Effort: L
Risk: High
Planned against: `c86dd43423ae` (`origin/main`, 2026-09-20)
Design authority: `advisor-plans/follower-affine-failover-design.md`
Dependencies: plan 018 baseline evidence; plan 012's node byte reservation API

## Executor instructions

Implement on `codex/019-indexed-follower-tail`. Read all of `follower.rs`, its
tests, node-log transport codecs, recovery paging, follower startup scrub, disk
budgeting, retirement, and corruption tests. Replace repeated full-lane scans
with one canonical derived index. Do not change node-frame, chunk, seal, or wire
formats. Use a unique external Cargo target.

## Drift check

```bash
git fetch origin main
git diff --stat c86dd43423ae..origin/main -- \
  crates/crab-cell-runtime/src/follower.rs \
  crates/crab-cell-runtime/src/follower \
  crates/crab-cell-runtime/src/node_log_transport.rs \
  crates/crab-cell-runtime/src/node_log_recovery.rs \
  crates/crab-http-server/src/peer.rs \
  crates/crab-http-server/src/peer/node_log_client.rs
```

Stop if main has changed the follower chunk/seal format or introduced another
authoritative index. Inventory tagged releases before adding a migration reader.

## Why this plan exists

Historically `FollowerStore::read_tail_page` called `read_tail_sync`; every call
invoked `scan_lane`, walked every chunk, parsed and verified every record, then
sought the requested page. The implementation now builds a bounded derived
index and revalidates only the selected records, while retaining the original
scan as the authoritative rebuild path.

The failover design now records indexed seek-only reads as implemented. Chunk
bytes plus the seal watermark remain the durable local evidence; the index only
accelerates access to them and protected large-tail evidence is still required
before claiming a production scaling result.

The repeated work is explicit in `crates/crab-cell-runtime/src/follower.rs`:

```rust
let retained = scan_lane(&directory.join("chunks"), lane, limits)?;
for (sequence, record) in retained.range(first_sequence..) {
    // seek, reread, and digest-check selected bytes
}
```

That scan runs only when a lane index is first built or must be rebuilt. Later
`read_tail_page` calls select a bounded range and reread only the selected
headers and bodies.

## Target contract

```text
append/rotate/seal under lane lock
  -> update chunk files and fsync existing authority
  -> publish/update derived in-memory index

first read/append/seal after startup, or index invalidation
  -> scan each chunk once, verify framing/LTX/digests/contiguity
  -> reserve exact derived-index bytes in the runtime resource ledger
  -> publish one new index generation

tail page(first)
  -> validate sealed watermark against indexed durable range
  -> binary/range seek records >= first
  -> reread each authoritative header, require exact cached metadata agreement
  -> reread body and recheck record digest
  -> return bounded page
```

The index contains sequence, chunk path, byte offset, length, and digest. It
does not replace chunk data, seal/retire watermarks, or node-frame verification.

## Files in scope

Only modify `crates/crab-cell-runtime/src/follower.rs`, its existing adjacent
test module/files, `crates/crab-http-server/src/server.rs` for production
resource-admission injection, and focused server construction tests. Transport,
node-frame, chunk, marker, and HTTP formats are read-only. Stop and amend this
plan before touching another production path.

## Scope

- Make one lane index owned by the existing per-lane lock/state.
- Keep startup scrub authoritative but do not retain every lane index. Build one
  lane lazily on first append/seal/read and release it on retirement.
- Update it atomically with successful append/chunk rotation/seal/retire.
- Serve bounded pages by range lookup and exact seeks.
- Add work counters proving page reads do not rescan the prefix.
- Preserve current `FollowerStore` and `NodeLogTransport` public contracts.

## Out of scope

- A new on-disk authoritative format or compatibility reader.
- `mmap`, unsafe code, unbounded file-descriptor caches, or async file I/O rewrite.
- Recovery inventory, executor placement, or manifest changes.
- Trusting index entries without rereading and digest-checking bytes.

## Implementation steps

### Step 1: define one admitted index generation

Replace `LaneMemory.records: BTreeMap<u64, [u8; 32]>` with a compact location
containing chunk identity, header offset, body length, and digest. Sequence is
the map key. Store chunk identity rather than a trusted absolute path. One
generation owns an exact `CellRuntime::try_reserve_node_bytes` reservation sized
with checked arithmetic from map capacity plus entry/path storage. Reservation
failure returns capacity and leaves the lane scan-only/unindexed; it must not
publish an unaccounted partial index.

**Verify:** a focused large-record-count test reaches the configured retained-
byte limit, returns capacity, and returns ledger usage to baseline after the
lane/index is dropped.

### Step 2: build lazily from the authoritative chunks

Refactor chunk scanning into a single rebuild function used by startup scrub
and index repair. It must preserve open-log suffix truncation semantics,
duplicate-equal acceptance, conflicting-record rejection, sequence-gap
rejection, and sealed-watermark validation.

Startup scrub validates all lanes but discards record locations. The first
append, seal, or read installs one admitted generation under the lane lock.

**Verify:** the restart test proves open-suffix repair occurs at startup, zero
indexes remain retained afterward, and the first read performs exactly one
rebuild.

### Step 3: make mutation publication transactional

Append writes and syncs the record before inserting its staged location. On any
write/sync error, do not mutate the current generation. Rotation syncs
`open.log`, renames it to the immutable chunk name, syncs the directory, rewrites
the affected entries to that chunk identity in a staged generation, then swaps
the generation under the lane lock. A failure after filesystem mutation marks
the in-memory generation invalid so the next operation rebuilds; it never serves
old `open.log` paths. Seal publishes its marker only after the indexed durable
range covers the watermark. Retirement removes the generation and drops its
reservation.

Add a test-only filesystem fault seam local to `follower.rs` for write, file
sync, rename, and directory sync; do not add production configuration.

**Verify:** fault-table tests cover every boundary and assert either the prior
generation remains valid or the next read performs a full verified rebuild.

### Step 4: seek while revalidating framing

Make `read_tail_page` take the lane lock, validate state/watermark, select a
bounded range from the index, then materialize only those records. Preserve the
post-seek BLAKE3 check. For every selected record, reread and parse the
authoritative 52-byte header and require exact agreement with cached magic,
sequence, length, and digest before reading its body. Resolve the chunk only
beneath the already-authorized lane directory; the header itself does not encode
session or epoch.

**Verify:** separate mutation tests for magic, sequence, length, digest, chunk
rename/escape, and body all return the current canonical error.

### Step 5: preserve async and lifecycle ownership

Keep blocking filesystem work inside `spawn_blocking`; do not hold a Tokio
mutex across `.await`. Review cancellation: cancellation may abandon the caller
but cannot leave half-published in-memory state.

Ensure retirement/removal invalidates the exact lane index and releases its
reservation. Add deterministic instrumentation/test hooks for chunks scanned,
record headers parsed, and bytes materialized. Production metrics use aggregate
counters without lane IDs.

**Verify:** cancellation, retire, rebuild failure, and store drop each return
node retained-byte usage to the exact pre-index snapshot.

### Step 6: delete the duplicate page reader

Delete the page-time full `scan_lane` path once all callers use the index.
Do not retain two page readers.

**Verify:** a generated multi-chunk test reads N pages and asserts one rebuild,
then N bounded header/body materializations; `rg "read_tail_sync"` shows only
the indexed implementation/callers.

## Git workflow

Use two reviewable commits after rebasing on current `origin/main`:

1. `refactor(cell): own verified follower lane indexes`
2. `perf(cell): seek indexed follower tail pages`

The first commit must include rebuild/corruption/crash tests. The second deletes
the old page-time scan path and adds complexity proof. Rebase before push.

## Verification

```bash
test -d "$HOME/Workspace/crabbuild-target" && \
  test -w "$HOME/Workspace/crabbuild-target"
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-019-follower-index \
  cargo test -p crab-cell-runtime follower --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-019-follower-index \
  cargo test -p crab-cell-runtime node_log_recovery --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-019-follower-index \
  cargo test -p crab-http-server peer --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-019-follower-index \
  cargo clippy -p crab-cell-runtime -p crab-http-server \
  --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

## Acceptance criteria

- A multi-page sealed tail scans each existing chunk at most once per lane index
  generation; later pages read only selected record bodies.
- Returned frames and errors are byte-for-byte/variant-equivalent to the old
  canonical reader for valid, truncated, gapped, conflicting, and mutated data.
- Restart without index state rebuilds from chunk files and returns the same
  verified pages.
- Append/sync/rename crash injection never exposes an index entry for
  non-durable bytes.
- Memory is proportional to record metadata, never payload bytes, and every
  retained generation holds an exact node-byte ledger reservation.
- No node-frame, follower chunk, marker, HTTP, or serialized authority format
  changes.

## Test plan

- Table tests: empty, one frame, multi-chunk, oversized single frame, exact page
  boundary, first after end, first before retained base.
- Corruption: bad header, bad body digest, bad LTX, conflicting duplicate,
  sequence gap, seal watermark mismatch, post-index file mutation.
- Crash/restart: open suffix truncation, rotation before/after rename, seal
  before/after marker sync, rebuilt index equivalence.
- Complexity: read N pages from a large prefix and assert one scan generation
  plus N bounded materializations, not N complete scans.

## Done criteria

- [ ] Only **Files in scope** changed.
- [ ] Old page-time scan path is deleted.
- [ ] Header/body mutation and every filesystem fault boundary fail closed.
- [ ] Ledger usage returns to baseline on retire, error, cancellation, and drop.
- [ ] Focused runtime/server tests, Clippy, and formatting pass.
- [ ] Generated complexity proof shows one scan generation plus bounded page
      materialization independent of prior prefix size.
- [ ] `git diff --name-only c86dd43423ae...HEAD` contains no unplanned path.

## Stop conditions

- A shipped contract requires the current scan/truncation behavior and cannot be
  preserved without a migration decision.
- The index needs unbounded payload retention or unsafe memory mapping.
- Correctness would depend on trusting index metadata after chunk mutation.
- The external Cargo target volume is unavailable.

## Maintenance note

Any follower storage mutation must update or invalidate the lane index under the
same lock. New fields are derived acceleration data unless a separate reviewed
persistent-format decision says otherwise.
