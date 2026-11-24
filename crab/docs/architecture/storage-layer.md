# Storage Layer

## Overview

`crab-storage` owns provider construction, canonical object-key routing,
conditional writes, bounded reads, retry classification, staged writes, and
multipart upload. Higher layers own Git, Xet, metadata, and publication policy.

Primary sources:

- `crates/crab-storage/src/provider_store.rs`
- `crates/crab-storage/src/store.rs`
- `crates/crab-storage/src/layout.rs`
- `crates/crab-storage/src/retry.rs`
- `crates/crab-storage/src/error_map.rs`

## Provider boundary

The provider builder selects exactly one S3-compatible, Google Cloud Storage,
or Azure Blob Storage adapter. `crab://` is the Git remote-helper scheme; the
project's stored provider selection determines which object-store builder
opens its bucket or container. Raw `s3://`, `gs://`, and `az://` forms are
normalized once at the composition boundary.

An implemented adapter is not automatically release-supported. The retained
[provider qualification matrix](../guides/provider-qualification.md) is the
support authority. It independently proves the conditional-write, multipart,
range, pagination, cancellation, and receipt contracts for the exact release
candidate.

## Store contract

`Store` wraps `Arc<dyn ObjectStore>` and gives every caller the same semantics:

```text
Store
├── put                  content-addressed create; identical conflict reuses
├── create_strict        create-only; every existing object conflicts
├── update               compare-and-swap with the complete ETag/version pair
├── get_with_etag        complete body plus provider identity
├── get_stream           backpressured full or bounded-range stream
├── range_get            exact [start, end) body
├── head                 metadata-only identity and size
├── list_prefix          provider-paginated listing
├── list_prefix_bounded  fail-closed bounded listing probe
├── put_multipart_retry  bounded multipart from shared bytes
├── put_multipart_file_retry
│                        bounded multipart directly from a local file
└── staged writes        upload under an isolated prefix, then verify durability
```

The CAS token retains both `e_tag` and `version`. Providers use different
combinations; discarding either can turn a valid match-token update into an
unconditional or permanently conflicting write.

Content-addressed `put` uses create-only semantics. If another writer wins,
Crab streams and hashes the current object and treats the conflict as success
only when the bytes match. Mutable coordination objects use `create_strict`
and `update`; they never fall back to an overwrite.

## Canonical layout

The normative key contract is
[Canonical Object Storage Layout V1](object-storage-layout.md). The important
ownership split is:

```text
{global_prefix}/
├── xorbs/{first-two}/{blake3}
├── shards/{first-two}/{blake3}
├── chunk_index_db/...
└── ref-registry/...

{repo_prefix}/
├── layout
├── manifest
├── manifests/...
├── metadata/...
├── packs/...
├── file_index_db/...
├── git_object_catalog_db/...
└── locks/...
```

Callers do not construct SlateDB child keys. They also do not probe retired
paths to infer a layout. The canonical v1 descriptor and manifest are the only
repository-open authority.

## Bounded reads

- Large immutable bodies are streamed with backpressure.
- Small control objects use explicit maximum body sizes before allocation.
- Range reads verify exact response bounds and byte count.
- Read observers count logical request kinds and delivered bytes without
  receiving keys, endpoints, or credentials.
- Prefix probes can stop at a fixed limit rather than materializing an
  unbounded namespace.

Xet reconstruction coalesces adjacent chunk ranges before it reaches the
store. The storage layer does not reinterpret shard terms or recipe pages.

## CDN read routing

Direct repositories can opt in through the user-owned Crab config. Each
`[[cdn]]` entry matches one resolved native origin and source repository
prefix. Repository config overlays and remote config cannot add CDN bindings.

```toml
[[cdn]]
origin = "s3://example-repository-bucket"
repo_prefix = "org/models"
base_url = "https://downloads.example.com"
access = "private"
approval_sha256 = "<reviewed private-binding receipt SHA-256>"
```

