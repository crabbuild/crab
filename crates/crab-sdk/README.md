# crab-sdk

Preview Rust client for Crab repositories. The implementation and qualification
contract lives in `crab/docs/architecture/crab-sdk.md`. The package remains
unpublished until every mandatory qualification cell passes; registry
publication is a separate release action.

## Feature profiles

The default feature exposes validated value types, request builders, limits,
errors, storage selectors, and `ClientBuilder` configuration without opening a
runtime or resolving credentials.

- `remote` adds pinned repository generations, refs, commits, paginated trees
  and history, diff, blame, raw blobs, ordinary file streams, and Git archives.
- `content` adds verified Crab and Git LFS reconstruction. It implies `remote`.
- `write` adds direct repository initialization, streamed remote commits,
  atomic branch and tag batches, durable recovery, and reconciliation. It
  implies `remote`.
- `local` adds explicit Git and Crab tools plus configure, open, clone, fetch,
  status, stage, commit, checkout, pull, hydrate, dehydrate, prefetch, and
  prepared push workflows. It implies `write` and `content`.
- `managed` adds managed repository resolution, lifecycle administration, grant
  refresh, and protected push orchestration. It can be combined with `local`.

The supported feature combinations are compiled on Rust 1.91.1 and current
stable. `crab/scripts/check-sdk-features.py` verifies that remote-only consumers
do not acquire CLI, server, VFS, or hydration dependencies.

## Remote repositories

`DirectStoreOptions` selects S3, GCS, Azure, or a development filesystem store.
Explicit cloud credential options override provider environment values, redact
their debug output, and separate authorization scopes in content caches.
Environment credential-chain constructors remain available when selected by the
application. Repository locators are separate from bucket, account, and
container configuration.

`RemoteRepository` captures one immutable generation. Existing snapshots stay
pinned when branches move; `refresh` returns a new handle. `read_blob` returns
exact Git bytes, including Crab and LFS pointers. `open_file` reconstructs
logical content in bounded frames and reports integrity only after successful
EOF. Hydrated Crab reads require an explicit absolute `ContentCache` directory
and finite retention budget.

With `write`, `prepare_commit` accepts exact-size ordinary or hydrated streams
without a checkout or Git executable. Hydrated content publishes verified xorbs
and reconstruction metadata before its pointer enters the Git pack. Ref creates,
updates, and deletes are atomic; update and delete operations require an exact
expected old object ID. History rewrites require explicit force-with-lease.

Persist a prepared mutation's recovery token before execution. Execution never
guesses after a lost response: it returns committed, rejected, or indeterminate.
`Client::reconcile` reads historical proof without writing repairs, and
`resume_mutation` reopens only an unattempted direct plan. A recorded but
unresolved attempt is never replayed automatically.

## Local workflows

Full local mode requires absolute paths to compatible Git and Crab executables:

```rust,no_run
use crab_sdk::{Client, DirectStoreOptions, LocalTools};

# fn build() -> crab_sdk::Result<Client> {
let tools = LocalTools::new("/usr/bin/git", "/usr/local/bin/crab")?;
Client::builder()
    .direct_store(DirectStoreOptions::s3_from_env("bucket")?)
    .local_tools(tools)
    .build()
# }
```

The capabilities handshake runs before repository mutation. Child processes use
the selected executables and process-local environment without changing the
application current directory, process environment, or global Git config.
Repository hooks and unrecognized executable Git drivers are disabled by
default. Select `LocalExecutionPolicy::Trusted` on `ClientBuilder` only for a
repository whose executable configuration the application intends to run.

Clone and fetch use shared Rust transfer services for direct and managed Crab
repositories and native smart HTTP for HTTP locators. Full and shallow history,
deepen, unshallow, tags, pruning, linked worktrees, and crash recovery are
supported. Clone reads the selected revision's committed `crab.toml` before its
first checkout and applies its hydration mode and automatic patterns. Explicit
`CloneOptions::lazy()` or `CloneOptions::eager()` overrides that repository
policy; with neither override, a repository without hydration policy remains
lazy. Opening a repository validates durable recovery state without
modifying it; the next fetch or pull completes recovery under the repository
lease. Partial clone, sparse checkout, recursive submodules, linked-worktree
creation, and SHA-256 repositories are rejected before mutation in 1.0.

Pull supports fast-forward-only, merge, and rebase. Conflicts return an opaque
`IntegrationId` and exact paths for `continue_integration` or
`abort_integration`; persist its string form and reconstruct it after restart.
Prepared push binds its recovery token to the repository,
remote URL, local source objects, ref expectations, and immutable artifacts.
Dry-run performs local validation without remote objects or leases.

## Managed repositories

`ManagedOptions` uses the existing encrypted Crab token cache and managed
authority. `ManagedRepositories` provides bounded pagination plus create,
rename, archive, and restore with service revision tokens. Managed reads use
read-scoped grants; protected pushes obtain distinct server-authorized sessions.
Grant refresh, revocation, placement changes, and finalize-response recovery stay
inside the auth and service owners. Cache identities include principal,
repository, and physical placement.

## Runtime and errors

Requests are lazy and accept cancellation, deadlines, aggregate read limits, and
bounded progress. Dropping a request signals cancellation; workers holding
leases or cleanup responsibility are drained. Applications must call
`Client::close().await` before destroying Tokio. Streams must reach EOF or be
closed explicitly to observe finalization errors.

`ErrorKind` provides stable categories while `Error` preserves the source,
operation identity, and any secondary cleanup error. Public outcome enums are
non-exhaustive.

## Examples and qualification

The package includes `remote_read`, `remote_archive`, `remote_edit`, and
`resolve_conflict` examples.
The public guide is served at <https://crab.build/docs/sdk>.

The dedicated SDK workflow validates feature profiles, MSRV and current Rust,
rustdoc, Clippy, public API and published-version semver, packaged external
consumers, Linux,
macOS, Windows, RustFS reads and writes, a 1 GiB local round trip, and controlled
1 GiB read benchmarks. Scheduled credentialed jobs qualify GCS, Azure, and the
real managed service. Inventory status means the API and named tests exist; only
retained passing qualification reports establish backend support.
