# Crab Rust SDK: technical design and delivery plan

Status: behavior implementation merged in PR #160; the API simplification is
implemented in PR #169.
Baseline: `ebd0e40d14ca862cefa5366c1f856847e2401660` (2026-09-06).
Public package: `crab-sdk`; Rust import: `crab_sdk`.
Implementation location: `crates/crab-sdk/`.

Delivery evidence: [capability inventory](sdk-capabilities.md) and
[qualification inputs and phase-0 record](sdk-qualification.md).
Phases 0 through 8 are implemented. Backend support is declared only after the
mandatory credentialed, platform, fault, package, and performance cells pass.
The [SDK API overhaul](sdk-api.md) is the plan of record for public
names, namespaces, unified opening, and repository handle shape. The behavior,
safety, recovery, performance, and qualification contracts in this document
remain authoritative.

## 1. Outcome and scope

The proposed [S3 gateway](crab-s3-gateway.md) is a separate server consumer of
this SDK. Its logical `crabfs://REPO/REF/KEY` namespace does not replace direct
storage locators. S3 protocol support and gateway-specific metadata/multipart
contracts require their own implementation and qualification.

Deliver a supported Rust client for two workflows:

1. Access and modify a remote Crab repository without a checkout or local Git
   object database. Read Git objects and reconstructed file content, create
   commits, update branches/tags, and initialize direct repositories.
2. Work with an ordinary local Git repository and working directory. Open,
   clone, fetch, stage, commit, checkout, pull, push, hydrate, and inspect status
   through Rust APIs, preserving interoperability with the Crab CLI and Git.

"Local" describes the presence of a working directory. Fetch, clone, and push
still use the network. Local open, status, and local history must work offline;
operations needing unavailable remote content return a typed error.

Repository management includes direct initialization and ref management, plus
managed-service repository listing, creation, rename, archive, and restore.
Organization administration, service-account administration, issues, reviews,
notifications, bucket administration, repository destruction, GC, replication,
and VFS mounts are outside SDK 1.0. These exclusions do not delay the repository
operations in this document.

SDK 1.0 promises the compatibility cells in section 8. It does not promise every
Git CLI flag or every preview capability in Crab's existing Git matrix.

### Corrections to the preliminary proposal

- `crab-write` already exists. Extend and compose its canonical implementation;
  do not create a competing publication implementation.
- Full local interoperability requires Git and a compatible Crab executable.
  Git invokes Crab's clean/smudge filter when handling hydrated worktrees.
  The earlier suggestion of a completely helper-free local profile omitted
  this dependency. Remote operations remain independent of both executables.
- Managed Crab control-plane APIs and `crab-http-server` application APIs are
  different contracts. The managed client already has repository lifecycle
  methods; incomplete HTTP application administration does not block them.
- Historical `crab-sdk` references describe a retired read-first package. Its
  API names, cache materialization, and compatibility assumptions are not the
  contract for this SDK.

## 2. Evidence and present gaps

Paths below are relative to the repository root. Source is authoritative when
historical documentation describes a retired implementation.

| Surface | Current source / consumers / proof | Design consequence |
| --- | --- | --- |
| Workspace | `Cargo.toml`; `crab/docs/architecture/multi-crate-transition.md` retirement notice | Add a new package; no existing SDK to extend |
| Remote reads | `crates/crab-remote-git/src/repository.rs`, `snapshot.rs`; HTTP read handlers; `tests/remote_repository.rs` | Preserve generation pinning, byte paths, limits, and explicit operation cleanup |
| Per-operation limits | `crates/crab-remote-git/src/operation.rs` reads aggregate limits from repository state for deadlines, budgets, accessors and batch admission | Move operation-specific limits through every consuming path; reopening a repository must not be required to tighten a snapshot read budget |
| SDK path validation | `crates/crab-remote-git/src/path.rs` deliberately preserves dot-like components; HTTP callers use that raw Git contract | SDK-owned `GitPath` must enforce the stricter section-4 input contract without changing the raw reader's semantics |
| File content | `crates/crab-read/src/lib.rs`; CLI hydration and protected receive | Share verified reconstruction; raw Git blobs and hydrated content are separate APIs |
| Content ranges | `crates/crab-read/src/hydrator.rs`; VFS range readers and CLI hydration facade | Existing range helper buffers the requested range and creates its own cancellation token; add shared bounded streaming with caller-owned cancellation for SDK streams |
| LFS streaming | `crates/crab-lfs/src/object_store.rs` `get_verified_stream_at`; HTTP LFS download | The current helper attempts a verification-receipt write, then opens another read; SDK reads need a read-only owner path and proof that delivered bytes match the verified object version |
| Ref publication | `crates/crab-write/src/journal.rs`, `generation.rs`; CLI push and HTTP receive; `tests/journal.rs`, `tests/generation.rs`, `tests/catalog.rs` | Journal commit does not itself own all locks, validation, or read readiness |
| Attribution | `crates/crab-metadata/src/plan_receipt.rs`; mirror publication and GC | Reuse durable intent/receipt mechanics; current ref equality is not historical commit proof |
| Direct transfer | `crab/src/git/fetch.rs`, `push.rs`, `push_native.rs`, `upload_pack_wire.rs`; remote helper and import | Extract orchestration with callers in the same phase; wrapping CLI argument structs is insufficient |
| Local workflow | `crab/src/cmd/clone.rs`, `pull.rs`, `add.rs`; `crab/src/git/process.rs`; `crab/tests/e2e_add_commit_push.rs` | Extract reusable worktree services and owned subprocess handling |
| Filters | `crab/src/cmd/init.rs` `DRIVER_CONFIG`; `crab/src/git/worktree_hydration.rs` | Full local mode needs a compatible executable and shared configuration |
| Managed access | `crates/crab-git/src/url.rs`; `crates/crab-auth-store/src/managed_repository.rs`; `crates/crab-auth/src/managed/client.rs` | Preserve direct/managed classification, grants, prepare/finalize, and lifecycle endpoints |
| Protected writer | `crates/crab-auth-server/src/receive/workflow.rs` | Final authorization and publication remain server responsibilities |
| HTTP receive | `crates/crab-http-server/src/receive/publish.rs`; `receive_tests.rs`, `receive_fault_tests.rs` | Share publication mechanics while retaining HTTP policy; current server rejects forced updates |
| Release | Supporting crate manifests use `publish = false`; workspace dependencies use paths | Publish the dependency closure deliberately before publishing the SDK |

