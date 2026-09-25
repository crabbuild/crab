# Cell-runtime qualification profiles

The JSON files in `profiles/` are canonical, versioned threshold inputs. The
receipt signer binds the profile digest; changing a threshold therefore makes
old evidence unusable. Profile schema 2 also binds workload thresholds to the
measured environment: protected profiles carry required provider and topology
labels, minimum throughput, peak RSS/local-disk/file-descriptor ceilings, and
an object-store call ceiling. Matrix verification requires the corresponding
`cells`, `operations`, `duration_secs`, `p99_latency_ms`,
`peak_local_disk_bytes`, and `peak_file_descriptors` metrics when a profile
sets those limits; a missing metric is a failed gate, not an assumed zero.
The release CLI additionally rejects protected receipts finished more than
seven days ago or more than five minutes ahead of its verifier clock. The
historical library verifier remains timestamp-neutral; use the fresh protected
matrix entry point for release decisions.

Generate and verify a deterministic mixed primitive workload with:

```text
cargo run --locked -p crab-cell-runtime --bin qualification_receipt -- \
  workload workload.json profiles/pr-contract-v1.json 7
cargo run --locked -p crab-cell-runtime --bin qualification_receipt -- \
  verify-workload workload.json profiles/pr-contract-v1.json
```

After a real harness has written one receipt and one or more raw artifacts for
each matrix workload, build the canonical manifest from that evidence tree:

```text
evidence/
├── receipts/<workload>.json
└── artifacts/<workload>/<artifact files>

cargo run --locked -p crab-cell-runtime --bin qualification_receipt -- \
  manifest evidence/qualification-matrix.json evidence/
```

The builder emits schema-2 rows in the required order and rejects missing,
symlinked, or non-file evidence entries. The manifest output must be directly
under the evidence directory so its relative paths remain verifiable. It only
indexes files; it does not create or sign receipts. Run `verify-matrix` with the
protected profile and pinned attestation key before treating the resulting
manifest as release evidence.

The protected release bundle has one canonical fresh-process verifier. It
requires all nine provider, scale, compatibility, and fault profiles, rejects
symlinks anywhere below the bundle, checks each profile name and protected
threshold contract, and verifies every matrix against the same source, image,
and pinned signer:

```text
cargo run --locked -p crab-cell-runtime --bin qualification_receipt -- \
  verify-protected-bundle protected/ <source-sha> <image-digest> \
  <trusted-signer-hex>
```

The workflow and release gate call this command directly; they only retain
their shell-side byte-for-byte comparison between each supplied profile and
the tracked profile from the exact source checkout.

Typed qualification adapters may use the bounded `QualificationWorkload::run_concurrent`
entry point when scheduled operations are independent or idempotent. The serial
`run` entry point remains the safe choice for workloads with application-level
ordering dependencies; both paths retain the same streaming schedule, counters,
latency histogram, and logical outcome digest. Protected adapters should use
`run_with_case_coverage` or `run_concurrent_with_case_coverage`, which require
each result to bind the lifecycle case it exercised.

After a protected adapter has captured a verified run artifact, write the
non-secret execution identity and fault/ownership observations with the
`QualificationExecutionEvidence` schema, then bind the receipt with the
trusted signing key held outside the evidence directory:

```text
cargo run --locked -p crab-cell-runtime --bin qualification_receipt -- \
  bind-protected receipt.json <source-sha> <image-digest> profile.json \
  execution-evidence.json /run/secrets/qualification-signing-key \
  run-artifact.json workload.json raw/provider-events.json
```

The command rejects local profiles, non-canonical evidence, mismatched
workload/run artifacts, missing protected resource measurements, and a signing
key symlink. It never generates a protected receipt from the synthetic
`emit` path; the resulting receipt must still pass `verify-matrix` with the
pinned public key before release packaging.

Named provider profiles also require one canonical
`QualificationProviderEvidence` raw artifact beside the run and workload
artifacts. It binds the provider/profile digest and workload seed, and records
successful conditional, range, and multipart checks. Missing, duplicated,
partial, or mismatched provider semantics are rejected by both the binder and
the fresh-process matrix verifier.

