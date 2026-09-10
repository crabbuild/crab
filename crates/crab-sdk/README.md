# Crab SDK for Rust

[![Rust SDK](https://github.com/crabbuild/crab/actions/workflows/sdk.yml/badge.svg)](https://github.com/crabbuild/crab/actions/workflows/sdk.yml)
[![MSRV](https://img.shields.io/badge/MSRV-1.91-blue)](https://github.com/crabbuild/crab/blob/main/Cargo.toml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](../../LICENSE)

`crab-sdk` is the asynchronous Rust client for Crab repositories. One `Client`
opens remote repositories and local Git worktrees through the same `Repository`
facade. Task-specific APIs live in the `remote`, `local`, `managed`, `storage`,
and `operation` modules.

> [!IMPORTANT]
> `crab-sdk` is not published on crates.io yet. Depend on a pinned Git revision
> until the first registry release.

## Installation

Add the SDK and a Tokio runtime to your `Cargo.toml`. Replace `<COMMIT>` with an
audited commit SHA.

```toml
[dependencies]
crab-sdk = { git = "https://github.com/crabbuild/crab", rev = "<COMMIT>", default-features = false, features = ["remote"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

The minimum supported Rust version is 1.91.

## Feature flags

The crate has no default features. Enable the smallest set required by your
application.

| Feature | API | Implies |
| --- | --- | --- |
| `remote` | Client construction, remote repositories, refs, snapshots, and raw Git reads | — |
| `content` | Verified Crab and Git LFS content reads and content caching | `remote` |
| `write` | Remote initialization, commit creation, atomic ref updates, and recovery | `remote` |
| `local` | Local clone, fetch, edit, integration, hydration, and push workflows | `write`, `content` |
| `managed` | Managed repository resolution and lifecycle administration | `remote` |

Typical dependency configurations are:

```toml
# Remote metadata and raw Git objects
crab-sdk = { git = "https://github.com/crabbuild/crab", rev = "<COMMIT>", default-features = false, features = ["remote"] }

# Remote logical content and writes
crab-sdk = { git = "https://github.com/crabbuild/crab", rev = "<COMMIT>", default-features = false, features = ["content", "write"] }

# Local and managed workflows
crab-sdk = { git = "https://github.com/crabbuild/crab", rev = "<COMMIT>", default-features = false, features = ["local", "managed"] }
```

## Remote repositories

Direct remote access talks to S3, GCS, or Azure object storage and does not
require local `git` or `crab` executables. This example uses the standard AWS
credential environment variables and reads a branch from the repository prefix
`repositories/example` in `my-bucket`.

```rust,no_run
use crab_sdk::storage::DirectStoreOptions;
use crab_sdk::{Client, OpenOptions, RepositoryLocator, Revision};

#[tokio::main]
async fn main() -> crab_sdk::Result<()> {
    let client = Client::builder()
        .direct_store(DirectStoreOptions::s3_from_env("my-bucket")?)
        .build()?;

    let result = read_main(&client).await;
    let close_result = client.close().await;
    result?;
    close_result
}

async fn read_main(client: &Client) -> crab_sdk::Result<()> {
    let locator = RepositoryLocator::new("repositories/example")?;
    let repository = client.open(OpenOptions::remote(locator)).await?;
    let snapshot = repository
        .remote()?
        .snapshot(Revision::branch("main")?)
        .await?;

    println!("{}", snapshot.commit_id()?);
    Ok(())
}
```

A remote `Repository` captures one immutable generation. Existing snapshots
stay pinned if refs move. Call `repository.remote()?.refresh()` to open the
latest visible generation.

`Snapshot::read_blob` returns exact Git blob bytes. With `content` enabled,
`Snapshot::open_file` streams verified logical content for regular Git blobs,
Crab pointers, and Git LFS pointers.

Remote writes use a prepare-then-execute flow. Persist the prepared mutation's
recovery token before calling `execute`. See the
[`remote_edit`](examples/remote_edit.rs) example for streamed Git and hydrated
file edits, commit creation, atomic ref updates, and result handling.

## Local repositories

Enable `local` and provide absolute paths to compatible Git and Crab
executables. Opening a local repository does not contact its remote.

```rust,no_run
use crab_sdk::local::{Options as LocalOptions, Tools};
use crab_sdk::storage::DirectStoreOptions;
use crab_sdk::{Client, OpenOptions};

#[tokio::main]
async fn main() -> crab_sdk::Result<()> {
    let tools = Tools::new("/usr/bin/git", "/usr/local/bin/crab")?;
    let client = Client::builder()
        .direct_store(DirectStoreOptions::s3_from_env("my-bucket")?)
        .local(LocalOptions::new(tools))
        .build()?;

    let result = inspect_worktree(&client).await;
    let close_result = client.close().await;
    result?;
    close_result
}

async fn inspect_worktree(client: &Client) -> crab_sdk::Result<()> {
    let repository = client
        .open(OpenOptions::local("/absolute/path/to/worktree"))
        .await?;
    let status = repository.local()?.status().await?;

    println!("clean={}", status.is_clean());
    Ok(())
}
```

Use `Client::clone_local` to create a worktree. The local interface provides
snapshot, fetch, status, stage, commit, checkout, pull, conflict
continue/abort, hydrate, dehydrate, prefetch, and prepared push operations.
Repository hooks and executable Git drivers are disabled by default.

## Managed repositories

Enable `managed` to resolve managed repository locators and administer their
lifecycle. The token cache path must be absolute.

```rust,no_run
use crab_sdk::managed::Options as ManagedOptions;
use crab_sdk::Client;

#[tokio::main]
async fn main() -> crab_sdk::Result<()> {
    let client = Client::builder()
        .managed(ManagedOptions::new("/absolute/path/to/token-cache")?)
        .build()?;

    let result = list_repositories(&client).await;
    let close_result = client.close().await;
    result?;
    close_result
}

async fn list_repositories(client: &Client) -> crab_sdk::Result<()> {
    let managed = client.managed().await?;
    let page = managed.list("organization", None, 100).await?;

    for repository in page.repositories() {
        println!("{}", repository.canonical_url());
    }
    Ok(())
}
```

`managed::Managed` also provides `create`, `rename`, `archive`, and `restore`.
Open a managed locator with `Client::open(OpenOptions::remote(locator))`, or
pass it to `Client::clone_local`.

## API overview

| Task | Entry point |
| --- | --- |
| Configure a client | `Client::builder()` |
| Open a remote repository | `client.open(OpenOptions::remote(locator))` |
| Open a local worktree | `client.open(OpenOptions::local(path))` |
| Initialize remote state | `client.initialize_remote(locator, head)` |
| Clone a local worktree | `client.clone_local(locator, destination, options)` |
| Configure an existing worktree | `client.configure_local(path)` |
| Read or write remote state | `repository.remote()?` |
| Use local workflows | `repository.local()?` |
| Administer managed repositories | `client.managed()` |
| Drain workers and resources | `client.close().await` |

All asynchronous SDK operations return lazy `operation::Request` values. Use
`Request::with_options` before awaiting to apply a timeout, deadline,
cancellation token, progress receiver, or read limits.

Prepared remote mutations and local pushes can finish after a caller loses the
response. Persist their family-specific recovery tokens and recover with:

- `Client::resume_remote` or `Client::reconcile_remote`
- `Client::resume_local_push` or `Client::reconcile_local_push`

The SDK never infers an indeterminate result from current refs and never
replays a mutation implicitly.

## Errors and shutdown

All public operations return `crab_sdk::Result<T>`. `ErrorKind` provides stable
categories for application decisions. `Error` preserves its source, operation
identity, redacted context, and cleanup failures.

Call `Client::close().await` before destroying the Tokio runtime. A content or
archive stream must reach successful EOF to prove complete integrity. If an
application stops reading early, call the stream's `close` method to drain its
worker and observe finalization errors.

## Examples

- [`remote_read`](examples/remote_read.rs): raw or hydrated remote file reads
- [`remote_archive`](examples/remote_archive.rs): streamed remote archives
- [`remote_edit`](examples/remote_edit.rs): remote commits and ref updates
- [`resolve_conflict`](examples/resolve_conflict.rs): continue or abort a local conflict

## Documentation

- [SDK guide](https://crab.build/docs/sdk)
- [Public API specification](../../crab/docs/architecture/crab-sdk-api.md)
- [Delivery and qualification plan](../../crab/docs/architecture/crab-sdk.md)

## License

Licensed under the [Apache License 2.0](../../LICENSE).
