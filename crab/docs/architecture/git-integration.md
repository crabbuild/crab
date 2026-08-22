# Git Integration

## Overview

Crab plugs into Git through two extension mechanisms: the remote helper
protocol (for push/fetch) and the filter driver protocol (for clean/smudge).
Everything else — commits, branches, merges, diffs, blame — works natively
on pointer blobs without crab involvement.

Source: `crab/src/git/`

## Remote Helper Protocol

When Git encounters a `crab://` URL, it spawns `git-remote-crab` as a
subprocess. The binary detects this mode via `argv[0]`:

```
argv[0] == "git-remote-crab"  →  remote helper mode
argv[0] == "crab"             →  CLI mode
```

### Protocol Exchange

```
git → helper:  capabilities
helper → git:  fetch\npush\noption\n[stateless-connect when the repository proof is current]\n\n

git → helper:  list [for-push]
helper → git:  {sha} refs/heads/main\n...\n\n

git → helper:  fetch {sha} {ref}\n...\n\n
helper:        (downloads packs, writes to local ODB)
helper → git:  \n

git → helper:  push {src}:{dst}\n...\n\n
helper:        (executes 14-step push pipeline)
helper → git:  ok {ref}\n...\n\n
```

For a repository with a complete generation-bound visibility proof, Git then
uses the terminal v2 path:

~~~
git → helper:  stateless-connect git-upload-pack
helper → git:  (raw blank line)
git ↔ helper:  protocol-v2 pkt-lines over the same stdio
~~~

The helper performs the temporary upload-pack role locally. It does not start
a listener or require a Crab service. Repositories without a current locator
and visibility proof remain on the legacy complete-pack path. That legacy path
downloads and installs the immutable packs named by the manifest, then lets
local Git satisfy the fetch; it is retained for older Git clients, repositories
above the synchronous 100,000-object proof profile, and recovery while derived
proof coverage is unavailable.

Source: `crab/src/git/remote_helper.rs`

### Direct object-store support contract

This matrix applies to `crab://bucket/repository` remotes backed directly by
S3 or an S3-compatible store. RustFS qualification proves the S3-compatible
path only; GCS, Azure, and other providers require their own conditional-write
and ETag evidence before Crab claims provider parity.

