# Object Storage Layout V1

This document is the normative, provider-neutral contract for Crab object
keys. It covers the core bucket-global and repository-local namespaces used by
the CLI, SDK, protected-push service, and managed service. Feature documents
may define additional paths, but they must not redefine the keys below.

The terms **MUST**, **MUST NOT**, **SHALL**, and **MAY** are normative.

## Scope roots

Every repository placement has two independent object-key prefixes:

- `repo_prefix`: mutable and immutable state owned by one repository.
- `global_prefix`: content and indexes shared across repositories in one
  authorization scope.

For a direct `crab://bucket/org/models` repository:

```text
repo_prefix   = org/models
global_prefix = .crab
```

For a managed or ACL-scoped view, both values come from the trusted storage
scope. Clients MUST use them exactly and MUST NOT derive `global_prefix` from
`repo_prefix`. A typical view uses:

```text
repo_prefix   = org/models/acl-views/v1/<scope>/<version>
global_prefix = org/models/acl-views/v1/<scope>/<version>/.crab
```

The bucket or container name is not part of either prefix. Given bucket
`team-data` and key `.crab/xorbs/abc`, provider adapters address the same key
as `s3://team-data/.crab/xorbs/abc`, `gs://team-data/.crab/xorbs/abc`, or the
equivalent Azure container object.

## Key grammar

Keys are UTF-8 object keys, not operating-system paths and not URLs.

- Stored object keys have no leading or trailing `/`.
- Prefixes used for list operations may end in `/`.
- Empty, `.`, and `..` segments are invalid.
- Joining two normalized components inserts exactly one `/`.
- Implementations MUST NOT case-fold, Unicode-normalize, percent-encode, or
  percent-decode validated key components during the join.
- Content hashes are lowercase hexadecimal. BLAKE3 and SHA-256 identities are
  64 characters unless an owning format explicitly states otherwise.
- Implementations MUST use string joining with `/`; functions such as
  Python's `os.path.join`, Java's `Path`, or Go's `filepath.Join` are not
  object-key constructors.

Language-neutral pseudocode:

```text
object_key(root, relative):
    require valid_prefix(root)
    require valid_relative_key(relative)
    return root + "/" + relative
```

Validation occurs before object-store I/O. URL parsing and percent-decoding,
when needed, happen once before the validated prefix reaches this layer.

## Bucket-global core

Paths are relative to `global_prefix`.

| Relative key | Owner and responsibility | Mutability |
| --- | --- | --- |
| `xorbs/{blake3}` | `crab-xet`: encoded chunk aggregates shared in the scope | Immutable, idempotent create |
| `shards/{blake3}` | `crab-xet`: reconstruction metadata shared in the scope | Immutable, idempotent create |
| `chunk_index_db/` | `crab-metadata`: scope-wide chunk-to-xorb SlateDB | Opaque; SlateDB owns children |
| `ref-registry` | `crab-metadata`: repository reachability and GC roots | Mutable JSON, CAS |

Xorbs and shards have no hash fan-out and no filename extension. Code outside
the metadata owner MUST NOT construct SlateDB child keys under
`chunk_index_db/`.

For direct repositories, the physical tree is:

```text
s3://{bucket}/
├── .crab/
│   ├── xorbs/{blake3}
│   ├── shards/{blake3}
│   ├── chunk_index_db/...
│   └── ref-registry
├── {repo-a}/...
└── {repo-b}/...
```

## Repository-local core

Paths are relative to `repo_prefix`.

| Relative key or prefix | Owner and responsibility | Mutability |
| --- | --- | --- |
| `manifest` | `crab-metadata`: authoritative ref map and publication pointer | Mutable JSON, CAS |
| `manifests/history/{generation20}-{blake3}.json` | historical manifest roots | Immutable |
| `manifests/{kind}-{blake3}` | manifest-referenced bulk metadata | Immutable |
| `metadata/{pack,shard}/segments/{blake3}.jsonl` | append-only metadata segments | Immutable |
| `metadata/{pack,shard}/indexes/{blake3}.json` | content-addressed segment indexes | Immutable |
| `metadata/pack-origin/{pack-id}.json` | version-bound pack integrity proof | Derived, replaceable |
| `metadata/generation-receipts/{generation20}.json` | committed metadata-index coverage for one manifest generation | Immutable, idempotent create |
| `packs/pack-{pack-id}.{pack,idx,rev}` | canonical Git pack body and indexes | Immutable |
| `packs/pack-{pack-id}.meta` | pack metadata sidecar | Derived, replaceable |
| `file_index_db/` | `crab-metadata`: file-to-shard SlateDB | Opaque; SlateDB owns children |
| `git_locator_db/` | `crab-metadata`: Git object-range SlateDB | Opaque; SlateDB owns children |
| `locks/` | coordination namespaces described below | Mutable |
| `lfs/objects/{aa}/{bb}/{sha256}` | Git LFS bodies | Immutable |
| `lfs/locks/{blake3(path)}` | Git LFS protocol locks | Mutable |
| `staging/{push-id}/` | protected-push temporary writes | Ephemeral, service-owned |