The protected `scale-v1` primitives row also requires one canonical raw JSON
artifact with `schema_version: 1`, `profile`, the 32-byte `profile_digest`,
`workload_seed`, and `cell_samples`. It has exactly fifteen samples, in this
order: `empty`, `sparse`, `resident`, `pending-publication`, and `churned`, each
at 1,000, 5,000, and 10,000 open Cells. Every sample records `state`,
`target_cells`, and `before`, `after`, and `peak` snapshots. Each snapshot
contains `active_cells`, `rss_bytes`, `allocator_bytes`, `threads`,
`file_descriptors`, `sqlite_cache_bytes`, `admitted_resident_bytes`,
`admitted_file_descriptors`, `retained_bytes`, `local_disk_reserved_bytes`,
and `local_disk_bytes`. The protected harness must capture OS process counters
and runtime admission counters from the same isolated sample, starting with
zero active Cells and observing the requested count after opening them.
`peak` records each field's high-water mark over that sample; its fields need
not come from one instant. The
verifier rejects missing, duplicate, partial, or false-open samples, requires
the peak snapshot to cover before and after, checks the admission ledger's
minimum active-Cell charges, and requires the measured run's RSS, disk, and
descriptor peaks to cover every sample. This artifact makes the per-Cell
slopes calculable from raw before/after counters; signing a scheduled
10,000-Cell workload alone is insufficient evidence that those Cells opened.
Each sample's ledger increase must cover its open Cells. Within each workload
state, the verifier compares the net `after` minus `before` growth at 1,000,
5,000, and 10,000 Cells. Growth between those points in RSS and allocator
bytes must fit the additional native and SQLite page-cache reservations plus
retained-byte charges. SQLite cache and descriptor growth must fit their own
per-Cell reservations. This comparison cancels fixed process overhead rather
than charging it repeatedly to every Cell. The protected report must still
compare that measured fixed overhead with the node's separate process reserve.

The default workload contains a deterministic, seed-bound case schedule for each
primitive: `happy`, `retry`, `duplicate`, `expiry`, `cancellation`, `owner-loss`,
and `recovery`. Adapters inspect `QualificationOperation::case()` (or its
bounded hint accessors) to drive the corresponding primitive-specific behavior;
the schedule is a case plan, not evidence that an external provider or owner
fault actually occurred. The workload's outcome counts are forecasts used to
describe that schedule. A measured run binds the scheduled attempt count for
each primitive and records actual acknowledgements, rejections, ambiguity,
retries, and verification independently; it need not reproduce those
forecasts. A PR wiring smoke marks only the primitive/case pairs it actually
exercises and checks; other scheduled pairs stay unmarked.

The measured summary also emits `throughput_ops_per_sec` using a conservative
rounded-up duration; protected profiles still verify the raw elapsed duration
and their independent resource counters. Protected `primitives` receipts must
also match the measured run artifact's cells, operations, duration, throughput,
and p50/p95/p99/max latency metrics in non-decreasing order; a signed receipt
with substituted threshold values is rejected.
Measured run artifacts use schema 4 and include a bounded primitive/case bitset;
named provider/topology profiles reject artifacts missing any lifecycle case.
Those protected profiles also require every acknowledged operation to have an
independent verification result; a partial verification count cannot be
promoted by the receipt binder. The local observed smoke remains allowed to
report partial coverage and is not release evidence.
The PR profile is a correctness gate.
`local-provider-v1`, `fault-v1`, `provider-v1`, `compatibility-v1`, and
`scale-v1` are release-candidate inputs only. The provider-specific
`provider-{s3,gcs,azure}-v1` and `fault-{s3,gcs,azure}-v1` profiles bind the
object-store provider and Kubernetes fault topology explicitly; they do not
claim provider or Kubernetes qualification until a protected run records
matching receipts and artifacts signed by the pinned qualification attestation
key. Profile verification also rejects those non-PR profiles when
the receipt is local, has no measured object-store/RSS counters, has an
environment label that does not match the profile, or has no ownership
watermark proof. The command-line `emit` helper only creates threshold metrics
for the PR correctness profile; protected evidence must come from the real
provider/fault/scale harness so it cannot be promoted from a synthetic local
receipt.