| Git operation or capability | Direct S3-compatible remote | Automated evidence | RustFS evidence |
|---|---|---|---|
| Ref list / `git ls-remote` | Supported; only an absent routed manifest is an empty repository | `git::remote_helper` manifest and resolved-session tests | missing, valid, and malformed manifest cases |
| Clone | Supported | `remote_helper_transcript`, `e2e_fetch_fsck` | fresh ordinary-Git and `crab clone` cases |
| Fetch into an existing clone | Supported | resolved helper session and `e2e_fetch_fsck` | second-clone branch update case |
| Push / branch update | Supported | `e2e_add_commit_push`, helper transcript tests | ordinary Git and Crab CLI cases |
| Force push | Supported when explicitly requested | push decision tests | force-update case |
| Branch deletion | Supported | helper delete-ref tests | delete/list/fresh-clone case |
| Atomic multi-ref push | Supported; one rejected ref aborts the batch | helper atomic-abort tests | two-ref rejection case |
| `git push --follow-tags` / `crab push --follow-tags` | Supported; Git sends selected tag refs, while the native CLI discovers eligible annotated tags | native follow-tags and multi-ref manifest tests | annotated-tag object and ref verification |
| Fetch tag following (`option followtags`) | Supported by complete-pack installation | resolved helper session installs an annotated tag object | tag fetch/clone verification |
| Remote-helper `include-tag` option | Unsupported; it is not a defined remote-helper option | option contract test | not advertised |
| Depth-based shallow clone/fetch | Supported through a bounded commit-graph summary; requests beyond its retained edge safely complete as a full fetch | shallow fetch, compaction-edge, deepen, and unshallow tests | depth, deepen, and unshallow cases |
| Date- and ref-exclusion-based shallow fetch | Unsupported; `--shallow-since` and `--shallow-exclude` fail explicitly instead of silently fetching full history | remote-helper option contract tests | expected-failure cases |
| Lazy checkout | Supported for Crab pointer payloads; it is not Git partial clone | clone and hydration tests | lazy clone retains pointers |
| Hydration | Supported and hash verified | `e2e_add_commit_push` | 64 MiB byte-identical reconstruction |
| Connectivity check | Supported for complete fetches | fetch connectivity tests | clone plus `git fsck --connectivity-only` |
| Immutable pack/index integrity | Fail closed before ref update or checkout when a required pack or published index is missing | pack validation and fetch fail-before-download tests | isolated missing-pack and missing-index clone failures |
| Git partial-clone filters | Supported on the proof-gated protocol-v2 path: `blob:none`, `blob:limit=<n>[kmg]`, `tree:<depth>`, `object:type={tag,commit,tree,blob}`, full-SHA-1 `sparse:oid`, and bounded repeated/combine intersections | `v2_fetch_transport`, `remote_helper_transcript`, live filter matrix/lazy-fetch proof | RustFS filter matrix, lazy blob retrieval, promisor sidecar, strict fsck |
| `connect` | Unsupported; there is no stateful takeover | helper command contract tests | not applicable |
| `stateless-connect git-upload-pack` | Supported only when the pinned snapshot has locator and visibility coverage; terminal and failure paths are fail-closed | raw wire transcripts and helper dispatch tests | `git ls-remote`, clone, fetch, shallow/deepen |
| Git wire protocol v2 | Supported profile: `ls-refs`, `fetch`, depth/deepen, sideband, tags, and the documented filter matrix; optional extensions remain unsupported | upload-pack wire tests | RustFS protocol trace, filter matrix, and Git operations |
| `packfile-uris`, `object-info`, `ref-in-want`, date/ref shallow selectors | Unsupported and rejected explicitly | parser/option contract tests | expected-failure cases |

The manual evidence owner for the protocol-v2 and partial-clone rows is
`crab/scripts/e2e/run_protocol_v2_partial_clone_rustfs_smoke.py`. It uses one
unique remote prefix per run, verifies expected failures as non-zero commands,
and never performs bucket-wide cleanup. The complete-pack rows remain owned by
`crab/scripts/e2e/run_add_commit_push_rustfs_smoke.py`.

The protocol-v2 and partial-clone rows describe the implemented development-
line profile. RustFS is qualified; provider and released-artifact evidence is
still required before those rows become released support claims.

## Filter Driver (Long-Running Process)

Git's filter driver mechanism transforms file content at two points:
- **Clean** (working tree → Git ODB): runs at `git add` time
- **Smudge** (Git ODB → working tree): runs at `git checkout` time

Crab uses Git's long-running clean/smudge **filter-process protocol v2**, where
a single persistent process handles all clean/smudge operations in a session.
This protocol is unrelated to Git wire protocol v2. Git wire protocol v2 is
implemented separately by the proof-gated local upload-pack session above.

### Registration

```ini
[filter "crab"]
    process = crab filter-process
    clean = crab filter-process
    smudge = crab filter-process
    required = true
```

Activated per file pattern in `.gitattributes`:

```
*.safetensors filter=crab diff=crab merge=crab -text
```

Source: `crab/src/git/filter_process.rs`

## Clean Path (git add)

When `git add model.safetensors` runs:

```
File content (10 GB)
    │
    ▼  Git sends to filter-process ("clean" command)
    │
    ├── 1. Single-pass: blake3 hash + CDC chunking simultaneously
    │
    ├── 2. Fast-path check (≥64 MiB files):
    │      bloom filter → file-index HEAD → skip staging if known
    │
    ├── 3. Classify each chunk (3-tier dedup):
    │      A (Existing) → skip
    │      B (Staged)   → skip
    │      C (New)      → stage to .crab/staging/
    │
    ├── 4. Stage class-C chunks:
    │      Append to segments/current.seg
    │      Index in index.db (SQLite WAL)
    │
    └── 5. Emit pointer blob (~200 bytes) → Git ODB
```

