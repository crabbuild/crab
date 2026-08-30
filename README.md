<p align="center">
  <img src="packages/web/public/crab.optimized.svg" alt="Crab logo" width="112">
</p>

<h1 align="center">Crab</h1>

<p align="center">Serverless Git for large files.</p>

<p align="center">
  <a href="https://github.com/crabbuild/crab-oss/actions/workflows/rust.yml"><img src="https://github.com/crabbuild/crab-oss/actions/workflows/rust.yml/badge.svg" alt="CI status"></a>
  <a href="https://github.com/crabbuild/crab-oss/releases/latest"><img src="https://img.shields.io/github/v/release/crabbuild/crab-oss?display_name=tag" alt="Latest release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/crabbuild/crab-oss" alt="Apache 2.0 license"></a>
</p>

<p align="center">
  <a href="https://crab.build">Website</a> ·
  <a href="https://crab.build/docs/cli">Documentation</a> ·
  <a href="https://crab.build/blog/crab-vs-git-lfs">Crab vs. Git LFS</a>
</p>

Crab keeps models, datasets, media, game assets, and build artifacts out of ordinary Git blobs. Git stores small pointer files while Crab stores the original content as deduplicated chunks in object storage you control.

In direct-storage mode, each developer connects to Amazon S3, Google Cloud Storage, Azure Blob Storage, or an S3-compatible bucket. There is no Crab data server or database to deploy.

## Install Crab

Install the latest release on macOS or Linux with Homebrew:

```bash
brew install crabbuild/tap/crab
```

You can also use the checksum-verifying installer on macOS or Linux:

```bash
curl -fsSL https://crab.build/install.sh | bash
```

On Windows, run the PowerShell installer:

```powershell
irm https://crab.build/install.ps1 | iex
```

Verify that Crab and its Git remote helper are available:

```bash
crab version
crab --help
```