Existing tests establish starting points, not SDK proof. Every acceptance test
below enters through the new public SDK API where that API exists.

## 3. Architecture and ownership

The public client exposes one concrete `Repository` facade with borrowed remote
and local interfaces. Backend dispatch is private and uses the existing
`RepositoryLocator` classification. Avoid a public generic `Repository<B>`, a
public backend trait, or one oversized method set containing unsupported local
and remote operations. The exact public shape and migration are defined by the
[SDK API overhaul](sdk-api.md).

```text
Rust application
  crab-sdk: Client, Repository, remote::Remote, local::Local, managed::Managed
    remote reads -> crab-remote-git + crab-read
    direct writes / transfers / local workflow -> crab-remote (new)
    managed resolution / publication / management -> crab-auth + crab-auth-store

crab CLI / Git helper ------> crab-remote
HTTP receive --------------> crab-remote publication module
protected receive ---------> existing server authorization + shared mechanics

crab-remote -> crab-git / crab-read / crab-write / crab-staging
           -> crab-storage / crab-metadata / crab-coordination / crab-xet
           -> crab-auth / crab-auth-store / cache and LFS owner crates as needed
```

`crab-remote` is an internal orchestration package with private modules for
transfer, publication, local workflow, and resolved client configuration.
Module boundaries hide ordering and resource ownership. They do not duplicate
storage, Git formats, auth, or reconstruction. Local/process dependencies are
feature gated so the HTTP receiver cannot acquire a Git executable dependency.

`crab-sdk` owns validated public request types and stable outcomes. Keep SDK
types independent of CLI errors/config, `gix`, `object_store`, SlateDB, Xet
implementation types, and server state. Re-export an existing Crab value type
only after documenting its semver support obligation. `bytes::Bytes`, Tokio
`AsyncRead`, and standard Rust path/time types are permitted public dependencies.

`crab-write` retains journal/catalog mechanisms. `crab-remote::publication`
owns leases, fences, verified object preparation, commit coordination, and
outcome recovery. It accepts caller policy through a narrow admission boundary.
HTTP authorization, branch protection, sessions, and application state remain
in `crab-http-server`. Protected receive remains a server composition and must
not trust a client's declaration that a push was authorized.

The CLI consumes shared services directly; it need not depend on the public SDK.
Remove extracted implementations from their old locations in the same change.
Keep only argument/config projection and output mapping at CLI entry points.

## 4. Behavioral contract and public API

The behavior, names, and semantics below define the implemented contract. The
namespace and repository-facade decisions are detailed in the
[SDK API overhaul](sdk-api.md).

```rust,ignore
let client = Client::builder()
    .direct_store(store_options)
    .build()?;

let repository = client.open(OpenOptions::remote(locator)).await?;
let remote = repository.remote()?;
let snapshot = remote.snapshot(Revision::branch("main")?).await?;
let raw = snapshot.read_blob(path.clone()).await?;
let mut content = snapshot.open_file(path).await?;

let local_client = Client::builder()
    .direct_store(store_options)
    .local(local::Options::new(local::Tools::new(git_path, crab_path)?))
    .build()?;
let repository = local_client
    .clone_local(locator, destination, local::CloneOptions::default())
    .await?;
let local = repository.local()?;
local.fetch(local::FetchOptions::default()).await?;
local.pull(local::PullOptions::fast_forward_only()).await?;
let push = local.prepare_push(local::PushOptions::current_branch()).await?;
persist(push.recovery_token())?;
let outcome = push.execute().await?;
```

| Public surface | Required methods and results |
| --- | --- |
| `Client` | `open`, `configure_local`, `clone_local`, `initialize_remote`, `managed`, family-specific recovery methods, `close` |
| `Repository` | `mode`, synchronous `remote` and `local` interface selection |
| `remote::Remote` | `refs`, `snapshot`, `refresh` returning a new repository, `capabilities`, `prepare_commit`, `prepare_ref_update`, `reconcile` |
| `remote::Snapshot` | `commit`, paginated `tree`/`history`, `diff`, `blame`, `read_blob`, `open_file`, `archive` |
| `local::Local` | `status`, `snapshot`, `fetch`, `stage`, `commit`, `checkout`, `pull`, `prepare_push`, `hydrate`, `dehydrate`, `prefetch_content`, `continue_integration`, `abort_integration` |
| `managed::Managed` | paginated `list`, `create`, `rename`, `archive`, `restore`; all use the existing managed service |
| `remote::write::PreparedMutation` | stable `recovery_token`, request-building `execute`; owns prepared data and operation lifetime |

`remote::Remote::capabilities` is a synchronous metadata query with no I/O.
It reports implemented operation families through the non-exhaustive
`remote::Capability` enum; it does not promise authorization or backend
qualification. The current read SDK reports `ReadGit`, plus `ReadContent` when
the `content` feature is enabled. `UpdateRefs` is implemented with `write` on
conditional-write cloud stores; filesystem stores reject publication before
lease admission. Other mutation families remain unadvertised until implemented.
Cached capability metadata remains inspectable after client close.

All modifying methods take an operation context. Read builders expose that
context without requiring it for the default case. A prepared object is
single-use; executing again requires reconstruction from its recovery token
and successful reconciliation. `prepare_push` defaults to the current branch's
configured upstream; absent or ambiguous upstream returns an input error.
An explicit destination ref overrides it. It never guesses a remote branch.

`CloneOptions` defaults: full history, remote symbolic HEAD, hydration from the
selected revision's committed `crab.toml` with lazy fallback, remote name
`origin`. Explicit lazy or eager mode overrides committed policy.
`FetchOptions` defaults: configured fetch refspecs,
no pruning, Git-style automatic following of reachable tags, no depth change.
`CommitOptions` requires author, committer and message; signing is outside 1.0.
`CheckoutOptions` defaults to preserving changes and lazy hydration.
`PushOptions` defaults to atomic publication and fast-forward updates;
unsupported atomicity is rejected, never silently downgraded.

