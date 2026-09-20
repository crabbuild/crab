# Follower-affine recovery assignment and takeover

Status: PROPOSED
Priority: P0
Effort: XL
Risk: High
Planned against: `c86dd43423ae` (`origin/main`, 2026-09-20)
Design authority: `advisor-plans/follower-affine-failover-design.md`
Dependencies: plans 018 and 020; plan 013's implemented signed eligibility APIs

## Executor instructions

Implement on `codex/021-follower-affine-takeover`. Read node records/claims,
stable physical-node resolution, signed placement, scheduler election, peer
authentication/routes, router activation, actor takeover, and all recovery/
placement tests. Keep policy pure and authority in existing CAS stores. Use a
unique external Cargo target.

## Drift check

```bash
git fetch origin main
git diff --stat c86dd43423ae..origin/main -- \
  crates/crab-cell-runtime/src/node.rs \
  crates/crab-cell-runtime/src/node_log_state.rs \
  crates/crab-cell-runtime/src/placement.rs \
  crates/crab-http-server/src/cells/scheduler.rs \
  crates/crab-http-server/src/cells/router.rs \
  crates/crab-http-server/src/peer.rs \
  crates/crab-http-server/src/server.rs
```

Stop if claim authority, signed advertisement format, activation hints, or
takeover ownership changed. Plan 013 need not have protected qualification, but
its signed observation/eligibility and deterministic planner APIs must exist.
Do not add a second placement or recovery authority.

## Why this plan exists

Only `preferred_scanner(0)` schedules node recovery. It calls
`recovery_candidates(self.session, ...)`, and `recover_node_session` claims with
that scanner's boot session. Candidate selection does not prefer physical
`NodeId`s in the failed log. After seal, `takeover_proof` may authorize any live
claimant and ordinary cold placement receives `locality_bonus: 0` from signed
capacity. The surviving follower may become owner in Compose, but only by
incidental scheduling/routing.

Stable follower identity already survives restart: log membership records
physical `NodeId`, and the directory resolves it to one current live boot
session. Use that fact explicitly; never reuse the failed boot session.

Current coordinator policy is effectively:

```rust
if preferred_scanner(0, &nodes)? == Some(self.session) {
    self.schedule_node_recovery(now_ms).await?;
}
let candidates = directory.recovery_candidates(self.session, now_ms, available).await?;
```

The same scanner session then becomes `claimant`; original follower membership
is not an input to that decision.

## Target ownership split

- The shard-zero scanner remains the recovery coordinator/discovery owner.
- A pure policy ranks live recovery executors from failed-log members plus
  signed eligibility/capacity.
- The coordinator sends an authenticated advisory nomination before any claim.
- The target reserves its bounded queue, recomputes that it is the preferred
  eligible target, then CAS-claims for its own current session and enqueues work.
  The hint grants no authority.
- A pre-claim rejection advances immediately. Unknown delivery is retried for
  two seconds before any-node fallback. A crash after claim waits for the
  existing 30-second claim expiry; this plan adds no unsafe release/transfer.
- Sealing clears the claimant, so activation routing re-runs the deterministic
  member ranker from the sealed log rather than reading executor identity from
  the tombstone. Cell takeover still uses
  `NodeTakeoverProof`, control CAS, actor admission, and fresh restore.

## Files in scope

Only modify the drift-check paths, the existing private peer route/client
modules beneath `crates/crab-http-server/src/peer/`, and their adjacent tests.
Node/tombstone serialization, claim lifetime, Cell control, activation-hint
wire shape, and public configuration are read-only. Stop and amend the plan if
claim release/transfer or another serialized field appears necessary.

## Scope

- Expose the smallest verified recovery-candidate descriptor needed for policy:
  failed session/node, expiry/claim state, log epoch, stable member NodeIds.
- Add pure deterministic executor ranking and reason codes.
- Add coordinator assignment, authenticated recovery hint, idempotent receiver,
  bounded retry, and any-node fallback.
- Carry recovery locality into takeover activation without mutating signed
  advertisements or trusting caller-supplied bonuses.
