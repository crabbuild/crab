# PB-Scale Repository Architecture

## Status

This document describes Crab's canonical v1 direction for repositories whose
payload and metadata exceed one machine's memory and disk. It is a hard
cutover design: there is no older layout reader, migration command, version
selector, dual writer, or rollback format.

The current implementation includes paged local recipes, prepared-xorb
authority, content-addressed global xorbs and shards, partition-ready metadata
contracts, and bounded push work. Provider and multi-TB evidence remain release
gates rather than implied support claims.

## Goals

- Keep payload processing streaming and bounded independently of repository
  size.
- Make add publication recoverable and bind Git's index entry to one exact
  staged recipe.
- Reuse remote chunks without building a second local payload.
- Let push adopt valid prepared work independently for each file.
- Read and verify each unique prepared source xorb no more than once per push
  attempt.
- Keep every file recipe and all of its required xorb metadata in one
  dependency-closed shard.
- Publish xorbs, shards, indexes, recipes, Git objects, and receipts before a
  ref may advance.
- Use one strict Crab-owned v1 contract everywhere.

## Canonical placement

The provider-neutral key grammar is normative in
[Canonical Object Storage Layout V1](object-storage-layout.md). Repository
state has two trusted roots:

- `repo_prefix` for one repository's mutable and immutable metadata;
- `global_prefix` for content-addressed xorbs, shards, and chunk receipts
  shared within the authorization scope.

Clients do not infer a layout by probing keys. Repository initialization
publishes the authoritative v1 layout descriptor before the generation-zero
manifest. Missing, malformed, or non-v1 descriptors fail closed with explicit
development-reset guidance.

## Add publication

The local staging root is `.crab/staging`. It contains one canonical v1 SQLite
index plus immutable segment and prepared-xorb files. A file recipe is an
immutable root over ordered, bounded pages. The recipe records every chunk
occurrence; physical segment or prepared-xorb authority is validated
separately against that exact recipe.

`crab add` uses a durable publication intent:

1. stream and chunk the source;
2. reuse proven remote chunks without retaining their payload;
3. pack unknown payload once into prepared xorbs;
4. persist the recipe pages and prepared placement metadata;
5. replace the Git index entry;
6. publish the exact staged path head;
7. retire the intent only after both authorities agree.

A crash leaves enough state to reconcile to either the old head or the exact
new recipe. It never makes a partial recipe current. `--skip-git-add` records
the prepared authority without changing Git's index; a later ordinary
`git add` adopts the same prepared work.

Restaging writes a new immutable recipe and atomically replaces one path head.
An in-flight push retains its leased immutable snapshot while the newer recipe
becomes current. Reclamation begins only after no current head, publication
intent, or push snapshot references the old objects.

## Push planning and memory bounds

Push loads recipe pages incrementally. Prepared-plan adoption is per file: a
missing or stale plan falls back only for that file. The push verifies each
prepared xorb's full footer, digest, payload size, and chunk placement before
using it.

Residual chunk requests are coalesced in a disk-backed schedule. Each unique
prepared source xorb is opened once and read sequentially, even when several
files reference it. Packing consumes owned chunk sets instead of retaining a
duplicate repository-sized clone.

The implementation has explicit bounds for:

- recipe page entries and decoded page bytes;
- residual schedule batches;
- simultaneous prepared-xorb readers;
- target and hard-cap xorb sizes;
- multipart part size and in-flight parts;
- upload concurrency;
- shard file-bundle size.

File-backed xorbs use multipart upload directly from the local file. They are
not converted into one in-memory `Bytes` body. A provider conflict at a
content-addressed key triggers identity verification and reuse rather than a
second upload.

## Dependency-closed shards

Shard partitioning operates on complete file bundles. One bundle includes the
file recipe and every xorb term required to reconstruct all of its chunks.
The partitioner never splits a bundle to satisfy a soft size target. A single
oversized bundle becomes one oversized shard and is reported as such.

Each emitted shard must independently reconstruct every file assigned to it.
Shared xorbs may appear in more than one shard's metadata when that is required
for independent closure. Identical ordered bundle input produces identical
partition boundaries and shard hashes.

Protected-view shard construction uses the same session and closure contract
as direct push. A sibling path cannot silently reintroduce xorb-first
partitioning.

## Durable publication order

The canonical push sequence is:

1. acquire the per-ref serialization boundary and retain the staging snapshot;
2. resolve and verify prepared or residual payloads;
3. upload or reuse content-addressed xorbs;
4. build and upload dependency-closed shards;
5. publish xorb, shard, file, and Git-object indexes;
6. persist version-bound origin and generation receipts;
7. publish the candidate manifest through compare-and-swap;
8. advance the Git ref only after the committed manifest proves complete
   durability;
9. release the snapshot and lock on every exit path.

Cancellation aborts multipart uploads and leaves immutable successful writes
safe to reuse. A stale manifest CAS replans from the new base without losing
metadata for xorbs already uploaded by the attempt.

## Provider qualification

Provider support is not inferred from a successful basic PUT. The retained
matrix in [Provider Qualification](../guides/provider-qualification.md) must
prove create-only and match-token writes, ETag/version identity, multipart
completion and abort, file-backed staged multipart, exact ranges, listing
across a provider page, retry/error mapping, cancellation, and version-bound
origin receipts.

The matrix is run under an isolated generated prefix and deletes only that
prefix. GCS and Azure remain unqualified until their real-service CI jobs emit
artifacts accepted by the strict v1 evidence verifier.

## Development hard cutover

Pre-cutover state is disposable, but deletion is never automatic. Operators
must stop every writer, remove `.crab/staging` and the selected local cache,
delete only the named isolated repository scope, reinitialize canonical v1,
re-add and push source files, then fresh-clone and compare digests. The
step-by-step safe procedure is in the provider qualification guide.

Normal open does not translate, infer, or rewrite old state. Unknown versions
fail with the exact repository scope that needs reinitialization.

## Release gates

- Contract inventory tests assert every Crab-owned serialized contract is v1
  and reject non-v1 fixtures.
- Transactional add crash and cancellation tests reconcile one visible recipe.
- Duplicate and restage canaries prove payload and xorb reuse.
- Forced shard partitions prove independent reconstruction and deterministic
  hashes for direct and protected push.
- Provider artifacts pass the strict evidence verifier for every advertised
  provider.
- A release-built binary completes add, push, fresh clone, hydrate, strict Git
  fsck, and byte-digest comparison on RustFS.
- Full tests, clippy, format, script tests, scale tests, and documentation link
  checks pass.