The release includes `crab`, `git-remote-crab`, and the available mount helpers. The shell installer supports `CRAB_VERSION` for release pinning and `CRAB_INSTALL_DIR` for a custom destination. See the [installation guide](https://crab.build/docs/cli/getting-started/installation) for platform details.

If you work with several Crab repositories, register the Git drivers once with `crab install --global`. `crab configure` and `crab clone` configure the current repository automatically.

## Create and ship a repository

Start with a directory, a cloud bucket, and credentials that can write to it:

```bash
mkdir my-project
cd my-project

crab configure s3://my-bucket/my-project
crab ship . -m "Initial commit"
```

`crab configure` selects the storage provider, discovers credentials, initializes Git, installs the filter driver, and detects large files. `crab ship` stages, commits, and pushes in one command.

Use the same flow with Google Cloud Storage or Azure Blob Storage:

```bash
crab configure gs://my-bucket/my-project
crab configure azure://my-container/my-project
```

Pass explicit tracking rules when you do not want automatic detection:

```bash
crab configure s3://my-bucket/my-project --no-auto-track
crab track '*.safetensors'
crab track 'datasets/**'
crab ship . -m "Track model and dataset files"
```

For separate Git steps, use Crab for large-file staging, then push through Crab's concurrent pipeline:

```bash
crab add .
git commit -m "Add project data"
crab push
```

A normal `git push` also works because Git invokes `git-remote-crab` for a `crab://` remote.

Preview a large operation with `crab ship . --dry-run -m "Preview"`.

## Why Crab

- **Own your storage:** Keep repository data in a bucket governed by your organization
- **Keep Git:** Clone, branch, commit, merge, push, fetch, and review with familiar commands
- **Upload changed chunks:** Content-defined chunking and content-addressed deduplication reuse content across file versions
- **Download on demand:** Lazy clones keep pointer files until you hydrate the files your workspace needs
- **Recover disk space:** Dehydrate clean files back to pointers without deleting their remote content
- **Automate with stable output:** Long-running commands support JSON, JSON Lines (JSONL), error codes, progress events, and cancellation

## How Crab works

Crab integrates with Git at two boundaries:

1. The filter driver converts files selected by `.gitattributes` into small pointer blobs and stages their content locally.
2. The `git-remote-crab` helper transfers Git objects, refs, and Crab-managed content for `crab://` remotes.

```text
working tree ── clean/smudge filter ── pointer blobs ── Git history
                       │
                       └── deduplicated chunks ── object storage
```

When you push, Crab uploads immutable chunks and reconstruction metadata before it publishes mutable ref state. When you hydrate, Crab verifies the chunks and reconstructs the original bytes.

## Supported storage backends

Crab accepts provider-prefixed URLs during setup, then records a canonical `crab://` Git remote:

| Backend | Configure with | Common credential sources |
| --- | --- | --- |
| Amazon S3 | `crab configure s3://bucket/repository` | AWS shared profiles/SSO, web identity, ECS or EC2 role, access-key environment variables |
| S3-compatible storage | `crab configure crab://bucket/repository --provider s3` | Access-key environment variables and `AWS_ENDPOINT_URL` |
| Google Cloud Storage | `crab configure gs://bucket/repository` | Application Default Credentials or `GOOGLE_APPLICATION_CREDENTIALS` |
| Azure Blob Storage | `crab configure azure://container/repository` | Workload or managed identity, connection string, account key, Shared Access Signature (SAS) credentials |

Never commit cloud credentials or place secret values in `crab.toml`. Read the [authentication guide](https://crab.build/docs/cli/authentication/configuration) for provider-specific setup.

## Build from source

Clone the workspace and use its supported installer:

```bash
git clone https://github.com/crabbuild/crab-oss.git
cd crab-oss/crab
make install
```

This builds the CLI, Git remote helper, and platform mount helpers. Filesystem in Userspace (FUSE) and Network File System (NFS) builds may require operating-system development packages.

## Clone and hydrate

### Lazy clone

By default, Crab clones Git history and checks out pointer files without
downloading every large payload:

~~~bash
crab clone crab://my-bucket/my-project
cd my-project

# Inspect pointer/hydration state.
crab status

# Materialize only what this workspace needs.
crab hydrate '*.safetensors'
crab hydrate 'datasets/validation/**'
~~~

Hydrate everything when the job needs a complete working tree:

~~~bash
crab hydrate --all
~~~

Hydration can also use a newline-delimited manifest, a manifest stored in a
Git ref, or a named profile in `crab.toml`:

~~~bash
crab hydrate --manifest .crab/manifests/ci.txt
git ls-files '*.rs' | crab hydrate --manifest -
crab hydrate --profile=ci
~~~

### Reclaim disk space

`crab dehydrate` replaces clean, verified full files with their pointer
blobs. It never replaces a dirty file with a stale pointer:

~~~bash
crab dehydrate '*.safetensors'
crab dehydrate --all

# Explicitly include protected profile files when reclaiming all space.
crab dehydrate --all --ignore-profiles
~~~

Use `crab fetch` to pre-warm the local cache without changing the working
tree:

~~~bash
crab fetch --include '*.safetensors'
crab cache stats
crab cache clean
~~~

Lazy checkout is a Crab pointer-materialization feature, not Git partial
clone. The wrapper does not request `--filter=blob:none`; ordinary Git may use
the proof-gated protocol-v2 profile described in Current limitations.

## Daily workflow

| Task | Command |
| --- | --- |
| Check hydration and tracking state | `crab status` |
| Explain one file's state | `crab why path/to/file` |
| List tracked files and hashes | `crab ls-files` |
| Add files through the parallel Crab path | `crab add <patterns>` |
| Commit and push in one step | `crab ship -m "message"` |
| Push an existing commit | `crab push` |
| Pull and hydrate newly fetched pointers | `crab pull` |
| Pre-fetch data without checkout changes | `crab fetch` |
| Materialize content | `crab hydrate <patterns>` |
| Free local disk | `crab dehydrate --all` |
| Compare files at two Git refs | `crab diff REF1 REF2` |
| Acquire advisory file locks | `crab lock path/to/file` |
| Diagnose local setup | `crab doctor` |

Crab also supports standard Git remotes alongside a Crab remote. For a
GitHub/GitLab code-review workflow, use [mirror mode](crab/docs/guides/mirror-mode.md)
so code and pointer blobs go to the normal Git remote while large-file content
is uploaded to Crab storage first.

## Migrating an existing repository

There are two migration strategies:

### Adopt the current working tree

`crab adopt` converts selected files in the current tree into pointers and
stages their original content. This is the safer default because it does not
rewrite existing commits:

~~~bash
crab init --storage-provider s3 crab://my-bucket/my-project
crab adopt --dry-run
crab adopt --pattern '*.bin' --pattern '*.safetensors'
git diff --cached
crab ship -m "Adopt large files into Crab"
~~~

### Rewrite history

`crab migrate import` can convert large files in history, while
`crab migrate export` can convert pointers back to full blobs. History
rewriting changes every affected commit and requires coordination, a backup,
and a force push. Use `--dry-run` first and read
[the migration guide](crab/docs/guides/migrate.md) before proceeding.

## Project configuration

`crab.toml` is the repository-committed project configuration. It tells
collaborators which Crab remote and storage provider to use and can declare
tracking and hydration policy:

~~~toml
[remote]
url = "crab://my-bucket/my-project"

[auth]
storage_provider = "s3"

[track]
patterns = ["*.safetensors", "*.bin", "datasets/**"]

[hydrate]
default = "lazy"
auto_patterns = ["README*", "*.toml", "src/**/*.rs"]
~~~

The local `.crab/local.toml` stores machine-specific settings such as an AWS
profile selector, cache paths, and operational tuning. It should not be
committed. Staging, caches, journals, and locks live elsewhere under `.crab/`;
staging may contain unpublished data and is not a disposable cache.

Useful configuration commands:

~~~bash
crab config get auth.storage_provider
crab config set auth.storage_provider gcs
crab config set auth.aws_profile ml-team
crab config set checkout.lazy true
crab config set hydrate.include '*.safetensors'
~~~

For teams that want named hydration sets, add them to `crab.toml`:

~~~toml
version = 1

[prefetch.profiles.always]
paths = ["README.md", "*.toml", "src/**/*.rs"]

[prefetch.profiles.ci]
paths = ["tests/fixtures/**", "scripts/**"]
~~~

See [Project Configuration](crab/docs/guides/project-config.md) for the full
schema and precedence rules.

## Advanced capabilities

The core Git/file workflow is the recommended starting point. The same CLI
also includes:

- **Selective download:** `crab download` (also available as `crab get`)
  retrieves selected paths without cloning a full working tree.
- **Object-store maintenance:** `crab gc`, `crab fsck`, `crab compact`,
  `crab repack`, `crab prune`, `crab optimize`, `crab tier`, and
  `crab metadb` inspect and maintain remote data, indexes, caches, and
  lifecycle policies. Use destructive maintenance commands only with an
  explicitly scoped repository and a reviewed dry run where available.
- **Workflow execution:** `crab run`, `crab repro`, `crab stage`,
  `crab exp`, `crab queue`, `crab params`, `crab metrics`, and
  `crab plots` provide content-addressed stages, experiments, metrics,
  parameters, plots, and queues. Fresh configurations enable the workflow
  layer; set `[workflow] enabled = false` for an explicit opt-out.
- **Git LFS interoperability:** `crab lfs` provides compatibility and
  conversion commands for repositories that already use Git LFS.
- **Virtual filesystems:** `crab mount` and `crab unmount` expose a
  repository through on-demand reads, using the available NFS or FUSE backend.
  Mount support is platform-dependent and requires its operating-system
  prerequisites.
- **Recovery and release metadata:** `crab recover`, `crab release`, and
  `crab audit` support verified repair plans, dataset release manifests, and
  local audit records.

Detailed command documentation is available in the
[CLI reference](https://crab.build/docs/cli/reference) and in
`crab/docs/guides/`.

## Automation and structured output

Human-readable output is the default. Automation can opt into:

~~~bash
crab status --json
crab hydrate --jsonl
crab push --json
crab doctor --json
~~~

`--json` emits one result envelope. `--jsonl` emits newline-delimited
progress events followed by a terminal result event. Successful envelopes
contain `schema`, `version`, `timestamp`, and `data`; failures contain
an `error` object with a stable `CRAB-E####` code, category, retryability,
and source chain.

Useful diagnostics for CI and bug reports:

~~~bash
crab env --json
crab doctor --json
crab errors
crab logs list
~~~

Do not include credential values or token-cache contents in bug reports.

## Reliability and storage safety

Crab's storage pipeline is designed around a few important boundaries:

- **Content verification:** file and chunk identities are hash-checked during
  staging, upload, cache reads, and reconstruction.
- **Immutable before mutable:** xorbs, shards, indexes, and Git packs are
  prepared before the manifest/ref commit is attempted.
- **Compare-and-swap refs:** concurrent writers are serialized through
  conditional updates rather than last-writer-wins mutation.
- **Local staging lifecycle:** staged chunks bridge `git add` and push;
  successful pushes retire staging entries and warm the local xorb cache.
- **Explicit maintenance:** `fsck`, `doctor`, `stat`, `du`, and
  `crab errors <code>` expose health and failure information instead of
  hiding it behind generic messages.

Garbage collection and cleanup can remove unreferenced remote data. Always
verify the repository scope and retention window before running them, especially
when a bucket contains more than one logical repository.

## Current limitations

The following behaviors are intentional and should be considered when
designing integrations:

- The current development-line helper implements proof-gated Git wire
  protocol v2 fetch, including `blob:none` partial clone, when the remote has
  current locator and visibility coverage. RustFS qualification is green;
  provider and released-artifact qualification remain before this becomes a
  released support claim. Git owns promisor configuration and pack sidecars;
  missing proof fails closed rather than silently fetching a full filtered
  clone.
- Stateful `connect` and receive-pack takeover are unsupported. The helper's
  terminal `stateless-connect git-upload-pack` profile is the local fetch path.
- Depth-based shallow operations are supported, but date-based and
  ref-exclusion shallow selectors are rejected explicitly.
- The `crab clone` wrapper's lazy checkout fetches Git history and packs; it
  only defers Crab-managed file payloads. Direct Git `--filter=blob:none` is
  the separate Git partial-clone path.
- FUSE and NFS mounting depend on the target operating system and build
  features. The regular CLI and direct object-storage workflow do not require a
  mount.
- Advanced managed-service, replication, cache-service, and lifecycle commands
  require additional service or cloud infrastructure beyond a direct bucket.

The [Git integration architecture guide](crab/docs/architecture/git-integration.md)
contains the detailed capability matrix and evidence boundaries.

## Repository layout

~~~text
.
├── crab/                 Rust CLI, remote helper, engine, and product wiring
├── crates/               Shared Rust contracts and storage/data-plane crates
├── crab/docs/            Architecture notes, guides, designs, and references
├── packages/web/         Marketing site and published documentation source
├── diagram/              Architecture diagrams and rendered assets
├── .github/workflows/    CI, release, and service evidence workflows
├── Cargo.toml            Rust workspace manifest
└── LICENSE               Apache-2.0 license
~~~

The Rust workspace contains the `crab` binary and the shared crates for auth,
cache, coordination, Git, LFS, metadata, reading, staging, storage, types,
virtual filesystems, workflows, and Xet-style chunking.

## Development

### Prerequisites

- Rust stable with Rust 2024 edition support
- Git
- Python 3 for repository checks and selected integration scripts
- Docker and a local S3-compatible service for the RustFS end-to-end workflow
- Platform mount dependencies only when working on FUSE/NFS support

### Build and verify

Run the standard checks from the CLI crate:

~~~bash
cd crab

# Fast compile check
make check

# Full Rust test suite
make test

# Strict linting
make clippy

# Format the workspace
make fmt
~~~

For a local release-style binary and Git helper:

~~~bash
cd crab
make install
crab version
command -v git-remote-crab
~~~

Use a separate `CARGO_TARGET_DIR` when building multiple checkouts or when
working on a disk-constrained machine. Do not commit generated build artifacts,
local credentials, cache contents, staging databases, or cloud test output.
Maintainers should follow [RELEASING.md](RELEASING.md) for versioning, tagging,
and GitHub release verification.

### Local end-to-end testing

The repository includes a RustFS-based local object-storage workflow that
exercises initialization, pointer conversion, push, fetch, hydration, and
conditional manifest/ref updates:

[Local RustFS development guide](crab/docs/guides/local-dev-rustfs.md)

The test suite also contains in-memory and no-cloud integration coverage for
the shared crates. Provider-specific credentials and live tests should be run
only in an isolated test bucket or environment.

### Documentation

When changing a user-visible command, configuration key, serialized format,
storage layout, or error contract, update the matching guide and architecture
note. The local documentation indexes are:

- [Guides](crab/docs/guides/README.md)
- [Architecture](crab/docs/architecture/README.md)
- [Design notes](crab/docs/design/overview.md)
- [Published CLI docs](https://crab.build/docs/cli)

## Troubleshooting

Start with the environment and health checks:

~~~bash
crab env
crab doctor
crab status
~~~

Common problems:

- **No credentials found:** configure the provider's standard credential chain
  and rerun `crab doctor`.
- **Git invokes the wrong helper:** confirm the Crab installation directory is
  on `PATH`, then run `crab install --global` and check
  `git config --global filter.crab.process`.
- **Files remain pointers:** run `crab status`, confirm the file pattern is
  in `.gitattributes`, and use `crab hydrate <pattern>`.
- **Push reports a conflict:** fetch or pull the remote branch, inspect
  `crab push --json`, and resolve the Git ref update before retrying.
- **Local cache is too large:** inspect `crab cache stats`, then use
  `crab prune` for selective eviction or `crab cache clean` to clear the
  complete local cache.
- **Storage corruption is suspected:** stop destructive maintenance, preserve
  logs, run `crab fsck`, and collect `crab env --json` without secrets.

For detailed command-specific troubleshooting, see
[the guide index](crab/docs/guides/README.md) or the
[online CLI documentation](https://crab.build/docs/cli).

## Contributing

Contributions are welcome. A useful change generally includes:

1. A focused implementation with clear ownership in the CLI or the shared
   crate that owns the relevant contract.
2. Regression tests for changed behavior and error paths.
3. Updated user documentation when the behavior or configuration surface
   changes.
4. Formatting, compile, lint, and targeted test evidence in the pull request.

Before opening a pull request, run the narrowest relevant checks and include
the exact commands and results. For storage or Git protocol changes, include
the provider assumptions and any live or local-object-store evidence required
to reproduce the behavior.

## License

Crab is licensed under the [Apache License 2.0](LICENSE).