Key properties:
- **No network I/O.** Clean is entirely local.
- **Streaming.** File never fully buffered in memory.
- **Deterministic.** Same content → same pointer.
- **Small files pass through.** Files below the chunk threshold (default 1 MiB)
  are not processed by the filter.

Source: `crab/src/git/clean.rs`

## Smudge Path (git checkout)

When Git checks out a file with `filter=crab`:

```
Pointer blob from Git ODB
    │
    ▼  Filter process receives "smudge" command
    │
    ├── 1. Parse pointer → (file_hash, size, shard_hint)
    │
    ├── 2. Lazy mode? → return pointer unchanged (instant)
    │
    ├── 3. Resolve file-index → shard_hash
    │      (shard-hint skips this lookup)
    │
    ├── 4. Load shard (cache or S3) → reconstruction terms
    │
    ├── 5. Coalesce byte ranges (COALESCE_GAP = 5 chunks)
    │
    ├── 6. Parallel Range GETs on xorbs (up to 16 concurrent)
    │
    ├── 7. Decompress chunks (zstd), verify blake3 per chunk
    │
    └── 8. Stream reconstructed content → Git writes to working tree
```

### Delayed Smudge

Git's filter protocol v2 supports a "delay" capability. The filter can tell
Git "I'm not done yet; give me the next file." This allows crab to queue
multiple smudge requests and satisfy them in parallel, dramatically speeding
up checkouts of repos with many large files.

Source: `crab/src/git/smudge.rs`

## Push Pipeline (14 Steps)

The push pipeline is the core of `git push`, implemented as `PushPipeline`
in `push.rs`:

```
Phase 1: CLASSIFY (steps 1-4)
  1. Enumerate pointer blobs via gix-traverse
  2. Staging lookup per pointer
  3. Pre-push shard sync → refresh ChunkIndex
  4. Classify chunks: A (existing), B (staged), C (new)

Phase 2: PACK (steps 5-6)
  5. Pack class-B/C chunks into ~64 MiB xorbs
  6. HEAD check for resume: skip already-uploaded xorbs

Phase 3: UPLOAD (steps 7-10)
  7. Parallel xorb uploads (up to 16 concurrent)
  8. Build shard (reconstruction metadata)
  9. Upload shard + file-index entries
  10. Upload bounded Git pack set (.pack + .idx + .meta per pack)

Phase 4: COMMIT (steps 11-14)
  11. Publish ref-scoped visibility evidence, then commit the ref journal marker
  12. One owner compacts the journal and publishes generation proof + locators
  13. Post-success cleanup (staging → cache, shard install)
  14. On failure before CAS: refs and manifest remain unchanged
```

### Critical Ordering Invariant

Steps 7-10 (immutable data uploads) and the supported-profile visibility
evidence must complete before the ref marker in step 11. This ensures the
fail-forward property: an interrupted push may leave orphaned immutable data
(cleaned by GC) but never creates dangling or proofless supported-profile
references. Protected/service paths apply the same rule to the candidate
generation proof before manifest or coordinator commit. Protected receive
builds that proof from its existing verified materialization workspace, so the
commit boundary does not download the candidate pack inventory a second time.

Current proof keys use the manifest Git-validation digest, and current proof
bodies store one sorted object-ID dictionary plus per-ref position closures.
Crab 1.0.15's generation-and-pack-index key remains readable and GC-rooted only
as a shipped data migration; the next write or explicit metadata repair
backfills the digest-bound proof.

Source: `crab/src/git/push.rs`, `crab/src/git/push_manifest.rs`,
`crates/crab-git/src/push_state.rs`

## Fetch Pipeline

When Git fetches from a crab remote:

```
1. GET the unified manifest → return its HEAD and ref map to Git
2. Follow its segmented pack-index hash
3. Diff against local .git/objects/pack/
4. Download missing packs in parallel (with SHA1 verification)
5. Opportunistic shard sync in background (warm ChunkIndex)
6. Write packs to .git/objects/pack/
```

