# crab-metadata

`crab-metadata` defines the schemas, codecs, indexes, and validation rules
that describe a Crab repository in object storage. It keeps the mutable
manifest small and makes large shard, pack, file, and chunk indexes immutable,
addressable, and independently verifiable.

## Why it exists

Git objects and Xet data are content-addressed, but a repository still needs a
consistent answer to questions such as:

- Which ref generation is current?
- Which shards and packs belong to that generation?
- Where can a file or chunk be reconstructed?
- Which receipts prove that an object was uploaded and committed?

Centralizing these payload contracts prevents readers and writers from
silently disagreeing about keys, generations, hashes, or serialization.

## Architecture

```text
{repo}/manifest                  mutable CAS root
        │ points to
        ├── segmented shard index immutable bulk object
        ├── segmented pack index  immutable bulk object
        ├── commit graph / refs   optional bulk summaries
        └── file_index_db         repo-scoped SlateDB index

.crab/chunk_index_db/             bucket-global chunk receipts and placements
```

`Manifest` is version 2 and contains the complete ref map, HEAD, generation,
and content hashes for larger metadata objects. `seal_git_validation` binds
the semantically validated Git state to a BLAKE3 digest; readers call
`validate_manifest_payload` before trusting refs or index pointers.

Payload modules cover manifests, segmented lists, pack metadata, commit-graph
summaries, ref registries, chunk/file indexes, receipts, transactions, and
canonical key/value codecs. Storage-backed helpers are feature-gated:

| Feature | Adds |
| --- | --- |
| `storage` | Object-store manifest, segmented-index, and prefilter helpers |
| `file-index-reader` | Read-only file-index lookup sessions |
| `local-index` | SQLite-backed local chunk index |
| `remote-index` | SlateDB remote index readers and writers |

Keep each SlateDB session's lifecycle explicit: every opened reader or writer
must be closed on success and error paths.

When validating dependencies against an already captured repository state, use
`FileIndexLookupSession::for_snapshot(&layout, &snapshot)` with a snapshot returned
by `read_repository_snapshot` for that layout. It scans only that snapshot's shard
inventory, including its captured journal overlay. Later manifests, journals and
file-index rows do not change its answers. This mode opens no SlateDB reader and
writes no checkpoints, so cancelling a lookup leaves no reader to close.
The ordinary `open`/`open_for_storage` methods continue to capture current state
and use acceleration where storage permissions allow it.

A selected shard is only a dependency candidate. Verify the file's content at
origin with `crab-read::pointer_proof`, and hold GC fences and recheck the exact
publication base before accepting a write. Snapshot lookup retains the canonical
scan's per-shard bounds; the composing request must also provide admission and
operation-wide limits.

## Usage

Create and validate a manifest payload without enabling any storage runtime:

```rust
use crab_metadata::manifests::{validate_manifest_payload, Manifest};

let mut manifest = Manifest::default_for_repo("refs/heads/main");
manifest.refs.insert(
    "refs/heads/main".into(),
    "0000000000000000000000000000000000000000".into(),
);
manifest.seal_git_validation();
validate_manifest_payload(&manifest)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

For remote indexes, construct a repo-aware layout and use the feature-gated
lookup or write helpers:

```toml
[dependencies]
crab-metadata = { version = "1", features = ["remote-index"] }
```

```rust
use crab_metadata::remote_index::RemoteIndexConfig;

let indexes = RemoteIndexConfig::for_repo("repositories/team/project");
assert_eq!(indexes.chunk_index_path, ".crab/chunk_index_db/");
```

## Boundaries

- [`crab-types`](../crab-types/README.md) owns cross-crate hash, storage, and
  replication types.
- [`crab-storage`](../crab-storage/README.md) owns object reads, writes, and
  CAS; metadata defines the objects and keys stored through it.
- [`crab-read`](../crab-read/README.md) owns reconstruction orchestration;
  this crate owns the metadata it consumes.
- [`crab-coordination`](../crab-coordination/README.md) decides when a new
  manifest generation is authoritative.
