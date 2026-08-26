# Crab LFS production-readiness implementation plans

Generated from a fresh source, test, documentation, and upstream-contract audit on 2026-08-25 at commit `2cbd0d92`. Execute in order unless a dependency explicitly permits parallel work. Every executor must read its phase file fully, honor its STOP conditions, and update the status row here.

## Audit verdict

Crab LFS is functional and has strong content-integrity foundations. Focused tests pass, and live RustFS qualification already proved the standalone transfer-agent path with Git LFS 3.7.1 for 10,900 distinct 1 MiB objects across 100 commits and 1,000 current paths. It is not yet justified as a total Git LFS replacement:

- The proven compatibility path is `lfs.standalonetransferagent=crab`; Crab does not expose Git LFS HTTP discovery, Batch, basic-transfer, or locking endpoints for unmodified clients.
- Upload and publication paths repeatedly download and hash immutable remote payloads. The common push durability gate rescans reachable pointers and verifies payloads again before refs become visible.
- Several downloads materialize whole objects in memory. The large-object resume path performs a range GET but still retains the remaining payload and then rereads the completed partial file.
- Transfer and Git-object discovery paths retain work proportional to total object count or command output instead of configured concurrency and byte budgets.
- Documentation says “full” and “drop-in” compatibility while describing a narrower custom-agent contract, and some source references describe pre-split files.
- There is no dedicated LFS RustFS release gate covering failures, interruption/resume, linked worktrees, locking, or the requested 100,000-object/1–10 MiB profile.

## Highest-value opportunities

| Finding | Evidence | Impact | Effort | Risk | Confidence |
|---------|----------|--------|--------|------|------------|
| Selected remote is discarded | `crab/src/cmd/lfs/mod.rs:1003`, `crab/src/cmd/lfs/push.rs:219`, `crab/src/cmd/lfs/transfer_agent.rs:19` | LFS bytes can publish to a different remote/prefix than Git refs | M | HIGH | High |
| Default install is globally unconditional | `crab/src/cmd/lfs/install.rs:126`, `crab/src/cmd/lfs/install.rs:18` | Unrelated Git LFS repositories can be redirected to Crab | M | MED | High |
| Payload-sized download memory | `crab/src/lfs/batch.rs:236`, `crab/src/lfs/transfer_agent.rs:490`, `crates/crab-lfs/src/object_store.rs:290` | OOM risk for large assets; resume does not bound memory | L | MED | High |
| Repeated remote and local hashing | `crab/src/lfs/batch.rs:162`, `crab/src/lfs/batch.rs:172`, `crates/crab-lfs/src/object_store.rs:221`, `crab/src/lfs/publication.rs:54` | Extra object-store reads and local I/O dominate push latency | L | HIGH | High |
| Unbounded work retention | `crab/src/lfs/batch.rs:150`, `crab/src/lfs/transfer_agent.rs:141`, `crab/src/cmd/lfs/push.rs:337` | Memory/process pressure at 100k+ objects | M | MED | High |
| Duplicate pointer scan/publication gate | `crab/src/lfs/publication.rs:15`, `crab/src/git/push.rs:15320` | Native push remains slow after fast LFS transfer | L | HIGH | High |
| Standard HTTP LFS surface absent | `crab/src/cmd/lfs/install.rs:18`, `crates/crab-auth-server/README.md:1` | External clients cannot use Crab without custom config and direct cloud access | XL | HIGH | High |
| Lock contract is Crab-specific | `crab/src/lfs/lock.rs:1`, `crab/src/cmd/lfs/locks.rs:1` | No standard File Locking API; unlock has a documented read/delete race | L | HIGH | High |
| Production qualification absent | `.github/workflows/rust.yml`, no LFS RustFS workflow | Regressions and provider differences are not release-blocking | L | MED | High |
| Docs overstate compatibility | `crab/docs/design/lfs.md:90`, `crab/docs/guides/lfs.md:13` | Operators choose an unsupported integration model | S | LOW | High |

## Execution order and status