Public inputs have private fields and validating constructors/builders.
`GitPath` preserves repository-relative bytes, rejects absolute paths, NUL,
empty components and `..`; it is distinct from an operating-system `Path`.
`ObjectId` stores the algorithm and bytes. SDK 1.0 supports SHA-1 Crab
repositories; SHA-256 input is rejected explicitly before mutation. Do not
truncate or accept a 64-character hash as SHA-1.

`Revision::branch` and `Revision::tag` are unambiguous. An optional textual
revision parser must reject ambiguous branch/tag names. Remote revisions only
resolve commits reachable from the captured authorized roots.

`read_blob` returns exact Git bytes, including pointer bytes. `open_file`
reconstructs Crab/LFS content with integrity validation and bounded range reads.
`archive` uses an explicit `ContentMode::{Git, Hydrated}`; default is Git.
Never silently replace a missing large file with its pointer text.

### Runtime, configuration, and features

- Async Tokio API; no SDK-owned global runtime or blocking facade in 1.0.
- `Client` is cheap to clone and `Send + Sync`. Handles share bounded runtime
  caches. Local mutations serialize by canonical Git common-directory identity;
  use a filesystem lease across SDK processes plus Git's native index/ref locks.
  Ordinary external Git does not honor the SDK lease: revalidate index/HEAD and
  worktree assumptions and use Git locks before applying changes.
- Explicit builder values override explicitly loaded configuration. No mutation
  of process environment, current directory, global Git config, or global
  tracing subscriber. Provider default credential chains run only when selected.
- Local profile loads the same resolved repository/provider/cache/hydration
  configuration as the CLI. Extract only the consumed projection and resolver
  into `crab-remote`; both clients use it. Preserve existing file schema and
  tested precedence. Do not copy the broad CLI `Config` aggregate.
- `default = []`: value types and builders only. `remote` enables read APIs;
  `write` implies `remote` and publication; `local` implies `write` plus local
  workflows/processes; `managed` enables existing managed auth clients.
- `content` enables hydration and LFS reconstruction and is implied by `local`.
  A metadata-only `remote` consumer must not acquire `crab-read`'s disk cache or
  reconstruction stack just to browse Git trees.
- No SDK `s3`/`gcs`/`azure` feature flags in 1.0: `crab-storage` currently selects
  all those `object_store` features. Document that measured dependency cost.
  Provider feature minimization is a separate owner-crate change.
- Direct provider options select S3/GCS/Azure through `crab-storage`. An explicit
  filesystem store is permitted for development. Raw object-store handles remain
  internal; tests inject at owner seams rather than widening the public SDK API.

Explicit S3 selection is implemented through
`DirectStoreOptions::s3(S3Options::new(bucket, region, access_key, secret_key)?)`.
`S3Options` accepts a session token and optional endpoint; the provider validates
the endpoint at client build. Explicit HTTP endpoint selection permits HTTP for
that store. Provider environment values cannot override these inputs. Debug
output is redacted, and explicit credential scopes have separate SDK cache
namespaces. Explicit GCS selection uses
`DirectStoreOptions::gcs(GcsOptions::new(bucket, access_token)?)`; it bypasses
application-default credentials, redacts debug output, and separates token cache
scopes. Explicit Azure selection uses `DirectStoreOptions::azure` with
`AzureOptions::bearer(account, container, token)?` or
`AzureOptions::sas(account, container, query_string)?`. Azure options accept an
optional endpoint with the same validation and explicit HTTP policy as S3.
Bearer tokens reject invalid header characters before provider construction;
SAS strings use the provider parser to avoid double encoding signatures. These
explicit credential forms are static. Managed grant refresh follows
the auth owner policy in phase 6; a new caller-defined refresh API is outside
this plan. Full live GCS/Azure qualification remains outstanding.

### Resources, limits, and errors

Operations carry `operation::Options` with a cancellation handle, optional
deadline, and limits. Defaults use the existing validated owner-crate limits;
phase 1 records their numeric values in generated API documentation and tests
that defaults stay aligned. No implicit unlimited buffer or repository scan.
Progress uses a bounded channel of typed events; coalesce progress when full.
Terminal results travel through the operation future, never through lossy events.

The SDK owns each operation task. Dropping its public future signals cancellation
but does not abort a worker that holds leases, publishes refs, or closes a
database. `Client::close().await` stops admission, cancels and drains workers and
closes resources. Applications must call it before destroying their runtime.
Process-crash recovery is tested separately; Rust `Drop` cannot promise async
cleanup after runtime or process termination.

Streams own their read session. EOF and explicit `close().await` report integrity
and close errors; dropping a stream schedules tracked cleanup. Consumers must
observe successful EOF to claim full-file integrity. Explicit close reports
unobserved finalization failures and drains cleanup; closing early does not verify
unread content. Partial range reads use the reconstruction owner's verified range
contract and do not claim a whole-file hash was recomputed.

`Error` preserves sources and exposes a stable `ErrorKind`: invalid input,
unsupported capability, authentication, authorization, not found, conflict,
indexing, corruption, limit exceeded, timeout, cancelled, I/O, and transport.
Errors carry operation identity and bounded redacted context. Public enums that
can grow are non-exhaustive. No string parsing to infer ref or merge outcomes.

Read retries follow the storage/auth owner contracts within the operation budget.
SDK code must not multiply retry loops. Mutation retry rules are in section 5.

## 5. Remote publication and recovery

### Preparation and commit

`prepare_commit` takes a base commit, target branch, expected old OID, author,
committer, message, and a stream of file edits. Edits specify Git or hydrated
content. Reuse the current object/tree builders and chunk/staging mechanics.
It creates no checkout, invokes no Git executable, and preserves object IDs.
Temporary spool files for bounded pack and large-file preparation are allowed;
they are not a local Git object database. The caller selects a scratch directory.

`prepare_ref_update` accepts a nonempty atomic batch of branch/tag create,
update, and delete operations. Every update/delete supplies an expected old
OID; create means the ref must be absent. Branch updates default to fast-forward.
Force-with-lease requires an explicit policy and expected OID. No blind force.

Preparation may create immutable staging objects but cannot advance refs. It
returns a serializable, versioned recovery token before `execute` attempts
publication. Token fields: repository identity including placement, request
digest, operation ID, backend, and backend recovery binding. Credentials are
never serialized. A resumed session must reauthorize and verify placement.

