# crab-git

`crab-git` provides the low-dependency Git mechanics shared by Crab’s push,
fetch, read, mount, and server paths. It translates between Git’s repository
formats and the contracts used by the Crab data plane without owning CLI or
remote-helper policy.

## Why it exists

Git discovery, ref validation, pack indexing, pointer detection, and worktree
behavior are easy to implement inconsistently. This crate gives every caller
one implementation for those correctness-sensitive operations and keeps the
product layers independent of a particular Git command flow.

## Architecture

```text
.git / Git URL
      │
      ├─ discovery, refs, worktrees, tags
      ├─ pack and object-database adapters
      ├─ reachable-object walking and pack locations
      └─ Crab / Git LFS pointer classification
                │
                ▼
       validated Git-domain values
```

The main surfaces are:

- `url`, `discover`, `ref_resolve`, `refname`, and `worktree` for repository
  identity and local Git layout;
- `receive_plan` for exact atomic ref comparisons, policy checks and bounded
  commit/tree/tag connectivity with generation-pinned proof frontiers;
- `receive_wire` for bounded native receive commands, advertisements and atomic
  status framing without consuming the following pack;
- `incoming_pack` for bounded full/thin-pack quarantine, with injected authorized
  base lookup and automatic private spool cleanup; `delta` for the shared bounded
  decoder used by incoming packs and remote reads;
- `pack`, `pack_locator`, `walk`, `odb_adapter`, and `repack` for immutable
  Git objects and pack mechanics;
- `pointer_detect`, `lfs_pointer`, `pointer_ref`, and `filter_attr_cache` for
  pointer-aware repository behavior;
- `tag` and `push_state` for annotated refs and push bookkeeping.

## Usage

```rust
use crab_git::{classify, PointerKind, RepositoryUrl};

let repository = RepositoryUrl::parse("s3://models/team/repository")?;
assert_eq!(repository.bucket, "models");
assert_eq!(repository.repo_prefix, "team/repository");

let blob_bytes = b"ordinary Git content";
match classify(blob_bytes) {
    PointerKind::Crab(pointer) => println!("Crab file: {} bytes", pointer.size),
    PointerKind::Lfs(pointer) => println!("LFS object: {} bytes", pointer.size),
    PointerKind::NotAPointer => println!("ordinary Git blob"),
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

For a local repository, use `discover_git_dir_from` and the ref helpers before
walking objects. `walk_reachable` returns the Git objects and Crab pointers
needed by the push pipeline; it does not upload anything.

## Feature flags and boundaries

- Default features keep the crate on the focused `gix-*` building blocks.
- `facade` enables the optional high-level `gix` repository facade for callers
  that need it.
- LFS pointer parsing belongs here, but LFS object bytes belong to
  [`crab-lfs`](../crab-lfs/README.md).
- Object-store paths and transport belong to [`crab-storage`](../crab-storage/README.md).
- Filesystem-free remote reads belong to
  [`crab-remote-git`](../crab-remote-git/README.md).

Errors preserve the distinction between malformed Git input, invalid refs,
pack corruption, and unsupported fetch conditions so higher layers can make a
user-facing decision without re-parsing error strings.

## Incoming pack boundary

`incoming_pack::quarantine` accepts a reader, an existing temporary directory,
explicit resource bounds, a cancellation probe and a thin-base lookup. Invoke it
on a blocking worker. Input reads and base lookups need caller-owned deadlines;
base lookup must authorize access before returning bounded object bytes. The
returned `IncomingPack` retains decoded objects in a private spool, exposes exact
object IDs/kinds/bytes, and removes its files when dropped.

This is an integrity boundary, not a Git publisher. It validates the complete pack
checksum, compressed streams, entry framing and delta reconstruction. Callers
use `receive_plan::validate` for object syntax, graph connectivity and exact ref
checks, then prove pointer payload dependencies and publish metadata under the
existing writer fences. Storage
credentials, canonical writes and the decision to acknowledge or reject a push
belong above this crate. HTTP push remains unavailable until those layers are
implemented.

`receive_wire` preserves command order and exact old/new SHA-1 values, limits the
command section to 1,024 refs and 1 MiB, and rejects malformed refs, duplicate
destinations and unadvertised capabilities. Call its synchronous reader on a
blocking worker with a transport deadline. The caller must validate pack presence
and completeness, authorize writes and determine the actual publication outcome
before emitting status. An uncertain commit must fail the transport rather than
reporting a ref rejection. Shallow pushes, certificates, sidebands and push options
are not advertised. Framing alone does not enable HTTP push.

`receive_plan::GraphSource` supplies committed objects and an optional trusted
kind. A trusted kind must come from a generation-bound proof of the object's
complete closure; pack/locator presence alone does not suffice. Without that proof,
validation traverses the object. The planner checks every incoming object, supports
raw-byte tree names and external gitlinks, and rejects malformed/sortedness/mode
violations and protected `.git` aliases. It also preserves source lookup errors.

The plan applies all old-tip comparisons before reading objects, validates the
final ref namespace, requires commits for branch tips, checks ancestry when force
updates are prohibited, and peels annotated tags without changing submitted OIDs.
The caller supplies per-ref deletion/non-fast-forward policy; Git's wire protocol
has no separate force flag. Returned pointer dependencies still require artifact
storage checks. Recheck the pinned base under writer locks before publishing; a
validated plan alone is not an atomic commit or evidence of working HTTP push.
