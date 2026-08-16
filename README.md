# Crab

**Serverless Git remotes for large files.**

Crab is a Rust command-line tool and Git remote helper for repositories that
contain models, datasets, media, game assets, build artifacts, and other files
that are too expensive or inconvenient to keep as ordinary Git blobs.

Crab keeps Git's commits, branches, tags, merges, and review workflow while
storing selected file contents in object storage that you control. Git stores
small, content-addressed pointer blobs; Crab stores the original bytes as
deduplicated chunks and reconstructs them only when a working tree or job needs
them.

In direct-storage mode, Crab does not require a Crab data server or database.
The repository lives in your S3, Google Cloud Storage, Azure Blob Storage, or
S3-compatible bucket, and your normal cloud credential chain authorizes access.

[Website](https://crab.build) ·
[CLI documentation](https://crab.build/docs/cli) ·
[GitHub repository](https://github.com/crabbuild/crab-oss) ·
[Apache-2.0 license](LICENSE)

## Why Crab

- **Bring your own storage.** Keep repository data in a bucket or container
  owned and governed by your organization.
- **Keep using Git.** Clone, branch, commit, merge, push, fetch, and review
  with Git. Crab integrates through Git's remote-helper and filter protocols.
- **Upload less data.** Content-defined chunking and content-addressed
  deduplication avoid re-uploading unchanged pieces of large files.
- **Clone lazily.** A clone can install lightweight pointer files and defer
  large payload downloads until they are needed.
- **Work selectively.** Hydrate a few files, a glob, a manifest, or a named
  prefetch profile instead of materializing an entire repository.
- **Recover disk space safely.** Dehydrate clean, materialized files back to
  pointers without losing their remote copy.
- **Automate reliably.** Long-running commands support structured JSON and
  JSONL output, stable error codes, progress events, and cancellation.

## How Crab fits around Git

Crab has two distinct Git integrations:

1. **Remote helper.** When Git sees a remote such as
   `crab://bucket/project`, it starts `git-remote-crab`. The helper handles
   ref discovery, fetch, push, branch updates, tags, deletes, shallow depth
   operations, and Git pack transfer.
2. **Filter driver.** Files selected in `.gitattributes` pass through Crab's
   clean/smudge filter. Clean converts large working-tree files into small
   pointer blobs and stages their content locally. Smudge either leaves a
   pointer in place for a lazy checkout or reconstructs the full file.

Everything else remains ordinary Git: commits and refs are still Git objects,
and Git remains the interface for code history and collaboration.

## Core data flow

~~~text
                         Git commands
                              │
                ┌─────────────┴─────────────┐
                │                           │
       clean/smudge filter           remote helper
        working tree ↔ pointer       fetch/push refs
                │                           │
                └─────────────┬─────────────┘
                              │
                 Crab engine, indexes, and cache
                  │                         │
          .crab/staging/              local cache
          chunks awaiting push       ~/.cache/crab/
                              │
                              ▼
       S3 / S3-compatible storage / Google Cloud Storage /
                         Azure Blob Storage

       Git objects, manifests, metadata, shards, and content-addressed xorbs
       are uploaded only after the required local data is ready.
~~~

### What happens to a large file?

When a tracked file is added:

1. Crab hashes the file with BLAKE3 while performing content-defined
   chunking. The file is streamed; it is not required to fit in memory.
2. Chunks already present locally or remotely are reused. New chunks are
   written to the local staging area.
3. Git receives a small pointer blob containing the file hash and size.
4. On push, Crab packs new chunks into compressed, immutable **xorbs** and
   uploads the xorbs together with reconstruction metadata and Git packs.
5. Crab publishes the mutable manifest/ref state only after the immutable data
   is durable. A failed push can leave unreferenced immutable data for garbage
   collection, but it does not publish a dangling ref.
6. On hydration, Crab resolves the pointer to reconstruction terms, reads the
   required ranges, decompresses and verifies the chunks, and atomically
   writes the original file.

The pointer format is intentionally small and stable:

~~~text
version https://crab.dev/spec/v1
file-hash <64-character-blake3-hash>
size <bytes>
shard-hint <optional-metadata-hash>
~~~

## Supported storage backends

| Backend | Repository URL | Credential examples |
| --- | --- | --- |
| Amazon S3 or an S3-compatible service | `crab://bucket/repository` | AWS SDK default chain, `AWS_PROFILE`, or access-key environment variables |
| Google Cloud Storage | `crab://bucket/repository` | Application Default Credentials or `GOOGLE_APPLICATION_CREDENTIALS` |
| Azure Blob Storage | `crab://container/repository` | `az login`, connection string, account key, or SAS credentials |

The `crab://` scheme is used for Git remotes regardless of the backing
provider. During initialization, provider-prefixed URLs such as `s3://`,
`gs://`, `gcs://`, or `azure://` may be accepted as a convenience, but
Crab persists the canonical Git remote as `crab://...` so Git invokes the
Crab helper.

Choose the provider explicitly when a repository must be portable across
machines:

~~~bash
crab init --storage-provider s3 crab://my-bucket/my-project
crab init --storage-provider gcs crab://my-gcs-bucket/my-project
crab init --storage-provider azure crab://my-container/my-project
~~~

For local S3-compatible development, set the standard AWS variables and an
endpoint:

~~~bash
export AWS_ACCESS_KEY_ID=crab
export AWS_SECRET_ACCESS_KEY=crab
export AWS_REGION=us-east-1
export AWS_ENDPOINT_URL=http://127.0.0.1:9000
~~~

See the [RustFS local development guide](crab/docs/guides/local-dev-rustfs.md)
for a complete local object-store setup. Never commit cloud credentials or
place secret values in `.crab.toml`.

## Installation

### Release binaries

On macOS or Linux, install the latest release with Homebrew:

~~~bash
brew install crabbuild/tap/crab
~~~

Or use the checksum-verifying installer:

~~~bash
curl -fsSL https://crab.build/install.sh | bash
~~~

On Windows, run the PowerShell installer:

~~~powershell
irm https://crab.build/install.ps1 | iex
~~~

The release archive includes the `crab` executable and the
`git-remote-crab` helper. Unix releases also include the optional mount
helpers when available. Verify the installation with:

~~~bash
crab version
crab --help
~~~

The installer supports `CRAB_VERSION` for pinning a release and
`CRAB_INSTALL_DIR` for selecting an installation directory.

### Build from source

The CLI is part of the Rust workspace and the supported local install path is
the Makefile in `crab/`:

~~~bash
git clone https://github.com/crabbuild/crab-oss.git
cd crab-oss/crab
make install
~~~

This builds the CLI, the Git remote-helper link, and the platform mount
helpers. The default build enables the feature set used by the supported
platforms. FUSE/NFS builds may require the corresponding operating-system
development packages; a minimal CLI build can disable platform-specific
features when needed.

## Quick start

The following example creates a new Crab-backed repository in an S3 bucket.
The same workflow works with `gcs` and `azure` after configuring the
matching credentials.

~~~bash
mkdir my-project
cd my-project

# crab init runs git init when this directory is not already a Git repository.
crab init --storage-provider s3 crab://my-bucket/my-project

# Scan for large files, write .gitattributes, and finish local Git setup.
crab setup

# Review the generated tracking rules and repository state.
git status
crab status

# Stage, commit, and push in one operation.
crab ship -m "Initial commit"
~~~

`crab setup` auto-detects files above the large-file threshold and common
binary formats. Review the generated `.gitattributes` rules; use explicit
patterns when automatic detection is not appropriate:

~~~bash
crab setup --no-auto-track
crab track '*.safetensors'
crab track 'datasets/**'
crab ship -m "Track model and dataset files"
~~~

If you prefer separate Git steps:

~~~bash
crab add .
git commit -m "Add project data"
crab push
~~~

`crab push` uses Crab's native concurrent pipeline. A normal `git push`
also works when the configured Git remote is a `crab://` remote because Git
starts `git-remote-crab`.

Run `crab ship --dry-run -m "preview"` before a large operation to preview
the files and work that would be staged, committed, and pushed.

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
Git ref, or a named profile in `.crab/prefetch.toml`:

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
clone. Crab currently rejects Git partial-clone filters such as
`--filter=blob:none` on its remote-helper path rather than silently changing
the requested transfer.

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

`.crab.toml` is the repository-committed project configuration. It tells
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

The local `.crab/config.toml` stores machine-specific state such as local
cache, staging, and operational settings. It should not be committed.

Useful configuration commands:

~~~bash
crab config get auth.storage_provider
crab config set auth.storage_provider gcs
crab config set checkout.lazy true
crab config set hydrate.include '*.safetensors'
~~~

For teams that want named hydration sets, commit a
`.crab/prefetch.toml` file:

~~~toml
version = 1

[[profile]]
name = "always"
paths = ["README.md", "*.toml", "src/**/*.rs"]

[[profile]]
name = "ci"
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
  parameters, plots, and queues. Enable the workflow layer with
  `[workflow] enabled = true` in local configuration.
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

- Git partial-clone filters, including `blob:none`, are not supported by the
  direct Crab remote helper.
- Remote-helper `connect` and `stateless-connect` sessions are not
  advertised or implemented.
- Depth-based shallow operations are supported, but date-based and
  ref-exclusion shallow selectors are rejected explicitly.
- Lazy Crab checkout fetches Git history and packs; it only defers Crab-managed
  file payloads. It is not a substitute for Git's partial-clone protocol.
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
├── crab-web/             Marketing site and published documentation source
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