The canonical execution order is:

1. Authorize the operation; validate request shape and current capabilities.
2. For a direct plan, acquire its renewable operation lease before any ref
   lease and check prior-attempt evidence under that lease; an unresolved
   attempt blocks replay. Acquire sorted per-ref leases, then global and
   repository GC writer fences; renew all leases until cleanup. Capture the
   ref snapshot under those leases.
3. Recheck expected OIDs and caller policy. Verify incoming Git connectivity,
   pointer dependencies, exact object IDs, and complete reconstruction terms.
4. Flush staged xorbs; upload immutable content, shards, packs and visibility
   evidence before journal publication. Preserve bounded streaming/backpressure.
5. Revalidate authorization/policy where the authority can change, expected
   state and lease health. Commit through `crab-write` and its namespace lease.
6. Record/reconcile the durable commit binding. Under generation ownership and
   GC fences, call `make_readable` if requested; reopen the read generation.
7. Drain catalog sessions, workers, heartbeats and every acquired lease. Cleanup
   failure cannot turn a known committed mutation into a rejection.

The SDK defaults to waiting for read readiness within the operation budget.
Read-only open never runs repair or writes storage. It returns `Indexing` until
a writer or authorized maintenance operation makes the catalog ready.

### Outcomes

```rust,ignore
enum MutationOutcome {
    Rejected { reasons: Vec<RefRejection> },
    Committed { receipt: CommitReceipt, readiness: Readiness },
    Indeterminate { recovery: RecoveryToken },
}
enum Readiness {
    Ready { generation: Generation },
    Pending,
}
```

`Result::Err` is used for failures known to occur before the commit attempt.
Once the visibility marker/CAS has been attempted, every return must preserve
whether the commit is known, rejected with proof, or unknown. A timeout after a
known commit returns `Committed { readiness: Pending }`. A lost marker response
without sufficient evidence returns `Indeterminate`. In an atomic request, a
policy/validation rejection rejects the whole batch.

Direct requests bind their request digest and a fresh operation nonce into a
plan ID and reuse existing plan-intent/receipt mechanics. Generalize the internal
Rust names currently prefixed `MirrorPlan` while preserving persisted keys and
version-1 fields. All mirror, CLI, managed, HTTP, and GC consumers must move in
the same phase. Add a renewable operation lease before ref leases for concurrent
execution of the same plan; reuse the coordination lease mechanism. A different
payload cannot reuse a token. An unresolved prior attempt blocks replay.

The request digest covers repository placement, ordered ref edits, expected
OIDs, commit/object identities, content digests, and write policy. Encode it
canonically with a versioned, domain-separated digest owned by `crab-metadata`.
The metadata encoding sorts JSON object keys recursively and preserves array
order, with separate versioned Blake3 domains for the request and nonce-bound
plan identity. Mirror plan files use format 2 with a retained operation nonce;
persisted intent and receipt formats remain version 1. The public token is a
recovery handle, not an authorization credential.

`reconcile` validates the bound receipt or historical commit evidence through
the owner crate. It may repair a missing receipt only with write authority.
Add a read-only receipt lookup beside the existing repairing resolver. Missing
evidence, a compacted marker, moved refs, or equal current OIDs never proves
rejection or historical success by itself. Return `Indeterminate` when proof is
insufficient; never infer rejection from `Option::None` in today's resolver.
`Client::reconcile_remote` also accepts the saved token directly: restarted
clients must not need a readable repository handle when remote `Client::open`
returns `Indexing`.
The repository method additionally verifies the handle's repository binding.
Preserve intents, receipts, and their proof roots under the existing GC contract;
phase 2 must prove retention across compaction, restart, and GC before writes ship.

SDK 1.0 does not promise unconditional exactly-once execution or unlimited
historical reconciliation. It promises no automatic replay of an uncertain
attempt, truthful outcomes, and retained recovery proof under the qualified
repository retention contract. Corrupt/deleted operator storage cannot be
reconstructed from matching refs.

Managed writes use existing prepare/finalize idempotency and server-side
verification. Reconcile using the same durable push ID and protocol-supported
repeated finalize request. If the deployed service cannot prove an outcome,
return `Indeterminate`; never publish directly to its canonical store.

`CommitReceipt` identifies its evidence authority: direct plan receipt, managed
push response, or Git receive-pack acknowledgement. Native smart HTTP cannot
provide a durable SDK receipt with today's server. A successful per-ref Git
acknowledgement proves that response's commit outcome; a lost response remains
Indeterminate and cannot be repaired by comparing current OIDs. Its recovery
token records attempted refs and request identity for diagnosis, without
promising historical attribution. Do not expose direct-plan replay semantics
on a native Git endpoint. Read readiness is Pending until independently proven.

## 6. Local workflow and executable contract

Full `local` requires explicit paths to Git and Crab. The SDK never installs
executables. Phase 4 adds a versioned `crab sdk-capabilities --json` handshake
containing protocol version, supported SDK integration features, storage/staging
format versions, and product build identity. Reject incompatible capabilities
before changing a repository. Product version strings alone are insufficient.
Git minimum is 2.30.9, matching the existing compatibility matrix; use only
commands available at that minimum and test it alongside current Git.

Extract `crab/src/git/process.rs` into the local feature of `crab-remote`.
Pass argv without a shell, explicitly set repository paths, bound diagnostics,
own the process tree, and drain it on cancellation. Git filter configuration is
a command string interpreted by Git: reuse and test the CLI quoting rules,
including spaces, quotes and non-ASCII executable paths on Windows and Unix.
Git hooks/external drivers require `local::ExecutionPolicy::Trusted`; the default
disables hooks and unrecognized executable drivers. Crab's validated filter
remains enabled. This policy is explicit in the builder and documented.

`Client::open(OpenOptions::local(...))` discovers ordinary and linked worktrees and validates configuration
without modifying it. A repository needing filter setup returns
`LocalSetupRequired`; `configure_local` is the explicit setup method on `Client`.
Clone performs that setup as part of creating the new repository. Configure
only the repository's local filter keys and hydration settings. Never alter
global Git config. The SDK transport uses libraries; the Crab executable is
used by Git's filter protocol, not as a JSON wrapper for SDK operations.

