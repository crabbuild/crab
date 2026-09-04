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
- `incoming_pack` for bounded full/thin-pack quarantine, with injected authorized
  base lookup and automatic private spool cleanup; `delta` for the shared bounded
  decoder used by incoming packs and remote reads;
- `pack`, `pack_locator`, `walk`, `odb_adapter`, and `repack` for immutable
  Git objects and pack mechanics;
- `pointer_detect`, `lfs_pointer`, `pointer_ref`, and `filter_attr_cache` for
  pointer-aware repository behavior;
- `tag` and `push_state` for annotated refs and push bookkeeping;
- `pre_push` for bounded, whole-batch hook input decoding, including exact
  object IDs, destination mappings, deletions, and duplicate-ref rejection.
  The caller chooses the byte limit and publication policy. Parsing SHA-256
  records does not establish SHA-256 support in the transport or object store.

`tag::peeled_revision_targets_at` inspects captured object IDs even when the
map keys are revisions or raw OIDs. `tag::peeled_tag_refs_at` deliberately
keeps the narrower `refs/tags/` scope used for manifest peeling hints. Both
return commit targets; neither implies support for arbitrary non-commit tag
targets in the product's push path.

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

`walk::scan_pointers` uses the same traversal with explicit distinct-object,
lookup-work and Gitoxide single-allocation limits. It consumes one ref closure
at a time and returns unique pointer candidates plus outstanding large-blob
headers, instead of retaining a full closure for every ref. Shared history consumes lookup work even when its objects are
already in the distinct-object union. Cancellation is cooperative between
object reads; callers must await their worker before releasing its local ODB.
Tags are resolved from the captured OIDs, including chains ending in commits,
trees or blobs; this does not extend the product's publication support profile.

Pointer discovery reads blob headers before bodies. Large ordinary blobs are
not inflated just to classify them. Missing/unreadable/wrong-kind blob objects
fail the shared walker; small candidate bodies must match their Git checksum.
The bounded scanner additionally verifies decoded commit/tree/tag checksums and
passes its allocation ceiling to the ODB for loose objects and packed deltas.
The returned `unchecked_blobs` are required work, not verified non-pointers.
`batch::verify_blob_batch` verifies native Git's ordered raw `cat-file --batch`
response against their captured OIDs, kinds and sizes, hashing bodies through a
64 KiB buffer with Git-compatible collision detection. Missing, reordered,
truncated, extra and checksum-invalid responses fail. The caller owns the
process, disables replacements/filters and supervises blocked I/O; the parser
does not spawn or detach workers. Mirror inspection/pre-push use this second
step before trusting the pointer inventory. Other reachable-set APIs retain
their documented narrower behavior. Neither the ODB allocation ceiling nor
the parser buffer is a total-process RSS guarantee; native Git delta memory,
pack-index mappings and filesystem I/O still need qualification.

`batch::visit_small_blobs` uses the same framing and checksum reader for LFS
discovery. It hashes every requested object's raw body and visits only blobs
within the caller's capture ceiling, retaining original request ordinals.
Both SHA-1 and SHA-256 Git OIDs are accepted here; this does not enable native
SHA-256 Crab transport. The parser reuses one 64 KiB body buffer per batch.
Callers must discard accumulated candidates after any later framing, checksum,
I/O, cancellation or child-exit failure, and enforce aggregate inventory limits.

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
credentials, canonical writes and receive-pack protocol responses belong above
this crate. HTTP push remains unavailable until those layers are implemented.

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
