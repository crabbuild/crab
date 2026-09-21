# Follower-affine failover design audit

Date: 2026-09-20
Planned against: `c86dd43423ae`
Verdict: executable after the named prerequisites; not production-qualified yet

## Evidence map

| Surface | Current owner/evidence | Audit result |
| --- | --- | --- |
| Discovery/job entry | `crates/crab-http-server/src/cells/scheduler.rs` | Every live node scheduler can discover recovery; the rendezvous scanner owns only bounded any-node fallback. |
| Claim fencing | `crates/crab-cell-runtime/src/node.rs`, `node_log_state.rs` | CAS/30-second expiry are safe; there is no release/transfer transition. |
| Follower evidence | `crates/crab-cell-runtime/src/follower.rs` | Durable chunks/markers remain authoritative; derived lane indexes support seek-only reads with exact header/body revalidation. |
| Witness recovery | `crates/crab-cell-runtime/src/node_log_recovery.rs`, `node_log.rs` | Conflict proof is strong; selected witness bytes and digest comparisons are bounded and file-backed. |
| Catalog/control validation | `catalog.rs`, `authority.rs` | Authenticated tail scopes group affected Cells by shard, avoiding an unconditional 256-shard discovery scan. |
| Immutable pin/load | `recovery_manifest.rs`, `crab-ltx/src/bundle.rs`, `replica.rs` | File-backed verified bundles and the shared artifact registry preserve mandatory immutable pinning while enabling same-host reuse. |
| Takeover routing | `crates/crab-http-server/src/cells/router.rs` | Authority remains correct; sealed-log member selection is explicit and advisory. |
| Artifact lifetime | scheduler/router/server/local disk | Pin and load construct different stores/budgets; a shared cache would otherwise never hit reliably. |
| Qualification | Compose, container, release workflows | Strong local correctness proof; release workflow now qualifies a run-scoped immutable candidate and promotes that digest without rebuilding; protected execution remains outstanding. |

## High-confidence findings and resolutions

1. **Unsafe immediate reassignment promise.** A claimed target cannot be skipped
   before the current 30-second TTL. Plan 021 now reserves/admission-checks before
   the target self-claims; pre-claim rejection advances immediately, while
   post-claim death waits for expiry. Every live scheduler uses the same path;
   no claim-stealing or peer nomination path is added.
2. **Indexed reads could trust stale headers.** Plan 019 now rereads the exact
   52-byte record header and body, uses transactional generation swaps across
   append/rotation, and charges every retained index generation to the runtime
   byte ledger.
3. **“Exact lookup per Cell” was not the claimed complexity.** Current catalog
   heads lack page key ranges. Plan 020 now groups scopes and loads each affected
   shard once, avoiding all 256 shards and repeated reads without a format change.
4. **Recovery streaming lacked an owner.** Plan 020 now gates on plan 010's
   verified multipart slice, factors one file-backed bundle builder/publisher,
   uses a disk-backed digest table, and retains the old algorithm only as a
   private test oracle.
5. **A second recovery dispatcher could become a work/claim DoS.** Plan 021
   keeps one scheduler/job engine. Follower affinity is recomputed from the
   failed log and local signed eligibility before claim; the rendezvous scanner
   supplies only the bounded any-node fallback.
6. **Sealed tombstones do not retain the executor.** Post-seal takeover now
   reruns the deterministic ranker over stable failed-log member NodeIds rather
   than reading a cleared claimant.
7. **Local artifact reuse had no shared owner.** Plan 022 selects one server-
   owned `Arc<RecoveryArtifactRegistry>`, one session root, and one canonical
   disk ledger injected into both scheduler pinning and router loading.
8. **Phase evidence crossed ownership boundaries.** Plan 018 limits server job
   phases to `claim`, `scope_validation`, `witness`, `pin_attach`, and `seal`;
   lease detection and first service remain end-to-end receipt timestamps.
9. **Raw receipt validation was duplicated shell policy.** Plan 018 assigns one
   typed `validate-cluster` command and requires every producer/consumer to use
   it.
10. **Published-image evidence was relabeled, not executed.** The release chain
    now builds an immutable candidate first, qualifies that digest, records it
    beside the raw receipt, then promotes the same manifest without rebuilding.

## Required execution order

```text
plan 010 verified bundle publication API
plan 012 byte/disk ledger + stale-session reclamation
plan 013 signed eligibility/planner APIs
                |
018 -> 019 -> 020 -> 021 -> 022
 |                         |
 +---------> 023 <---------+
              |
       protected release matrix (015)
```

Plans 019-022 are deliberately sequential because they share follower storage,
recovery signatures, scheduler ownership, and artifact lifetime. Plan 023 may be
developed after 018 in parallel, but its protected run gates release.

## Production usability gates

- Deterministic fault/complexity tests pass with exact work and resource bounds.
- The isolated Compose receipt proves two follower-affine losses and one bounded
  nonmember fallback with monotonic exact roots.
- Queue-full/ineligible targets reject before claim; post-claim death is visibly
  bounded by the existing claim TTL.
- Large tails show one index generation plus bounded page reads; large catalogs
  read only affected shards once.
- Local hits preserve mandatory immutable object pinning; eviction/restart uses
  object storage and releases/reconciles all reservations.
- Protected multi-Pod/provider/fault/scale receipts exercise the immutable
  candidate digest, and release tags resolve to that same digest.

## Remaining blockers

- Plans 010, 012, 013, and 015 are still marked IN PROGRESS. Their named API or
  qualification slices must be complete before dependent work starts.
- Plans 018-022 are implemented on the current branch with local unit and
  three-node RustFS evidence. Plan 023's candidate-first workflow is
  implemented, but it still needs one protected registry-backed release run.
- No numeric production RTO SLO is justified until phase receipts exist. The
  protocol bounds are two seconds before pre-claim nonmember fallback and the
  existing 30-second wait after a claimant dies.

## Audit conclusion

The follower-affine direction is the best fix for the observed recovery shape:
it reuses durable data already on a surviving physical follower while retaining
the existing object-store pin, claim fencing, takeover proof, epoch CAS, and
fresh-database restore. With the revisions above, each optimization has one
owner, one fallback, bounded resources, and an executable proof. Production
claims remain blocked until the implementation and protected gates complete.