An SDK-created checkout works with ordinary Git after installing the standard
Crab distribution, including its remote helper. SDK fetch/push themselves do
not require that helper on PATH. This distinction must appear in the quickstart.

### Clone and fetch

Clone creates a uniquely owned sibling staging directory on the destination
filesystem. Require the destination to be absent; do not adopt or delete an
existing directory. Initialize Git, fetch objects, validate closure, install
packs/indexes, establish refs/HEAD/config, install Crab filters, checkout, and
apply hydration policy. Publish the destination with a no-clobber move. If that
primitive is unavailable on a supported OS, fail before starting clone; do not
replace an existing target. Cleanup removes only the owned staging directory.

Fetch resolves the current remote snapshot, computes refspec mappings, transfers
verified packs, and installs them before updating remote-tracking refs. Use
expected-old-value Git ref transactions; reject duplicate destinations and
namespace conflicts before updating refs. Never update the checked-out branch
through fetch. Tags follow the explicit tag policy; prune applies only to refs
owned by the selected fetch mapping. Write `FETCH_HEAD` with an owned temporary
file. Refs, shallow metadata and `FETCH_HEAD` are multiple files: use a durable
local fetch intent and recovery ordering, rather than claiming filesystem-wide
atomicity. New shallow boundaries must be valid for installed objects before
refs become visible. Old extra shallow boundaries are conservative during
recovery; do not expose a boundary that hides a required newly fetched parent.

A failed ref transaction may leave verified unreachable objects in the ODB;
that is acceptable. It must not leave refs pointing at missing objects.
No partial-clone promisor configuration is written in 1.0; see section 8.

### Stage, commit, status, checkout, and pull

Stage extracts the current add pipeline, including publication intent, paged
recipes, prepared payloads, Git index update and staged path-head reconciliation.
It must preserve a user's hydrated file while placing its exact pointer in the
index. Commit uses the existing index, explicit identity/message, and reports
the resulting OID. It does not implicitly stage files or push.

Status must recognize unchanged hydrated files as clean using the canonical
filter/staging behavior. Checkout uses Git plus the validated Crab filter;
default hydration is lazy. Dirty or untracked files that would be overwritten
cause a conflict. Dehydrate likewise refuses to replace modified content.

Pull runs SDK fetch first, then integrates the captured fetched OID using local
Git; do not invoke `git pull` and perform a second transport. Fast-forward-only
is default. Explicit merge/rebase use Git's conflict/index state and preserve
resumable operation state. `continue_integration` and `abort_integration` name
the active operation ID; neither may operate on an unrelated user operation.
No automatic stash, reset, force checkout, or hidden conflict resolution.
Crab/LFS pointer conflicts remain conflicts, never synthesized valid content.

Pull reports fetch completion, integration state, and hydration state separately.
If HEAD advanced and hydration failed, return the new OID plus pending/failed
paths; do not report that pull left HEAD unchanged. Cancellation retains the
same distinction. Abort restores only Git's owned integration state; user edits
made after a conflict must be checked before any overwriting action.

Push sends explicit refspecs, current-branch/upstream resolution, expected remote
OIDs, prepared staging and options to the shared transport. Uploading staged
content is mandatory before publishing its Git pointer dependencies. Push does
not implicitly fetch, merge, rebase, stage, or commit.

## 7. Management and authorization

`initialize_remote` targets one direct repository prefix and uses
`crab_write::initialize::initialize_repository`. Existing valid initialization
is adopted according to that owner contract; incompatible/nonempty prefixes
fail without destructive conversion. It never creates a bucket.

`managed::Managed` wraps `crab_auth::managed::client` methods with SDK-owned
inputs/results. Require service capabilities and authorization on each request.
Preserve service pagination, concurrency tokens and idempotency keys wherever
the corresponding endpoint defines them. Do not invent client-only CAS or
idempotency guarantees for endpoints that lack those contracts.

Managed read caches include authorized repository identity and placement.
Reauthorization/placement changes invalidate the handle before fetching new
remote data. Do not combine caches or continuation cursors across principals.
Previously returned bytes cannot be revoked; document that ordinary limitation.

The `crab-http-server` smart HTTP transport is supported through Git for its
qualified Git operations in the local profile. REST application management and
remote-only HTTP browsing are excluded from 1.0; they require a separately
versioned application API contract. Direct object-store and managed-store
remote-only reads remain mandatory.

## 8. SDK 1.0 compatibility matrix

"Required" means release-blocking SDK E2E proof. "Rejected" means deterministic
typed rejection with a test, before any unsupported mutation.

| Operation | Direct | Managed | Native smart HTTP local |
| --- | --- | --- | --- |
| Remote snapshot / raw Git / hydrated file reads | Required | Required with grants | Outside 1.0 |
| Remote commit/file edits / atomic refs | Required | Required through protected publish | Outside 1.0 |
| Open local, stage, commit, status, checkout | Required | Required | Required |
| Clone full / branch / lazy or hydrated | Required | Required | Required |
| Fetch refs / tags / prune | Required | Required | Required |
| Shallow clone, deepen, unshallow | Required | Required when service advertises support | Required within server capabilities |
| Pull FF / merge / rebase, continue / abort | Required | Required | Required |
| Push branch/tag create/update/delete | Required | Required under server policy | Required under server policy |
| Atomic multi-ref push | Required | Required if advertised; otherwise rejected | Required if advertised; otherwise rejected |
| Force-with-lease | Required for direct authorized writes | Required if allowed; otherwise rejected | Rejected by current Crab HTTP server |
| Push dry-run | Validate plan; no remote writes/leases | Local validation only; report server validation unavailable | Git/server advertised behavior; never claim admission reservation |
| Partial clone filters / lazy Git object fetching | Rejected | Rejected | Rejected by SDK 1.0 |
| Recursive submodules, sparse checkout, linked-worktree creation | Rejected | Rejected | Rejected |
| Open existing linked worktree | Required | Required | Required |
| SHA-256 repository format | Rejected | Rejected | Rejected |
| Direct initialization / managed lifecycle | Required initialization | Required list/create/rename/archive/restore | Outside 1.0 |

