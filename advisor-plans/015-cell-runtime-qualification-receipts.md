# Cell runtime production qualification and receipts

Status: IN PROGRESS — schema-v5 receipt/profile binding, complete matrix verification, local RustFS provider/fault evidence, and the instrumented local warm-path probe pass; scale, matched latency, Kubernetes, and protected release-receipt gates remain
Priority: P0
Effort: XL
Risk: Medium
Planned against: `4a77b6f1252a` (2026-09-17)
Design authority: `crates/crab-cell-runtime/docs/canonical-ltx-scaling.md`
Dependencies: plans 006 through 014

## Executor instructions

Implement on `codex/015-cell-qualification`. Read all of
`crates/crab-cell-runtime/docs/delivery.md`, existing RustFS ignored tests,
Compose/Kubernetes qualification scripts, load-report validators, container
workflow, and release workflow. Reuse the current qualification owners and
receipt conventions. Never print credentials or run bucket-wide GC.

## Drift check

```bash
git fetch origin main
git diff --stat 4a77b6f1252a..origin/main -- \
  crates/crab-cell-runtime/docs/delivery.md \
  crates/crab-http-server/deploy \
  .github/workflows/http-server-container.yml \
  .github/workflows/http-server-release.yml \
  crates/crab-cell-runtime/tests \
  crates/crab-ltx/examples
```

If qualification ownership moved, extend the canonical harness rather than add
a parallel script. Stop if any prerequisite plan lacks passing local evidence.

## Why this plan exists

Unit and in-process tests cannot prove provider conditional semantics, cgroup
observation, multi-Pod fencing, bounded memory under large transactions, warm
latency, balancing convergence, or failover through real networking. Release
claims need signed, schema-validated receipts tied to exact source, image,
storage profile, workload, and fault.

## Required qualification matrix

| Dimension | Minimum evidence |
| --- | --- |
| Protocol | Fast/broad simulation and TLA+ summaries with source revision |
| Storage | Pinned RustFS, conditional writes, ranges, multipart, lost response |
| Publication | Multi-GiB native and bundle publish/restore with peak RSS |
| Warm path | Resident SQL/KV route with zero bucket calls and latency histogram |
| Churn | More Cells than capacity, eviction/reopen, no lost acknowledged roots |
| Fleet | Three nodes/Pods, weighted distribution, hysteresis, paced drain |
| Failover | kill, partition, latency, lost reply at commit/release windows |
| Primitives | SQL, Blob, KV, Queue, Workflow, Cron/activity/effect recovery |
| Accounting | advertised vs measured cgroup/memory/disk/job totals |
| Compatibility | mixed node advertisement/runtime versions per supported matrix |

## Receipt contract

Extend one versioned JSON schema rather than ad hoc logs. A receipt includes
source SHA, dirty flag (must be false for release evidence), image/chart digest,
Rust/tool versions, profile, provider/image digest, topology, workload seed,
fault schedule, timestamps, raw artifact digests, measured percentiles/RSS,
bucket-call counts, ownership epochs/roots/sequences, pass/fail, and signer/
attestation identity. Secrets and endpoints with embedded credentials are
forbidden.

Local warm-path evidence includes
`resident_route_reports_zero_origin_reads_and_latency_percentiles`, which
performs 64 resident-handle plus SQL reads after activation through a
`Store::with_read_request_observer` seam. The latest local run reported p50
67us, p95 90us, p99 364us, max 364us, and zero object-store read attempts.
`restored_sparse_route_promotes_before_zero_origin_reads` separately publishes
and drains a Cell, reacquires its exact root through a new runtime, waits for
verified sparse hydration promotion, and observes zero origin calls on the
subsequent SQL read. These values are local regression signals only; they do
not satisfy the matched-hardware, provider, or signed release receipt criteria
below.

The isolated local RustFS run on 2026-09-18 used RustFS 1.0.0-rc.1, a fresh
bucket, and separate `qualification/*-20260918-local` prefixes. The LTX
round-trip/parity/CAS-race case, Cell source-loss takeover, retention graph,
HTTP receive-fault, native HTTP push, and public collaboration/takeover cases
all passed. These results close the local provider/fault iteration seam; they
remain unsigned local evidence and do not satisfy the protected three-Pod,
multi-GiB RSS, matched-latency, or release-receipt criteria.

The canonical Compose qualification was also rerun from the current checkout
on 2026-09-18 with a freshly built `crab-http-server:local` image, RustFS, and
an isolated project/port set. Its version-5 receipt passed the validator
projection for two owner losses, follower replacement, exact-root monotonicity,
and follower-only durable primitive commits. The raw receipt is retained under
the external qualification target volume rather than tracked source. This is
process/local-disk evidence only; it does not satisfy the protected Pod,
matched-hardware, RSS, or signed-release rows.

The typed primitive owner-loss slice now uses the same fencing and takeover
boundary as the repository Cell path. `typed_blob_and_cron_recover_after_owner_loss`,
`typed_kv_namespace_recovers_after_owner_loss`, and
`typed_queue_namespace_recovers_after_owner_loss` drop the first runtime
without releasing authority, fence its exact session through `NodeDirectory`,
take over the published root, and verify the typed read/ack/tick result after
recovery. Workflow/activity owner loss remains covered by the native activity
failover test, SQL publication/source-loss is covered by the actor takeover
suite, and `typed_effect_source_publishes_claim_validation_ack_and_lost_lease`
now recovers the durable effect ledger before claiming and acknowledging it.
This is local in-memory ownership evidence; it does not close the protected
three-Pod primitive-fault receipt.

