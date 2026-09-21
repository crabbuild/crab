# Same-host follower recovery and verified artifact reuse

Status: IMPLEMENTED — protected cache-reuse evidence pending
Priority: P1
Effort: L
Risk: High
Planned against: `c86dd43423ae` (`origin/main`, 2026-09-20)
Design authority: `advisor-plans/follower-affine-failover-design.md`
Dependencies: plans 012, 019, 020, and 021

## Executor instructions

Implement on `codex/022-local-follower-fast-path`. Read server construction,
`NodeLogHttpTransport`, `LocalFollowerTransport`, follower authorization, recovery
manifest pin/load, actor takeover, directory cache ownership, and corruption/
restart tests. Add local acceleration beneath the canonical recovery contract;
do not fork recovery logic. Use a unique external Cargo target.

## Drift check

```bash
git fetch origin main
git diff --stat c86dd43423ae..origin/main -- \
  crates/crab-http-server/src/server.rs \
  crates/crab-http-server/src/cells/scheduler.rs \
  crates/crab-http-server/src/cells/router.rs \
  crates/crab-http-server/src/local_disk.rs \
  crates/crab-http-server/src/peer.rs \
  crates/crab-http-server/src/peer/node_log_client.rs \
  crates/crab-cell-runtime/src/node_log_transport.rs \
  crates/crab-cell-runtime/src/recovery_manifest.rs \
  crates/crab-cell-runtime/src/actor.rs \
  crates/crab-ltx/src/replica/directory.rs \
  crates/crab-ltx/src/environment.rs
```

Stop if server transport composition, recovery bundle layout, or directory-cache
ownership changed. Extend canonical owners rather than create parallel readers.

## Why this plan exists

The historical production path constructed only `NodeLogHttpTransport`. Its
`remote(member)` resolved every physical member to an endpoint and used mTLS
HTTP, including when the recovery executor was that physical follower. The
current composition dispatches an exact self-`NodeId` to the authorized local
follower store and keeps mTLS for every other member.

After recovery builds a bundle, `RecoveryManifestStore::pin` still publishes the
immutable manifest and bundle. Activation first verifies the control-pinned
digest and consults the bounded server-owned artifact registry; misses,
evictions, and restarts use the canonical object-store reader.

The current production transport always resolves an endpoint first:

```rust
async fn remote(&self, member: NodeId) -> Result<RemoteFollower> {
    let advertisement = self.directory.resolve_node(member, now_ms).await?;
    // construct mTLS client
}
```

`RecoveryManifestStore::pin` calls `bundle().read_all()` before publication;
`load_overlay` later fetches the same digest-addressed bundle from the store.

## Target contract

One composite transport and one overlay loader:

```text
member == local stable NodeId
  -> authorize from persisted claim/log scope
  -> call local FollowerStore
member != local stable NodeId
  -> existing mTLS NodeLogHttpTransport

pin recovered bundle
  -> publish immutable object + manifest (mandatory)
  -> retain digest-addressed verified local artifact within disk budget
activate
  -> load/verify manifest authority
  -> if local artifact digest+scope match, reuse it
  -> otherwise canonical object-store bundle load
```

The local path must return the same types and errors and use the same frame/LTX,
manifest, and bundle verification as the remote/object path.

## Files in scope

Only modify the drift-check paths above, a new private server-owned artifact
registry module beside the Cell router/scheduler, and focused adjacent tests.
Recovery manifest/bundle formats, Cell control, node claims, and public config
are read-only. Plan 012's canonical local-disk ledger and stale-session
reclamation contract must be present before implementation; do not create a
second budget or ad hoc stale-session deleter here.

## Scope

- Compose a production `NodeLogTransport` that dispatches exact self-NodeId
  operations locally and other members remotely.
- Centralize claimant/log authorization so local calls prove the same persisted
  facts as the private HTTP handler.
- Retain recovery bundles in a bounded digest-addressed local artifact cache
  after successful immutable pin.
