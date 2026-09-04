# Repository Object Store Key Layout

| Field | Value |
| --- | --- |
| Status | Proposal for standardization; implementation inventory plus decision record |
| Baseline | Initial inspection at `cf52d83`; source audit at `e26d139` |
| Investigation date | 2026-09-03 UTC |
| Scope | Direct object-storage repositories, authorized views, and currently implemented feature namespaces |
| Existing contract | [Canonical Object Storage Layout V1](../architecture/object-storage-layout.md) |
| Intended readers | Storage, metadata, Git transport, coordination, service, workflow, and maintenance owners |

The [evidence audit](object-store-key-layout-evidence.md) maps the factual
claims below to pinned source locations, dependency versions, and saved
measurements. It also records corrections and the limits of each proof.

## Contents

1. [Purpose and recommended direction](#1-purpose-and-recommended-direction)
2. [Repository and shared roots](#2-what-a-repository-is-in-object-storage)
3. [Key grammar and identities](#3-key-grammar-and-identities)
4. [Physical layout](#4-physical-layout-at-a-glance)
5. [Repository roots and metadata](#5-repository-roots-and-immutable-metadata)
6. [Git packs and read caches](#6-git-pack-storage-and-read-caches)
7. [Shared content and databases](#7-shared-content-registries-and-databases)
8. [Coordination and GC journals](#8-coordination-and-gc-journals)
9. [Feature namespaces](#9-feature-namespaces)
10. [Authority and reachability](#10-authority-publication-and-reachability)
11. [Kubernetes repository evidence](#11-evidence-from-the-inspected-kubernetes-repository)
12. [Optimization priorities](#12-optimization-priorities)
13. [Standardization and rollout](#13-standardization-and-rollout-decisions)
14. [Conformance and validation](#14-conformance-and-validation-plan)
15. [Operational use and finalization](#15-operational-use-and-finalization-checklist)

## 1. Purpose and recommended direction

Standardize the physical keys that make up a Crab repository, including the
objects outside its repository prefix that are required to reconstruct it.
Specify who owns each namespace, what gives its objects authority, and when
they may be removed. A directory tree alone is insufficient: deleting a small
pointer can make a large amount of retained content inaccessible.

The recommended baseline preserves the existing core names and the separation
between repository state and shared content. The most useful improvements are
to enforce exact key construction, complete lifecycle contracts, and avoid
redundant data. Moving every object under a new cosmetic hierarchy would add
migration cost without addressing those problems.

This document has three distinct kinds of statements:

- **Current:** behavior traced in the baseline source, with live observations
  identified separately. Source inspection establishes what these code paths
  implement; it does not establish that every behavior shipped or passed E2E.
- **Proposed:** the contract recommended for acceptance. “Must” in a proposed
  rule describes the target contract, not a claim that enforcement exists.
- **Open:** a decision or proof gap that must be resolved before finalization.

Recommendations, required retention edges, and acceptance criteria are design
judgments. They are not measurements or claims that the implementation already
satisfies them. “Immutable” in the key tables describes the owning format and
writer contract, not a provider-enforced object lock or a ban on GC deletion.

The existing architecture document remains the published normative reference.
This proposal records discrepancies rather than silently superseding it.
Acceptance requires reconciling that reference, implementation, and tests.
Writing this document changes no stored objects or runtime behavior.

### Recommended decisions

| Decision | Recommendation | Remaining work |
| --- | --- | --- |
| Scope roots | Keep independent repository and shared-content roots | Prove root allocation and validation at every entry point |
| Existing core paths | Keep current canonical names | Complete namespace registry and remove documentation drift |
| Name encoding | Validate first; transform feature names once; preserve final key bytes | Resolve `object_store::path::Path` conversion behavior |
| Authority | Define a repository snapshot as the compacted manifest plus committed ref-journal state | Update descriptions that say the manifest alone is always current |
| Database layout | Standardize database roots, keep SlateDB children opaque | Document checkpoint and maintenance ownership |
| Generated packs | Reuse canonical pack storage when the selected bytes are identical | Define descriptor format, retention, and release transition |
| GC | Require complete transitive reachability for every registered namespace | Close the concrete gaps in section 10 |
| History | Preserve explicit history retention and explicit pruning | Make the storage cost and recovery guarantees visible |
| Versioning | Version incompatible contracts; avoid aliases and implicit fallback readers | Audit release tags before choosing any transition |

## 2. What a repository is in object storage

`crab` in the inspected RustFS URL is the bucket. `k8s/` is the repository's
object-key prefix. It is not a directory containing checked-out Kubernetes
source files. Git trees, commits, and ordinary blobs are packed; large-file
content may be reconstructed from shared chunks. Metadata, coordination, and
derived read artifacts therefore appear alongside pack files.

Object storage has flat keys. `/` gives listing tools a prefix hierarchy; it
does not create a filesystem directory or provide directory transactions.
For S3, names are case-sensitive UTF-8 strings with a maximum length of 1,024
bytes, including prefixes. See [S3 object keys](https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-keys.html)
and [prefix listings](https://docs.aws.amazon.com/AmazonS3/latest/userguide/using-prefixes.html).

Every storage placement has two roots:

| Symbol | Meaning | Direct repository example |
| --- | --- | --- |
| `R` | `repo_prefix`: state owned by one repository placement | `k8s` or `org/models` |
| `G` | `global_prefix`: content shared within an authorized storage scope | `.crab` |
| `D` | A coordination or GC domain; explicitly either `R` or `G` | `k8s` or `.crab` |

The provider, endpoint, and bucket/container identify the store. They are not
part of `R`, `G`, or an object key. For example:

```text
bucket: crab
R:      k8s
G:      .crab

key:    k8s/manifest
key:    .crab/xorbs/ab/<full-content-hash>
```

The word “global” means shared in this scope, not universally accessible and
not necessarily located at the bucket's `.crab` prefix. Trusted
`StorageScope` values override both roots. An ACL view can use:

```text
R = org/models/acl-views/v1/<scope-hash>/<generation>-<snapshot-state-digest>
G = org/models/acl-views/v1/<scope-hash>/<generation>-<snapshot-state-digest>/.crab
```

Here `snapshot-state-digest` is `snapshot.journal.state_digest`, passed by
`materialize_view_with_store_and_credentials` to `view_prefix`. It is not the
compacted manifest's `git_validation_digest`: the view identity includes
journal state that may not yet have been compacted.

Writers and readers must use the supplied roots; they must not derive `G`
from `R`, assume all xorbs are bucket-global, or prepend an already-qualified
root. An inventory or backup of `R/` alone does not capture all dependencies
of a direct repository.

**Proposed allocation rule:** unrelated repositories must not overlap through
reserved owner namespaces. A placement under another repository's `metadata/`,
`packs/`, or GC candidate prefixes can be mistaken for that owner's data.
Intentional service-owned nesting, such as ACL views, needs an explicit owner
and cleanup boundary. Prefix validation and allocation are separate concerns;
the existing join helper alone does not prove either.

Sources: [`StoreLayout`](../../../crates/crab-storage/src/layout.rs),
[`StorageScope`](../../../crates/crab-types/src/storage.rs), and
[view materialization](../../../crates/crab-auth-server/src/view.rs).

## 3. Key grammar and identities

### 3.1 Final physical keys

The proposed contract carries forward these rules from the architecture
reference:

1. Keys use UTF-8 and `/`, independently of the host operating system.
2. Stored object keys have no leading or trailing `/`. A listing prefix may
   end in `/`; that is not a directory-marker object.
3. Empty, `.` and `..` segments are invalid. Reject them rather than repairing
   or resolving them.
4. Joining validated components inserts exactly one `/`. It must not
   case-fold, Unicode-normalize, percent-encode, or percent-decode them.
5. URL parsing and any feature-specific name encoding happen before the
   final-key boundary, exactly once at their respective owning layer.
6. Hash validators enforce the owning format's algorithm, length, and
   canonical representation. A generic “looks hexadecimal” check is not
   sufficient.
7. Full key length includes `R` or `G`, feature suffixes, and encoded names.
   Adopt a portable 1,024-byte upper bound with provider-specific checks where
   stricter constraints apply. Central enforcement is proposed, not proven.

Do not use filesystem path normalization or generic URL encoding to construct
stored keys. A literal percent sequence in a validated prefix is part of its
identity; it is not an instruction to decode another path separator.

### 3.2 Current SDK conversion gap

The workspace depends on `object_store` 0.14.1. Its `Path::from` constructor
encodes problematic path segments and removes empty segments. For example,
the dependency documents these transformations:

```text
Path::from("foo//bar")       -> "foo/bar"
Path::from("foo/../bar")     -> "foo/%2E%2E/bar"
Path::from("foo/foo%2Fbar")  -> "foo/foo%252Fbar"
```

Current layout and feature constructors use `Path::from`. Consequently, the
byte-preservation rules in the architecture reference are not established by
those helpers. `Path::parse` preserves an already-encoded string but also has
boundary-slash handling; substituting it without prior validation is not a
complete fix.

Workflow artifacts additionally encode logical names before constructing an
object-store path. The final physical encoding must be checked across the
whole writer/reader chain. Do not infer that a logical `model%2Fv1` suffix is
stored with exactly that spelling merely from the feature formatter.

**Proposed:** one validated final-key boundary, with explicit feature-name
encoding before that boundary. Test final stored keys, list/read round trips,
and URL-to-prefix parsing together. Do not introduce a reader that tries
multiple encodings. Any changed physical spelling needs the release and
migration decision in section 13.

Dependency evidence: `object_store` 0.14.1, `src/path/mod.rs`, its `Path`
documentation and `From<&str>`/`FromIterator` implementations; version pinned
in [`Cargo.lock`](../../../Cargo.lock). Feature evidence:
[`artifact.rs`](../../../crates/crab-workflow/src/artifact.rs).

### 3.3 Placeholder definitions

Placeholders in the tables below are not literal key segments. Their owning
serializers and validators define the exact identities.

| Token | Contract |
| --- | --- |
| `hash` | Lowercase 64-character content identity unless a row states otherwise; algorithm and preimage belong to that format |
| `h2` | First two hexadecimal characters of the associated full hash; 256 possible partitions |
| `pack-id` | Crab's content identity for canonical pack bytes; distinct from the Git checksum inside the pack |
| `validation-digest` | Identity of a validated Git state; not a generation number or pack checksum |
| `snapshot-state-digest` | Ref-journal snapshot identity; `RefJournalSnapshot.state_digest`, used to name ACL views |
| `generation20` / `index20` | Unsigned decimal number formatted to width 20 with leading zeroes |
| `ref-hash` | BLAKE3 of the canonical full ref name's UTF-8 bytes |
| `transaction-id` | BLAKE3 of the canonical serialized ref transaction; not a random UUID |
| `repo-hash` | BLAKE3 of `repo_prefix` UTF-8 bytes within its storage scope |
| `root-partition` | First four hexadecimal characters of BLAKE3 of the shard-hash string, not the shard hash's first four characters |
| `request-hash` | Generated-pack request identity, including the owning request/selection and visibility contract |
| `run-id` | UUIDv7 generated by the GC journal owner |
| `push-id` | Validated protected-push session identity owned by the service |
| `encoded-name` | Feature-encoded logical name; final physical conversion remains subject to section 3.2 |
| `version-hex` | Artifact manifest version identity without the API-level `b3:` prefix |

Not all 64-character identities are interchangeable. Xet content formats,
JSON metadata hashes, Git validation digests, request hashes, and LFS SHA-256
have different preimages or hash contracts. Rehashing parsed JSON is not
equivalent to hashing the bytes written by its canonical serializer. Git
pack checksums and `.idx`/`.rev` formats follow
[Git's pack-format contract](https://git-scm.com/docs/gitformat-pack).

Fan-out rules are namespace-specific. Xorbs use one two-character directory;
LFS bodies use two. Ref-registry root partitions hash the shard identity
again. Durable GC marks have their own partitioning. Do not consolidate these
by changing paths under a shared “hash prefix” utility.

## 4. Physical layout at a glance

This tree summarizes the core, not the set of objects every repository must
have. Features create their namespaces only when used. Tables in subsequent
sections enumerate the additional feature paths and lifecycle rules.

```text
<bucket>/
├── <G>/
│   ├── xorbs/<h2>/<hash>
│   ├── shards/<h2>/<hash>
│   ├── chunk_index_db/<opaque database children>
│   ├── ref-registry/
│   │   ├── records/<h2>/<repo-hash>.json
│   │   ├── shard-roots/<repo-hash>/<root-partition>.json
│   │   └── coverage.json
│   ├── gc/{closures,closure-segments,runs}/...
│   └── locks/internal/gc-fence/state
└── <R>/
    ├── layout
    ├── manifest
    ├── manifests/history/<generation20>-<hash>.json
    ├── manifests/{commit-graph,ref-registry}-<hash>
    ├── refs/journal/{heads,transactions,active,frontiers}/...
    ├── packs/pack-<pack-id>.{pack,idx,rev,meta,kinds}
    ├── generated-packs/v1/{artifacts,requests}/<h2>/...
    ├── metadata/
    │   ├── {pack,shard}/{indexes,segments}/...
    │   ├── pack-origin/<pack-id>.json
    │   ├── generation-receipts/<generation20>.json
    │   ├── git-visibility/v1/{digest,catalog}/<validation-digest>.json
    │   ├── git-visibility-pending/v1/<validation-digest>.json
    │   ├── git-visibility-edits/<hash>.json
    │   ├── shallow-closure/<validation-digest>.json
    │   ├── shallow-closure/entries/<hash>.bin
    │   ├── commit-graph/layers/<hash>.bin
    │   └── replica-discovery.json
    ├── file_index_db/<opaque database children>
    ├── git_object_catalog_db/<opaque database children and checkpoint markers>
    ├── locks/...
    ├── gc/runs/<run-id>/...
    ├── lfs/{objects,receipts,locks}/...
    ├── workflow/...
    ├── refs/crab/...
    ├── staging/<push-id>/...
    ├── protected-push-sessions/...
    └── acl-views/v1/<scope-hash>/<generation>-<snapshot-state-digest>/...
```

Physical `refs/crab/...` are feature-owned objects. Do not interpret an
arbitrary `R/refs/heads/main`, `R/HEAD`, `R/pack-list`, or `R/shard-list` as a
second authoritative Git representation. Generic path helpers or old tests
that can construct a name do not establish a current production namespace.

## 5. Repository roots and immutable metadata

All keys in this section are relative to `R`.

### 5.1 Publication and ref state

| Key | Purpose and owner | Mutation and lifecycle |
| --- | --- | --- |
| `layout` | `crab-metadata`: repository layout descriptor; validated by open/setup boundaries | Create and validate exact supported contract; not a tunable configuration file |
| `manifest` | `crab-metadata`: compacted repository state, refs, generation, and metadata roots | Conditional replacement; archive retained state before replacement |
| `manifests/history/{generation20}-{hash}.json` | Retained immutable manifest snapshot | Root until explicit history pruning; age alone is insufficient |
| `refs/journal/heads/{ref-hash}.json` | Per-ref committed/prepared transaction positions | CAS; owner validates embedded ref identity |
| `refs/journal/transactions/{transaction-id}.json` | Canonical transaction containing edits and new pack/shard dependencies | Immutable exact bytes; body hash verified on read |
| `refs/journal/active/{transaction-id}.json` | Atomic commit marker for a prepared transaction | Immutable create publishes the transaction; removed after safe compaction cleanup |
| `refs/journal/frontiers/{validation-digest}.json` | Journal positions already represented by a compacted manifest | Immutable; publish before the corresponding manifest CAS |

The current `layout` schema is version 1 and admits only `partitioned` with
canonical parameters: chunk/file/receipt partition bits of 8, recipe pages of
512 entries, and a 64 KiB recipe-page bound. These fields do **not** establish
physical `file_index_db/<partition>/` or `chunk_index_db/<partition>/` database
roots: the current database owners still use their documented single roots.
Unknown or mismatched descriptors are rejected. A naming standard must not
turn these fields into unsupported configuration switches.

### 5.2 Metadata objects

| Key | Meaning | Reachability and replacement |
| --- | --- | --- |
| `metadata/{pack,shard}/indexes/{hash}.json` | Content-addressed index of bounded metadata segments | Retain through all current, journal, and historical roots that need it |
| `metadata/{pack,shard}/segments/{hash}.jsonl` | Serialized pack entries or shard identities | Immutable; retain through referring indexes |
| `metadata/generation-receipts/{generation20}.json` | Index coverage receipt bound to generation and metadata roots | Immutable validated create; does not replace the manifest as authority |
| `metadata/pack-origin/{pack-id}.json` | Version-bound integrity evidence for a stored canonical pack | Derived and replaceable; verify bindings before trusting it |
| `metadata/git-visibility/v1/digest/{validation-digest}.json` | Visibility proof with an embedded object dictionary | Immutable; may be needed for current or historical reads |
| `metadata/git-visibility/v1/catalog/{validation-digest}.json` | Visibility proof over ordinals in a pinned Git catalog | Immutable; useful only with its matching readable catalog checkpoint |
| `metadata/git-visibility-pending/v1/{validation-digest}.json` | Visibility delta awaiting catalog-owner publication | Recovery state; current GC explicitly protects the current pending root |
| `metadata/git-visibility-edits/{hash}.json` | Immutable visibility evidence for ref edits | Referenced by publication/recovery paths; transitive GC proof is a finalization gate |
| `metadata/shallow-closure/{validation-digest}.json` | Bound shallow-fetch closure and its referenced objects | Immutable; descriptor and its dependencies form one closure |
| `metadata/shallow-closure/entries/{hash}.bin` | Binary shallow selection: object IDs and boundary commits for a tip/depth | Immutable; entries are uploaded before their referring descriptor |
| `manifests/commit-graph-{hash}` | Complete split commit-graph descriptor | Immutable, pinned by a manifest |
| `metadata/commit-graph/layers/{hash}.bin` | Descriptor-referenced commit records and parent ordinals | Immutable; retain every layer needed by a retained descriptor |
| `manifests/ref-registry-{hash}` | Optional bulk ref-registry dependency still recognized by manifest/recovery/GC readers | Immutable; no production publisher found at baseline; distinct from scope-wide `G/ref-registry/` records |
| `metadata/replica-discovery.json` | Replication owner: routing hints for primary and replicas | Mutable overwrite; not a content-addressed metadata leaf or Git commit point |

The general `manifests/{kind}-{hash}` helper does not authorize arbitrary new
`kind` values. Register concrete production consumers. The canonical snapshot
reader obtains pack and shard lists through segmented indexes. There is an
implemented exception that must not be hidden by that description:

| Noncanonical key relative to `R` | Current caller and behavior | Standardization status |
| --- | --- | --- |
| `manifests/shard-list` | CLI `run_compact_command` calls `run_compact_with_cancel`; `run_compact_inner` reads this standalone JSON list, CAS-updates it, then unions registry roots | Inconsistent with the canonical manifest's segmented shard-index path; not an alternate root used by `read_repository_snapshot` |

`read_shard_list` in the compactor returns an empty default when that key is
absent. Therefore a canonical-only repository can produce the compactor's
“no shards” path despite having manifest-referenced shards. This is a source
inference from the caller and callee, not an E2E result for the inspected repo.
Resolve the ownership/publication mismatch before declaring all CLI paths
conformant. See [compactor](../../src/cmd/compact.rs) and
[CLI dispatch](../../src/main.rs).

The optional bulk ref-registry field needs a release/ownership decision before
being promoted as an actively published format or removed as an unused one.

Sources: [manifest store](../../../crates/crab-metadata/src/manifest_store.rs),
[ref journal](../../../crates/crab-metadata/src/ref_journal.rs),
[descriptor](../../../crates/crab-metadata/src/layout_descriptor.rs),
[segmented storage](../../../crates/crab-metadata/src/segmented_store.rs),
[visibility](../../../crates/crab-metadata/src/git_visibility.rs),
[split commit graph](../../../crates/crab-metadata/src/split_commit_graph.rs),
[shallow closures](../../../crates/crab-metadata/src/shallow_closure.rs),
[receipts](../../../crates/crab-metadata/src/receipts.rs), and
[replica discovery](../../src/replication/discovery.rs).

## 6. Git pack storage and read caches

### 6.1 Canonical pack family

| Key relative to `R` | Purpose | Contract |
| --- | --- | --- |
| `packs/pack-{pack-id}.pack` | Canonical Git pack bytes | Immutable; content identity and Git pack structure are separate checks |
| `packs/pack-{pack-id}.idx` | Git OID-to-pack-offset lookup | Immutable, bound to the corresponding pack |
| `packs/pack-{pack-id}.rev` | Reverse index between pack and index order | Immutable, bound to the corresponding pack/index |
| `packs/pack-{pack-id}.meta` | Derived pack metadata | Replaceable under owner validation |
| `packs/pack-{pack-id}.kinds` | Object-kind evidence in pack-offset order | Immutable; bound to pack checksum and object count |

These five objects are a pack family, not five full copies of the repository.
Readers use different sidecars to avoid scanning or inflating the entire
pack. GC must apply the owner's retention rules to the family and associated
origin proof. A sidecar's small size does not make it optional for every
reader, and “derived” does not authorize arbitrary deletion during reads.

### 6.2 Generated-pack cache

| Key relative to `R` | Purpose | Current lifecycle |
| --- | --- | --- |
| `generated-packs/v1/requests/{h2}/{request-hash}.json` | Descriptor binding a read request to verified pack content | Immutable; GC retains sufficiently recent descriptors and their artifacts |
| `generated-packs/v1/artifacts/{h2}/{hash}.pack` | Materialized pack for a requested object selection | Immutable; retained through eligible cache descriptors |

The request identity and content identity solve different problems. Different
requests can produce identical bytes; identical-looking requests in different
authorization/visibility states must not share a descriptor accidentally.
Current descriptors contain version, request hash, content hash, Git checksum,
size, object count, and selection object count. The source's descriptor
version is **3**, despite the physical namespace being `v1`. Namespace and
payload versions are distinct contracts; neither can be inferred from the
other.

Current cache retention is evaluated from stored object age, not a last-access
timestamp. Repository GC uses the configured grace period with a one-hour
minimum; the normal default grace period is 24 hours. An old object is not
automatically deleted when it expires. It remains until maintenance runs and
the full sweep conditions permit removal.

**Optimization proposal:** when an exact selection already exists as a
verified canonical pack, the request descriptor should be able to reference
that pack without uploading a second body. Current `try_reuse_single_pack`
avoids rebuilding bytes, but `publish_cached_pack` still publishes them under
`generated-packs/`.

Acceptance requires an explicit descriptor target contract and one reader
path, not a “try canonical, then cache” lookup. An unexpired descriptor that
references a canonical pack becomes a retention edge even if repacking removes
that pack from the newest manifest. Both remote-helper and wire-protocol
callers, authorization identity, checksum validation, cache coalescing, and
GC must agree before the duplicate upload can be removed.

Sources: [pack generation/cache](../../../crates/crab-remote-git/src/pack.rs),
[remote helper](../../src/git/remote_helper.rs),
[wire upload-pack](../../src/git/upload_pack_wire.rs), and
[remote repository tests](../../../crates/crab-remote-git/tests/remote_repository.rs).

## 7. Shared content, registries, and databases

### 7.1 Shared content and root coordination

All keys in this table are relative to `G`.

| Key | Owner and purpose | Lifecycle |
| --- | --- | --- |
| `xorbs/{h2}/{hash}` | Xet chunk aggregates shared in the authorized scope | Immutable; collect only when no protected repository/shard closure references them and grace permits |
| `shards/{h2}/{hash}` | Xet reconstruction metadata | Immutable; roots come from repository snapshots and shared registry coverage |
| `ref-registry/records/{h2}/{repo-hash}.json` | Per-repository coordination record | CAS; independent records avoid one aggregate mutable object |
| `ref-registry/shard-roots/{repo-hash}/{root-partition}.json` | Bounded shard-root partition for one repository | CAS; update consistently with the registry protocol |
| `ref-registry/coverage.json` | Registry-repair coverage proof | Exclusive repair owner; missing records are not proof of unreferenced content |
| `gc/closures/{shard-hash}.json` | Bounded, verified summary of a shard's xorb closure | Immutable; bound to its shard and segment metadata |
| `gc/closure-segments/{shard-hash}/{index20}.json` | Bounded xorb-hash and file-hash lists for a closure | Immutable; publisher takes up to 4,096 entries from each list per segment, not 4,096 combined |

Xorbs and shards have no file extension. The `h2` fan-out makes 256 partitions
available to listing and maintenance. It does not force 256 requests for every
operation or justify adding fan-out to every tiny control namespace. Listing
strategy may adapt without changing the physical key grammar.

Shared bytes must be accounted for separately from bytes owned exclusively by
`R`. Summing the dependencies of multiple repositories double-counts shared
xorbs. Removing a repository prefix does not, by itself, authorize deletion
of anything under `G`.

Sources: [layout](../../../crates/crab-storage/src/layout.rs),
[registry](../../../crates/crab-metadata/src/ref_registry.rs), and
[GC closures](../../src/cmd/gc/closure.rs).

### 7.2 Database roots and opaque children

| Root | Owner | Logical purpose |
| --- | --- | --- |
| `G/chunk_index_db/` | Metadata/staging owners using SlateDB | Chunk-to-xorb lookup and deduplication acceleration |
| `R/file_index_db/` | Metadata owner using SlateDB | File-to-shard lookup |
| `R/git_object_catalog_db/` | Exclusive Git locator writer using SlateDB | OID locations, ordinals, pack rows, and derived pack membership |

These are standard/default placements, not proof that every configured
database uses them. `MetaDbConfig::default` sets the chunk path literally to
`.crab/chunk_index_db/`; `for_repo` sets the file path. The product's
`build_metadb_config_from` accepts file/chunk path overrides, and database
open uses the configured string. This audit has not established automatic
propagation of a trusted scoped `G` through every such configuration caller;
that remains a K1 gate. Inventory tools must resolve the actual configuration.

Database directories may contain `sst/`, `wal/`, `manifest/`, `compactions/`,
`gc/`, and checkpoint-related objects. Some files can be empty, and some
manifests or SSTs can coexist after compaction. Those observations do not
establish corruption or abandonment.

Standardize these database **roots**, not SlateDB's ULIDs, sequence numbers,
manifest suffixes, or internal GC filenames. Crab must not synthesize, move,
or delete database children through its generic repository sweep. SlateDB
owns their format and liveness graph. The pinned dependency is 0.15.0;
upstream concepts are described in [files](https://slatedb.io/docs/design/files/),
[checkpoints](https://slatedb.io/docs/design/checkpoints/), and
[garbage collection](https://slatedb.io/docs/design/gc/).

One explicit Crab-owned integration inside the catalog root is
`R/git_object_catalog_db/checkpoints/{catalog-digest}.json`. The locator owner
publishes this marker after database flush/checkpoint creation. A
catalog-bound visibility proof requires the matching checkpoint to be
readable; the proof JSON alone is insufficient.

Current catalog writer settings disable the ordinary data WAL and background
GC, use owner-driven checkpoint maintenance, and schedule GC at coverage
boundaries spanning 32 generations. Fence WAL objects can still exist. Older
catalog markers are retired by the writer, with old checkpoints given a
bounded expiry. File-index and chunk-index databases have different wrappers
and maintenance lifecycles; catalog settings are not universal SlateDB
defaults.

The pack-membership keyspace is derived writer state used to make stale-pack
cleanup proportional to affected rows. Canonical readers depend on OID,
ordinal, and pack rows, not that derived index. Its completeness protocol and
rebuild belong inside the database owner.

**Proposed:** report database maintenance separately from repository GC, with
checkpoint retention aligned to promised historical reads. Never attach a
blanket age-based lifecycle rule to an opaque database prefix.

Sources: [catalog writer](../../../crates/crab-metadata/src/git_object_locator/writer.rs),
[catalog reader](../../../crates/crab-metadata/src/git_object_locator/reader.rs),
[file-index database wrapper](../../src/metadata/metadb/db.rs), and
[metadata wiring](../../src/metadata/metadb/mod.rs).

## 8. Coordination and GC journals

### 8.1 Lease and admission keys

| Fully qualified key | Protected responsibility | Value and lifecycle |
| --- | --- | --- |
| `R/locks/{full-ref}/lock` | One validated Git ref | CAS lease; release writes a tombstone |
| `R/locks/internal/{resource}/lock` | One named internal owner or operation | Same lease protocol; resource name is an owner-controlled slug |
| `R/locks/internal/generated-pack-{request-hash}/lock` | Coalescing production of one generated-pack request | Lease protocol; request-derived resource names can accumulate tombstones |
| `R/locks/internal/git-read-admission-{slot}/lock` | Bounded Git-read admission slot | Lease protocol; capacity comes from admission policy |
| `R/locks/internal/push-admission/slots/{slot}` | Bounded push-admission slot | Admission lease; decimal slot, not a Git ref |
| `D/locks/internal/gc-fence/state` | Coordination between writers and a destructive sweep in domain `D` | Dedicated fence state with epochs/holders; not a `PushLock` JSON body |
| `{coordination-anchor}/clock` | Backend-time probe for the corresponding coordination object | Empty overwritten object; anchors include a lease `lock`, the push-admission `slots` prefix, or GC-fence `state` |
| `R/locks/files/{path-hash}` | Native file lock | File-lock owner/payload and released-state protocol |
| `R/lfs/locks/{path-hash}` | Git LFS protocol lock | LFS file-lock protocol; distinct from push leases |

`full-ref` already starts with exactly one `refs/`. For example:

```text
R = org/models
full-ref = refs/heads/main
key = org/models/locks/refs/heads/main/lock
```

Current internal lease users include `git-object-locator`,
`git-generation-owner`, `git-manifest`, `batch`, `history-recovery`, and
`repository-maintenance`, plus generated-pack request coordination. This is
an inventory, not permission to allocate arbitrary unrelated resources under
another owner's slug.

The current push-lease body includes `holder`, `expires_at`, and `lease_secs`.
An `expires_at` value of zero denotes a released tombstone. Backend object
modification time plus the lease duration participates in expiry decisions;
reading the wall-clock field alone is insufficient. Acquisition, takeover,
renewal, and release use the owner identity and observed object version.
`authoritative_expiry` also has an explicit branch for payloads with
`lease_secs == 0`: it uses `expires_at` for a non-released record. This is
current reader behavior; the code comment alone does not prove its claimed
release history. Push admission uses its own expiry helper and does not apply
that branch as a universal lease rule.

Native/LFS file locks use their own fields, including released-state handling;
they must not be decoded as push leases. GC fences also have a separate
protocol. A generic “delete expired JSON under locks” job would conflate these
contracts.

Tombstones explain why locks remain after successful operations. The inspected
59 released leases occupied part of a 4,493-byte coordination namespace; no
billing or request-cost measurement was made. Bounded slot keys can
be reused; content/request-derived resource names can accumulate. Any
tombstone compaction needs an owner-approved concurrent acquire/release proof,
including the associated clock objects.

Sources: [push leases](../../../crates/crab-coordination/src/push_lock.rs),
[read admission](../../../crates/crab-coordination/src/read_admission.rs),
[push admission](../../../crates/crab-coordination/src/push_admission.rs),
[GC fence](../../../crates/crab-coordination/src/gc_fence.rs), and
[LFS locks](../../../crates/crab-lfs/src/lock.rs).

### 8.2 Durable GC runs

The journal root is `D/gc/runs/{run-id}/`. Repository runs use `R`; shared-scope
runs use their shared domain. The run records its scope and retention policy
so resumption cannot silently reinterpret an old plan.

| Relative to the run root | Meaning | Mutation |
| --- | --- | --- |
| `state.json` | Run identity, phase, policy, progress, completion summary | Conditional updates |
| `batches/{index20}.json` | Bounded candidate/deletion plan | Immutable |
| `outcomes/{index20}.json` | Recorded result of a batch | Immutable |
| `marks/{namespace}/{partition}/{id}.json` | Durable reachability marks | Immutable shards; owner-generated UUID identity |

Completion removes temporary journal material according to the journal owner
while retaining the summary. A paused run is operational recovery state, not
unreferenced user payload.

Mark partitioning is separate from content fan-out: key-mode marks partition
using the first four hexadecimal characters of BLAKE3 of the complete key;
hash-mode partition widths use the mark owner's byte-based setting. Do not
infer a mark partition from a xorb's `h2` directory.

Sources: [GC journal](../../src/cmd/gc/journal.rs) and
[mark storage](../../src/cmd/gc/marks.rs).

## 9. Feature namespaces

These namespaces are part of the repository inventory even when absent from
a particular Git-only repository. Their owners must supply their own root
visitors and lifecycle behavior. A shared-looking word such as `xorb` does
not make a feature payload part of the core shared Xet namespace.

### 9.1 Git LFS

| Key relative to `R` | Purpose | Lifecycle |
| --- | --- | --- |
| `lfs/objects/{aa}/{bb}/{sha256}` | Complete Git LFS object body; `aa` and `bb` are the first two pairs of its SHA-256 | Content-addressed, hash-verified; conditional corrupt-object repair is owner-specific |
| `lfs/receipts/{aa}/{bb}/{sha256}.bin` | Verification receipt binding OID, size, physical object path, ETag/version, and verifier identity | Best-effort mutable overwrite; presence checks may use a matching receipt instead of streaming the body |
| `lfs/locks/{path-hash}` | LFS lock protocol object | Owner-managed release/expiry |

LFS bodies use SHA-256, not Xet chunk identities. Repository GC's general
candidate list does not cover `lfs/`; lifecycle policy belongs to the LFS
owner. Do not assume Xet deduplication, shared-scope placement, or ordinary
repo-GC retention applies.

Receipt failure or mismatch makes the reader hash the object body. The LFS
`delete` method deletes the body only, and the lifecycle listing/policy targets
`lfs/objects/`; receipt cleanup is not established by either path. Record this
as a separate lifecycle gap rather than assuming receipts are removed with
their bodies.

Sources: [LFS objects](../../../crates/crab-lfs/src/object_store.rs) and
[LFS lifecycle](../../src/lfs/lifecycle.rs).

### 9.2 Workflow stage cache and experiments

| Key relative to the selected repository/feature store root | Purpose |
| --- | --- |
| `workflow/stages/{h2}/{stage-hash}.json` | Stage-cache manifest |
| `workflow/xorbs/{file-hash}.xorb` | Cached output payload used by workflow reconstruction |
| `refs/crab/stages/{stage-hash}` | Published stage-cache pointer |
| `workflow/exp/{experiment-id}/meta.json` | Experiment metadata |
| `workflow/exp/{experiment-id}/stage-refs.json` | Experiment's stage references |
| `refs/crab/exp/{experiment-id}` | Experiment ref |
| `refs/crab/exp-meta/{experiment-id}` | Experiment metadata ref |

Workflow cache publication uploads output content, then the stage manifest,
then its conditional ref. Named artifact remotes can place output payloads in
another configured store; do not assume the primary repository prefix holds
every workflow dependency. GC must follow the workflow owner's published
refs and manifests rather than expiring everything under `workflow/` as one
cache class.

Sources: [workflow cache](../../../crates/crab-workflow/src/cache.rs) and
[experiments](../../../crates/crab-workflow/src/experiment.rs).

### 9.3 Artifact registry

| Key relative to `R` | Purpose and lifecycle |
| --- | --- |
| `workflow/artifacts/manifests/{encoded-name}/{version-hex}.json` | Immutable artifact manifest |
| `workflow/artifacts/payloads/{content-hex}/file` | Single-file artifact body |
| `workflow/artifacts/payloads/{content-hex}/tree.json` | Directory artifact descriptor |
| `workflow/artifacts/payloads/{content-hex}/files/{encoded-relative-path}` | Directory artifact file payload |
| `refs/crab/artifacts/{encoded-name}/versions/{version-hex}` | Published immutable-version ref |
| `refs/crab/artifacts/{encoded-name}/stages/{stage}` | Mutable stage pointer; current owner lowercases stage labels |
| `workflow/artifacts/pending/{promotion-id}.json` | In-progress promotion recovery state |
| `workflow/artifacts/history/{encoded-name}/{unix-ms}-{promotion-id}.json` | Promotion history and retention root |

Artifact version identity is not simply the file's content hash. The owner
hashes a canonical artifact manifest with identity/time fields normalized.
Public `b3:` prefixes are removed in physical digest segments. Promotion IDs
are canonical UUID strings; history timestamps are decimal milliseconds,
not the fixed-width generation format.

The name encoder keeps unreserved ASCII and encodes other UTF-8 bytes as
uppercase `%XX`. Directory paths encode each validated segment. These are
the feature's logical formatting rules; the SDK conversion issue in section
3.2 remains an explicit gate for the final physical grammar.

Artifact version refs, stage refs, pending records, and retained promotion
history participate in GC roots. Artifact history must not inherit
generated-pack expiry merely because both features store derived objects.

Source: [artifact implementation and root visitor](../../../crates/crab-workflow/src/artifact.rs).

### 9.4 Protected pushes and authorized views

| Key relative to the source `R` | Purpose and lifecycle |
| --- | --- |
| `staging/{push-id}/push-plan.json` | Prepared push plan |
| `staging/{push-id}/objects/{canonical-key}` | Staged upload envelope containing the fully qualified eventual object key |
| `protected-push-sessions/{push-id}.json` | Protected-push session state |
| `protected-push-sessions/{push-id}.verified.json` | Verified source materialization for the session |
| `acl-views/v1/{scope-hash}/{generation}-{snapshot-state-digest}/...` | Materialized authorized repository placement, with its own `R`, `G`, and applicable core namespaces |

The staged `canonical-key` is deliberately fully qualified. A repeated
repository prefix inside that envelope is not the accidental double-join
forbidden at ordinary layout boundaries. Receive validation restricts which
canonical destinations the envelope may publish.

After a successful receive, the helper attempts staging and prepare-record
cleanup; failures are warnings. Expired cleanup defaults to 24 hours and is
disabled when its TTL setting is zero. It excludes this context's push prefix
and two session keys, then checks other objects' modification times. That is
not proof that it discovers every other active session. These are service
rules, not the repo GC grace period. Finalizing a uniform cleanup policy
requires proving all live session roots, not just age or the current context.

Authorized views are complete scoped placements, not harmless cached JSON.
View expiry and retirement need a defined owner and reader-lease/checkpoint
contract. This proposal does not invent an existing automatic view collector.

Sources: [receive/session lifecycle](../../../crates/crab-auth-server/src/receive/session.rs),
[receive validation](../../../crates/crab-auth-server/src/receive.rs),
[helper cleanup calls](../../../crates/crab-auth-server/src/bin/crab_auth_receive.rs), and
[view builder](../../../crates/crab-auth-server/src/view.rs).

### 9.5 Explicitly outside this registry

Local `.git/` objects, `.crab/` worktree state, local caches, configuration,
workflow checkpoints on disk, build outputs, and proposed future managed
commit logs are not remote keys merely because documentation uses similar
names. A feature proposal such as [Continuity](continuity.md) does not allocate
a production object-store namespace until its implementation and lifecycle
are accepted.

## 10. Authority, publication, and reachability

### 10.1 Current publication protocol

A correct snapshot is more than a recursive listing or an isolated GET of
`R/manifest`. The sequence below describes journal-backed publication and its
compaction into the ordinary object-store manifest.

1. Data and immutable metadata needed by the publication are uploaded and
   validated first. Staged xorbs must be flushed before the dependent push
   is published.
2. Journal publication operates under the relevant ref leases. It writes an
   immutable transaction body and prepares the per-ref head objects with CAS.
   Prepared state is not yet visible.
3. Creating the transaction's `active` marker is the atomic visibility point
   for that transaction. Head promotion follows; a promotion cleanup failure
   does not undo a committed transaction.
4. `read_repository_snapshot` captures active transaction candidates before
   reading the compacted manifest and materializes committed edits relative
   to its frontier. Readers must use this coherent owner API rather than
   reconstructing authority from directory names.
5. Compaction publishes immutable metadata and a frontier, then conditionally
   replaces the compacted manifest. Historical roots are archived by the
   manifest owner. Safe cleanup can then remove folded active markers.

Direct manifest publication paths still have their manifest CAS boundary.
The design must describe both paths. “The manifest is the only publication
point” is not an accurate description of journal-backed publication.

Active-active receive has an additional authority boundary. Its
`commit_active_active_manifest` commits refs through the configured
coordinator, then calls `materialize_active_active_manifest_projection` to
write the regional `R/manifest`. That projection is not the coordinator's
write authority. This document inventories the object-store keys; it does not
specify the coordinator's separate persistence layout. See
[receive finalization](../../../crates/crab-auth-server/src/receive/finalize.rs)
and [manifest projection](../../../crates/crab-metadata/src/manifest_store.rs).

Sources: [snapshot and compaction](../../../crates/crab-metadata/src/manifest_store.rs)
and [transaction commit/cleanup](../../../crates/crab-metadata/src/ref_journal.rs).

### 10.2 Required retention graph

| Root or protected state | Dependencies that must remain usable |
| --- | --- |
| Current coherent repository snapshot | Compacted manifest, required journal positions/transactions, pack and shard metadata, Git visibility, and content |
| Retained historical manifest | Its indexes and segments, canonical pack families, shards/xorbs, visibility proof with readable dependencies, and optional graph |
| Committed journal transaction not safely folded away | New packs/shards, ref ancestry needed to resolve it, visibility edit evidence, and commit marker/head relationships |
| Pending visibility publication | Pending descriptor, its base/catalog or digest inputs, and edit evidence required to complete it |
| Catalog-bound visibility | Exact checkpoint/marker and the SlateDB files retained by that checkpoint |
| Commit-graph descriptor | Every split layer needed by its positional/parent references |
| Generated-pack request retained by cache policy | Verified artifact; a future canonical-pack target would be another explicit edge |
| Workflow/artifact root | Referenced manifests, payloads, stage/experiment state, and retained promotion recovery/history dependencies |
| Active protected push or authorized view | Owner-validated staging/session or scoped-repository dependencies |
| Shared shard root | Shard and all xorbs reachable through its verified reconstruction closure |

Retention is transitive. A manifest hash, a catalog-proof file, or a recovery
descriptor is not useful if its dependencies have already been collected.
Creating a GC root without a bounded visitor for its edges does not complete
the lifecycle contract.

### 10.3 Current sweep boundaries

Repository GC explicitly enumerates `packs/`, `generated-packs/`, `metadata/`,
`manifests/`, and the registered workflow/artifact/experiment prefixes. It
walks current and retained historical manifests, protects both visibility
formats, and preserves ref-journal objects. It does not use a recursive sweep
of the entire repository root.

The candidate list excludes opaque database roots, ordinary coordination
keys, `lfs/`, staging, sessions, views, and the GC journal itself. Each needs
its own lifecycle owner. “Repository GC ran” therefore does not mean “all
unnecessary keys under `R/` were removed.”

Normal, non-force collection protects objects inside the grace period;
referenced objects remain protected independently of age.
Shared-content collection additionally needs complete registry coverage and
scope-wide coordination. Provider expiration policies do not understand
these Crab reference graphs.

### 10.4 Proof gaps to close before standardization

These are bounded follow-up investigations from the baseline source, not
claims that data loss occurred in the inspected repository.

| Surface | Evidence at baseline | Required resolution |
| --- | --- | --- |
| Split commit graphs | Repository mark paths visibly add the pinned graph descriptor; transitive layer marking was not established | Prove or implement layer traversal in both preview and durable streaming paths; test a retained historical graph |
| Replica discovery | `metadata/replica-discovery.json` is mutable live routing state inside a generally swept prefix; an explicit mark was not found | Give it an explicit root/exclusion policy and a live retention test |
| Pending visibility and edit evidence | Current pending descriptor is protected; complete retention of its base/evidence inputs was not established | Trace interrupted publication through both GC implementations and prove recovery after grace expires |
| Ref-journal growth | Compaction removes active markers; transaction bodies, heads, and frontiers are retained | Define bounded retention only after proving current readers, historical recovery, and concurrent compaction dependencies |
| Historical catalog usability | Old catalog markers/checkpoints can retire while historical catalog-proof JSON remains | State which self-contained proof or retained checkpoint guarantees each promised historical read |
| Views and sessions | Service-owned cleanup exists for sessions; a complete view-retirement contract was not established | Document owner, active-reader protection, TTL/retention semantics, and retry behavior |
| Physical key validation | Normative byte-preservation conflicts with generic SDK conversion | Add exact writer/list/reader conformance at the final boundary |
| CLI shard compaction | Reachable CLI path reads/CAS-updates `manifests/shard-list`; canonical snapshot reads use segmented indexes | Reconcile the compactor with canonical publication and scoped roots; test a repository that only has canonical metadata |
| LFS receipt retirement | `LfsObjectStore::delete` removes the body; the inspected lifecycle paths enumerate only `lfs/objects/` | Define receipt cleanup at the LFS owner and prove concurrent verification/repair behavior |

The two repo-GC paths must share the same reachability invariant. A fix only
in preview can report safety that execution does not provide; a fix only in
execution can produce misleading plans. Shared-scope GC, workflow visitors,
LFS lifecycle, and database maintenance are sibling surfaces with different
ownership and must be checked explicitly.

Sources: [repo GC and candidate prefixes](../../src/cmd/gc/mod.rs),
[shared-scope GC](../../src/cmd/gc/bucket.rs), and the owners linked above.

## 11. Evidence from the inspected Kubernetes repository

The read-only RustFS inspection explains why this proposal covers more than
Git packs. The snapshot was taken at `2026-09-03T05:20:24Z` for bucket `crab`,
prefix `k8s/`. These are logical object sizes, excluding storage-system
replication/erasure-coding overhead and provider version history.

The [saved evidence](evidence/k8s-object-store-2026-09-03.json) contains the
235 key/size/time records, 16 attributed shared records, selected metadata
fields, saved pack headers, and the original full-stream hash results.
The [audit method](object-store-key-layout-evidence.md#saved-rustfs-evidence)
explains which claims can be recomputed offline and which require live data.

| Prefix under `k8s/` | Objects | Bytes | Interpretation |
| --- | ---: | ---: | --- |
| `packs/` | 15 | 1,321,391,505 | Three canonical packs with five members each |
| `generated-packs/` | 68 | 1,479,482,489 | 34 request descriptors and 34 artifact bodies |
| `git_object_catalog_db/` | 40 | 165,829,301 | Catalog data, manifests, fences, and checkpoint material |
| `metadata/` | 22 | 71,907,519 | Segmented indexes, visibility, receipts, and proofs |
| `file_index_db/` | 19 | 6,142 | Small file-index database and maintenance objects |
| `locks/` | 61 | 4,493 | 59 released lease objects plus GC fence state and clock |
| `manifests/history/` | 3 | 1,603 | Retained historical roots |
| `refs/journal/` | 5 | 1,967 | Ref publication/compaction state |
| `manifest` and `layout` | 2 | 871 | Current compacted root and layout contract |
| **Total** | **235** | **3,038,625,890** | Repository-prefix objects only |

At the original snapshot, the manifest was generation 3. Its catalog recorded
1,646,517 Git objects; the three distinct pack entries contained 1,646,507,
four, and six objects. Those records establish the count breakdown, not which
user commands created each pack. The prefix also depended on 16 shared objects
totaling 579,388,387 bytes: two shards, four GC closure records/segments, and ten unique
xorbs. This is a dependency attribution, not exclusive ownership of the shared
bytes; shared database and registry overhead is not included.

The strongest measured optimization was a byte-identical canonical pack and
generated pack, each **1,267,048,888 bytes**. Full streaming SHA-256 matched,
in addition to pack metadata checks. One extra copy represented about 41.7%
of the repository-prefix bytes. This supports canonical-pack reference reuse
more directly than adding hash directories or renaming metadata prefixes.

At the original snapshot, all observed generated-pack descriptors were older
than the default 24-hour grace period. That supported a maintenance preview,
not an unconditional deletion claim: collection must evaluate a fresh
snapshot, references, fences, and configured policy. Other large metadata
included a historical self-contained visibility proof of roughly 71 MB. Its small catalog-based
counterpart was not proof that the large file could safely be removed after
old checkpoint retirement.

**Follow-up observation:** the read-only LIST at
`2026-09-03T06:17:44.377640+00:00` returned **164 objects and 1,559,141,400 bytes**,
including **zero generated-pack objects**. Relative to the original inventory,
74 keys were absent (68 generated-pack and six metadata keys) and three keys
had appeared under `gc/`. The listing does not establish who changed them or
whether recovery/integrity checks pass. The duplicate-pack result above is
historical evidence for the optimization proposal, not a current reclaimable
byte estimate.

Verification compared beginning/end listings and key metadata, followed
manifest/index/segment dependencies, inspected all 37 pack headers, and
hashed the duplicate full bodies. It did not parse every SlateDB SST, run a
complete repository `fsck`, execute GC, or establish the installed binary's
exact source provenance. The inventory is evidence for priorities, not a
conformance certification or a permanent expected-object-count fixture.

## 12. Optimization priorities

### 12.1 Prioritize avoided bytes and complete lifecycles

| Priority | Change | Expected benefit | Required proof |
| --- | --- | --- | --- |
| First | Resolve exact key construction and GC dependency gaps | Prevent inaccessible names or incomplete recovery closures | Final physical-key vectors; interrupted publication and retained-history cases |
| First | Reuse canonical pack storage in generated-pack descriptors | Avoid the measured full-pack duplicate on exact selections | Both Git read callers, immutable verification, descriptor retention, repack interaction |
| Next | Make generated-pack maintenance visible and effective | Reclaim expired materializations without changing canonical data | Fresh repo-scoped preview, grace enforcement, concurrent-reader/writer protection |
| Next | Align historical proof/checkpoint retention | Bound large embedded dictionaries while retaining promised recovery | History reads after catalog maintenance; no deletion based on JSON size alone |
| Next | Define journal and request-lock compaction | Bound accumulated small-object count and listing cost | CAS races, old readers, folded transactions, and clock lifecycle |
| Later | Tune database compaction/checkpoint cadence from workloads | Reduce stale SST/manifest overhead and read amplification | Owner-level metrics and live crash/reopen tests |
| Later | Review canonical pack repacking | Bound small-pack count and lookup overhead | Existing geometric policy, retained-history cost, writer interruption and GC interaction |

Do not merge all JSON records into one mutable object merely to reduce object
count. It would reintroduce contention, enlarge conditional writes, and make
independent publication harder. Do not add fan-out solely to improve the
console tree: require a measured listing or partitioning need.

### 12.2 Standard inventory categories

An operator-facing inventory should distinguish:

- Authoritative repository roots and retained history.
- Canonical Git/content bytes and their required metadata.
- Shared dependencies, with shared versus exclusive byte accounting.
- Rebuildable read artifacts, with descriptor age and reference information.
- Opaque database storage, reported by its owner.
- Coordination state, separating active leases from tombstones.
- In-progress publication/recovery and feature-owned retention.
- Unrecognized objects, reported without guessing their deletion policy.

Track both count and bytes. Large generated packs dominate capacity; tiny
lock or journal objects can dominate listing requests. A sum over current
LIST results also differs from a provider's bill when object versions,
unfinished multipart uploads, or backend redundancy are retained.

## 13. Standardization and rollout decisions

### 13.1 Namespace registration rule

Every production namespace must have the following before acceptance:

| Field | Required content |
| --- | --- |
| Root | `R`, `G`, or another explicitly authorized placement |
| Grammar | Literal segments, placeholder validators, name encoding, maximum sizes |
| Owner | One crate/module responsible for serialization and lifecycle |
| Callers | All product/service entry points that read or write it |
| Authority | Publication point or referring root; whether it is merely a hint |
| Mutation | Immutable create, CAS, owner-validated replacement, or ephemeral state |
| Dependencies | Bounded root visitor and every transitive edge |
| Cleanup | Grace/retention, concurrency protection, recovery, and operator policy |
| Version | Physical namespace version and payload version, independently |
| Evidence | Tests, live qualification, and release compatibility if applicable |

Common routing belongs in `crab-storage`; payload contracts and relative
metadata formats belong to their existing owners. Product commands should
not duplicate key policy. Generation-receipt path strings currently appear
in both CLI metadata and service receive wiring; consolidation should happen
at their real shared owner, not through another product-specific wrapper.

### 13.2 Compatibility and migration

Do not infer a shipped compatibility obligation from current `main`, a test,
or an older design document. For each physical or payload change, inspect
release tags and identify the first and last reachable writers/readers.

The architecture reference already describes hard cutovers for flat global
hash keys, an aggregate ref-registry object, and duplicated ref-lock paths.
This proposal does not reintroduce those shapes or certify their stated
release ranges. Reconcile release evidence before adding migration advice.

For any accepted incompatible change:

1. Identify the old and new key/payload contract and every active writer.
2. Decide whether it is an unshipped replacement or needs an explicit shipped
   upgrade path. Record the evidence.
3. Define writer quiescence, lock-domain transitions, and reader behavior.
4. Provide an explicit migration/doctor operation where required, with a
   complete inventory and rollback/recovery boundary.
5. Verify reads and publication against the new placement before cleanup of
   old objects is separately authorized.
6. Remove the retired production path once the agreed transition permits it.

Do not rename live prefixes in a console, mix old/new lock domains, create
silent dual writers, or use fallback readers to conceal a partial migration.
“Reset this development repository” in a current error message is not an
operator-approved migration plan for retained user data.

### 13.3 Decision register

| ID | Decision to finalize | Recommended outcome | Acceptance evidence |
| --- | --- | --- | --- |
| K1 | Root and final-key validation | Strict validated boundary preserving final bytes | Direct, service, scoped view, and feature-name vectors pass |
| K2 | Core names and feature registration | Preserve enumerated names; register missing namespaces | Every production writer maps to one table row and owner |
| K3 | Publication authority wording | Manifest plus committed journal snapshot | Direct and journal publication/read/recovery tests |
| K4 | Generated-pack target format | Explicit canonical/generated content target with no duplicate exact body | Wire and helper E2E, cache GC, retained target after repack |
| K5 | Metadata and history retention | Complete dependency graph with explicit history policy | Historical clone/fetch/recovery after maintenance and interrupted publication |
| K6 | Database boundary | Opaque children; owner-maintained checkpoint contract | Pinned dependency tests and crash/reopen evidence |
| K7 | Control-object growth | Owner-specific journal/lease lifecycle | Concurrent compaction/acquisition tests; bounded steady-state inventory |
| K8 | Physical/payload version policy | Independent explicit versions, tagged transition evidence | Supported-reader/writer matrix and removal plan |
| K9 | Service/feature cleanup | Explicit roots and retirement rules for every namespace | Session/view/artifact/LFS owner qualification |

These decisions are recommendations awaiting review, not approved runtime
changes. A single global `v2/` prefix is not needed to document current names;
version only the contracts whose interpretation actually changes.

## 14. Conformance and validation plan

### 14.1 Key vectors

Apply these at the physical storage boundary, not only to an intermediate
formatted string. Proposed rejection cases require validation work where
current helpers normalize or encode instead.

| Input or operation | Expected contract |
| --- | --- |
| `R=org/models`, manifest | `org/models/manifest` |
| `G=.crab`, xorb hash = 64 `a` characters | `.crab/xorbs/aa/` followed by the full 64-character hash |
| Ref `refs/heads/main` | `R/locks/refs/heads/main/lock`, exactly one `refs/` |
| Generation `42` | `00000000000000000042` in fixed-width generation fields |
| LFS hash beginning `a1b2` | `R/lfs/objects/a1/b2/{full-sha256}` |
| Same content in two authorized scopes | Same relative hash suffix, each under its trusted `G` |
| Validated prefix containing literal `%2F` | Preserve those bytes; never turn it into `/` or `%252F` at join time |
| Logical artifact name containing `/` | Owner encodes one name segment; physical spelling and round trip fixed by K1 |
| Empty segment, `.` or `..` | Reject before I/O, rather than normalize |
| Leading/trailing slash in a stored-key component | Reject at the appropriate boundary; listing prefix handled separately |
| Case-distinct or Unicode-distinct permitted names | Remain distinct; no implicit case folding or normalization |
| Uppercase or truncated canonical content hash | Reject if outside the owning format's canonical grammar |
| Key exceeding byte budget after name encoding/root join | Reject with an actionable error before I/O |
| Already-qualified canonical key in a staging envelope | Preserve the service's explicit envelope grammar; never treat it as an ordinary relative layout argument |

Also test reserved-root collisions, malicious name separators, mismatched
embedded identity versus filename, and unsupported payload versions. Tests
should prove public behavior rather than mirror each string formatter.

### 14.2 Evidence map and existing tests

The following map identifies implementation and adjacent tests to extend.
Existing tests were inspected for this proposal; no Rust suite was executed
as part of this documentation-only change.

| Surface / entry point | Owner and dependency boundary | Existing evidence location |
| --- | --- | --- |
| CLI/service storage placement | `StoreLayout`, `StorageScope`, descriptor validation | [layout tests](../../../crates/crab-storage/src/layout.rs), [descriptor tests](../../../crates/crab-metadata/src/layout_descriptor.rs), [remote layout wiring](../../src/core/remote_layout.rs) |
| Push and snapshot reads | Manifest store → ref journal → immutable metadata/storage CAS | [manifest tests](../../../crates/crab-metadata/src/manifest_store.rs), [atomic journal tests](../../../crates/crab-metadata/src/ref_journal.rs) |
| Git helper and wire fetch | Shared generated-pack implementation → storage and Git pack verification | [remote repository integration tests](../../../crates/crab-remote-git/tests/remote_repository.rs) cover reuse, corruption, coalescing, and request/scope identity |
| Catalog publication/read | Locator writer/reader → SlateDB checkpoint and immutable visibility | [writer tests](../../../crates/crab-metadata/src/git_object_locator/writer.rs), [visibility tests](../../../crates/crab-metadata/src/git_visibility.rs) |
| Metadata generation coverage | CLI metadb and receive → receipt/segmented formats | [CLI metadata integration](../../src/cmd/metadb.rs), [receive owner](../../../crates/crab-auth-server/src/receive.rs), [segmented tests](../../../crates/crab-metadata/src/segmented_store.rs) |
| Repo and shared-scope maintenance | GC root visitors → journals/marks → conditional deletion | [GC tests](../../src/cmd/gc/mod.rs), [journal tests](../../src/cmd/gc/journal.rs), [registry tests](../../../crates/crab-metadata/src/ref_registry.rs) |
| Coordination | Ref/internal leases, admission, fence → storage versions/backend time | [push-lock tests](../../../crates/crab-coordination/src/push_lock.rs), [fence tests](../../../crates/crab-coordination/src/gc_fence.rs) |
| Feature publication | Workflow, artifact, LFS, receive/view owners → authorized stores | [artifact tests](../../../crates/crab-workflow/src/artifact.rs), [cache tests](../../../crates/crab-workflow/src/cache.rs), [LFS tests](../../../crates/crab-lfs/src/object_store.rs), [session tests](../../../crates/crab-auth-server/src/receive/session.rs) |

For each accepted implementation change, require a user action through a real
entry point, real object-store side effects, and a visible read/recovery
result. Include an interrupted publication, retention beyond the grace
period, a scoped placement, and the affected sibling path. Mock-only tests
cannot establish physical keys or provider CAS/checkpoint behavior.

Run broad or live qualification in CI or the dedicated test environment.
Use isolated repository placements; do not mutate the inspected Kubernetes
repository to create fixtures. If Cargo compilation is needed, follow the
repository's external per-checkout target-directory policy.

### 14.3 Documentation reconciliation

| Existing document | Drift to resolve when this proposal is accepted |
| --- | --- |
| [Architecture layout reference](../architecture/object-storage-layout.md) | Add descriptor, journal, generated-pack, admission/fence, service, and feature inventory; correct authority and lock payload descriptions |
| [Push design](push.md) | Distinguish journal commit-marker publication from manifest compaction CAS |
| [Metadata crate README](../../../crates/crab-metadata/README.md) | Reconcile stated manifest/layout versions with current serializers |
| [Layout descriptor implementation plan](../../../plans/002-layout-descriptor-and-dispatch.md) | Treat historical alternatives as proposals; do not imply physical partitioned database roots exist |
| [GC CLI documentation](../../../packages/web/content/docs/cli/reference/crab-gc.mdx) | Keep candidate scopes, defaults, preview behavior, and owner boundaries synchronized |
| [Recovery CLI documentation](../../../packages/web/content/docs/cli/reference/crab-recover.mdx) | Keep explicit history retention/pruning and recovery dependencies synchronized |

## 15. Operational use and finalization checklist

For inspection, list the exact `R/` prefix with complete pagination, retain
key/size/version-or-ETag/modified-time metadata, and read bounded root objects.
Resolve the trusted `G` and follow the repository's dependencies separately.
Do not treat a bare string prefix matching another repository name as a
repository boundary. Compare a second inventory or use a coherent snapshot
when writers may be active.

Before reclamation, use the repository-scoped maintenance preview and the
owning recovery/retention controls. History pruning is explicit, using
`crab recover history prune --keep-last N` for preview and `--apply` only for
the intended change. Pruning roots and collecting newly unreachable objects
are separate operations. See [GC CLI reference](https://crab.build/docs/cli/reference/crab-gc)
and [recovery CLI reference](https://crab.build/docs/cli/reference/crab-recover).

A recoverable export must include the coherent repository roots and their
transitive content, plus owner-supported database snapshots where required.
Copying only `R/`, only the newest pack, or arbitrary live SST files is not a
complete repository backup contract.

Finalize the standard when:

- [ ] Every current production namespace has an owner, exact grammar,
  mutation contract, dependency visitor, and cleanup policy.
- [ ] K1–K9 have recorded outcomes with source/release evidence where needed.
- [ ] Final physical-key vectors pass across direct and authorized scopes.
- [ ] Current and historical snapshots remain usable after the relevant GC,
  catalog maintenance, repack, and interrupted-publication scenarios.
- [ ] Both preview and execution use the same reachability rules.
- [ ] Any new generated-pack target format has complete reader and GC proof.
- [ ] SlateDB child keys remain an owner/dependency boundary rather than an
  independently implemented public protocol.
- [ ] Shipped transitions have an explicit migration/removal plan; unshipped
  paths are replaced without speculative compatibility readers.
- [ ] The normative architecture reference and affected feature/CLI docs are
  reconciled, leaving one accepted description of each contract.

The outcome should be a small stable set of names with explicit lifecycle
semantics. Storage optimizations can then change how often objects are
written, retained, compacted, or reused without repeatedly reorganizing the
repository's physical namespace.