Shallow requests to a service lacking the capability fail; no silent full clone.
Partial-clone support already present elsewhere in Crab does not automatically
qualify this SDK: future promisor fetching needs an executable/config contract.
Existing partial/sparse/submodule worktrees may be inspected where safe but
mutations requiring those semantics return `UnsupportedCapability`.

## 9. Executable phases

Execute phases in numerical order. Each phase is a mergeable change set with
the listed prerequisites, implementation, and evidence. A phase is incomplete
if a required cell is skipped, ignored, or demonstrated only through a mock.
Do not publish unsupported methods containing placeholders. Features become
public only when their owning phase passes.

### Phase 0 — Freeze contracts and build qualification inputs

Context: the package is absent; current release/feature claims describe other
consumers. Prerequisite: this design is the implementation plan of record.

Work:

1. Create `crab/docs/architecture/sdk-capabilities.json` from section 8 with
   operation, backend, status, phase, and required test-name fields; generate its
   Markdown view. This is a new SDK matrix, not a change to the Git baseline.
2. Add `crab/scripts/verify_sdk_capabilities.py` to reject missing/duplicate
   cells, unresolved test references, and unsupported statuses. Tests must be
   discoverable once their phase activates; future phases remain `planned`.
3. Record the exact Rust toolchain used by current CI. Compute the highest
   declared `rust-version` in the SDK's intended locked dependency closure and
   compile-probe that toolchain using a disposable consumer manifest importing
   the current owner crates under the external Workspace volume; increase to
   the first passing stable version
   if an undeclared dependency minimum fails. Record that exact MSRV in the SDK
   manifest when phase 1 creates it. Do not assume edition 2024 alone sets MSRV.
4. Define release profiles: Linux x86_64, macOS arm64, Windows x86_64; Git 2.30.9
   and current; RustFS for all OS cells; live S3/GCS/Azure on Linux; managed
   service on Linux plus local workflow smoke on macOS/Windows. Unsupported
   architectures remain outside the initial release promise.
5. Record a private test-prefix convention and fixtures for an empty repo,
   branch/tag namespace conflicts, 10,000 ordinary files, a 1 GiB Crab file,
   a 32 MiB LFS file, and non-UTF-8 paths on Unix.

Acceptance:

- Matrix validator passes; each required row has an owning phase and concrete
  test name. No existing capability inventory is relaxed.
- MSRV selection and exact toolchain versions are recorded with compile output.
- Fixture specification contains deterministic content seeds/digests, scope
  cleanup rules, and no credentials. Qualification repositories stay external.

### Phase 1 — Public read SDK

Context: remote-git already supplies bounded pinned reads, but callers manually
manage resources. Prerequisite: phase 0.

Work: add the workspace package, SDK errors/value types, features, Client,
`Repository`, `remote::Remote`, `remote::Snapshot`, direct provider configuration, cancellation, bounded
progress and `close`. Implement all section 4 read methods; add `content` for
Crab/LFS reconstruction. Create compiling examples `remote_read.rs` and
`remote_archive.rs`. Add owner-default and feature-closure checks.

Extend the read owners where their present interfaces cannot express the SDK
contract: aggregate operation limits must govern deadlines, cache-hit charging,
accessors and batch admission on the same pinned snapshot. Hydrated range
streams must use a bounded writer and the caller's cancellation/deadline;
buffering a caller-sized range or starting detached reconstruction is not a
streaming implementation. Preserve existing raw Git path and VFS range
semantics at their owner boundary, with SDK validation applied before I/O.
LFS stream verification must not publish remote receipts from SDK reads. Keep
receipt creation at authorized write/maintenance boundaries; bind any separate
verification and delivery reads to the same object version, or verify delivered
full-file bytes through EOF. Preserve bounded memory and range integrity.

Acceptance:

- `remote_read` reads exact Git bytes and hydrated bytes through a real RustFS
  repository after the fixture's source checkout has been removed.
- Branch movement does not change an existing snapshot; refresh returns new
  state. Opening a lagging catalog returns Indexing with zero storage writes.
- Unix byte paths round-trip, pagination stays generation-bound, raw pointer
  reads differ from reconstructed reads, corruption and invalid ranges error.
- Raw, Crab and LFS reads attempt zero remote writes. LFS replacement between
  verification and delivery cannot return unverified bytes as a successful
  full-file or range read; premature EOF and same-size corruption fail.
- Stream EOF, early close, drop, timeout and Client close leave no owned sessions
  or tasks. Limits fail predictably on cold and warm caches.
- Storage-request and fetched-byte limits charge Store retries, provider HTTP
  retries, each listing page, and response-body chunks. Admission rejection
  stops before the next physical request.
- Default/remote/remote+content compile separately; no CLI/server/VFS normal
  dependency; remote without content excludes the hydration stack.

### Phase 2 — Shared publication and recovery

Context: journal commit exists, but complete lifecycle and durable attribution
are caller responsibilities. Prerequisite: phase 1.

Work: add `crab-remote` publication feature; extract shared validation/lifecycle
from CLI and HTTP callers; generalize existing plan attribution as specified in
section 5. Add read-only reconciliation, operation serialization, readiness
results and owned cleanup. Update CLI/mirror/HTTP callers and metadata/GC tests
together. Protected receive retains its authorization boundary and consumes
only mechanics appropriate to its existing manifest authority.

Acceptance:

- CLI and HTTP publication use the extracted path; original duplicated
  lifecycle implementations are removed. HTTP build excludes local/process
  features. Protected receive cannot be invoked as client-authorized commit.
- Fault tests before marker, lost marker reply, after marker, receipt failure,
  catalog failure, lease loss and process termination produce the outcomes in
  section 5. Fresh-process reconciliation works after refs advance again.
- Same-token concurrent execution creates at most one committed attempt;
  payload mismatch rejects; unresolved attempt never replays automatically.
- Concurrent ref prefix conflicts reject atomically; global/repo fences and
  all leases drain. Existing mirror receipt fixtures remain readable.
- Compaction plus scoped GC retains required receipt proof and referenced
  content; a missing proof returns Indeterminate rather than invented rejection.
- Run existing journal/catalog/generation tests, HTTP receive/fault RustFS
  tests, and CLI push/reconstruction/mirror regressions through their callers.

### Phase 3 — Remote writes and direct management

Context: safe publication now has a shared entry point. Prerequisite: phase 2.