- Record selected class (`member`, `nonmember`) and fallback reason through plan
  018 metrics/receipt using bounded labels.

## Out of scope

- Changing node lease, recovery claim, takeover proof, Cell control, or owner
  epoch formats.
- A public configuration flag or operator-picked primary.
- Keeping a writable SQLite standby or serving follower reads.
- Waiting indefinitely for a follower.
- Treating placement score or a peer hint as authority.

## Implementation steps

### Step 1: expose and rank verified recovery candidates

Add a crate-private/purpose-built `RecoveryCandidate` returned by the node
directory with verified failed-log membership and timing. Keep existing
serialized node records unchanged. Search all `recovery_candidates` consumers
and remove the session-only path when migrated.
Implement a pure ranker over candidate plus live signed advertisements. Hard
exclusions match plan 013: incompatible, stale, draining, critical pressure,
insufficient disk/memory/job capacity, duplicate physical node identity.
During the follower grace window, only live sessions resolving member `NodeId`s
are eligible. Use deterministic fixed-point score and stable hash tie-break; no
floating ordering.

**Verify:** `cargo test -p crab-cell-runtime node --locked` and
`cargo test -p crab-cell-runtime placement --locked` prove deterministic,
permutation-invariant ranking and stable-NodeId restart.

### Step 2: add one admitted nomination receiver

Add a bounded `mpsc` recovery dispatch queue in `cells/scheduler.rs`. Its
receiver is owned by the scheduler loop; a cloneable sender is injected into
the private peer receiver during server construction. The same existing two-job
semaphore, heartbeat, transport, recovery implementation, and metrics serve
periodic and hinted work. The loop selects between queue input and its one-second
scan tick; do not start a second scheduler.
Add one private mTLS nomination endpoint containing only the failed session and
coordinator-observed log epoch. Authenticate the enrolled peer, bound body and
deadline to 500 ms, reload canonical state, recompute signed eligibility and
deterministic rank, and require the local current session to be the preferred
target. Reserve queue capacity first; then call
`claim_expired(failed, self.session, now)` and consume the permit by enqueueing.
Return accepted only after enqueue. Full/ineligible/stale requests reject before
claim. A duplicate reloads the self-owned claim and deduplicates by failed
session/generation in the scheduler queue; arbitrary peer nominations cannot
force work.

**Verify:** `cargo test -p crab-http-server cells::scheduler --locked` and
`cargo test -p crab-http-server peer --locked` prove queue-full/ineligible
rejection before claim, peer authentication, duplicate delivery, stale
generation, and enqueue-before-success response.

### Step 3: coordinate bounded fallback without claim stealing

The coordinator never claims for another session. It sends nominations in
rank order. Explicit pre-claim rejection advances immediately. Lost/unknown
responses are retried only until `FOLLOWER_FIRST_GRACE = 2s` from first eligible
observation, then the same path ranks all eligible nodes. If a target dies after
claiming, the coordinator observes the claim and waits for the existing
30-second TTL; expired takeover retains current generation CAS.
Refactor scheduler recovery execution so a target resumes its own persisted
claim from queued or periodic discovery. Preserve cancellation and retry safety.
Do not add claim release, reassignment, or stealing.

**Verify:** paused-time scheduler tests prove explicit reject advances now,
unknown delivery transitions at two seconds, two coordinators converge, and a
post-claim target death waits for the existing 30-second expiry.

### Step 4: re-rank sealed-log members for takeover

For Cell activation after sealed recovery, re-run the pure ranker from the
sealed log's stable member NodeIds because `NodeTombstone::seal` clears the
claimant. Prefer the top live eligible member, else plan 013 normal placement.
Send the existing authenticated activation hint; destination revalidates
liveness, admission, takeover proof, and control CAS.
Remove the hardcoded-zero recovery-locality path for this decision without
adding caller-controlled signed fields. Ordinary non-recovery placement keeps
its current signed observation contract.
Add structured traces and bounded metrics for assignment class, attempts,
follower-grace expiry, rejection reason, and activation class. No raw IDs in
labels.