| Phase | Plan | Priority | Effort | Depends on | Status |
|-------|------|----------|--------|------------|--------|
| 0 | [Define the compatibility contract and baseline](001-compatibility-contract-and-baseline.md) | P1 | M | — | TODO |
| 1 | [Make object transfer streaming and bounded](002-streaming-object-io.md) | P1 | L | 001 | TODO |
| 2 | [Create one bounded transfer coordinator](003-canonical-transfer-coordinator.md) | P1 | L | 002 | TODO |
| 3 | [Make pointer discovery and publication scale](004-scalable-discovery-and-publication.md) | P1 | L | 002, 003 | TODO |
| 4 | [Add durable presence receipts and safe locking](005-presence-receipts-and-locking.md) | P1 | L | 002, 003 | TODO |
| 5 | [Make history migration bounded and transactional](006-bounded-history-migration.md) | P1 | L | 001, 002 | TODO |
| 6 | [Complete managed transfers and expose standard HTTP LFS](007-managed-and-standard-http-lfs.md) | P2 | XL | 001–005 | TODO |
| 7 | [Establish production qualification and release gates](008-production-qualification.md) | P1 | L | 001–005; 006 for migration; 007 for managed/HTTP profiles | TODO |

Status values: `TODO`, `IN PROGRESS`, `DONE`, `BLOCKED: reason`, or `REJECTED: reason`.

## Dependency notes

- Phase 0 fixes the product vocabulary and creates deterministic compatibility fixtures. Later phases must not invent a different meaning of “compatible.”
- Phase 1 establishes streaming integrity primitives. Phase 2 must compose them instead of retaining the existing `Bytes`-based paths.
- Phase 3 changes the object-before-ref durability gate, so it follows the canonical transfer coordinator and must preserve lock-before-publication ordering.
- Phase 4 makes remote presence cheap without trusting provider ETags as SHA-256. It is needed before the HTTP Batch gateway can return upload/download actions cheaply and safely.
- Phase 5 is independent of the push fast path but is required before claiming Git LFS migration parity on large repositories.
- Phase 6 first closes managed standalone-agent authorization, then adds the optional HTTP product surface. Direct/serverless Crab remains supported.
- Phase 7 can start native/standalone profiles after Phase 4. Migration waits for Phase 5; managed/HTTP profiles wait for Phase 6.

## Product decision encoded by this roadmap

“Replace Git LFS” has two separate support levels:

1. **Crab-managed repositories**: Crab supplies filters, porcelain, publication, object storage, and locking; `git-lfs` is optional.
2. **Standard Git LFS interoperability**: unmodified Git LFS clients discover an HTTPS endpoint and use the documented Batch, basic-transfer, and File Locking APIs.

Phase 7 may certify these levels independently. Documentation must not say “drop-in replacement” until the standard-interoperability profile passes its release gate.

## Findings considered and rejected or deferred

- Implementing the proposed pipelined custom-transfer-agent protocol now: deferred. Current Git LFS custom-transfer protocol is serial per process; speculative pipelining would not improve current clients.
- Treating S3 ETag as the LFS OID: rejected. Multipart ETags are not SHA-256 and provider semantics differ.
- Removing full verification for legacy objects: rejected. Objects without a trusted Crab receipt must continue to use streamed SHA-256 verification.
- Adding an HTTP server to `crab-lfs`: rejected. `crab-lfs` owns reusable object mechanics; authentication and HTTP composition belong at a product/server boundary.
- Wiring `crab/src/lfs/lifecycle.rs::run_prune` or `run_fsck`: rejected. Those are stale duplicate implementations; Phase 7 removes or narrows them after proving the canonical CLI paths.
- Bucket-wide LFS garbage collection in this roadmap: deferred. It requires the repository-wide GC ownership and fencing design; local LFS prune and remote lifecycle policy generation are the only in-scope maintenance behaviors.

## Upstream contracts

- Git LFS custom transfers: https://github.com/git-lfs/git-lfs/blob/main/docs/custom-transfers.md
- Git LFS configuration: https://github.com/git-lfs/git-lfs/blob/main/docs/man/git-lfs-config.adoc
- Git LFS API overview: https://github.com/git-lfs/git-lfs/blob/main/docs/api/README.md
- Batch API: https://github.com/git-lfs/git-lfs/blob/main/docs/api/batch.md
- Basic transfer: https://github.com/git-lfs/git-lfs/blob/main/docs/api/basic-transfers.md
- File Locking API: https://github.com/git-lfs/git-lfs/blob/main/docs/api/locking.md