Work: expose initialize, prepared commits, streamed file edits, prepared ref
batches, receipts and reconciliation. Reuse Git object encoding, receive-plan
validation and staged payload preparation; extract required preparation code
from CLI without creating a worktree-dependent remote writer.

Acceptance:

- `remote_edit.rs` initializes an isolated prefix, creates a commit with text
  and large-file content, tags it, changes and deletes refs; an independent Git
  clone and remote SDK read verify exact OIDs, modes and hydrated digests.
- Run with Git and Crab absent from PATH; no local Git ODB or checkout created.
- A second writer invalidates expected OIDs; the first writer publishes no
  partial ref batch. Existing-object tags and tag-only unborn HEAD work.
- Cancelled/oversized uploads never expose missing dependencies; retries use
  the recovery contract. Existing nonempty incompatible prefix is unchanged.

### Phase 4 — Local runtime, shared configuration, and fetch/clone

Context: current clone/pull combine product output, Git processes and filters.
Prerequisite: phase 3. This phase resolves the local executable dependency.

Work: extract process ownership, configuration projection, filter installation,
pack transfer/install and worktree discovery. Implement capabilities handshake,
configure/open local, clone, fetch, shallow recovery and durable local intent.
Move CLI consumers onto those services in the same changes. Implement local
execution trust policy and native HTTP Git transport with bounded output.

Acceptance:

- SDK clone succeeds without `git-remote-crab` on PATH using explicitly supplied
  Git/Crab paths; direct transfer enters shared Rust services. Missing or
  incompatible tools fail before creating/changing the destination.
- Full/shallow/deepen/unshallow, tags and prune match Git fixture expectations;
  `git fsck` succeeds and independent checkout content matches.
- Destination collision leaves it byte-identical; cancellation/crash recovery
  leaves no refs to absent objects. Injection between each fetch metadata write
  proves recoverable refs, shallow state and FETCH_HEAD.
- Open and fetch work in linked worktrees without changing sibling HEAD/index;
  external Git ref/index races reject or recover without overwriting user state.
- Explicit config overrides and repository file precedence match CLI fixtures;
  paths with spaces/quotes work on all three OS profiles. Untrusted hooks do
  not execute; trusted hooks do execute and errors reach the caller.

### Phase 5 — Complete local editing, pull, and direct push

Context: clone/fetch is useful but insufficient for a developer SDK.
Prerequisite: phase 4.

Work: extract stage/status/hydration services; add commit, checkout, pull
FF/merge/rebase, continue/abort, direct prepared push and dry-run. CLI commands
use the same services. Create `local_round_trip.rs` and `resolve_conflict.rs`.

Acceptance:

- SDK clone -> modify text/large file -> stage -> commit -> push -> independent
  clone yields exact content, valid Git OIDs and clean hydrated status.
- Crab CLI and SDK alternate stage/push/hydrate in the same checkout; staged
  recipes, cache and pointer interpretation remain identical.
- Divergent history: FF-only leaves HEAD unchanged; merge/rebase produce Git-
  equivalent parents/content; conflicts expose paths/stages and continue/abort
  work. No implicit stash or push integration occurs.
- Dirty/untracked overwrite cases preserve user data. Pull hydration failure
  reports committed HEAD separately and can resume hydration.
- Force-with-lease rejects stale expected OIDs, atomic push rejects all refs on
  one invalid edit, and dry-run makes no remote writes or remote lease objects.
- 1 GiB content streams without file-sized buffering; interruption during add
  publication recovers index/path-head/recipe consistency.

### Phase 6 — Managed backend and repository lifecycle

Context: existing managed APIs provide grants and protected publishing.
Prerequisite: phase 5; the test service must implement its advertised contract.

Work: enable managed profile resolution, scoped credentials, prepared/finalized
push and recovery, repository listing/create/rename/archive/restore. Add
`managed_round_trip.rs`. Keep service calls in auth owners; SDK maps outcomes.

Acceptance:

- Real service fixture: create -> clone -> commit -> protected push -> read ->
  rename -> archive -> restore. A fresh client observes each durable state.
- Expired grant refresh works according to auth owner policy; revocation before
  finalization rejects without ref publication. No read token gains write scope.
- Lost finalize response reconciles by the same push ID; restarted client
  never bypasses server commit. Unsupported service capabilities fail explicitly.
- Cross-principal/placement cache isolation and permission-denied errors pass.
  A renamed repository is resolved by service response; no client URL guessing.
- HTTP local Git round trip uses the actual Crab HTTP server; its force-push
  rejection is represented correctly. No HTTP REST administration is implied.

### Phase 7 — Qualification and public documentation

Context: functional slices exist; cross-system proof and release ergonomics
remain. Prerequisite: phase 6.

Work: add `.github/workflows/sdk.yml` for features, MSRV/current Rust, docs,
clippy and package consumer tests. Add a dedicated SDK qualification runner for
section 8 using real Git and private RustFS/provider/service prefixes. Store
machine-readable reports as CI artifacts. Add public docs under
`packages/web/content/docs/sdk/` and navigation following adjacent conventions.

Acceptance:

- Every required cell passes on the section 9 phase-0 profiles. No skipped or
  ignored test counts as evidence. Each report records source SHA, feature set,
  tool versions, backend, fixture digest, timing, RSS and terminal state.
- All mandatory corruption, cancellation, lost-response, concurrency, restart,
  and scoped-GC cases pass; Git fsck and independent byte comparison pass.
- Controlled Linux runner: 1 GiB streaming read peak RSS <= 512 MiB; local
  clone/stage/push peak RSS <= 2 GiB. Across five cold/warm runs against the same
  shared-core baseline, median read requests/bytes and wall time regress <= 10%
  from facade overhead. Publish absolute timings and fixture/runner details;
  if the baseline violates an absolute limit, fix before promoting the cell.
- Quickstart, local prerequisites, async shutdown, pointer/content distinction,
  conflict recovery, credentials and indeterminate push examples compile.
- Public docs are served at `https://crab.build/docs/sdk`; run web typecheck,
  lint, tests and link validation for their actual changed surface.

### Phase 8 — Package and release

Context: crates.io cannot resolve private path-only dependencies.
Prerequisite: phase 7; publishing authorization is separate from implementation.