**Verify:** `cargo test -p crab-http-server cells::router --locked` proves the
sealed tombstone has no claimant dependency and all authority checks remain.

### Step 5: qualify member preference and nonmember fallback

Extend Compose qualification: first and second owner loss must select a live
original follower; a third scenario makes all original followers unavailable
and proves bounded any-node fallback with exact data.

**Verify:** the isolated Compose receipt reports two member selections and one
nonmember fallback with monotonic exact roots.

## Git workflow

Use three reviewable commits after rebasing on current `origin/main`:

1. `feat(cell): rank follower recovery executors`
2. `feat(http): dispatch claimed recovery hints`
3. `test(cell): qualify follower-affine takeover fallback`

The first commit is pure policy plus node-directory tests. The second adds the
single authenticated execution path and activation preference. The third adds
fault/Compose evidence. Rebase before push; no merge commit.

## Verification

```bash
test -d "$HOME/Workspace/crabbuild-target" && \
  test -w "$HOME/Workspace/crabbuild-target"
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-021-follower-affinity \
  cargo test -p crab-cell-runtime node --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-021-follower-affinity \
  cargo test -p crab-cell-runtime placement --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-021-follower-affinity \
  cargo test -p crab-http-server cells::scheduler --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-021-follower-affinity \
  cargo test -p crab-http-server cells::router --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-021-follower-affinity \
  cargo test -p crab-http-server peer --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-021-follower-affinity \
  cargo clippy -p crab-cell-runtime -p crab-http-server \
  --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Run the full Compose/RustFS cluster qualification in an isolated environment.
Use CI/protected infrastructure for multi-Pod partition and matched-latency proof.

## Acceptance criteria

- With one healthy eligible original follower, that physical node's current
  boot session receives the claim, executes recovery, and is preferred for
  takeover.
- Restarted follower resolution uses stable `NodeId` plus a new `SessionId`;
  stale/overlapping sessions fail closed.
- Two coordinators or duplicate hints converge on one persisted claim generation.
- An unavailable/full/draining target that rejects before claiming cannot block
  recovery beyond two seconds; a target that dies after claiming is fenced for
  the existing 30-second claim TTL before reassignment.
- No hint or placement decision can bypass node claim CAS, takeover proof, Cell
  control CAS, epoch increment, actor admission, or fresh Db restore.
- Existing two owner-loss proofs still preserve follower-only commits and exact
  monotonic roots.

## Test plan

- Pure ranker: determinism, permutation invariance, member preference, capacity
  exclusion, stable tie-break, restart resolution, grace-to-fallback transition.
- Claim races: two coordinators, duplicate/lost hint, target death before claim,
  after claim, during recovery, and after seal.
- Security: forged sender, request target differs from persisted claimant,
  generation mismatch, expired claimant, cross-fleet node.
- Router: executor follower, different surviving follower, nonmember fallback,
  destination rejection, activation-hint loss, control CAS race.
- Compose: two follower-affine losses plus all-followers-unavailable fallback.

## Done criteria

- [ ] Only **Files in scope** changed.
- [ ] One pure ranker owns recovery executor and post-seal takeover preference.
- [ ] One enqueue-only private nomination path exists; only the target claims.
- [ ] Queue capacity and policy rejection happen before claim CAS.
- [ ] Session-only recovery candidate selection is removed from production.
- [ ] Focused tests, Clippy, formatting, Compose, and receipt validation pass.
- [ ] `git diff --name-only c86dd43423ae...HEAD` contains no unplanned path.
- [ ] Protected multi-Pod evidence is deferred to plans 015/023 before release.

## Stop conditions

- Assigning another claimant cannot be made idempotent under current tombstone
  CAS semantics.
- The hint would need to carry authority or secrets instead of naming persisted
  state.
- Signed placement cannot prove target compatibility/capacity.
- A new public config or serialized compatibility layer appears necessary.
- The external Cargo target volume is unavailable.

## Maintenance note

Recovery affinity is an availability preference, not authority. New placement
inputs must update pure policy tests and fallback proof; they must not leak into
node/Cell CAS rules.
