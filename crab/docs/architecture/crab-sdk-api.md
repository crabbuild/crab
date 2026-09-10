# Crab SDK API

This document defines the public `crab-sdk` API: repository access, task
namespaces, lifecycle operations, feature boundaries, recovery, and performance
contracts. The behavior, safety, and qualification requirements in
[the SDK delivery plan](crab-sdk.md) are authoritative.

## 1. API model

Applications open every existing repository through one `Client::open` method
and receive one `Repository` handle. `OpenOptions` selects remote or local mode:

```rust,ignore
let repository = client
    .open(OpenOptions::remote(locator))
    .await?;
let snapshot = repository
    .remote()?
    .snapshot(Revision::branch("main")?)
    .await?;

let repository = client
    .open(OpenOptions::local("/work/models"))
    .await?;
let status = repository.local()?.status().await?;
```

`Repository` is a concrete facade over a private backend enum. It holds the
shared repository lifetime and provides one borrowed, mode-specific interface:

```rust,ignore
pub struct Repository { /* private */ }

#[non_exhaustive]
pub enum RepositoryMode {
    Remote,
    Local,
}

impl Repository {
    pub fn mode(&self) -> RepositoryMode;
    pub fn remote(&self) -> Result<remote::Remote<'_>>;
    pub fn local(&self) -> Result<local::Local<'_>>;
}
```

`remote()` and `local()` perform no I/O. Selecting the wrong interface returns
`ErrorKind::UnsupportedCapability`. The borrowed interfaces cannot outlive the
repository handle and add no second resource owner.

Direct object storage, managed-service resolution, and native Git HTTP are
transport or authorization choices. They are not repository modes:

- A direct or managed locator can open a remote repository.
- A direct, managed, or HTTP locator can be cloned into a local repository.
- Managed repository administration is a client service.
- Local mode means an on-disk Git worktree. Its fetch, pull, and push operations
  may still use the network.

Repository creation stays explicit. `initialize_remote`, `clone_local`, and
`configure_local` create or change durable state, so they do not become variants
of `open`.

| Task | API entry point |
| --- | --- |
| Open an object-store or managed repository | `client.open(OpenOptions::remote(locator))` |
| Open an existing Git worktree | `client.open(OpenOptions::local(path))` |
| Initialize a remote repository | `client.initialize_remote(locator, head)` |
| Clone into a new local worktree | `client.clone_local(locator, destination, options)` |
| Configure an existing local worktree | `client.configure_local(path)` |
| Administer managed repositories | `client.managed()` |
| Recover a remote mutation | `client.resume_remote(token, scratch)` or `client.reconcile_remote(token)` |
| Recover a local push | `client.resume_local_push(token, scratch)` or `client.reconcile_local_push(token)` |

## 2. Public namespaces

The crate root contains only the types needed to construct a client, identify a
repository, select an interface, and handle errors. Task-specific types live in
the modules below.

The modules form the public task-oriented interface:

| Namespace | Public responsibility |
| --- | --- |
| `remote` | Remote interface, immutable snapshots, refs, Git reads, hydrated reads, archives, and remote publication |
| `remote::write` | Commit preparation, streamed file edits, atomic ref updates, mutation outcomes, receipts, and recovery tokens |
| `local` | Local interface, tool policy, clone/fetch/status/edit/integration/content/push workflows, and local recovery types |
| `managed` | Managed-service configuration, repository lifecycle administration, pages, records, and states |
| `storage` | Direct provider configuration and shared content-cache configuration |
| `operation` | Per-operation options, read limits, cancellation, progress, request builders, and operation identity |

The export inventory is explicit. Items may live in private source
modules, but rustdoc exposes them only at these paths:

