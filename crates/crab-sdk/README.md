# crab-sdk

[![Rust SDK](https://github.com/crabbuild/crab/actions/workflows/sdk.yml/badge.svg)](https://github.com/crabbuild/crab/actions/workflows/sdk.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](../../LICENSE)

`crab-sdk` is the asynchronous Rust API for reading and changing Crab
repositories. It provides one `Repository` facade for remote repositories and
local Git worktrees, with task-specific APIs under `remote`, `local`, `managed`,
`storage`, and `operation`.

> [!NOTE]
> The SDK is not published on crates.io yet. Its API and qualification gates are
> implemented in this repository. Registry publication is a separate release
> action.

## Requirements

- Rust 1.91 or newer
- Tokio for asynchronous applications
- Storage credentials for direct S3, GCS, or Azure access
- Absolute paths to compatible `git` and `crab` executables for local workflows

Remote object-store operations do not require local tools.

## Installation

Until the first registry release, depend on the Git repository. Pin `rev` to an
audited commit for reproducible application builds.

```toml
[dependencies]
crab-sdk = { git = "https://github.com/crabbuild/crab", branch = "main", default-features = false, features = ["remote"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

For development inside this repository, use the workspace dependency:

```toml
[dependencies]
crab-sdk = { workspace = true, features = ["remote"] }
```

## Features

The crate has no default features. Enable only the workflows your application
uses.

| Feature | Adds | Implies |
| --- | --- | --- |
| `remote` | `Client`, remote repositories, immutable snapshots, Git reads, and operation controls | — |
| `content` | Verified Crab and Git LFS content reconstruction and content caching | `remote` |
| `write` | Remote initialization, commit creation, atomic ref updates, and recovery | `remote` |
| `local` | Local open/clone/fetch/edit/pull/hydration/push workflows | `write`, `content` |
| `managed` | Managed repository resolution and lifecycle administration | `remote` |

Common profiles:

```toml
# Remote metadata and raw Git objects
features = ["remote"]

# Remote logical file content
features = ["content"]

# Remote reads and writes
features = ["content", "write"]

# Complete local workflows
features = ["local"]

# Managed remote and local repositories
features = ["local", "managed"]
```

## Quick start: remote read

This example uses the standard AWS credential environment variables. The
repository locator is the repository prefix inside the selected bucket.

```rust
use crab_sdk::storage::DirectStoreOptions;
use crab_sdk::{Client, OpenOptions, RepositoryLocator, Revision};

#[tokio::main]
async fn main() -> crab_sdk::Result<()> {
    let client = Client::builder()
        .direct_store(DirectStoreOptions::s3_from_env("my-bucket")?)
        .build()?;

    let result = async {
        let locator = RepositoryLocator::new("repositories/example")?;
        let repository = client.open(OpenOptions::remote(locator)).await?;
        let snapshot = repository
            .remote()?
            .snapshot(Revision::branch("main")?)
            .await?;

        println!("{}", snapshot.commit_id()?);
        Ok::<(), crab_sdk::Error>(())
    }
    .await;

    let cleanup = client.close().await;
    result?;
    cleanup
}
```

An opened remote repository captures an immutable generation. Existing
snapshots stay pinned when refs move. Call `repository.remote()?.refresh()` to
obtain a new `Repository` at the latest visible generation.

`Snapshot::read_blob` returns exact Git bytes. Enable `content` and use
`Snapshot::open_file` to reconstruct logical Crab or Git LFS content with
bounded memory and integrity verification.

## Local worktrees

Local workflows require the `local` feature and explicit absolute paths to the
Git and Crab executables.

```rust
use crab_sdk::local::{Options, Tools};
use crab_sdk::storage::DirectStoreOptions;
use crab_sdk::{Client, OpenOptions};

#[tokio::main]
async fn main() -> crab_sdk::Result<()> {
    let tools = Tools::new("/usr/bin/git", "/usr/local/bin/crab")?;
    let client = Client::builder()
        .direct_store(DirectStoreOptions::s3_from_env("my-bucket")?)
        .local(Options::new(tools))
        .build()?;

    let result = async {
        let repository = client
            .open(OpenOptions::local("/absolute/path/to/worktree"))
            .await?;
        let status = repository.local()?.status().await?;

        println!("clean={}", status.is_clean());
        Ok::<(), crab_sdk::Error>(())
    }
    .await;

    let cleanup = client.close().await;
    result?;
    cleanup
}
```

Use `Client::clone_local` to create a worktree. The local interface provides
fetch, status, stage, commit, checkout, pull, conflict continue/abort, hydrate,
dehydrate, prefetch, and prepared push operations. Hooks and unrecognized
executable Git drivers are disabled by default. Use
`local::ExecutionPolicy::Trusted` only when the application intends to run the
repository's executable configuration.

## Managed repositories

Enable `managed` to resolve managed locators and administer repositories. The
administration API is client-scoped because list and create operations do not
belong to one opened repository.

```rust
use crab_sdk::managed::Options;
use crab_sdk::Client;

#[tokio::main]
async fn main() -> crab_sdk::Result<()> {
    let client = Client::builder()
        .managed(Options::new("/absolute/path/to/token-cache")?)
        .build()?;

    let result = async {
        let managed = client.managed().await?;
        let page = managed.list("organization", None, 100).await?;

        for repository in page.repositories() {
            println!("{}", repository.canonical_url());
        }
        Ok::<(), crab_sdk::Error>(())
    }
    .await;

    let cleanup = client.close().await;
    result?;
    cleanup
}
```

`managed::Managed` also provides create, rename, archive, and restore. Managed
locators open through `Client::open(OpenOptions::remote(...))` and can be passed
to `Client::clone_local`.

## API overview

| Task | API |
| --- | --- |
| Build shared configuration | `Client::builder()` |
| Open a remote repository | `client.open(OpenOptions::remote(locator))` |
| Open a local worktree | `client.open(OpenOptions::local(path))` |
| Initialize remote state | `client.initialize_remote(locator, head)` |
| Clone a worktree | `client.clone_local(locator, destination, options)` |
| Configure a worktree | `client.configure_local(path)` |
| Read remote state | `repository.remote()?` |
| Work with local state | `repository.local()?` |
| Administer managed repositories | `client.managed()` |
| Close workers and resources | `client.close().await` |

The public namespaces are:

- `remote` for refs, snapshots, reads, archives, and remote publication
- `remote::write` for commit, ref-update, outcome, and recovery types
- `local` for tool policy and local worktree workflows
- `managed` for service configuration and repository administration
- `storage` for direct providers and the content cache
- `operation` for timeouts, cancellation, progress, read limits, and requests

## Requests and recovery

Asynchronous operations return lazy `operation::Request` values. Apply
operation policy before awaiting:

```rust
use std::time::Duration;

use crab_sdk::operation;
use crab_sdk::{Client, OpenOptions, RepositoryLocator};

async fn open_with_timeout(client: &Client) -> crab_sdk::Result<()> {
    let options = operation::Options::default()
        .with_timeout(Duration::from_secs(30))?;
    let locator = RepositoryLocator::new("repositories/example")?;
    let repository = client
        .open(OpenOptions::remote(locator))
        .with_options(options)
        .await?;

    println!("{:?}", repository.mode());
    Ok(())
}
```

Remote publication returns `remote::write::MutationOutcome`; local push returns
`local::PushOutcome`. Persist a prepared operation's recovery token before
execution. Recover uncertain outcomes with the matching client method:

- `Client::resume_remote` or `Client::reconcile_remote`
- `Client::resume_local_push` or `Client::reconcile_local_push`

The SDK never guesses an indeterminate result from current refs and never
replays it implicitly.

## Shutdown and errors

Call `Client::close().await` before destroying the Tokio runtime. Closing stops
new operation admission, cancels and drains workers, closes metadata owners, and
reports unobserved cleanup failures.

Streams must reach successful EOF to prove complete integrity. Close a stream
explicitly when stopping early so its worker and operation state are drained.

All public operations return `crab_sdk::Result<T>`. `ErrorKind` provides stable
categories, while `Error` retains the source, operation identity, redacted
context, and cleanup failures.

## Documentation

- [SDK guide](https://crab.build/docs/sdk)
- [Public API specification](../../crab/docs/architecture/crab-sdk-api.md)
- [Delivery and qualification plan](../../crab/docs/architecture/crab-sdk.md)
- [Runnable examples](examples)

## License

Licensed under the [Apache License 2.0](../../LICENSE).
