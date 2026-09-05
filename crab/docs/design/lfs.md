# crab — Git LFS Compatibility Deep Dive

**How crab manages Git LFS pointers without a centralized server, storing
objects directly in cloud object storage alongside xorbs.**

-----

## Document Metadata

| Field        | Value                                                          |
|--------------|----------------------------------------------------------------|
| Project      | crab                                                         |
| Scope        | LFS architecture, data flow, transfer agent, CLI, migration    |
| Status       | Living document                                                |
| Companion to | `Crab-overview.md` (full workflow), `Crab-push.md` (push)  |
| Version      | 0.1                                                            |

-----

## Table of Contents

- [crab — Git LFS Compatibility Deep Dive](#crab--git-lfs-compatibility-deep-dive)
  - [Document Metadata](#document-metadata)
  - [Table of Contents](#table-of-contents)
  - [1. Overview](#1-overview)
  - [2. Key Architectural Difference: Serverless LFS](#2-key-architectural-difference-serverless-lfs)
  - [3. Dual Pointer System](#3-dual-pointer-system)
    - [LFS Pointer Format (spec v1)](#lfs-pointer-format-spec-v1)
    - [Crab Pointer Format (existing)](#crab-pointer-format-existing)
    - [Detection Logic](#detection-logic)
  - [4. LFS Object Storage Layout](#4-lfs-object-storage-layout)
    - [Local Cache Layout](#local-cache-layout)
    - [Integrity Verification](#integrity-verification)
  - [5. Filter Process Integration](#5-filter-process-integration)
    - [Clean (git add)](#clean-git-add)
    - [Smudge (git checkout)](#smudge-git-checkout)
  - [6. Standalone Transfer Agent](#6-standalone-transfer-agent)
    - [Protocol Flow](#protocol-flow)
    - [Message Types](#message-types)
    - [Error Codes](#error-codes)
    - [Concurrency](#concurrency)
    - [Resume for Large Objects](#resume-for-large-objects)
  - [7. Batch Resolver: Push and Fetch](#7-batch-resolver-push-and-fetch)
    - [Push Flow](#push-flow)
    - [Fetch Flow](#fetch-flow)
    - [Pull = Fetch + Checkout](#pull--fetch--checkout)
  - [8. Advisory File Locking](#8-advisory-file-locking)
    - [Lock Storage](#lock-storage)
    - [Lock Lifecycle](#lock-lifecycle)
    - [Pre-Push Lock Conflict Check](#pre-push-lock-conflict-check)
  - [9. Migration Engine](#9-migration-engine)
    - [migrate import](#migrate-import)
    - [migrate export](#migrate-export)
    - [migrate info](#migrate-info)
    - [Crab ↔ LFS Conversion](#crab--lfs-conversion)
    - [Safety Properties](#safety-properties)
  - [10. CLI Command Reference](#10-cli-command-reference)
    - [Setup and Configuration](#setup-and-configuration)
    - [Tracking](#tracking)
    - [Transfer](#transfer)
    - [Inspection](#inspection)
    - [Locking](#locking)
    - [Migration](#migration)
    - [Maintenance](#maintenance)
    - [Standalone Filters](#standalone-filters)
  - [11. Configuration Reference](#11-configuration-reference)
    - [LFS Config Keys](#lfs-config-keys)
    - [Configuration Precedence](#configuration-precedence)
    - [Transfer Agent Config (set by `crab lfs install`)](#transfer-agent-config-set-by-crab-lfs-install)
  - [12. Object Store Layout (Full)](#12-object-store-layout-full)
    - [Object Mutability](#object-mutability)
  - [13. Error Handling](#13-error-handling)
    - [Error Variants](#error-variants)
    - [Retry Strategy](#retry-strategy)
  - [14. Comparison: crab LFS vs Official git-lfs](#14-comparison-crab-lfs-vs-official-git-lfs)
    - [Transfer Size Comparison (2 GB File)](#transfer-size-comparison-2-gb-file)
  - [15. Worked Example: LFS Workflow for a 2 GB Model](#15-worked-example-lfs-workflow-for-a-2-gb-model)
    - [Setup](#setup)
    - [Add and Commit](#add-and-commit)
    - [Push](#push)
    - [Clone (Another Machine)](#clone-another-machine)
    - [Lock and Edit](#lock-and-edit)
  - [16. Source Map](#16-source-map)
  - [17. Invariants Checklist](#17-invariants-checklist)

-----

## 1. Overview

Crab provides a Crab-managed LFS implementation backed directly by cloud
object storage. The standard `git-lfs` client normally uses an HTTP Batch API,
but Crab's supported interoperability path is the standalone custom transfer
agent: Git LFS invokes the Crab CLI over stdin/stdout and Crab accesses the
configured object store directly. Crab does not expose or require a deployable
LFS gateway.

The supported profiles are:

| Profile | Contract | Release status |
|---------|----------|----------------|
| `crab-native` | Crab filters, porcelain, and direct object storage | Supported |
| `git-lfs-standalone-direct` | Git LFS custom transfer agent with direct Crab storage access | Supported for qualified providers |
| `git-lfs-standalone-managed` | Custom agent with short-lived repository grants | Planned; not shipped |
| `git-lfs-http` | Standard Batch/basic/File Locking HTTPS API | Not supported; use an external LFS server for this profile |

“Compatible” below refers to the named profile. The CLI sections describe the
standalone-direct profile. Managed, HTTP, SSH, and TUS integrations require
separate qualification; standard HTTP requires an external LFS server.

Crab operates in two LFS modes simultaneously:

1. **Native mode** — crab handles LFS pointers directly in its
   filter-process. Files with `filter=lfs` in `.gitattributes` are
   cleaned to LFS pointers (SHA-256) and smudged back to content.
   No separate `git-lfs` binary needed.

2. **Transfer agent mode** — crab acts as a Git LFS standalone
   transfer agent. An unmodified `git-lfs` client delegates uploads
   and downloads to crab via the JSON-lines protocol on stdin/stdout.

Both modes store objects in the same location. A repository can mix
crab-native pointers (`filter=crab`, Blake3) and LFS pointers
(`filter=lfs`, SHA-256) seamlessly.

> **Key invariant: LFS objects are content-addressed by SHA-256 OID.
> An uploaded object's hash is verified before the PUT is confirmed.
> A downloaded object's hash is verified before it reaches the working tree.**

-----

## 2. Key Architectural Difference: Serverless LFS

```
Official git-lfs:
┌──────┐     ┌──────────────┐     ┌─────────────────┐
│  Git │────►│  git-lfs     │────►│  LFS Server     │────► Object Storage
│      │     │  (client)    │     │ (HTTP Batch API)│
└──────┘     └──────────────┘     └─────────────────┘
                                   POST /objects/batch
                                   → returns signed URLs
                                   → client uploads/downloads

Crab:
┌──────┐     ┌──────────────────────────────────────┐
│  Git │────►│  crab                              │────► Object Storage
│      │     │  (remote helper + filter + LFS agent)│     (direct PUT/GET)
└──────┘     └──────────────────────────────────────┘
              No Crab service. No Batch API.
              Direct S3/GCS/Azure access.
```

This means:

- **No Crab LFS service to deploy or maintain.** The object store is accessed
  directly by the local Crab binary.
- **No signed URL negotiation.** Crab has direct cloud credentials.
- **No HTTP round-trips for batch resolution.** Existence checks are
  HEAD requests directly to the object store.
- **Locking uses CAS on object storage** instead of a server-side lock API.

-----

## 3. Dual Pointer System

Crab supports two pointer formats in the same repository. The filter
process detects which format to use by inspecting `.gitattributes` and
the version line prefix of existing pointers.

### LFS Pointer Format (spec v1)

```
version https://git-lfs.github.com/spec/v1
oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393
size 12345
```

- Maximum size: 1 KB (1024 bytes)
- OID: SHA-256, 64 lowercase hex characters
- Empty file (size=0): serializes to empty bytes
- Legacy version aliases accepted on parse:
  `http://git-media.io/v/2`, `https://hawser.github.com/spec/v1`
- Extensions: `ext-{priority}-{name} sha256:{hex}` (sorted by priority)

### Crab Pointer Format (existing)

```
version https://crab.build/spec/v1
file-hash 7c1f2a3b4d5e6f...  (blake3, 64 hex chars)
size 10737418240
shard-hint a1b2c3d4...  (optional)
```

### Detection Logic

```
classify(blob: &[u8]) → PointerKind

  1. If blob > 1 KB → NotAPointer (fast reject)
  2. Read first line (up to 256 bytes)
  3. Match version prefix:
     "version https://git-lfs.github.com/spec/v1"  → parse as LFS
     "version http://git-media.io/v/2"              → parse as LFS (legacy)
     "version https://hawser.github.com/spec/v1"    → parse as LFS (legacy)
     "version https://crab.build/spec/v1"           → parse as Crab
     "version https://crab.dev/spec/v1"             → parse as Crab (legacy)
     anything else                                  → NotAPointer
```

**Source:** `crab/src/lfs/detect.rs`, `crates/crab-git/src/lfs_pointer.rs`

-----

## 4. LFS Object Storage Layout

LFS objects live in a dedicated namespace within the same object store
bucket used for xorbs and shards:

```
{prefix}/lfs/objects/{oid[0:2]}/{oid[2:4]}/{oid}
```

The two-level hex fan-out (`ab/cd/abcdef...`) prevents flat-directory
performance issues on object stores with prefix-based listing.

### Local Cache Layout

Local LFS objects mirror the remote layout under `.git/lfs/`:

```
.git/lfs/
├── objects/
│   ├── ab/
│   │   └── cd/
│   │       └── abcdef0123456789...  (raw file content)
│   └── ...
└── tmp/
    └── ...  (temporary files handed to Git LFS after download)
```

### Integrity Verification

- **On upload:** SHA-256 of the bytes is computed and compared to the
  declared OID before the PUT is confirmed. Mismatch → error, no upload.
- **On download:** SHA-256 of the received bytes is verified before
  writing to local storage. Mismatch → `LfsObjectCorrupt` error.
- **Idempotent put:** If an object with the same OID already exists,
  the upload is skipped and reported as success.

**Source:** `crates/crab-lfs/src/object_store.rs`

-----

## 5. Filter Process Integration

The existing crab filter-process (`git filter protocol v2`) is extended
to handle LFS-tracked files natively. The routing decision is based on
`.gitattributes`:

```
*.bin    filter=lfs diff=lfs merge=lfs -text     ← LFS path (SHA-256)
*.sft    filter=crab diff=crab merge=crab  ← Crab path (Blake3 + CDC)
```

### Clean (git add)

```
Working Tree File (e.g. weights.bin, 500 MB)
    │
    ▼  Git sends content to filter-process ("clean" command)
    │
    ├── Check .gitattributes for pathname
    │
    ├── filter=lfs path:
    │   ┌─────────────────────────────────────────────────┐
    │   │  SHA-256 hash content (in spawn_blocking)       │
    │   │  Stage raw bytes → local LFS cache              │
    │   │  Emit LFS pointer (~120 bytes) → Git ODB        │
    │   └─────────────────────────────────────────────────┘
    │
    └── filter=crab path:
        ┌─────────────────────────────────────────────────┐
        │  Blake3 hash + CDC chunking (single pass)       │
        │  Stage chunks → staging area                    │
        │  Emit crab pointer (~200 bytes) → Git ODB       │
        └─────────────────────────────────────────────────┘
```

### Smudge (git checkout)

```
Pointer Blob from Git ODB
    │
    ▼  Git sends content to filter-process ("smudge" command)
    │
    ├── classify(content) → PointerKind
    │
    ├── LFS pointer + non-lazy mode:
    │   Download from LfsObjectStore → return raw content
    │
    ├── LFS pointer + lazy mode (--skip-smudge):
    │   Pass pointer through unchanged
    │
    └── Crab pointer:
        Existing hydration path (fetch xorb chunks, reconstruct)
```

**Source:** `crab/src/git/filter_process.rs`, `crab/src/git/clean.rs`

-----

## 6. Standalone Transfer Agent

When `git-lfs` is configured to use crab as a standalone transfer
agent, it spawns `crab lfs-transfer-agent` and communicates via
JSON lines on stdin/stdout.

### Protocol Flow

```
┌──────────┐                              ┌─────────────────────────┐
│  git-lfs │  stdin/stdout JSON lines     │  crab                   │
│  client  │◄────────────────────────────►│  lfs-transfer-agent     │
└──────────┘                              └───────────┬─────────────┘
                                                      │
                                          ┌───────────▼───────────┐
                                          │  LfsObjectStore       │
                                          │  (direct S3 PUT/GET)  │
                                          └───────────────────────┘
```

### Message Types

| Direction | Event | Fields | Description |
|-----------|-------|--------|-------------|
| client → agent | `init` | `operation`, `remote`, `concurrent`, `concurrenttransfers` | Initialize session |
| agent → client | (init response) | `{}` | Acknowledge init |
| client → agent | `upload` | `oid`, `size`, `path` | Upload file at path |
| client → agent | `download` | `oid`, `size` | Download object by OID |
| agent → client | `progress` | `oid`, `bytesSoFar`, `bytesSinceLast` | Transfer progress |
| agent → client | `complete` | `oid`, optional `path`, optional `error` | Transfer done |
| client → agent | `terminate` | (none) | Shutdown agent |

### Error Codes

| Code | Meaning |
|------|---------|
| 1 | Generic error |
| 2 | Object not found |
| 3 | Object already exists |
| 4 | Unauthorized |
| 5 | Rate limited |

### Concurrency

The installed transfer agent sets `lfs.customtransfer.crab.concurrent=true`,
so Git LFS distributes object transfers across up to
`lfs.concurrenttransfers` Crab processes. Each process handles its protocol
requests serially, as required by the custom-transfer contract, while each
object uses bounded multipart part concurrency. A configured transfer
bandwidth cap and per-process byte budget are enforced by the coordinator.
Setting the custom-agent option to `false` selects one Crab process; standard
Git LFS still sends that process one object at a time, so this mode is useful
only when a future transfer implementation owns parallelism inside one
request.

### Shutdown

The protocol loop authorizes each subsequent stdin read only after accepting
the previous event. A fatal init/protocol decision or failed init response
therefore stops and joins the reader even when the client keeps its pipe open.
Normal termination still drains admitted transfers. This is not cancellation
of an idle input read or a stalled transfer; those remain separate lifecycle
boundaries.

### Resume for Large Objects

Uploads stream through 8 MiB multipart parts with at most four parts in flight,
and downloads stream into a unique temporary file. Retries are bounded and
restart the current object transfer; the standalone agent does not persist
multipart upload IDs or download offsets across process restarts. This keeps
the direct profile provider-neutral and bounded, while cross-process resume
remains a production follow-up rather than an implied compatibility guarantee.

**Source:** `crab/src/lfs/transfer_agent.rs`

-----

## 7. Batch Resolver: Push and Fetch

The `BatchResolver` determines which LFS objects need to be transferred
by comparing local and remote state, then drives concurrent transfers.

### Push Flow

```
git push
    │
    ▼  pre-push hook fires
    │
    ├── Walk commits being pushed (via gitoxide)
    │   Collect all LFS pointers in new commits
    │
    ├── Concurrent HEAD checks against remote LfsObjectStore
    │   (bounded by concurrent_transfers)
    │   → filter to missing OIDs only
    │
    ├── Check lock conflicts (LockManager.check_conflicts)
    │   → warn if files locked by another user
    │
    └── Concurrent uploads of missing objects
        (bounded by concurrent_transfers)
        → abort push on any upload failure
```

### Fetch Flow

```
crab lfs fetch [--include *.bin] [--exclude docs/*]
    │
    ├── Walk reachable commits from HEAD (or --all / --recent)
    │   Collect all LFS pointers with file paths
    │
    ├── Apply include/exclude glob filters
    │
    ├── Check local .git/lfs/objects/ for each OID
    │   → filter to missing OIDs only
    │
    └── Concurrent downloads from remote LfsObjectStore
        (bounded by concurrent_transfers)
        → skip_download_errors: log and continue if configured
```

### Pull = Fetch + Checkout

`crab lfs pull` runs the fetch flow above, then replaces LFS pointers
in the working tree with actual file content (equivalent to
`crab lfs checkout` after fetch).

**Source:** `crab/src/lfs/batch.rs`

-----

## 8. Advisory File Locking

LFS file locks prevent concurrent edits to binary files that can't be
merged. Since crab is serverless, locks are stored as JSON objects in
cloud storage, managed via compare-and-swap (CAS).

### Lock Storage

```
Path:    {prefix}/lfs/locks/{blake3-hash-of-filepath}
Payload: { "path": "models/large.bin",
           "owner": "user@example.com",
           "locked_at": 1719849600,
           "id": "a1b2c3d4-...",
           "released_at": null }
```

The file path is hashed with Blake3 to produce a fixed-length key that
avoids special characters in object store paths.

### Lock Lifecycle

```
User A                              S3                          User B
───────                             ──                          ───────

crab lfs lock models/large.bin
  PUT locks/{hash} (CAS: must not exist)
  ◄── 200 OK ──────────────────►  lock created

                                                  crab lfs lock models/large.bin
                                  ◄── CAS conflict ──────────────────►
                                                  "file locked by user@example.com"

... editing ...

crab lfs unlock models/large.bin
  UPDATE locks/{hash} (CAS: verify owner and mark tombstone)
  ◄── 200 OK ──────────────────►  lock released

                                                  crab lfs lock models/large.bin
                                  ◄── 200 OK ──────────────────────►
                                                  lock acquired; stale unlocks cannot remove it
```

### Pre-Push Lock Conflict Check

During `git push`, the pre-push hook checks whether any files being
pushed are locked by another user. If conflicts are found, the push
warns the user and requires `--force` to proceed.

**Source:** `crab/src/lfs/lock.rs`

These are Crab CLI locks, not the Git LFS File Locking HTTP API. The
standalone custom-transfer profile handles object upload and download only;
stock `git lfs lock`, `git lfs unlock`, and `git lfs locks` require an
external LFS server. Direct repositories should use the corresponding
`crab lfs` commands.

-----

## 9. Migration Engine

The migration engine rewrites git history to convert between LFS-tracked
and non-LFS-tracked files, using gitoxide for tree rewriting.

### migrate import

Converts large files to LFS pointers:

```
crab lfs migrate import --include "*.bin" [--everything]

For each reachable commit:
  For each blob matching the pattern:
    1. Read original content
    2. Compute SHA-256 OID
    3. Upload content to LfsObjectStore
    4. Replace blob with LFS pointer
    5. Update .gitattributes to include LFS tracking
  Rewrite commit with new tree
```

### migrate export

Converts LFS pointers back to regular files:

```
crab lfs migrate export --include "*.bin"

For each reachable commit:
  For each LFS pointer matching the pattern:
    1. Parse pointer → extract OID
    2. Download content from LfsObjectStore
    3. Replace pointer with original content
    4. Remove LFS tracking from .gitattributes
  Rewrite commit with new tree
```

### migrate info

Analyzes the repository without modifying anything:

```
crab lfs migrate info [--above 10mb] [--include "*.bin"]

Output:
  *.bin    1.2 GB    47 files    3 versions
  *.dat    800 MB    12 files    2 versions
```

### Crab ↔ LFS Conversion

```
crab lfs migrate import --from-crab --include "*.sft"
  → Converts crab pointers (Blake3) to LFS pointers (SHA-256)
  → Updates .gitattributes: filter=crab → filter=lfs

crab lfs migrate export --to-crab --include "*.bin"
  → Converts LFS pointers (SHA-256) to crab pointers (Blake3 + CDC)
  → Updates .gitattributes: filter=lfs → filter=crab
```

### Safety Properties

- Requires clean working tree before starting
- Original refs are preserved until rewrite completes
- Interrupted migration leaves original refs intact
- Missing LFS objects abort the export with a list of missing OIDs

**Source:** `crab/src/lfs/migrate.rs`

-----

## 10. CLI Command Reference

All LFS commands live under `crab lfs <subcommand>`:

### Setup and Configuration

| Command | Description |
|---------|-------------|
| `crab lfs install [--local] [--skip-smudge]` | Configure git to use crab as LFS transfer agent |
| `crab lfs uninstall` | Remove crab LFS configuration |
| `crab lfs update [--force] [--manual]` | Update hooks and config to current crab version |
| `crab lfs env` | Display direct-storage remote, transfer agent, storage path |
| `crab lfs version` | Display crab version and LFS protocol version |

### Tracking

| Command | Description |
|---------|-------------|
| `crab lfs track <pattern>` | Add LFS tracking for a file pattern |
| `crab lfs track` | List all tracked LFS patterns |
| `crab lfs untrack <pattern>` | Remove LFS tracking for a pattern |

### Transfer

| Command | Description |
|---------|-------------|
| `crab lfs fetch [--include] [--exclude] [--recent] [--all] [--dry-run]` | Download LFS objects |
| `crab lfs pull [--include] [--exclude]` | Fetch + replace pointers with content |
| `crab lfs push [--all] [--object-id <oid>] [--dry-run]` | Upload LFS objects |
| `crab lfs pre-push` | Pre-push hook entry point (internal) |
| `crab lfs checkout [<path>] [--to <path>]` | Replace pointers with content in working tree |

### Inspection

| Command | Description |
|---------|-------------|
| `crab lfs ls-files [--all] [--name-only] [--size] [--debug]` | List LFS-tracked files |
| `crab lfs status [--json] [--porcelain]` | Show staged/modified LFS files |
| `crab lfs pointer [--file] [--stdin] [--check] [--strict]` | Generate/validate LFS pointers |
| `crab lfs fsck [--pointers] [--objects]` | Verify integrity of local LFS objects |

### Locking

| Command | Description |
|---------|-------------|
| `crab lfs lock <path>` | Create advisory file lock |
| `crab lfs unlock <path> [--force]` | Remove file lock |
| `crab lfs locks [--json]` | List active locks |

### Migration

| Command | Description |
|---------|-------------|
| `crab lfs migrate import --include <pat> [--everything] [--from-crab]` | Convert files to LFS pointers |
| `crab lfs migrate export --include <pat> [--to-crab]` | Convert LFS pointers back to files |
| `crab lfs migrate info [--above <size>] [--include <pat>] [--pointers]` | Analyze repo for migration |

### Maintenance

| Command | Description |
|---------|-------------|
| `crab lfs prune [--dry-run] [--force]` | Remove unreferenced local LFS objects |
| `crab lfs prune --verify-remote` | Remove only unreferenced local LFS objects confirmed present remotely |
| `crab lfs convert ...` | Convert indexed paths between LFS and Crab-native pointers |
| `crab lfs dedup [--dry-run]` | Remove verified local LFS cache duplicates already present in Crab staging |

### Standalone Filters

| Command | Description |
|---------|-------------|
| `crab lfs clean` | Standalone clean filter (stdin → stdout) |
| `crab lfs smudge [--skip]` | Standalone smudge filter (stdin → stdout) |
| `crab lfs-transfer-agent` | Standalone transfer agent (invoked by git-lfs) |

-----

## 11. Configuration Reference

### LFS Config Keys

| Key | Default | Description |
|-----|---------|-------------|
| `lfs.concurrenttransfers` | 8 | Max concurrent uploads/downloads (range 1–100) |
| `lfs.fetchrecentrefsdays` | 7 | Days of recent refs to include in `--recent` fetch |
| `lfs.fetchrecentcommitsdays` | 0 | History window measured backward from each selected ref's commit time; zero disables it |
| `lfs.pruneoffsetdays` | 3 | Grace period before pruning unreferenced objects |
| `lfs.fetchinclude` | (none) | Glob pattern for paths to include in fetch |
| `lfs.fetchexclude` | (none) | Glob pattern for paths to exclude from fetch |
| `lfs.transfer.maxretries` | 8 | Max retries per object on transient failure |
| `lfs.transfer.maxretrydelay` | 10s | Max delay between retries |
| `lfs.transfer.maxbandwidth` | 0 | Aggregate transfer limit in bytes/second; 0 is unlimited |
| `lfs.skipdownloaderrors` | false | Continue on download errors instead of aborting |
| `lfs.storage` | `.git/lfs` | Override local LFS storage; relative paths resolve inside the common Git directory |
| `lfs.lfsdir` | `.git/lfs` | Legacy Crab alias for `lfs.storage` |
| `lfs.pruneverifyremotealways` | false | Verify prune candidates remotely unless the CLI overrides it |
| `lfs.pruneverifyunreachablealways` | false | Also verify unreachable prune candidates unless the CLI overrides it |

### Complete History Transfers

`crab lfs fetch --all <remote> [refs...]` inventories all objects reachable
from the selected refs, including versions replaced or deleted before their
tips. With no explicit refs, Git's `--all` also covers tags and detached HEAD.
Each explicit operand resolves to one object ID before an owned stdin stream
feeds the traversal; invalid operands cannot turn into revision options or
silently widen the selected graph.

One `rev-list --objects` traversal feeds the same bounded, checksum-verifying
batch reader as other LFS discovery. Historical promised blobs may be fetched;
publication and mirror inspection retain local-only access. Git's reported
object name is only a display hint (possibly empty or newline-normalized),
not a canonical path or complete alias list. Unnamed reachable pointer blobs
are included. Bulk fetch has no path filtering: explicit include/exclude and
recent flags conflict with `--all`, and configured include/exclude paths do
not narrow it. Default/recent fetch and pull retain path-preserving tree scans.
Malformed/truncated records, missing or corrupt objects, exhausted inventory
budgets and conflicting pointer sizes fail before LFS payload transfers.

`crab lfs push --all <remote> [refs...]` shares the history traversal and
verified batch reader, with local-only Git object access. With no explicit
refs, upload roots are local branches and tags (`--branches --tags`), not
fetch's broader `--all` scope. Remote-only and detached histories require
explicit operands. Replaced and deleted versions are included; missing
promised blobs fail without fetching from a source remote. The operation
owns both root selection and access policy, preventing upload from inheriting
fetch's transport permission. A bounded OID/size inventory rejects conflicting
payload declarations before upload. No new discovery parser or fallback.

This does not certify normal (non-`--all`) standalone push's introduced-history
selection, object-ID/stdin admission or pre-push path-alias lock coverage.
Those remain separate qualification gaps.

### Recent Commit Selection

Ordinary fetch honors Git's typed `lfs.fetchrecentalways` boolean through the
same supervised Git-config reader as recent-ref selection. Explicit `--recent`
also enables this selection; `--all` bypasses configured recent and path
defaults. Pull does not fetch historical versions solely because recent-always
is enabled.

Fetch and pull resolve include/exclude overrides independently against their
LFS configuration, then use the shared LFS pattern compiler. Empty patterns
clear a restriction rather than constructing an empty generic path matcher.
The same empty-pattern contract applies to standalone and process smudge.
The real-Git RustFS LFS selection qualification compares cold-cache inventories
and checked-out bytes with native Git LFS for these settings and overrides.

Recent commit selection resolves each requested revision to one commit before
walking its history. Each tip supplies its own cutoff; prune adds
`lfs.pruneoffsetdays` only when the commit window is enabled. One streamed
topological walk visits shared ancestry once, propagating the widest applicable
cutoff to each parent. Pending graph entries and deduplicated results share
the existing scan budget; each input record is bounded separately. Missing selected
revisions, malformed timestamped commit records and cancelled Git queries
fail rather than returning an incomplete successful selection.

Fetch scans current selected trees, then the old sides of changes in the
selected recent commits. The previous blob is included even when its parent
commit predates the cutoff. A non-recursive raw Git log emits NUL-framed old
object IDs and paths; the shared bounded batch reader verifies the actual
blob bytes before interpreting pointers. No patch-text pointer parser or
whole-parent-tree scan is used. Each merge parent is compared separately;
rename heuristics and content transforms are disabled. Display settings cannot
silently suppress merge parents. Historical aliases remain distinct until
path filtering and transfer deduplication. Malformed/truncated records,
missing or corrupt old blobs, and conflicting declared sizes fail the entire
inventory before any LFS transfers begin.

Prune still uses its separate all-ancestry inventory. Its error handling,
retention scope, staged-index/stash coverage and flag contracts need further
qualification; fetch's previous-object proof does not certify prune safety.

### Configuration Precedence

```
Highest priority:
  1. Environment variables (GIT_LFS_*)
  2. Git config (local → global → system, including Git-managed includes)
  3. .lfsconfig (repository root)
  4. Defaults
Lowest priority
```

When a key is set in both `.lfsconfig` and Git config, Git config wins,
except tracked `.lfsconfig` files cannot set `lfs.storage` or
`lfs.lfsdir`. Repository-controlled configuration must not redirect local
writes or prune deletion outside the repository's Git directory.

### Transfer Agent Config (set by `crab lfs install`)

```
[filter "lfs"]
    clean = /path/to/crab lfs clean
    smudge = /path/to/crab lfs smudge
    required = true
[lfs "customtransfer.crab"]
    path = /path/to/crab
    args = lfs-transfer-agent
    concurrent = true
    direction = both
[lfs]
    standalonetransferagent = crab
```

-----

## 12. Object Store Layout (Full)

After LFS operations, the remote object store contains:

```
s3://{bucket}/{prefix}/
│
├── xorbs/                          ← crab chunk aggregates
│   └── {merkle_hash}
│
├── shards/                         ← crab reconstruction metadata
│   └── {merkle_hash}.shard
│
├── file-index/                     ← crab file → shard mapping
│   └── {file_hash}
│
├── lfs/
│   ├── objects/                    ← LFS objects (content-addressed)
│   │   ├── ab/
│   │   │   └── cd/
│   │   │       └── abcdef...      ← raw file content, keyed by SHA-256
│   │   └── ...
│   │
│   ├── receipts/                   ← validator-bound verification receipts
│   │   └── {oid[:2]}/{oid[2:4]}/{oid}.bin
│   │
│   └── locks/                      ← advisory file locks
│       └── {blake3-hash-of-path}   ← JSON lock record
│
├── packs/                          ← Git packfiles
│   ├── pack-{blake3}.pack
│   └── pack-{blake3}.meta
│
├── refs/                           ← Git refs
│   └── heads/
│       └── main
│
└── manifests/                      ← mutable manifests (CAS-updated)
    └── pack-list
```

### Object Mutability

| Object | Mutability | Update Mechanism |
|--------|------------|------------------|
| `lfs/objects/*` | Immutable | PUT once, never updated |
| `lfs/receipts/*` | Rebuildable | Best-effort validator-bound metadata |
| `lfs/locks/*` | Mutable | CAS create/tombstone |
| `xorbs/*` | Immutable | PUT once, never updated |
| `shards/*` | Immutable | PUT once, never updated |
| `refs/*` | Mutable | CAS (etag-based) |

**GC safety:** LFS objects are never deleted by the push pipeline. Local
`crab lfs prune` deletes only unreferenced local-cache objects. With
`--verify-remote`, each candidate must exist in the configured LFS object store
before Crab deletes the local copy; missing remote objects are kept locally.

-----

## 13. Error Handling

### Error Variants

| Code | Variant | Exit Code | Description |
|------|---------|-----------|-------------|
| E0100 | `InvalidLfsPointer` | 1 | Malformed pointer (bad OID, bad size, too large) |
| E0101 | `LfsObjectCorrupt` | 4 | SHA-256 mismatch on download |
| E0102 | `LfsObjectMissing` | 1 | Object not found in remote store |
| E0103 | `LfsLockConflict` | 1 | File locked by another user |
| E0104 | `LfsTransferProtocol` | 1 | Invalid JSON or unknown event in transfer agent |
| E0105 | `LfsMigrationFailed` | 1 | Migration aborted (dirty tree, missing object) |

### Retry Strategy

- **Transient errors** (network timeout, throttle, 5xx): retry with
  exponential backoff, up to `lfs.transfer.maxretries` attempts.
- **Permanent errors** (not found, access denied): report immediately,
  no retry.
- **Transfer agent**: individual transfer failures are reported as
  `complete` events with error objects. The agent continues processing
  other events — it does not exit on a single failure.
- **Filter process**: LFS errors in clean/smudge are caught per-file.
  A failed clean/smudge for one file does not tear down the session.

-----

## 14. Comparison: crab LFS vs Official git-lfs

| Aspect | Official git-lfs | crab LFS |
|--------|-----------------|------------|
| **Server required** | Yes for the standard HTTP profile | No for native/direct |
| **Transport** | HTTP Batch API | Direct S3/GCS/Azure PUT/GET |
| **Transfer protocol** | HTTP + custom transfer agents | Standalone JSON lines |
| **Pointer format** | SHA-256 LFS pointer | SHA-256 LFS pointer (compatible) |
| **Hashing** | SHA-256 only | SHA-256 (LFS) + Blake3 (crab native) |
| **Dedup** | None (file-level) | CDC chunking for crab-tracked files |
| **Locking** | Server-side lock API | CAS on object storage |
| **Concurrency** | Server/client negotiated | Git LFS process fan-out; Crab bounds each process to one object |
| **Resume** | TUS protocol (server-dependent) | Bounded retries; object restart after agent/process failure |
| **Mixed formats** | LFS only | LFS + crab pointers in same repo |
| **Migration** | import/export/info | import/export/info + crab↔LFS conversion |

### Transfer Size Comparison (2 GB File)

```
                        git-lfs         crab (LFS mode)    crab (native mode)
First push:             2.0 GB          2.0 GB               ~1.2 GB (zstd compressed)
10% changed (update):   2.0 GB          2.0 GB               ~200 MB (only changed chunks)
```

LFS mode stores whole files (no dedup). For dedup benefits, use
crab-native mode (`filter=crab`). Both modes coexist in the same
repository — use LFS for compatibility, crab-native for performance.
Cross-format `crab lfs dedup` deletes only local LFS cache objects after
verifying the LFS SHA-256 path, a reachable Crab pointer with matching
Blake3/size, and byte-identical reconstruction from local Crab staging.

-----

## 15. Worked Example: LFS Workflow for a 2 GB Model

### Setup

```
$ crab lfs install
  → sets filter.lfs.clean = /path/to/crab lfs clean
  → sets filter.lfs.smudge = /path/to/crab lfs smudge
  → sets filter.lfs.required = true
  → sets lfs.customtransfer.crab.path in git config
  → sets lfs.customtransfer.crab.args = lfs-transfer-agent
  → sets lfs.customtransfer.crab.concurrent = true
  → sets lfs.customtransfer.crab.direction = both
  → sets lfs.standalonetransferagent = crab
  → installs pre-push hook

$ crab lfs track "*.safetensors"
  → adds "*.safetensors filter=lfs diff=lfs merge=lfs -text" to .gitattributes
```

### Add and Commit

```
$ cp ~/models/llama-7b.safetensors .
$ git add llama-7b.safetensors

  filter-process receives clean command:
    1. Detect filter=lfs in .gitattributes
    2. SHA-256 hash content (2.0 GB) → oid = 4d7a2146...
       (runs in spawn_blocking to avoid blocking tokio)
    3. Stage raw bytes → LfsObjectStore (local .git/lfs/objects/4d/7a/4d7a2146...)
    4. Emit LFS pointer (120 bytes) → Git ODB

$ git commit -m "add llama-7b model"
```

### Push

```
$ git push origin main

  pre-push hook fires:
    1. Walk commits being pushed → find 1 LFS pointer (oid=4d7a2146...)
    2. HEAD check: does 4d7a2146... exist on remote? → No
    3. Check lock conflicts → none
    4. Upload 2.0 GB to s3://bucket/repo/lfs/objects/4d/7a/4d7a2146...
       (multipart upload, 8 MiB parts, 8 concurrent)
       Progress: "Uploading LFS objects: 1/1 (2.0 GB)"
    5. Upload completes → push proceeds with git pack

  Total: ~60s at 250 Mbps (network-bound)
```

### Clone (Another Machine)

```
$ git clone crab://bucket/repo
  → clones git objects (commits, trees, LFS pointers)
  → filter-process smudges LFS pointers:
      1. Detect LFS pointer → oid = 4d7a2146...
      2. Download from s3://bucket/repo/lfs/objects/4d/7a/4d7a2146...
      3. Write 2.0 GB to working tree

  Or with --skip-smudge for lazy checkout:
$ git clone crab://bucket/repo
$ crab lfs install --skip-smudge
  → pointers stay as-is in working tree
$ crab lfs pull --include "*.safetensors"
  → downloads only the files you need
```

### Lock and Edit

```
$ crab lfs lock llama-7b.safetensors
  → CAS-creates lock record in s3://bucket/repo/lfs/locks/{hash}
  → "Locked llama-7b.safetensors"

$ crab lfs locks
  llama-7b.safetensors    user@example.com    ID:a1b2c3d4

  ... edit file, commit, push ...

$ crab lfs unlock llama-7b.safetensors
  → CAS-deletes lock record (verifies owner)
```

-----

## 16. Source Map

| Component | File | Key Types/Functions |
|-----------|------|---------------------|
| LFS pointer parse/serialize | `lfs/pointer.rs` | `LfsPointer`, `parse`, `serialize`, `is_canonical` |
| Dual pointer detection | `lfs/detect.rs` | `PointerKind`, `classify` |
| LFS object storage | `lfs/object_store.rs` | `LfsObjectStore`, `put`, `get`, `exists`, `verify` |
| Transfer agent protocol | `lfs/transfer_agent.rs` | `run_transfer_agent` |
| Batch resolver | `lfs/batch.rs` | `BatchResolver`, `find_missing_for_push`, `upload_missing` |
| Advisory file locking | `lfs/lock.rs` | `LockManager`, `LockRecord`, `lock`, `unlock`, `check_conflicts` |
| Migration engine | `lfs/migrate.rs` | `migrate_import`, `migrate_export`, `migrate_info` |
| Prune engine | `lfs/prune.rs` | `prune`, `build_referenced_set` |
| LFS configuration | `lfs/config.rs` | `LfsConfig`, `resolve` |
| Track/untrack | `lfs/track.rs` | `track`, `untrack`, `list` |
| LFS status | `lfs/status.rs` | `LfsFileStatus`, `lfs_status` |
| CLI commands | `cmd/lfs/mod.rs` | Subcommand dispatch for all `crab lfs *` commands |
| Filter process (LFS integration) | `git/filter_process.rs` | `dispatch_command` (LFS clean/smudge routing) |
| Clean session (LFS path) | `git/clean.rs` | `CleanSession` (SHA-256 hashing + LFS staging) |

-----

## 17. Invariants Checklist

These invariants must hold across all LFS code paths.

| # | Invariant | Enforced By |
|---|-----------|-------------|
| 1 | LFS object SHA-256 verified on upload before PUT confirmed | `LfsObjectStore::put` |
| 2 | LFS object SHA-256 verified on download before writing to local storage | `BatchResolver::download_missing` |
| 3 | Idempotent put: uploading the same OID twice succeeds without error | `LfsObjectStore::put` (exists check) |
| 4 | Lock CAS prevents race conditions on create/delete | `LockManager` uses `coordination::cas` |
| 5 | Pre-push hook checks lock conflicts before uploading | `pre-push` command wires `LockManager::check_conflicts` |
| 6 | Migration leaves original refs intact on failure | `migrate.rs` abort path |
| 7 | Filter process errors are per-file, not per-session | Error catch in `filter_process.rs` dispatch |
| 8 | Transfer agent continues on individual transfer failure | Error → `complete` event with error object, no exit |
| 9 | Pointer round-trip: `parse(serialize(p)) == p` for all valid pointers | Property-based test (proptest) |
| 10 | No `unwrap`/`panic` in LFS library code | Code style rule; errors propagated via `?` |
