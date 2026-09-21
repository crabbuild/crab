# Follower-affine recovery assignment and takeover

Status: IMPLEMENTED — direct self-discovery is canonical; protected scale evidence pending
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

The historical path let only `preferred_scanner(0)` schedule node recovery and
did not use physical `NodeId` membership when selecting a claimant. The current
path lets every live scheduler inspect recovery, filters each follower to its
own stable `NodeId`, and retains the rendezvous scanner only for bounded
any-node fallback. After seal, `takeover_proof` still authorizes the current
live claimant and Cell control remains the owner authority.

Stable follower identity already survives restart: log membership records
physical `NodeId`, and the directory resolves it to one current live boot
session. Use that fact explicitly; never reuse the failed boot session.

Current scheduler policy is effectively:

```rust
if self.node.is_some() || preferred_scanner(0, &nodes)? == Some(self.session) {
    self.schedule_node_recovery(now_ms, preferred_scanner(0, &nodes)? == Some(self.session))
        .await?;
}
let candidates = if let Some(node) = self.node {
    directory
        .recovery_candidates_for_node(self.session, node, now_ms, available)
        .await?
} else {
    Vec::new()
};
```

The same scanner session then becomes `claimant`; original follower membership
is not an input to that decision.

## Target ownership split

- Every live node scheduler participates in recovery discovery. A stable
  physical follower filters its own candidate set first; the shard-zero
  rendezvous scanner remains the bounded any-node fallback coordinator.
- The scheduler reserves bounded work capacity, then the current session
  revalidates eligibility and CAS-claims the failed session. There is no
  peer-triggered nomination queue and no second recovery engine.
- A pre-claim capacity or fencing rejection advances immediately. A crash after
  claim waits for the existing 30-second claim expiry; this plan adds no unsafe
  release/transfer.
- Sealing clears the claimant, so activation routing re-runs the deterministic
  member ranker from the sealed log rather than reading executor identity from
  the tombstone. Cell takeover still uses
  `NodeTakeoverProof`, control CAS, actor admission, and fresh restore.

## Files in scope

Only modify the drift-check paths and their adjacent tests. Node/tombstone
serialization, claim lifetime, Cell control, activation-hint wire shape,
private peer routes, and public configuration remain unchanged. The canonical
implementation keeps recovery discovery in every node scheduler; no
peer-triggered nomination queue or second recovery engine is introduced.

## Scope

- Expose the smallest verified recovery-candidate descriptor needed for policy:
  failed session/node, expiry/claim state, log epoch, stable member NodeIds.
- Apply deterministic physical-NodeId affinity and signed capacity eligibility
  in every live scheduler; do not add a second dispatch or claim authority.
- Admit bounded recovery work before claim and use the existing two-second
  follower-first grace plus explicit any-node fallback.
- Carry recovery locality into takeover activation without mutating signed
  advertisements or trusting caller-supplied bonuses.
- Keep node claim CAS, heartbeat, takeover proof, and Cell control as the only
  authority; scheduler preference is advisory and restart-safe.

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
Implement deterministic candidate filtering over the failed log's stable member
`NodeId`s plus live signed advertisements. Hard exclusions match plan 013:
incompatible, stale, draining, critical pressure, insufficient disk/memory/job
capacity, and duplicate physical node identity. During the follower grace
window, each physical follower scheduler only claims sessions for its own
`NodeId`; after the grace window (or when no eligible follower is live), the
preferred scanner may use the bounded any-node fallback. Stable `NodeId` order
is the tie-break; no floating ordering or caller-supplied score is authority.

**Verify:** `cargo test -p crab-cell-runtime node --locked` and
`cargo test -p crab-cell-runtime placement --locked` prove deterministic,
permutation-invariant ranking and stable-NodeId restart.

### Step 2: keep one admitted scheduler path

Every live node scheduler uses the existing recovery job set and semaphore. It
loads canonical records, applies the stable-NodeId filter, reserves bounded
work capacity, and then enters the existing claim/heartbeat/recovery path. The
shard-zero rendezvous scanner is a bounded discovery fallback only; it does not
send a peer-triggered hint and cannot grant authority. Keep the one-second scan
tick and do not start a second scheduler.

**Verify:** `cargo test -p crab-http-server cells::scheduler --locked` and
`cargo test -p crab-cell-runtime node --locked` prove ineligible/over-capacity
claimants do not start work, physical follower identity survives boot-session
rotation, and all claims remain CAS-protected.

### Step 3: coordinate bounded fallback without claim stealing

Each follower scheduler claims only its own physical member sessions during the
two-second grace window. The preferred scanner may use the no-live-follower
fallback immediately when no eligible member is live; otherwise it admits
nonmember candidates only after the grace window. If a target dies after
claiming, every scheduler observes the persisted claim and waits for the
existing 30-second TTL; expired takeover retains current generation CAS.
Preserve cancellation and retry safety. Do not add claim release,
reassignment, or stealing.

**Verify:** scheduler tests prove follower-first assignment, capacity-drained
followers are excluded, fallback becomes eligible after two seconds, two
coordinators converge, and a post-claim target death waits for the existing
30-second expiry.

### Step 4: re-rank sealed-log members for takeover

For Cell activation after sealed recovery, re-run the pure ranker from the
sealed log's stable member NodeIds because `NodeTombstone::seal` clears the
claimant. Prefer the top live eligible member, else plan 013 normal placement.
Send the existing authenticated activation hint; destination revalidates
liveness, admission, takeover proof, and control CAS.
Remove the hardcoded-zero recovery-locality path for this decision without
adding caller-controlled signed fields. Ordinary non-recovery placement keeps
its current signed observation contract.
Keep structured traces and bounded metrics for recovery phase and terminal
result. Do not add raw node/session IDs or a second assignment label set.

**Verify:** `cargo test -p crab-http-server cells::router --locked` proves the
sealed tombstone has no claimant dependency and all authority checks remain.

### Step 5: qualify member preference and nonmember fallback

Extend Compose qualification: first and second owner loss must select a live
original follower; a third scenario makes all original followers unavailable
and proves bounded any-node fallback with exact data.

**Verify:** the isolated Compose receipt reports two member selections and one
nonmember fallback with monotonic exact roots.

## Git workflow

Use reviewable commits after rebasing on current `origin/main`:

1. `feat(cell): rank follower recovery executors`
2. `test(cell): qualify follower-affine takeover fallback`

The first commit is node-directory policy plus scheduler integration. The second
adds fault/Compose evidence. No peer nomination route or second scheduler is
part of this contract. Rebase before push; no merge commit.

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