| Namespace | Exported types |
| --- | --- |
| crate root | `Client`, `ClientBuilder`, `Repository`, `RepositoryMode`, `OpenOptions`, `RepositoryLocator`, `Revision`, `ObjectId`, `HashAlgorithm`, `GitPath`, `WritePolicy`, `Error`, `ErrorKind`, `Result` |
| `remote` | `Remote`, `Snapshot`, `Capability`, `Reference`, `References`, `Commit`, `Signature`, `SignatureHeader`, `TreeEntry`, `EntryMode`, `HistoryTraversal`, `Page`, `PageCursor`, `PageRequest`, `Diff`, `DiffClassification`, `DiffHunk`, `Blame`, `BlameRange`, `ContentMode`, `ContentStream`, `ArchiveEvent`, `ArchiveStream` |
| `remote::write` | `PreparedMutation`, `CommitIdentity`, `CommitOptions`, `FileEdit`, `RefUpdate`, `RefBatch`, `MutationOutcome`, `Readiness`, `CommitReceipt`, `RefRejection`, `RecoveryToken` |
| `local` | `Local`, `Options`, `Tools`, `Configuration`, `ExecutionPolicy`, `Snapshot`, `CloneOptions`, `FetchDepth`, `FetchOptions`, `FetchOutcome`, `Status`, `StatusEntry`, `StageOutcome`, `CommitOptions`, `CheckoutOptions`, `HydrationState`, `PullMode`, `PullOptions`, `PullOutcome`, `IntegrationId`, `IntegrationKind`, `ConflictState`, `PushOptions`, `PushRefspec`, `PreparedPush`, `PushOutcome`, `PushRecoveryToken` |
| `managed` | `Managed`, `Options`, `Page`, `RepositoryInfo`, `RepositoryState` |
| `storage` | `DirectStoreOptions`, `S3Options`, `GcsOptions`, `AzureOptions`, `ContentCache` |
| `operation` | `Request`, `Options`, `ReadOptions`, `ReadLimits`, `Cancellation`, `Progress`, `ProgressEvent`, `ProgressReceiver`, `ProgressUpdate`, `Id` |

Each item retains its feature gate from section 9. Future additions must belong
to one of these responsibilities and pass the same public API review. Owner-crate
implementation types stay private.

`Error`, `ErrorKind`, and `Result` live at the root because every workflow uses
them. Core identifiers live at the root because they cross remote, local,
managed, and recovery boundaries. `WritePolicy` also lives at the root because
remote mutations and local pushes enforce the same policy contract.

## 3. Client and repository lifecycle

### Client construction

The client is cheap to clone and owns shared bounded runtimes, credential
resolution, operation tracking, and cleanup. Configuration groups related
values into task options:

```rust,ignore
let client = Client::builder()
    .direct_store(storage::DirectStoreOptions::s3(s3))
    .content_cache(storage::ContentCache::new(cache_root, cache_bytes)?)
    .local(local::Options::new(local::Tools::new(git, crab)?)
        .with_execution_policy(local::ExecutionPolicy::Untrusted))
    .managed(managed::Options::new(token_cache)?)
    .build()?;
```

`local::Options` groups the tool paths and execution policy. It requires
explicit tools because local workflows depend on compatible Git and Crab
executables. A client that uses only the remote interface does not require
`local::Options` or either executable.

Builder precedence is deterministic: explicit builder values win over
explicitly loaded configuration, provider defaults run only when selected, and
the SDK does not mutate process environment, current directory, global Git
configuration, or tracing.

### Opening repositories

`OpenOptions` is a validated, closed selection of an existing repository mode:

```rust,ignore
pub struct OpenOptions { /* private */ }

impl OpenOptions {
    pub fn remote(locator: RepositoryLocator) -> Self;

    #[cfg(feature = "local")]
    pub fn local(path: impl Into<PathBuf>) -> Self;
}

impl Client {
    pub fn open(
        &self,
        options: OpenOptions,
    ) -> operation::Request<'_, Repository, operation::Options>;
}
```

The constructors do not perform filesystem or network I/O. `Client::open`
validates mode-specific configuration before mutation or storage access. Local
open canonicalizes the worktree and Git common-directory identities and performs
interrupted-operation inspection before returning the handle.
Remote open captures an immutable repository generation and never repairs it.

`Repository` owns exactly one existing mode:

```rust,ignore
pub struct Repository {
    backend: RepositoryBackend,
}

enum RepositoryBackend {
    #[cfg(feature = "remote")]
    Remote(RemoteRepositoryOwner),
    #[cfg(feature = "local")]
    Local(LocalRepositoryOwner),
}
```

The enum and owner types are private. Public callers do not match backend
variants, depend on owner crates, or select direct versus managed execution.
Dispatch occurs once when an interface starts an operation. Streaming loops and
content chunks do not dispatch through the enum.

### Creating repository state

State-creating methods live on `Client`. Clone returns an opened `Repository`;
initialization and configuration return their smallest durable result because a
new remote repository may still need indexing and local configuration does not
need to open a long-lived handle:

```rust,ignore
impl Client {
    #[cfg(feature = "write")]
    pub fn initialize_remote(
        &self,
        locator: RepositoryLocator,
        head: &str,
    ) -> operation::Request<'_, (), operation::Options>;

    #[cfg(feature = "local")]
    pub fn configure_local(
        &self,
        path: impl Into<PathBuf>,
    ) -> operation::Request<'_, local::Configuration, operation::Options>;

    #[cfg(feature = "local")]
    pub fn clone_local(
        &self,
        source: RepositoryLocator,
        destination: impl Into<PathBuf>,
        options: local::CloneOptions,
    ) -> operation::Request<'_, Repository, operation::Options>;
}
```

