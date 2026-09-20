# Cell-runtime qualification profiles

The JSON files in `profiles/` are canonical, versioned threshold inputs. The
receipt signer binds the profile digest; changing a threshold therefore makes
old evidence unusable.

Generate and verify a deterministic mixed primitive workload with:

```text
cargo run --locked -p crab-cell-runtime --bin qualification_receipt -- \
  workload workload.json profiles/pr-contract-v1.json 7
cargo run --locked -p crab-cell-runtime --bin qualification_receipt -- \
  verify-workload workload.json profiles/pr-contract-v1.json
```

The PR profile is a correctness gate. `local-provider-v1`, `fault-v1`,
`provider-v1`, `compatibility-v1`, and `scale-v1` are release-candidate inputs
only; they do not claim provider or Kubernetes qualification until a protected
run records matching receipts and artifacts signed by the pinned qualification
attestation key.
