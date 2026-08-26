# LFS Compatibility Layer

## Overview

Crab provides two independently supported ways to manage Git LFS pointers.
LFS objects are stored directly in cloud object storage alongside xorbs and
shards. A repository can mix crab-native pointers (`filter=crab`, Blake3)
and LFS pointers (`filter=lfs`, SHA-256) when its attributes explicitly route
each path.

The custom transfer-agent path is not the Git LFS HTTP API. It bypasses HTTP
discovery and requires repository-scoped Crab configuration and direct cloud
authorization. The support contract below is the release boundary; tests and
qualification evidence must name the profile they prove.

Source: `crab-lfs-server` and `crab/src/lfs/`

## Support matrix

| Profile | Transport | Client | Auth/storage | Status |
|---------|-----------|--------|--------------|--------|
| `crab-native` | Crab filters and porcelain | Crab CLI; `git` | Direct object storage selected by Crab | Supported and tested |
| `git-lfs-standalone-direct` | Git LFS custom transfer agent | Git LFS 3.7.x | Direct object-storage credentials available to Crab | Supported for qualified storage providers |
| `git-lfs-standalone-managed` | Custom transfer agent with managed grants | Unmodified Git LFS | Protected, repository-scoped grants | Not supported until managed-transfer qualification passes |
| `git-lfs-http` | Standard Batch/basic/File Locking HTTP APIs | Unmodified Git LFS | HTTPS gateway authentication | Implemented and qualified against Git LFS 3.7.1 + RustFS; CI/release provider matrix remains required |

Compatibility claims are profile-specific. The HTTP profile is the standard
Git LFS interoperability boundary; the standalone-direct and Crab-native
profiles have separate client, authorization, and storage contracts. A
passing claim names the profile, Git LFS version, provider, and qualification
evidence rather than treating one profile as proof of all others.

## Two Operating Modes

### Native Mode

Crab handles LFS pointers directly in its filter-process. Files with
`filter=lfs` in `.gitattributes` are cleaned to LFS pointers (SHA-256) and
smudged back to content. No separate `git-lfs` binary needed.

### Transfer Agent Mode

Crab acts as a Git LFS standalone transfer agent. An unmodified `git-lfs`
client delegates uploads and downloads to crab via the JSON-lines protocol
on stdin/stdout.

Both modes store objects in the same location.

## Dual Pointer System

### Detection Logic

```
classify(blob):
  1. If blob > 1 KB → NotAPointer
  2. Read first line
  3. Match version prefix:
     "version https://git-lfs.github.com/spec/v1"  → LFS pointer
     "version https://crab.dev/spec/v1"           → Crab pointer
     anything else                                  → NotAPointer
```

### LFS Pointer Format

```
version https://git-lfs.github.com/spec/v1
oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393
size 12345
```

### Crab Pointer Format

```
version https://crab.dev/spec/v1
file-hash 7c1f2a3b4d5e6f...  (blake3, 64 hex chars)
size 10737418240
shard-hint a1b2c3d4...  (optional)
```

Source: `crab/src/lfs/detect.rs`, `crates/crab-git/src/lfs_pointer.rs`

## LFS Object Storage

LFS objects live in a dedicated namespace:

```
Remote:  {prefix}/lfs/objects/{oid[:2]}/{oid[2:4]}/{oid}
Local:   .git/lfs/objects/{oid[:2]}/{oid[2:4]}/{oid}
```

The local path is the Git LFS default. The standard `lfs.storage` override is
resolved against the repository's common Git directory and is honored by
filters, transfer commands, fsck, prune, logs, and status reporting. Crab also
accepts its legacy `lfs.lfsdir` and `GIT_LFS_DIR` aliases; use `lfs.storage`
when the same cache must be shared with an unmodified Git LFS client. A
tracked `.lfsconfig` cannot redirect local storage.

Two-level hex fan-out prevents flat-directory performance issues.

### Integrity

- Upload: SHA-256 computed and verified against declared OID before PUT
- Download: SHA-256 verified before writing to local storage
- Idempotent: duplicate uploads are detected and skipped

Source: `crates/crab-lfs/src/object_store.rs`

## Transfer Agent Protocol

The standalone transfer agent communicates with `git-lfs` via JSON lines:

```
client → agent:  {"event":"init","operation":"upload","concurrent":true,...}
agent → client:  {}

client → agent:  {"event":"upload","oid":"abc...","size":12345,"path":"/tmp/..."}
agent → client:  {"event":"progress","oid":"abc...","bytesSoFar":4096,...}
agent → client:  {"event":"complete","oid":"abc..."}

client → agent:  {"event":"terminate"}
```

Concurrency is bounded by object and logical-byte semaphores. The object limit
comes from `concurrenttransfers` (default 8); the default aggregate in-flight
byte budget is 128 MiB. A configured transfer bandwidth cap is enforced by the
same coordinator.

Source: `crab/src/lfs/transfer_agent.rs`

## Batch Resolver

The `BatchResolver` determines which LFS objects need transfer:

### Push Flow

1. Walk commits being pushed → collect LFS pointers
2. Concurrent HEAD checks against remote → filter to missing OIDs
3. Check lock conflicts (warn if files locked by another user)
4. Concurrent uploads of missing objects

### Fetch Flow

1. Walk reachable commits → collect LFS pointers with paths
2. Apply include/exclude glob filters
3. Check local `.git/lfs/objects/` → filter to missing OIDs
4. Concurrent downloads from remote

Source: `crab/src/lfs/batch.rs`

## Advisory File Locking

LFS locks prevent concurrent edits to binary files. Since crab is serverless,
locks are stored as JSON objects in S3 managed via CAS:

```
Path:    {prefix}/lfs/locks/{blake3(filepath)}
Content: {"path":"models/large.bin","owner":"user@example.com","id":"...","locked_at":...,"released_at":null}
```

### Lock Operations

| Operation | Mechanism |
|-----------|-----------|
| Lock | `PutMode::Create` (atomic, fails if exists) |
| Unlock | CAS tombstone with owner and optional lock-ID verification |
| Force-unlock | CAS tombstone without owner check |
| List | List objects under `lfs/locks/` prefix |

Source: `crates/crab-lfs/src/lock.rs`, `crab/src/lfs/lock.rs`

## Migration Engine

The migration engine rewrites git history to convert between formats:

| Command | Direction |
|---------|-----------|
| `migrate import` | Large files → LFS pointers |
| `migrate export` | LFS pointers → full files |
| `migrate import --from-crab` | Crab pointers → LFS pointers |
| `migrate export --to-crab` | LFS pointers → crab pointers |

Source: `crab/src/lfs/migrate.rs`

## Configuration

LFS configuration follows Git's effective config precedence while preserving
Git LFS's lower-priority tracked `.lfsconfig` behavior:

```
1. Environment variables (GIT_LFS_*)
2. Git config (local → global → system, including Git-managed includes)
3. .lfsconfig (repository root)
4. Defaults
```

Key settings: `lfs.concurrenttransfers`, `lfs.fetchinclude`,
`lfs.fetchexclude`, `lfs.transfer.maxretries`.

Source: `crab/src/lfs/config.rs`

## Comparison with Official git-lfs

| Aspect | Official git-lfs | Crab LFS |
|--------|-----------------|------------|
| Server required | Yes | Native/direct: no; HTTP profile: yes |
| Transport | HTTP Batch/basic/locking | Native/direct: direct object storage; HTTP: Batch/basic/locking |
| Dedup | None (file-level) | CDC for crab-native files |
| Locking | Server-side API | CAS on object storage |
| Mixed formats | LFS only | LFS + crab in same repo |