## Implementation steps

1. Inventory and map every existing test/script to the matrix. Delete no useful
   proof; consolidate duplicate setup and receipt emission.
2. Define/extend the receipt schema and fail-closed validator. Add positive,
   missing-field, type/range, source mismatch, artifact digest mismatch, dirty
   source, forged signature/attestation, and secret-redaction fixtures.
   The implementation now also provides a bounded canonical matrix manifest
   with one required row per qualification dimension, relative-path validation,
   duplicate/missing-row rejection, and a fresh-process `verify-matrix` command
   that recomputes every referenced artifact digest.
3. Add reproducible workload generators for large publication, resident reads,
   mixed primitive traffic, Cell churn, and skewed fleet demand. Seeds and
   concurrency are recorded; generators verify visible outcomes, not just HTTP
   success.
4. Instrument bucket operations, process/cgroup memory, disk, job ledger, route
   latency, ownership changes, and movement concurrency. Sampling method and
   known error must be recorded.
5. Extend local three-process/RustFS qualification for rapid iteration. It may
   prove process/local-disk loss but must label that it does not prove Pod
   partition behavior.
6. Extend Kubernetes three-Pod qualification with kill and directional
   partition/latency/lost-response faults. Require signed session expiry and a
   different session/higher epoch before takeover success.
7. Add matched before/after or Crab/celld benchmark profiles only where workload
   and hardware/resource limits are equivalent. External celld checkout belongs
   under `$HOME/Workspace/Github/denoland/celld`, is read-only, and its exact
   revision/config is recorded. Do not block Crab correctness evidence on celld.
8. Add CI tiers: PR fast contract tests; scheduled broad simulator/model/load;
   protected provider/Kubernetes qualification; release workflow consumes only
   validated receipts for the exact tagged SHA/image digest.
9. Update delivery/scalability docs with exact commands, environments, artifact
   locations, thresholds, and latest validated receipts. Claims without a
   receipt remain explicitly unqualified.

## Verification

At implementation time, document and run exact canonical commands for:

```bash
node crates/crab-cell-runtime/docs/validate.mjs
cargo fmt --all -- --check
make -C crab architecture-check

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-015-qualification \
  cargo test -p crab-ltx --features replica --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-015-qualification \
  cargo test -p crab-cell-runtime --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-015-qualification \
  cargo test -p crab-http-server --locked
```

Then run the canonical local and protected Kubernetes qualification entry
points. Feed each emitted receipt back through its validator in a fresh process.
Recompute every referenced artifact digest and compare. Release qualification
must fail when handed a receipt from a different SHA or image digest.

The latest isolated RustFS iteration on 2026-09-18 passed the canonical LTX
round trip, Cell source-loss takeover, retention sweep, HTTP receive-fault,
native HTTP push, and public collaboration/takeover tests. The runtime actor
suite also passes the process-level movement probes and the hydration shutdown
cancellation regression. These runs use a disposable local provider prefix and
are provider evidence for iteration; they are not signed release receipts and
do not close the protected Kubernetes or matched-hardware gates.

The receipt contract now includes a canonical matrix verifier. Its ten required
rows are protocol, storage, publication, warm-path, churn, fleet, failover,
primitives, accounting, and compatibility. The verifier requires one signed
passing receipt per row, checks the exact source/image identity, and hashes every
raw artifact listed by each receipt. This closes the validator implementation
gap; it does not claim that the protected rows have been executed.

The PR contract workflow now creates ten independent signed fixture receipts,
writes a canonical relative-path manifest, and invokes `verify-matrix` from a
fresh process. That job is deliberately a validator smoke test, not a
qualification receipt: real release rows must come from the corresponding
provider, scale, latency, and fault harnesses and retain their raw artifacts.

## Acceptance criteria

- [ ] Every matrix row emits schema-valid evidence tied to exact source and
      executable/container digest.
- [ ] Warm resident request proves zero bucket operations and reports p50/p95/
      p99 latency under a recorded load/profile.
- [ ] Large native/bundle publication reports bounded peak RSS and exact restore.
- [ ] Three-Pod faults preserve one owner and every acknowledged root/outcome.
- [ ] Fleet distribution converges within a declared tolerance; pressure traces
      show hysteresis and movement never exceeds the paced limit.
- [ ] Advertised resource totals reconcile with measured cgroup/disk/job usage
      within a declared tolerance.
- [x] SQL, Blob, KV, Queue, Workflow, Cron/activity/effect survive owner loss
      through the canonical local fencing/takeover tests; protected three-Pod
      fault evidence remains a separate matrix gate.
- [x] Receipt validator rejects dirty/mismatched/forged/incomplete evidence.
- [x] Release workflow cannot consume evidence for another source/image: it
      checks the exact source revision and artifact digest, then validates the
      receipt against the published image digest before attaching it.
- [ ] Any celld comparison records matched inputs and is labeled measurement,
      not proof of Crab safety.

## Stop conditions

- Environment cannot isolate a unique test prefix/bucket.
- Provider credentials would be logged or stored in an artifact.
- Fault injection cannot prove it occurred at the intended boundary.
- Receipt cannot bind source, image, workload, and raw artifact digests.
- A threshold is proposed without a baseline and product rationale.

## Maintenance note

Receipts are release inputs. Version schema changes explicitly, keep validators
fail-closed—including the library `verify_for` path rejecting signed
`passed=false` evidence—and never “fix” evidence after a run; rerun from exact
source.