Git then updates local refs and checks out the working tree, triggering
the smudge filter for changed pointer files.

Source: `crab/src/git/fetch.rs`

## Pack Generation

The `pack` module generates standard non-thin Git packfiles for upload. It
shells out to `git pack-objects` in base-name mode with `--max-pack-size`, so a
single push may produce multiple independently usable packs. The configured
`receive.maxInputSize` bounds each pack, not the aggregate reachable closure;
all generated packs enter one manifest generation and ref CAS. A single object
that cannot fit in the limit remains a `pack-too-large` rejection.

Source: `crab/src/git/pack.rs`

## Compact Git Object Locator

Standard packs and their canonical indexes remain the source of truth. After
manifest CAS, Crab streams verified `.idx/.rev` locations into the sole
`{repo}/git_locator_db/` SlateDB database. Each Git OID has one fixed-width
current row containing a numeric pack slot, byte offset, entry length, and
CRC32. Pack-slot records join those rows to immutable pack identities.

The locator has no generation-history, head, or reverse-offset key families.
Exact coverage records the one manifest generation and pack-index hash whose
complete inventory was published. Planning against a different snapshot
validates misses through that snapshot's canonical `.idx` files. Rebuild
streams every pinned pack, removes stale slots, and advances coverage only
after the full inventory is durable.

Locator writers disable SlateDB's periodic background garbage collector because
each helper is short-lived and SlateDB's first timer tick runs immediately.
The writer compactor commits and polls on a 500 ms cadence: short publications
avoid the request burst caused by 100 ms polling, while longer publications
still claim compaction work before level-zero backpressure. This cadence is a
request-cost policy, not part of locator correctness.
After exact coverage crosses each 32-generation boundary, that publication runs
one foreground collection for superseded manifest and compaction objects. The
collector keeps SlateDB's five-minute minimum age, so it cannot race freshly
published locator state.

Source: `crates/crab-metadata/src/git_object_locator/`,
`crates/crab-git/src/pack_locator.rs`

## Tree Walking

The `walk` module traverses commit and tree objects using gitoxide:

- `walk_reachable()`: Walk all reachable commits and blobs from tip SHAs
- Pointer detection: blobs ≤256 bytes are tested with `Pointer::parse()`

The `incremental_walk` module optimizes this by using the commit graph
summary to skip already-processed commits.

Source: `crates/crab-git/src/walk.rs`, `crab/src/git/incremental_walk.rs`

## URL Parsing

The `url` module parses `crab://bucket/repo-path` URLs into structured
`CrabUrl` types with `bucket` and `repo_path` fields.

Source: `crab/src/git/url.rs`

## Shallow Clone Support

The `shallow` module handles shallow clone semantics, ensuring that
shallow-cloned repositories work correctly with crab's push and fetch
pipelines.

Source: `crab/src/git/shallow.rs`

## Partial Clone Status

The current development-line implementation supports direct filtered clones
and filtered fetches over protocol v2 when the remote generation has complete
locator and all-object visibility coverage. The accepted forms are
`blob:none`, `blob:limit=<n>[kmg]`, `tree:<depth>`,
`object:type={tag,commit,tree,blob}`, full-SHA-1 `sparse:oid`, and bounded
repeated/combine intersections; see the support table above for semantics.
RustFS lifecycle qualification is green; AWS/provider and released-artifact
qualification remain before this is a released support claim.
The planner authorizes raw OIDs from that immutable proof before reading bytes;
the local helper produces a standard Git pack, and Git owns its promisor config,
pack installation, and `.promisor` sidecars. A later `git cat-file`, checkout,
diff, or merge can request missing blobs through a new helper session.

The `crab clone` wrapper's default lazy mode is different: it configures Crab's
pointer checkout and does not request a Git partial clone. Use ordinary Git
with one of the supported `--filter=...` forms when Git-level filtering is
desired. Date-based
and ref-exclusion shallow selectors, stateful `connect`, `packfile-uris`,
`object-info`, and `ref-in-want` remain unsupported and fail explicitly.
