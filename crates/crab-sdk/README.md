# crab-sdk

Preview Rust client for Crab repositories. Every existing repository opens
through `Client::open(OpenOptions)`, which returns one `Repository`. Select its
borrowed `remote()` or `local()` interface for the workflow you need.

The implementation and qualification contract lives in
`crab/docs/architecture/crab-sdk.md`. The package remains unpublished until all
mandatory qualification gates pass. Registry publication is a separate release
action.

## Feature profiles

The crate has no default features. The default surface contains validated core
values, errors, storage configuration, and `ClientBuilder` without a runtime.

- `remote` adds `Client`, unified open, pinned remote reads, and `operation`.
- `content` adds verified Crab and Git LFS reconstruction. It implies `remote`.
- `write` adds remote initialization, commits, atomic ref updates, and recovery.
  It implies `remote`.
- `local` adds local tool configuration, open, clone, fetch, edit, integration,
  hydration, and push workflows. It implies `write` and `content`.
- `managed` adds managed resolution and repository administration. It implies
  `remote` and can be combined with `local`.

Rust 1.91.1 is the minimum supported version. CI compiles every supported
profile and verifies that disabled task namespaces and their dependencies do
not leak into smaller profiles.

## Open a remote repository

Direct storage configuration lives under `storage`. A remote-only application
does not configure Git or Crab executables.

```rust,no_run
use crab_sdk::storage::{DirectStoreOptions, S3Options};
use crab_sdk::{Client, OpenOptions, RepositoryLocator, Revision};

# async fn read() -> crab_sdk::Result<()> {
let store = DirectStoreOptions::s3(S3Options::new(
    "bucket",
    "us-west-2",
    "access-key",
    "secret-key",
)?);
let client = Client::builder().direct_store(store).build()?;
let repository = client
    .open(OpenOptions::remote(RepositoryLocator::new("repositories/example")?))
    .await?;
let snapshot = repository
    .remote()?
    .snapshot(Revision::branch("main")?)
    .await?;
println!("{}", snapshot.commit_id()?);
client.close().await?;
# Ok(())
# }
```

Remote handles capture an immutable generation. Existing snapshots stay pinned
when refs move; `remote.refresh()` returns a new `Repository`. `read_blob`
returns exact Git bytes, including Crab and LFS pointers. With `content`,
`open_file` reconstructs logical content in bounded frames and verifies it at
successful EOF.

With `write`, `remote.prepare_commit` accepts exact-size ordinary or hydrated
streams without a checkout or Git executable. `remote.prepare_ref_update`
prepares an atomic branch and tag batch. Persist the prepared operation's
recovery token before calling `execute()`.

## Open or clone a local repository

Local workflows require exact absolute paths to compatible Git and Crab
executables. Configuration is grouped under `local::Options`.

```rust,no_run
use crab_sdk::local::{Options, Tools};
use crab_sdk::storage::DirectStoreOptions;
use crab_sdk::{Client, OpenOptions};

# async fn local() -> crab_sdk::Result<()> {
let tools = Tools::new("/usr/bin/git", "/usr/local/bin/crab")?;
let client = Client::builder()
    .direct_store(DirectStoreOptions::s3_from_env("bucket")?)
    .local(Options::new(tools))
    .build()?;
let repository = client.open(OpenOptions::local("/work/models")).await?;
let status = repository.local()?.status().await?;
println!("clean={}", status.is_clean());
client.close().await?;
# Ok(())
# }
```

Use `Client::clone_local` when the destination does not exist. Local operations
include fetch, status, stage, commit, checkout, pull, conflict continue or abort,
hydrate, dehydrate, prefetch, and prepared push. Direct and managed Crab clones
use shared Rust transfer services; HTTP locators use native Git smart HTTP.
Hooks and unrecognized executable Git drivers are disabled by default. Select
`local::ExecutionPolicy::Trusted` only for repositories whose executable
configuration the application intends to run.

## Managed repositories

Configure managed authentication with `managed::Options`. A managed locator
still opens through `Client::open`, and it can be the source for
`Client::clone_local`. Repository administration is client-scoped:

```rust,no_run
use crab_sdk::managed::Options;
use crab_sdk::Client;

# async fn list() -> crab_sdk::Result<()> {
let client = Client::builder()
    .managed(Options::new("/absolute/token-cache")?)
    .build()?;
let managed = client.managed().await?;
let page = managed.list("organization", None, 100).await?;
for repository in page.repositories() {
    println!("{}", repository.canonical_url());
}
client.close().await?;
# Ok(())
# }
```

## Requests, recovery, and shutdown

Asynchronous SDK operations return lazy `operation::Request` builders. Apply
`operation::Options` with `with_options` before awaiting. Remote reads that need
range or read-specific controls use `operation::ReadOptions`.

Remote publication returns `remote::write::MutationOutcome`; local push returns
`local::PushOutcome`. An indeterminate result is never guessed or replayed.
Use `Client::reconcile_remote`, `Client::resume_remote`,
`Client::reconcile_local_push`, or `Client::resume_local_push` with the persisted
family-specific recovery token.

Call `Client::close().await` before destroying Tokio. Streams must reach EOF or
be closed explicitly to observe finalization errors. `ErrorKind` supplies stable
categories while `Error` preserves source and cleanup failures.

The public guide is served at <https://crab.build/docs/sdk>.