The `manifest` is authoritative. Physical `refs/`, `HEAD`, `pack-list`,
`shard-list`, or `commit-graph-summary` objects are compatibility or
feature-owned surfaces, not an alternate source of truth.

An optional feature that adds a repository-local namespace must document its
owner, relative grammar, mutability, reachability, and cleanup policy. The
feature owner constructs relative keys; `crab-storage::StoreLayout` supplies
the authorized scope roots.

## Lock namespaces

Lock keys identify what they protect:

| Key | Owner | Protected responsibility |
| --- | --- | --- |
| `{repo}/locks/{full-ref}/lock` | `crab-coordination` | one validated Git ref |
| `{repo}/locks/internal/{resource}/lock` | `crab-coordination` | one repository-internal resource |
| `{repo}/locks/files/{blake3(path-bytes)}` | native `crab lock` | one worktree path |
| `{repo}/lfs/locks/{blake3(path-bytes)}` | Git LFS lock protocol | one LFS path |

`full-ref` begins with exactly one `refs/`. Therefore `refs/heads/main` maps
to:

```text
org/models/locks/refs/heads/main/lock
```

It does not map to `org/models/locks/refs/refs/heads/main/lock`.

Internal resources are lowercase ASCII slugs. V1 defines
`git-object-locator`, `repack`, `batch`, and `history-recovery`. They do not
pretend to be Git refs.

Lock values are UTF-8 JSON objects with this language-neutral schema:

```json
{"holder":"opaque-unique-attempt-id","expires_at":1700000000}
```

`holder` identifies one acquisition attempt. `expires_at` is an unsigned Unix
timestamp in seconds; zero is a released tombstone. Acquisition uses
create-if-absent. An existing tombstone or expired lease may be replaced only
with compare-and-swap against the observed object version. Renewal and release
also use compare-and-swap after confirming the holder; release writes a
tombstone rather than deleting the key. A malformed value blocks acquisition
and requires repair instead of being overwritten.

### Hard cutover from duplicated keys

Crab releases v1.0.4 through v1.0.14 wrote the unintended legacy ref key:

```text
{repo}/locks/refs/{full-ref}/lock
```

V1 implementations MUST NOT construct, read, acquire, renew, or release that
key. They use only the canonical key. Existing duplicated-key objects are
inert and do not require a bucket migration.

This is a hard cutover. Operators MUST stop pre-cutover writers and either
verify no old lease is live or wait for the maximum configured old-client TTL
before enabling V1 writers. Old and new writers do not coordinate and MUST NOT
run concurrently.

## Conformance vectors

Implementations in every language must produce these exact UTF-8 strings:

| Input | Result |
| --- | --- |
| `repo_prefix=org/models`, manifest | `org/models/manifest` |
| `global_prefix=.crab`, xorb hash `a` × 64 | `.crab/xorbs/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa` |
| `global_prefix=.crab`, shard hash `b` × 64 | `.crab/shards/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb` |
| `repo_prefix=org/models`, ref `refs/heads/main` | `org/models/locks/refs/heads/main/lock` |
| `repo_prefix=org/models`, internal `git-object-locator` | `org/models/locks/internal/git-object-locator/lock` |
| `repo_prefix=org/models`, pack ID `b` × 64 | `org/models/packs/pack-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb.pack` |
| LFS SHA-256 beginning `a1b2` | `org/models/lfs/objects/a1/b2/{full-sha256}` |
| scoped `repo_prefix=org/models/acl-views/v1/scope/7-deadbeef`, manifest | `org/models/acl-views/v1/scope/7-deadbeef/manifest` |
| scoped `global_prefix=org/models/acl-views/v1/scope/7-deadbeef/.crab`, xorb hash `a` × 64 | `org/models/acl-views/v1/scope/7-deadbeef/.crab/xorbs/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa` |

For any other scoped placement, replace only the applicable root. Relative
keys do not change.

## Implementation ownership

- `crab-storage::StoreLayout` owns scope selection and common global/repo key
  construction.
- `crab-coordination` owns lease target validation and canonical lock keys.
- `crab-metadata` owns manifest, segmented metadata, receipts, and database
  relative layouts.
- `crab-lfs` owns LFS object fan-out; the LFS protocol layer owns LFS locks.
- Feature crates own their documented extension namespaces.

A caller must not prepend `.crab`, `refs`, `locks`, or `repo_prefix` to a key
that an owner has already qualified.