`initialize_remote` cannot accept an HTTP locator. `clone_local` may accept direct,
managed, or HTTP locators, subject to compiled features and advertised service
capabilities. Each method validates unsupported combinations before durable
mutation.

### Shutdown

Call `Client::close().await` to stop admission, cancel and drain tracked workers,
close read owners and metadata databases, and report the first unobserved cleanup
failure. Repository and interface drops do not promise asynchronous cleanup
after runtime or process termination.

## 4. Remote interface

`remote::Remote<'a>` is a small borrowed interface. It owns validation,
capability checks, and public request construction; owner crates retain the
mechanics.

```rust,ignore
let remote = repository.remote()?;
let capabilities = remote.capabilities();
let refs = remote.refs().await?;
let snapshot = remote.snapshot(Revision::branch("main")?).await?;
```

Its public methods are:

| Method | Contract |
| --- | --- |
| `capabilities` | Synchronous implemented operation families; no authorization promise or I/O |
| `refs` | Refs and symbolic HEAD from the captured generation |
| `snapshot` | Immutable repository read view for a branch, tag, or reachable commit |
| `refresh` | Open a new remote generation and return a new `Repository` handle |
| `prepare_commit` | Build a remote commit and streamed file edits without a worktree |
| `prepare_ref_update` | Prepare an atomic ref batch against expected old object IDs |
| `reconcile` | Verify recovery evidence against this repository binding |

`remote::Snapshot` exposes commit metadata, paginated tree/history, diff,
blame, raw blob bytes, hydrated file streams, and archives. Remote reads retain
generation pinning, authorization roots, byte-path validation, aggregate request
budgets, bounded streaming, integrity finalization, and explicit close behavior.

Remote publication types live in `remote::write` because they are meaningful
only for remote state:

```rust,ignore
use crab_sdk::remote::write::{
    CommitIdentity, CommitOptions, FileEdit, MutationOutcome, RefBatch,
    RefUpdate,
};
use crab_sdk::WritePolicy;

let prepared = repository
    .remote()?
    .prepare_commit(options, edits, scratch)
    .await?;
persist(prepared.recovery_token().to_json()?)?;
let outcome = prepared
    .execute()
    .with_options(operation::Options::default())
    .await?;
```

Preparation, durable tokens, truthful `Committed`/`Rejected`/`Indeterminate`
outcomes, read readiness, lease ordering, GC fencing, and finalize recovery use
the contracts in the SDK delivery plan.

## 5. Local interface

`local::Local<'a>` controls one opened Git worktree. The public name is local;
documentation uses the precise domain terms worktree and working tree when
describing on-disk state.

```rust,ignore
let local = repository.local()?;
let status = local.status().await?;
local.stage(paths).await?;
local.commit(local::CommitOptions::new(author, committer, message)?).await?;
local.pull(local::PullOptions::fast_forward_only()).await?;
```

The interface contains only operations that require or inspect an on-disk Git
worktree: path and common-directory identity, local snapshots, status, fetch,
stage, commit, checkout, pull, conflict continue/abort, hydrate, dehydrate,
prefetch, and prepared push. It preserves local serialization, filesystem
leases, Git locks, HEAD/index revalidation, user-file protection, and
interrupted-operation recovery.

`local::Snapshot` supplies offline local history. It is distinct from
`remote::Snapshot`: the former reads a local object database while the latter is
an immutable remote repository read view. A common snapshot trait would expose
their least-common denominator and make async trait behavior public, so the two
concrete types remain separate.

## 6. Managed administration

Managed service access is client-scoped because listing and creating
repositories do not operate on an already opened repository:

```rust,ignore
let managed = client.managed().await?;
let page = managed.list(organization, cursor, limit).await?;
let created = managed.create(organization, name).await?;
```

```rust,ignore
impl Client {
    pub fn managed(
        &self,
    ) -> operation::Request<'_, managed::Managed, operation::Options>;
}
```

`managed::Managed` provides `list`, `create`, `rename`, `archive`, and
`restore`. It owns a cheap client clone and the validated service connection, so
it shares credential resolution and the operation runtime without reconnecting
for each call. Its methods return `operation::Request` builders. Managed
repository records expose logical identity and state, never physical placement
or credentials.

A managed locator still opens through `Client::open(OpenOptions::remote(...))`
or acts as the source for `Client::clone_local`. The caller does not receive a third
managed repository handle.

## 7. Operation and recovery model

Every admitted repository, administration, and recovery operation uses the same
request builder:

```rust,ignore
let snapshot = client
    .open(OpenOptions::remote(locator))
    .with_options(operation::Options::default()
        .with_timeout(Duration::from_secs(30))?
        .with_cancellation(cancel))
    .await?;
```

