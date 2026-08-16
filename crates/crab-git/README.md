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