- Reuse a cached artifact only after loading/verifying the control-pinned
  manifest and checking bundle digest, Cell, incarnation, epoch, and positions.
- Fall back to canonical remote/object reads on miss, eviction, or restart.
- Account local artifacts in the existing node/runtime disk budget.

## Out of scope

- Skipping immutable bundle/manifest publication.
- Trusting local files by path, mtime, or prior process memory.
- Reusing the predecessor's writable SQLite directory.
- A general page-cache redesign; plan 011's verified directory cache and plan
  009's background hydration remain the owners of those concerns.
- A compatibility cache format or public cache configuration.

## Implementation steps

### Step 1: share local/HTTP authorization

Extract the smallest recovery authorization function from the peer handler so
both HTTP and local dispatch validate failed session, claimant, physical member,
log epoch, claim generation/expiry, and current node lease. Keep HTTP
authentication at the server boundary; do not pretend local calls have TLS.

Add a composite transport in `crab-http-server`, not `crab-cell-runtime`
policy. It owns local physical `NodeId`, `FollowerStore`, and existing HTTP
transport. Dispatch only on exact physical identity; ambiguous resolution fails
closed. Use the existing `NodeLogTransport` trait and delete any duplicated local
client logic.

**Verify:** local and HTTP table tests return identical receipt/page/error
variants for valid, stale-claim, generation-change, corrupt, and terminal-fence
cases; the self case records zero HTTP requests.

### Step 2: preserve witness equivalence

Ensure local seal/tail work uses the same lane lock, plan 019 index, page
bounds, checksums, LTX verification, receipt validation, and disk admission.
Remote witness comparison remains mandatory for other reachable members.

**Verify:** a mixed local/remote witness test detects a remote conflict and a
local corruption exactly as the all-HTTP transport does.

### Step 3: construct one server-owned artifact registry

Construct one `Arc<RecoveryArtifactRegistry>` in `server.rs`, rooted at
`session_dir/recovery-artifacts` and backed by the same runtime/local-disk
budget used by activation. Inject it into the scheduler's long-lived
`RecoveryManifestStore` and every router activation store. Remove the
scheduler's independent 512 MiB recovery budget. The registry key is immutable
bundle digest plus application/Cell/incarnation/Cell epoch and positions. Each
entry owns one RAII disk reservation and an immutable file handle/path.

Use bounded count and bytes from the existing ledger, with LRU eviction; do not
add a config knob. Clean shutdown and eviction delete files and release tokens.
Crash leftovers are never imported as trusted cache entries: plan 012 startup
inventory/reclamation accounts and later removes them only after its canonical
stale-session proof. Restart always uses object storage.

**Verify:** a construction test proves scheduler pinning and router loading see
the same `Arc`/root/budget; restart inventory accounts orphan bytes without
registering a hit, and canonical reclamation releases them.

### Step 4: populate only after immutable publication

Publish bundle and manifest to object storage before returning pins. Only
after publish success may the local artifact become eligible for reuse. Failure
leaves no control reference and removes/invalidates current scratch.

Move or retain the already file-backed bundle by atomic rename into the
registry; do not copy or `read_all`. Fsync the file and parent before publishing
the in-memory entry.

**Verify:** failures before/after bundle upload, manifest upload, fsync, and
rename leave no usable entry and release the exact reservation.

### Step 5: consume only after authority validation

In `load_overlay`, always load and digest-check the manifest named by control.
Select the row and validate scope/positions. Then try the local bundle by exact
digest; rehash before opening. Cache corruption deletes/quarantines the artifact
and falls back to object storage; object corruption still fails.

Transfer a strong entry/file owner into overlay restore so eviction cannot
unlink in-use bytes. Recovery-job completion drops only scratch, not registry
entries; eviction, invalidation, or shutdown owns cache lifetime.

**Verify:** hit, concurrent eviction, corrupt hit, miss, and restart tests all
produce the exact root; corrupt local data causes one object fallback.

### Step 6: qualify and observe

