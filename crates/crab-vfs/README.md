# crab-vfs

`crab-vfs` exposes Crab repositories as a virtual filesystem. It combines a
local or cloud-backed Git source, snapshot and overlay state, the canonical
read/hydration path, and a filesystem engine. FUSE and NFS integrations are
feature-gated so applications that only need source parsing or shared mount
contracts do not inherit platform-specific runtimes.

## Why it exists

A mounted repository must feel like a normal filesystem even though many file
contents are lazy Xet pointers. The VFS needs a stable Git tree snapshot,
write overlay semantics, background hydration, cancellation, refresh, and
consistent read-after-write behavior. This crate owns that integration rather
than putting filesystem lifecycle policy into the storage or Xet layers.

## Architecture

```text
MountSource
  ├── local .git / bare repository
  └── remote crab://, s3://, gs://, az:// source
          │
          ▼
blobless Git clone or local Git directory
          │
snapshot SQLite + overlay reconciliation + read-tree
          │
crab-read hydrator + shared chunk cache
          │
resolver (snapshot + overlay) → VFS engine → FUSE or NFS
```

`MountPipelineBuilder::execute` runs the preparation pipeline: source clone or
reuse, HEAD resolution, snapshot, overlay setup, reconciliation, index
population, hydration workers, resolver creation, and engine wiring. Mounting
and refresh are lifecycle operations performed outside the pipeline so a
daemon, coordinator, or foreground CLI can own cancellation.

The `fuse` feature enables the FUSE session and mount lifecycle; `nfs` enables
the NFS server path; `gix-facade` enables the optional Git facade integration.
FUSE/NFS deployments also need the corresponding operating-system support.

## Usage

Source detection is available with either `fuse` or `nfs`:

```toml
[dependencies]
crab-vfs = { version = "1", features = ["fuse"] }
```

```rust
use crab_vfs::source::MountSource;

let source = MountSource::parse("crab://models/team/project")?;
assert!(matches!(source, MountSource::Remote { .. }));

let local = MountSource::parse("./working-copy")?;
assert!(matches!(local, MountSource::Local { .. }));
# Ok::<(), Box<dyn std::error::Error>>(())
```

For a real mount, construct a `PipelineConfig`, run
`MountPipelineBuilder::new(config).execute()`, and hand its engine/resolver to
the selected FUSE or NFS lifecycle. Use a `CancellationToken` and shut down
hydration workers before releasing the snapshot, overlay, or Git resources.

## Boundaries

- [`crab-read`](../crab-read/README.md) owns verified pointer reconstruction;
  VFS supplies it with mount context.
- [`crab-git`](../crab-git/README.md) owns repository URL and Git mechanics;
  VFS owns mount lifecycle and filesystem presentation.
- [`crab-cache`](../crab-cache/README.md) owns reusable chunk/cache contracts;
  VFS owns per-mount integration and invalidation.
