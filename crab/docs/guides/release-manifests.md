# Release Manifests

Dataset/model release records for Crab repositories.

Status: `create`, `verify`, `export`, optional `create --publish`, published
lookup by `--name`, remote listing, deep local metadata verification, and
deep Crab pointer byte reconstruction against the configured remote are
implemented. Ed25519 detached signature verification is also implemented.

## Commands

```bash
crab release create --name model-v1 --rev HEAD --output release.json --json
crab release create --name model-v1 --rev refs/tags/v1 --publish --json
crab release verify --manifest release.json --json
crab release verify --name model-v1 --deep --json
crab release list --remote --json
crab release export --manifest release.json --output portable-release.json --json
crab release export --name model-v1 --output portable-release.json --json
```

`crab release create` refuses dirty worktrees by default. Pass `--allow-dirty`
only when you intentionally want the manifest to describe the committed
revision while uncommitted files remain in the workspace.

`crab release verify --manifest` verifies the manifest identity and optional
signature metadata. Unsigned manifests are valid and report signature state as
`unsigned`. Signed manifests require `--signature <PATH>` and
`--public-key <PATH>`; Crab verifies an Ed25519 detached signature over the
canonical unsigned manifest identity. The public key and signature files may be
raw 32/64-byte material or base64 text. The manifest `key_id` and
`signature_digest` must match the Blake3 digests of those files.

`crab release verify --deep` resolves the manifest commit locally, compares the
manifest pointer inventory against Git, reconstructs each Crab pointer-backed
large file through the configured remote, and checks the reconstructed Blake3
digest and size. If the remote is not readable, verification returns `verified:
false` with a `release.deep.content_unavailable` issue instead of silently
falling back to metadata-only proof.

## Scope

Release manifests bind a Git revision to Crab-managed content and workflow
metadata: resolved commit, selected refs/tags, pointer inventory, large-file
hashes, file sizes, params, metrics, schema version, and optional detached
signature status.

Publication writes the canonical manifest under the repository
`.crab/releases/` namespace using conditional object-store writes. Re-publishing
identical bytes is idempotent; different bytes for the same release id conflict.