Add plan 018 metrics for local/remote follower reads, local artifact hit/miss/
corrupt, peer bytes avoided, and object bytes avoided. Use fixed labels only.

Extend Compose to assert the selected follower uses local tail reads and at
least one verified local artifact hit, while a restarted/evicted successor
proves the canonical remote/object fallback.

**Verify:** the isolated receipt records a local tail read and artifact hit for
the follower-affine case, and an object read with identical root after forced
eviction/restart.

## Git workflow

Use three reviewable commits after rebasing on current `origin/main`:

1. `perf(http): route self follower recovery locally`
2. `perf(cell): reuse verified recovery artifacts`
3. `test(cell): qualify local recovery fallbacks`

Keep authorization extraction with the transport change and corruption/disk-
budget proof with artifact reuse. Rebase before push; do not merge main.

## Verification

```bash
test -d "$HOME/Workspace/crabbuild-target" && \
  test -w "$HOME/Workspace/crabbuild-target"
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-022-local-recovery \
  cargo test -p crab-cell-runtime node_log_transport --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-022-local-recovery \
  cargo test -p crab-cell-runtime recovery_manifest --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-022-local-recovery \
  cargo test -p crab-cell-runtime --test actor --features process-test-support --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-022-local-recovery \
  cargo test -p crab-http-server peer --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-022-local-recovery \
  cargo test -p crab-http-server cells --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-022-local-recovery \
  cargo clippy -p crab-cell-runtime -p crab-http-server \
  --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Run the full Compose/RustFS qualifier in an isolated environment and compare its
phase/byte counters with plan 018's pre-fast-path baseline.

## Acceptance criteria

- Recovery executed on an original follower reads that follower through direct
  `FollowerStore`, with no loopback HTTP request/body encode for the local member.
- Other witnesses still use authenticated mTLS and participate in conflict proof.
- Every recovery overlay is durably pinned in object storage before Cell control
  references it, regardless of local cache success.
- A local artifact hit is accepted only after manifest and bundle digest/scope
  checks; corrupted local data falls back without changing the exact root.
- Cache miss/restart/eviction completes through the existing object path.
- Disk reservations are released on all error, cancellation, eviction, and
  shutdown paths.
- Fresh `crab_ltx::Db` restore remains mandatory; no predecessor writable state
  is reopened.

## Test plan

- Dispatch: exact self member, remote member, ambiguous/missing resolution,
  stale claim, claim generation change, terminal node fence.
- Equivalence: local and HTTP transports return identical receipts/pages/errors
  for valid and corrupt lanes.
- Artifact: hit, miss, restart, eviction, partial temp write, bad digest, wrong
  Cell/incarnation/epoch, changed manifest, object lost response.
- Crash matrix: after bundle upload, after manifest upload, before cache rename,
  after cache rename, before control attach, before activation.
- Compose: local fast path on follower-affine takeover and canonical fallback on
  forced nonmember/restart takeover.

## Done criteria

- [ ] Only **Files in scope** changed.
- [ ] Production composes one local-aware transport; no recovery fork exists.
- [ ] One server-owned registry is shared by scheduler and router and uses one
      canonical disk ledger.
- [ ] Manifest/bundle object pin remains mandatory and is proven in tests.
- [ ] Eviction/restart/orphan accounting returns reservations to baseline.
- [ ] Focused tests, Clippy, formatting, Compose, and doc validation pass.
- [ ] `git diff --name-only c86dd43423ae...HEAD` contains no unplanned path.
- [ ] Plans 015/023 retain protected provider/published-image release gates.

## Stop conditions

- Local authorization cannot prove the same persisted claim/log scope as HTTP.
- Reuse would require bypassing manifest or bundle digest verification.
- Artifact accounting cannot share the canonical disk budget.
- A new cache format/config compatibility surface appears necessary.
- The external Cargo target volume is unavailable.

## Maintenance note

Local recovery is a verified cache hit. Any future optimization must preserve
the same immutable object pin, control reference, digest checks, and canonical
fallback.
