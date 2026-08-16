# Native LFS Import

Import LFS-format object-storage trees into Crab-native large-file content.

Status: implemented for explicit `fail`, `skip`, and `resolve` modes on
`crab import`. This is not the Git LFS compatibility command group; it is the
migration path for source buckets that already contain LFS pointer files.

## Commands

```bash
crab import --from s3://source/repo --to s3://dest/repo
crab import --from s3://source/repo --to s3://dest/repo --lfs-source skip
crab import \
  --from s3://source/repo \
  --to s3://dest/repo \
  --lfs-source resolve \
  --lfs-objects s3://source/lfs/objects
```

## Current Scope

- `fail` is the safety mode when the source is detected as LFS-format.
- `skip` omits LFS pointer paths and imports the remaining tree.
- `resolve` reads LFS objects from the selected object root, verifies SHA-256
  against the pointer OID, then publishes Crab-native content.
- Import summaries report `lfs_resolved`, `lfs_skipped`, and `lfs_failed`.
- Resume journals bind the selected LFS mode and resolved object identity.

Provider-specific LFS layout discovery is intentionally conservative. Use
`--lfs-objects` when the object root is not the default `.lfsstore` layout.