Work: compute the exact transitive Crab dependency closure for supported feature
sets; promote those packages with versioned workspace dependencies and retain
workspace paths for local development. Supporting implementation packages use
a coordinated version and exact sibling version requirements; SDK public API
has its own semver. Verify registry ownership/name availability before release.
Publish support crates in dependency order, then SDK. No vendored source copies
or Git-only dependencies in registry packages.

Acceptance:

- Package verification and an external consumer of packaged artifacts pass
  outside this workspace for every supported feature profile. No accidental
  workspace file, local patch, CLI library, or server normal dependency.
- MSRV/current stable, rustdoc with warnings denied, public API snapshot and
  semver checks pass. Required licenses/readme/examples are included.
- Supporting crates and compatible Crab executable are published before the
  SDK release that requires them. The capabilities handshake is tested against
  supported and rejected executable builds.
- Pre-1.0 releases are explicitly labeled preview with completed cells only.
  SDK 1.0 requires every phase above and every Required cell; no absent backend
  is described as supported. Release records contain immutable evidence links.

## 10. Verification commands and completion records

### Required test targets

These are new test targets to create in their owning phases. Names identify
user-visible contracts and are the initial matrix validator references.

| Phase | Target path relative to repository root | Required cases |
| --- | --- | --- |
| 1 | `crates/crab-sdk/tests/remote_read.rs` | `snapshot_stays_pinned`, `raw_and_hydrated_bytes_are_distinct`, `read_only_open_never_repairs`, `byte_paths_round_trip` |
| 1 | `crates/crab-sdk/tests/lifecycle.rs` | `close_drains_dropped_operations`, `stream_close_reports_integrity_failure`, `warm_cache_respects_limits` |
| 2 | `crates/crab-remote/tests/publication.rs` | `lost_marker_reply_preserves_outcome`, `concurrent_same_plan_commits_once`, `compaction_and_gc_preserve_receipt_proof`, `namespace_conflict_rejects_batch` |
| 3 | `crates/crab-sdk/tests/remote_write.rs` | `remote_edit_round_trip_without_git`, `stale_ref_batch_changes_no_refs`, `initialization_preserves_foreign_content` |
| 4 | `crates/crab-sdk/tests/local_fetch.rs` | `clone_uses_explicit_tools_without_helper_path`, `clone_preserves_existing_destination`, `fetch_recovers_each_metadata_boundary`, `shallow_deepen_unshallow_round_trip`, `linked_worktree_preserves_sibling` |
| 5 | `crates/crab-sdk/tests/local_edit.rs` | `stage_commit_push_round_trip`, `cli_and_sdk_share_staging`, `hydrated_status_is_clean`, `dirty_checkout_preserves_bytes` |
| 5 | `crates/crab-sdk/tests/local_pull.rs` | `ff_only_preserves_diverged_head`, `merge_and_rebase_resume_conflicts`, `hydration_failure_reports_new_head`, `abort_preserves_later_user_edits` |
| 6 | `crates/crab-sdk/tests/managed.rs` | `managed_lifecycle_round_trip`, `revoked_push_never_commits`, `lost_finalize_reuses_push_id`, `placement_change_invalidates_cached_access` |
| 6 | `crates/crab-sdk/tests/http_git.rs` | `native_http_round_trip`, `server_force_rejection_is_typed`, `lost_acknowledgement_is_indeterminate` |
| 7 | `crab/scripts/qualify_sdk.py` | Runs real-backend targets and independent Git/byte checks; emits the required report fields |
| 8 | `crates/crab-sdk/tests/package-consumer/` | Separate workspace for minimal, remote/content, local and managed published-package consumers |

Live-backend tests may be opt-in locally. The qualification runner must select
them explicitly (including `--ignored` when necessary), verify their executed
count, and fail on missing credentials/infrastructure. A skipped live test is
never a passing matrix cell. Names above are mandatory minimums; additional
tests must prove distinct behavior rather than repeat implementation branches.

### Commands

Commands below become valid as their packages/tests are introduced. They are
required implementation checks, not a claim that the current tree can run them.
Use a unique external target directory for this checkout. Verify the Workspace
volume is mounted and writable first; stop if unavailable. Full matrix/cloud
checks run in CI or a dedicated test environment.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-2485-sdk" cargo check -p crab-sdk --locked --no-default-features
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-2485-sdk" cargo test -p crab-sdk --locked --no-default-features --features remote
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-2485-sdk" cargo test -p crab-sdk --locked --no-default-features --features remote,content,write
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-2485-sdk" cargo test -p crab-sdk --locked --no-default-features --features local,managed
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-2485-sdk" cargo clippy -p crab-sdk -p crab-remote --locked --all-targets --all-features -- -D warnings
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-2485-sdk" cargo doc -p crab-sdk --locked --all-features --no-deps
python3 crab/scripts/verify_sdk_capabilities.py
python3 crab/scripts/check-sdk-features.py
```

Feature CI additionally builds `managed` alone and with `remote`, `remote,content`
and `write` to prove additive features. Each extracting phase tests every touched
owner plus direct callers, not just `crab-sdk`. SDK package tests use a dedicated
consumer workspace; CI is responsible for building the matching filter binary.
Local executable installation uses the existing `make install` workflow.

Each phase closes with a record containing source SHA, changed owners/callers,
commands and CI URLs, acceptance-cell outcomes, measured limits where applicable,
and removed duplicate paths. A failed prerequisite blocks dependent phases;
report the exact failed criterion rather than changing the matrix to green.
The sequence ends with a usable package and qualified user workflows, not merely
with a library that compiles.

## 11. Dependency contract references

- [Git update-ref](https://git-scm.com/docs/git-update-ref): expected-old-value
  checks and multi-ref transaction protocol. Validate the minimum supported Git
  version; do not depend on newer symbolic-ref or partial-batch extensions.
- [Git attributes](https://git-scm.com/docs/gitattributes): clean/smudge/process
  filters and required filter behavior underpin the local executable contract.
- [Cargo dependency locations](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#multiple-locations):
  registry publication requires versioned dependencies even when workspace
  development uses paths.
- `Cargo.toml` pins `gix` 0.83.0; its `push` module defines configuration values,
  not a complete client push implementation. Reuse Crab transport and Git
  porcelain as specified here; no Git engine replacement is required.
