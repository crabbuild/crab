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

Generate and verify a deterministic mixed primitive workload with:

```text
cargo run --locked -p crab-cell-runtime --bin qualification_receipt -- \
  workload workload.json profiles/pr-contract-v1.json 7
cargo run --locked -p crab-cell-runtime --bin qualification_receipt -- \
  verify-workload workload.json profiles/pr-contract-v1.json
```

Typed qualification adapters may use the bounded `QualificationWorkload::run_concurrent`
entry point when scheduled operations are independent or idempotent. The serial
`run` entry point remains the safe choice for workloads with application-level
ordering dependencies; both paths retain the same streaming schedule, counters,
latency histogram, and logical outcome digest.

The PR profile is a correctness gate. `local-provider-v1`, `fault-v1`,
`provider-v1`, `compatibility-v1`, and `scale-v1` are release-candidate inputs
only; they do not claim provider or Kubernetes qualification until a protected
run records matching receipts and artifacts signed by the pinned qualification
attestation key. Profile verification also rejects those non-PR profiles when
the receipt is local, has no measured object-store/RSS counters, has an
environment label that does not match the profile, or has no ownership
watermark proof. The command-line `emit` helper only creates threshold metrics
for the PR correctness profile; protected evidence must come from the real
provider/fault/scale harness so it cannot be promoted from a synthetic local
receipt.