## Release handoff

The HTTP server release uses three runs so the protected receipt and the
promoted image share one immutable digest:

1. Push an annotated `crab-http-server-v*` tag reachable from `origin/main`.
   `.github/workflows/http-server-release.yml` builds and Compose-qualifies a
   candidate, records its source, image reference, and digest in the
   `http-server-candidate-<run-id>-<attempt>` artifact, and stops before
   promotion. Keep this candidate run ID.
2. Dispatch `.github/workflows/cell-runtime-protected-qualification.yml` from
   that exact tag/commit with the candidate artifact's source as `source_ref`
   and digest as `image_digest`. Wait for its successful protected evidence run
   and keep that run ID.
3. Manually dispatch `.github/workflows/http-server-release.yml` **from that
   exact tag ref** with `tag`, `candidate_run_id`, and
   `cell_runtime_evidence_run_id`. The workflow rejects a dispatch SHA/ref
   different from the tag so its provenance cannot bind to another commit.
   The release reloads
   the original candidate instead of rebuilding it, repeats Compose
   qualification on that digest, verifies the complete protected bundle, and
   only then promotes the image and chart. An optional
   `cell_runtime_evidence_artifact` selects a non-default artifact name.

Use the same tag ref for both manual dispatches (with values read from the
candidate and protected run artifacts):

```sh
gh workflow run cell-runtime-protected-qualification.yml --ref "$tag" \
  -f source_ref="$source_sha" -f image_digest="$candidate_digest"
gh workflow run http-server-release.yml --ref "$tag" \
  -f tag="$tag" -f candidate_run_id="$candidate_run_id" \
  -f cell_runtime_evidence_run_id="$protected_run_id"
```

The protected workflow rejects a dispatch ref different from `source_ref` and uses a
protected self-hosted runner labelled `crab-cell-runtime-protected`, an exact
source commit and image digest, and the
operator-installed executable
`/opt/crab/bin/crab-cell-runtime-protected-qualifier`. That executable is the
provider/Kubernetes boundary; it must run the real workload and write the
complete evidence tree, and the workflow fails when it is absent. There is no
local, emulator, or synthetic fallback.

The protected workflow must be named `Cell runtime protected qualification`,
run against the exact release commit, and upload an artifact named
`cell-runtime-protected-<run-id>-<attempt>`. The artifact contains one
`protected/` directory with the tracked profiles, verified matrix manifests,
receipts, and raw artifacts. The workflow independently verifies every required
provider, scale, compatibility, and fault matrix with the pinned signer before
uploading it. The release job checks the run status, workflow name,
manual-dispatch event, run ID, attempt, and commit before moving that directory
beside the exact-source Compose receipt; the existing pinned-signer and
image/profile-bound matrix verifier remains authoritative.

The protected executable receives these arguments and must not print secrets:

```text
crab-cell-runtime-protected-qualifier \
  --source-sha <40-hex-commit> \
  --image-digest sha256:<64-hex> \
  --signing-key-file /run/secrets/crab-cell-runtime-qualification-signing-key \
  --output <directory>
```

It is responsible for isolated provider prefixes/namespaces, fault injection,
resource sampling, and writing the signed matrices. The workflow verifies the
result in a fresh Cargo process; it does not turn a command that merely claims
to have run a workload into release evidence.

The manual release verifies that the candidate artifact came from a successful
candidate job in this workflow on the exact source commit and tag-push run.
Missing, failed, stale, wrong-workflow, wrong-commit, symlinked, or malformed
candidate or protected evidence fails closed. No synthetic receipt is accepted
as a substitute.

The public host's local process-fault smoke can be run without credentials:

```text
cargo test --locked -p crab-http-server --test public_cell_process_fault \
  filesystem_owner_kill_ -- \
  --nocapture
```

It starts an owner, successor, and independent observer as separate processes,
kills the owner before writes, after leases, and after settlements, and
verifies all primitive outcomes through typed `CellNode` handles. The filter
selects all three local lifecycle-boundary tests. The
filesystem CAS backend is a deterministic lifecycle regression fixture only;
it is not a provider, Kubernetes, or large-scale qualification receipt.