`operation::Request<'a, T, O>` is awaitable and accepts its operation
options through `with_options`. Repository methods return the builder instead of
adding an options parameter to every common call. Remote read methods use
`operation::ReadOptions`; other work uses `operation::Options`. Consuming
`PreparedMutation::execute` and `PreparedPush::execute` also return request
builders rather than taking operation options directly.

Recovery is client-owned when an operation may need to resume before a
repository can be opened. Names state the recovery family explicitly:

```rust,ignore
client.resume_remote(token, scratch).await?;
client.reconcile_remote(token).await?;
client.resume_local_push(token, scratch).await?;
client.reconcile_local_push(token).await?;
```

These methods return `operation::Request` builders, so callers apply
`operation::Options` through `with_options` like every other asynchronous SDK
operation. An opened remote interface also exposes `reconcile` so it can verify
that a token belongs to the handle. Remote mutation and local push tokens are
separate versioned types because they bind different evidence and replay
contracts. The API does not introduce a broad token enum or infer recovery
outcomes from current refs.

## 8. Errors and capability behavior

All errors use root `Error` and stable `ErrorKind`. Sources, operation identity,
bounded redacted context, and cleanup errors are inspectable.

The facade enforces one deterministic rule: asking a repository for the wrong
interface returns `UnsupportedCapability` synchronously and performs no I/O.
Other unsupported combinations are rejected before mutation:

- HTTP locator passed to remote open;
- missing `local` feature or local options for a local operation;
- write operation on a store without qualified conditional publication;
- operation family absent from remote capability metadata;
- SHA-256 repository mutation in SDK 1.0;
- unsupported Git protocol extensions or worktree mutations.

Capabilities report implemented mechanisms for the opened backend. They do not
predict authorization, branch policy, transient service state, or credential
validity. Those are operation outcomes.

## 9. Features and dependency isolation

The feature graph is:

| Feature | Public additions | Dependency rule |
| --- | --- | --- |
| default | Value, error, storage option, and builder types | No Tokio, Git process, auth, or provider runtime |
| `remote` | `Client`, remote open/read interface, operation module | No Git or Crab executable dependency |
| `content` | Hydrated remote reads and content cache | Implies `remote` |
| `write` | Remote initialization and `remote::write` | Implies `remote` |
| `local` | Local options, open/clone/configure, and local workflows | Implies `write` and `content` |
| `managed` | Managed locator resolution and administration | Implies `remote` |

Modules, exports, examples, tests, and dependencies use the same gates. A
remote-only consumer never compiles local process code. Namespaces organize the
public API; they do not weaken Cargo feature isolation or create dummy types for
disabled features.

## 10. Internal ownership and performance

The public facade is an adapter with a small, deep interface. It validates mode
selection and constructs SDK requests. It does not forward storage or Git owner
types into public signatures.

Implementation ownership:

```text
crab-sdk::Repository / remote::Remote / local::Local
  -> crab-remote orchestration
  -> crab-remote-git + crab-read for immutable remote reads
  -> crab-write + crab-staging + crab-coordination for publication
  -> crab-auth + crab-auth-store for managed resolution and protected push
  -> crab-git and owned processes for local worktree operations
```

The facade reuses existing owner objects. It must not reopen a repository
when selecting an interface, copy configuration into a second runtime, or wrap
streams in per-chunk dynamic dispatch. One enum match per operation is allowed.
The following performance contracts are release gates:

- no additional storage requests or transferred bytes for the same remote read;
- no additional full-file buffer for streaming reads, archives, edits, fetch, or
  push;
- current request budgets, backpressure, cancellation, and cleanup behavior;
- 512 MiB large-file resident-memory bound from the SDK delivery plan;
- benchmark ratios and request/byte accounting stay within the configured CI
  thresholds.

## 11. Qualification

The public API is qualified only when all of the following are true:

- every SDK acceptance test enters through `Client::open`,
  `Client::clone_local`, `Client::configure_local`, or `Client::initialize_remote`;
- remote and local interface mismatch tests prove typed, synchronous, side-effect
  free failure;
- direct, managed, and HTTP behavior retains every applicable cell in
  [the capability inventory](sdk-capabilities.md);
- rustdoc, examples, guides, package consumers, and the checked API snapshot
  expose the inventory in section 2;
- minimal, remote, content, write, local, managed, and combined feature builds
  pass;
- compile-fail coverage proves disabled features do not leak public types;
- dropped requests, streams, prepared mutations, leases, cancellation, and
  shutdown retain their fault-matrix behavior;
- remote request counts, transferred bytes, peak memory, and benchmark ratios
  do not regress;
- public docs describe direct, managed, HTTP, remote, and local distinctions
  consistently;
- external consumer, MSRV, rustdoc warning, API snapshot, semver, and packaging
  checks pass.
