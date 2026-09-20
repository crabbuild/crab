# Qualify and promote the exact published server image

Status: PROPOSED
Priority: P0 release gate
Effort: L
Risk: High
Planned against: `c86dd43423ae` (`origin/main`, 2026-09-20)
Design authority: `advisor-plans/follower-affine-failover-design.md`
Dependencies: plan 015 receipt matrix and plan 018 raw cluster-receipt validator

## Executor instructions

Implement on `codex/023-qualified-image-promotion`. This is a release-chain
correctness fix, not a failover algorithm change. Build and publish an immutable
candidate first, run qualification against that exact digest, and promote the
same digest only after validation. Never mint an image-bound receipt from an
artifact produced by another image. Use a unique external Cargo target.

## Evidence and defect

`http-server-container.yml` currently qualifies local image
`crab-http-server:test`. `http-server-release.yml` consumes that raw artifact,
then builds a different multi-platform image and emits a new receipt containing
the new digest. `qualification_receipt emit` hashes caller-supplied evidence but
does not execute or inspect the named image. Source equality does not prove that
the published digest is the image exercised by failover qualification.

## Files in scope

Only modify:

- `.github/workflows/http-server-container.yml`
- `.github/workflows/http-server-release.yml`
- `.github/workflows/cell-runtime-qualification-contract.yml`
- `crates/crab-http-server/tests/qualify_compose_cluster.sh`
- `crates/crab-cell-runtime/src/bin/qualification_receipt.rs`

Keep negative fixtures generated inside the existing qualification-contract
workflow and Rust `#[cfg(test)]` module; do not add a second schema or validator.

Dockerfiles, runtime code, receipt cryptography, and failover behavior are
read-only. Stop if registry permissions cannot create an untagged/candidate
digest before release promotion.

## Target chain

```text
tagged source
  -> build and push linux/amd64 + linux/arm64 candidate manifest
  -> record immutable manifest digest and source/build attestations
  -> pull image@digest in each required qualification environment
  -> run raw receipt v6 validator and protected matrix
  -> emit/verify signed receipt bound to source + exact digest + raw artifacts
  -> promote the already-qualified digest to version/latest tags
  -> verify every promoted tag resolves to the same digest
```

Promotion changes tags only. It must not rebuild layers or a manifest.

## Implementation steps

### Step 1: make the reusable qualifier accept only an immutable image

Add an explicit workflow input for `image_digest_reference`. Release-mode jobs
must reject tags and require `registry/name@sha256:<64 hex>`. Pass that reference
as `CRAB_HTTP_SERVER_IMAGE` to Compose with build disabled. Retain local image
construction only for pull-request correctness jobs, whose evidence is clearly
marked source-only and cannot satisfy a release matrix row.

**Verify:** workflow contract fixtures reject a tag/empty digest in release mode
and show the digest reference in `docker compose config`. The qualifier records
the requested index digest, selected platform-manifest digest, and running
container config image ID in raw receipt v6.

### Step 2: build the candidate before qualification

Reorder `http-server-release.yml`: checkout exact tagged source, build and push
the multi-platform candidate under a run-scoped non-release tag, capture the
manifest digest, inspect that it contains linux/amd64 and linux/arm64, and emit
the build provenance before calling the reusable qualification workflow. Do not
create the public version or `latest` tag yet.

**Verify:** a workflow test/dry-run asserts qualification `needs` the candidate
job and receives its digest output; no promotion job can run before the receipt
matrix succeeds.

### Step 3: validate evidence before signing

Run plan 018's `qualification_receipt validate-cluster` on every raw cluster
artifact. Extend receipt emission so release mode resolves the requested OCI
index, proves the recorded platform manifest belongs to that index, and proves
the running container config ID equals that platform manifest's config digest.
Verify the complete plan-015 matrix against one source SHA and one index digest;
a source-only artifact is not accepted in a release matrix.

**Verify:** negative fixtures for relabeled digest, source-only artifact,
missing platform row, malformed cluster receipt, and mixed source SHA all fail;
the exact candidate matrix passes.

### Step 4: promote without rebuilding

After the signed matrix passes, create release/version/latest tags by copying
the already-qualified manifest digest with `buildx imagetools create` (or the
registry's equivalent manifest-tag operation). Reinspect every reference and
require exact digest equality. Publish chart/provenance references only after
that check. Failed qualification leaves only a run-scoped candidate eligible
for registry retention cleanup.

**Verify:** a fixture/integration job proves all promoted references resolve to
the candidate digest and that the promotion job contains no image build step.

## Verification

```bash
test -d "$HOME/Workspace/crabbuild-target" && test -w "$HOME/Workspace/crabbuild-target"
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-023-image-evidence \
  cargo test -p crab-cell-runtime qualification --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-023-image-evidence \
  cargo test -p crab-cell-runtime --bin qualification_receipt --locked
cargo fmt --all -- --check
git diff --check
```

The final proof must run in protected CI with registry write access. Retain the
candidate digest, raw version-6 receipts, signed matrix, and promoted references.

## Acceptance criteria

- Every release receipt names the exact digest actually pulled and exercised.
- The raw artifact binds requested index, selected platform manifest, and running
  config image; emit/verify rejects caller-supplied digest relabeling.
- The required provider/platform/fault matrix shares one source and one digest.
- Public tags are created only after matrix validation and resolve to that same
  digest; no post-qualification rebuild occurs.
- PR/local source-only receipts remain useful but cannot satisfy release gates.

## Done criteria

- [ ] Only **Files in scope** changed.
- [ ] Positive and relabeling-negative workflow/receipt fixtures pass.
- [ ] One protected dry run qualifies a candidate and promotes that digest.
- [ ] `git diff --name-only c86dd43423ae...HEAD` contains no unplanned path.
- [ ] Plans 015 and 018 link this digest-bound gate and remove any completed
      claim that source-only qualification covers a published image.
- [ ] `advisor-plans/README.md` records the protected run and marks 023 DONE.

## Stop conditions

- The registry cannot retain/promote an immutable candidate without rebuilding.
- Required architecture/provider runners cannot pull by manifest digest.
- Raw evidence cannot observe and bind the running container digest.
- Signing keys or protected credentials would need to enter an untrusted PR job.

## Maintenance note

Release evidence is valid only for bytes actually exercised. Any future image,
chart, or provenance workflow must preserve candidate-first qualification and
digest-preserving promotion.