Use `gs://<bucket>` for GCS or
`https://<account>.blob.core.windows.net/<container>` for Azure. Private access
uses the Crab Auth issuer in the same trusted user config to obtain a scoped
viewer grant. Set `approval_sha256` to the digest reviewed by the operator
and pinned in that issuer. Crab accepts a viewer grant only when the
issuer confirms the selected entry's digest. A private binding without
an approval digest uses the issuer's native authorization.
Public access requires `access = "public"` and an
`export_origin` that names a separate native bucket or container in the
same cloud. Keep `origin` set to the live repository's storage. To enable
public acceleration, set `publication = { path = "/absolute/path/to/publication.json",
sha256 = "<publisher-reported digest>" }` in the same trusted binding.
Crab checks the receipt digest, source, destination, viewer URL and repository
prefix. It uses the edge only for listed keys whose total size matches the
read descriptor. A binding without publication approval and an unlisted
object both use native storage. An unreadable, invalid or mismatched receipt
fails the read. Live metadata, writes and
maintenance use native credentials. A ready direct
replica can use its own private binding for the replica's native origin and
physical repository prefix. The issuer must authorize that same target and
layout before Crab reads the replica's current root. Reads under a private
replica binding stay on that replica, including recovery from a transient
edge failure at its native origin.

The direct read path covers authenticated history segments, checkpoint and
capsule controls, layered Git pack windows, lazy Git indexes, scoped Xet shards and xorbs,
and full LFS downloads to a local file. Pack and LFS file transfers fetch
batches of four 8 MiB ranges and verify staged bytes before publication. Xet
selective reads verify metadata and requested chunks from the same source.
Shard reads require the file's authenticated catalog binding, exact encoded
size, and Xet content hash. Pointer hints alone can only select origin reads.
An allowed restart at the selected origin shares the edge attempt's deadline.
File transfers cancel pending reads and drain local writes before returning.
Historical recovery and checkpoint consolidation use the selected origin.
Cold-clone sidecar bundles and LFS streams requiring origin validators also use
that path. Provider HTTPS routing is disabled in default builds while these
profiles are unqualified. Dedicated builds enable `provider-cdn-qualification`.
Check provider qualification in
[Plan 021](../../../plans/021-provider-cdn-support.md).

## Multipart uploads

Multipart retries restart the complete upload attempt. `object_store` assigns
part indexes by call order, so retrying one failed part through the high-level
API could shift indexes and corrupt completion. Crab aborts the failed attempt,
creates a new upload, and sends every part again from index zero.

The progress-aware path keeps at most four part futures in flight. The
file-backed path reads one fixed-size buffer per scheduled part and never
materializes the complete xorb body. Before completion it verifies:

- the file size before and after upload;
- the streaming BLAKE3 digest;
- cancellation at part boundaries;
- successful completion of every scheduled part.

Any failure or cancellation aborts the provider upload. Successful immutable
parts from an abandoned higher-level push are harmless; future content-addressed
writes verify and reuse them.

Protected pushes use a staging-write store. Canonical keys map to generated
staging keys, and `flush_staged_writes` verifies every recorded staged size
before the receive service may promote or publish metadata. Flush is a
durability barrier, not a client-side canonical promotion.

## Retry and error mapping

Provider errors are mapped once at the storage boundary:

| Storage error | Retry behavior |
| --- | --- |
| Network transient | Exponential backoff with jitter, bounded attempts |
| Throttled | Provider delay hint plus bounded jitter |
| State conflict | Higher state-dependent budget or caller CAS replan |
| Corrupt response | One retry, then fail closed |
| Missing, forbidden, credentials, cancellation, invalid input | No retry |

`update` itself does not retry. A network failure after a CAS request is
ambiguous—the provider may have committed it—so the owner must re-read state
and decide with a fresh token. Retrying the stale token blindly cannot restore
correctness.

## Durability receipts

`crab-metadata` records origin receipts after verifying canonical immutable
bodies. A receipt binds namespace, object key, content digest, size, and the
provider's ETag/version identity. A later check avoids rehashing the body only
when the current provider identity matches the receipt exactly. Backends that
do not expose either identity are rehashed every time and cannot pass provider
qualification.

## Safety rules

- Never turn a create/CAS failure into an unconditional overwrite.
- Never log credentials, signed URLs, bearer tokens, or secret environment
  values.
- Never run bucket-wide cleanup as part of qualification.
- Keep provider qualification under one generated prefix and verify it is
  empty before and after the run.
- Keep old-layout deletion explicit and operator-scoped; normal open fails
  closed instead of translating or deleting state.
