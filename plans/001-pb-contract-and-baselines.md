# Plan 001: Freeze the PB contract and establish retained baselines

> **Executor instructions**: Follow every step and verification gate. Do not
> implement the PB layout in this plan. If a STOP condition occurs, report it
> rather than broadening scope. Update the plan row in `plans/README.md` when
> complete.
>
> **Drift check (run first)**:
> `git diff --stat 1f9dae74..HEAD -- crab/docs/architecture/pb-scale-repositories.md crab/scripts/e2e crates/crab-staging/src/scale_1tib_manual.rs`
> Reconcile any changed contract or workload before proceeding.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: direction, perf, tests, docs
- **Planned at**: commit `1f9dae74`, 2026-08-19

## Why this matters

The current proposal identifies the right large components but calls the new
remote format "v2", describes two visible CAS steps, and gives its staged-file
manifest an unbounded chunk array. Crab has no compatibility obligation: every
surviving Crab-owned serialized contract is canonical v1, and existing higher
version labels are replaced rather than migrated. This phase turns that hard
cutover into a testable contract before any format ships.

## Current state

- `crab/docs/architecture/pb-scale-repositories.md:113` describes upload then
  "CAS manifests and refs"; `:586-587` repeats separate CAS steps.
- `crates/crab-metadata/src/manifests.rs:16-57` defines manifest format version
  2 and `{repo}/manifest` as the single CAS target containing the complete ref
  map.
- `crates/crab-staging/src/stream.rs:255-264` returns complete-file
  `chunk_pairs` and `FileRecipe`; `crates/crab-staging/src/recipe.rs:43-82`
  records every chunk in a `Vec`.
- `crab/docs/architecture/pb-scale-repositories.md:471-482` proposes another
  complete-file chunk list in `FileStageManifest`.
- `crates/crab-staging/src/scale_1tib_manual.rs:1-8` explicitly calls itself a
  manual skeleton. `crab/scripts/e2e/run_production_scale_rustfs.py` provides a
  deterministic 200 GiB production-shaped workload, but not the 1 TB baseline
  or 10 TiB metadata simulation required here.
- At a 64 KiB average, 10 TB is roughly 150 million chunk occurrences. Any
  design whose memory grows with that count without spilling is rejected.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Script contract | `python3 crab/scripts/e2e/test_run_production_scale_rustfs.py` | prints `PASS` |
| Python tests | `python3 -m unittest discover -s crab/scripts/e2e -p 'test_*scale*.py'` | all pass |
| Docs references | `rg -n "Partitioned1|single CAS|bounded" crab/docs/architecture/pb-scale-repositories.md` | all three contracts found |

## Scope

**In scope**:

- `crab/docs/architecture/pb-scale-repositories.md`
- `crab/docs/architecture/pb-benchmark-contract.md` (create)
- `crab/scripts/e2e/run_pb_metadata_scale.py` (create)
- `crab/scripts/e2e/test_run_pb_metadata_scale.py` (create)
- `crab/scripts/e2e/run_production_scale_rustfs.py`
- `crab/scripts/e2e/test_run_production_scale_rustfs.py`

**Out of scope**:

- Rust production behavior or serialized metadata types.
- A claim that sparse files prove payload throughput.
- Provider qualification or production rollout.
- Hard-coded pass thresholds before reference hardware is recorded.

## Git workflow

- Branch: `advisor/001-pb-contract-baselines`
- Use conventional commits, for example `docs: freeze PB layout invariants`.
- Do not push or open a PR unless instructed.

## Steps

### Step 1: Correct the architecture contract

Revise the proposal to name the layout `RemoteLayout::Partitioned` with schema
version 1. Manifest, pointer, shard, staging, and local push-plan contracts also
retain the value/name v1; semantic names distinguish them, not higher version
numbers. Replace every separate
manifest/ref CAS sequence with: write immutable objects and metadata, upload
Git packs, then CAS the one unified manifest root. Replace the proposed
unbounded staged chunk array with a paged local recipe root and bounded
iterator. Mark current prepared-xorb/indexed-plan support as partial existing
implementation, not missing infrastructure. Freeze the initial partition
contract as 8 leading hash bits by default, with only 4 through 12 accepted;
changing those bounds before users requires another hard replacement of v1;
after users exist, stop and define an explicit compatibility policy. Record the
measured leaf/page byte and entry limits before Plan 002 serializes them.

**Verify**: run the docs-reference command; inspect that no commit sequence
contains separate `CAS refs` after `CAS manifests`.

### Step 2: Define a reproducible benchmark artifact

Create `pb-benchmark-contract.md` with workload identity, chunk profile,
logical versus materialized bytes, host CPU/RAM/disk/network, object-store
provider, Crab commit, command, result schema, warm/cold state, and failure
rules. Required metrics: wall time, CPU, peak RSS, local bytes read/written,
remote GET/HEAD/PUT/LIST counts and bytes, partitions touched, CAS retries,
cache mode, and correctness digest. Results are JSONL plus one immutable
summary JSON.

**Verify**: `rg -n "peak_rss|logical_bytes|materialized_bytes|object_store|cas_retries" crab/docs/architecture/pb-benchmark-contract.md` → every field exists.

### Step 3: Add the bounded metadata simulator

Create a Python simulator that generates chunk/file/xorb/partition cardinality
without materializing payload bytes. It must model 1 TB, 10 TB, 1 PB, alternate
chunk profiles, partition bits, duplicate ratios, recipe page fanout, and ref
counts. Stream JSONL rows; keep memory independent of total chunk count. Unit
tests must compare small exact fixtures and assert invalid/overflowing inputs
fail.

**Verify**: `python3 -m unittest crab/scripts/e2e/test_run_pb_metadata_scale.py` → all tests pass; `python3 crab/scripts/e2e/run_pb_metadata_scale.py --logical-bytes 1000000000000000 --output /tmp/crab-pb-model.jsonl` → exit 0 and bounded output row count.

### Step 4: Retain a v1 baseline

Extend the RustFS runner only enough to emit the benchmark contract. Run the
existing 200 GiB workload first, then a 1 TB logical workload on named reference
hardware. Record that sparse/logical workloads qualify control-plane behavior,
not storage bandwidth. Store results in the project’s approved CI artifact
system; do not commit bulky results.

**Verify**: the script contract and Python tests pass; the retained run has a
Crab commit, hardware profile, commands, metrics JSONL, summary JSON, and final
byte-identity result.

## Test plan

- Exact cardinalities for a tiny fixed input.
- Decimal PB versus binary PiB distinction.
- Invalid chunk size, partition bits, duplicate ratio, and integer overflow.
- Streaming output does not emit one row per chunk.
- Existing 200 GiB workload contract remains unchanged except metrics schema.

## Done criteria

- [ ] Architecture doc contains every decision in `plans/README.md`.
- [ ] No separate visible ref CAS remains in the PB commit sequence.
- [ ] 10 TB/1 PB models run without payload allocation or chunk-sized output.
- [ ] Retained 1 TB v1 baseline includes environment and correctness evidence.
- [ ] Script tests pass; only in-scope files plus `plans/README.md` changed.

## STOP conditions

- Current manifest is no longer the one CAS root.
- Benchmark would require real 1 PB storage allocation.
- Reference hardware or artifact retention location cannot be named.
- A proposed metric requires printing credentials or sensitive object URLs.

## Maintenance notes

Every later phase must extend the same benchmark schema. Reviewers should
reject throughput comparisons whose hardware, logical/materialized byte count,
or warm/cold cache state differs without being called out.
