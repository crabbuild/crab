# crab-types

`crab-types` is Crab’s dependency-light contract crate. It contains the data
shapes that must mean the same thing in the CLI, storage, metadata, workflow,
authentication, and service layers.

## Why it exists

Crab moves repository state through several crates and through persistent
objects. Shared types keep those boundaries explicit and prevent each caller
from inventing a slightly different pointer, storage identity, or replication
format. The crate has no network, filesystem, object-store, or runtime policy.

## What it owns

- `pointer` — the compact Crab pointer wire format: a BLAKE3 file hash, byte
  size, and optional shard hint.
- `storage` — provider kinds, normalized bucket identity, and auth-issued
  path scopes.
- `replication` — read-replica and active-active configuration contracts.
- `workflow` — stable stage hashes used by workflow and metadata consumers.
- `error` and `time` — cross-crate error categories and serialized timestamps.

The serialized forms are contracts. A change to a field, tag, or validation
rule requires checking every consumer and any persisted data that uses it.

## Architecture

```text
CLI / auth / storage / metadata / workflow
                         │
                         ▼
                   crab-types
             (wire shapes and identity)
```

Higher-level crates own behavior. For example, [`crab-git`](../crab-git/README.md)
parses Git and LFS pointers, while this crate owns the Crab-native pointer
format used by the data plane.

## Usage

```rust
use crab_types::pointer::{is_pointer, Pointer};

let pointer = Pointer {
    file_hash: [0x42; 32],
    size: 3,
    shard_hint: None,
};

let bytes = pointer.serialize();
assert!(is_pointer(&bytes));
assert_eq!(Pointer::parse(&bytes)?.size, 3);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Use `serde`/`schemars` derives on shared configuration types when a contract
must cross a JSON or YAML boundary. Keep implementation-specific state in its
owner crate; do not move a type here merely because two modules currently
share it.

## Feature and dependency policy

There are no optional features. The dependency surface is intentionally limited
to serialization and schema generation so this crate can sit at the bottom of
the workspace dependency graph.
