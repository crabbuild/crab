# Operate and integrate `crab-server`

The server embeds Cellar 0.1.2. [CELLAR.md](CELLAR.md) defines its native
storage and lifecycle boundary, retired operator controls, and verification
limitations. Only native Cellar application storage is supported.

## Find the contract you need

Use this map to enter the reference without reading it in order.

| Goal | Section |
| --- | --- |
| Build and initialize a local server | [Run the current development build](#run-the-current-development-build) |
| Build and operate the container | [Run the container](#run-the-container) |
| Understand the process and storage boundaries | [Understand the runtime architecture](#understand-the-runtime-architecture) |
| Call repository browser APIs | [Repository browser and application APIs](#repository-browser-and-application-apis) |
| Configure browser sign-in (OIDC or GitHub OAuth) | [Team sign-in](#team-sign-in) |
| Import a repository from Git | [Git imports](#git-imports) |
| Fork a private organization repository | [Private organization forks](#private-organization-forks) |
| Configure OpenID Connect (OIDC) | [Team sign-in](#team-sign-in) |
| Clone or fetch with native Git | [Git HTTP reads](#git-http-reads) |
| Transfer LFS objects | [Git LFS transfers](#git-lfs-transfers) |
| Push branches and tags | [Native Git push](#native-git-push) |
| Use issues, pulls, reviews, labels, or assignees | [Issues, pull requests and reviews](#issues-pull-requests-and-reviews) |
| Report continuous integration (CI) results | [Commit statuses and required checks](#commit-statuses-and-required-checks) |
| Inspect limits and failure behavior | [Resource and lifecycle limits](#resource-and-lifecycle-limits) |
| Run verification | [Current verification](#current-verification) |
| Assess release readiness | [Completion requirements](#completion-requirements) |

## Bind browser mutations to repository identity

Repository catalog responses expose an immutable `id` UUID. Session-authenticated
and local-trust mutations under `/api/repos/{owner}/{name}/...` must send exactly
one `X-Crab-Repository-Id` header containing the UUID captured when the repository
was opened. Keep it with the original intent through retries; do not resolve a
new UUID just to make an old action pass.

The shared HTTP boundary compares the header with the decoded route's current
repository after authentication, CSRF and durable membership checks. On resolved,
authorized routes, missing, malformed, duplicate or different UUIDs return
409 `repository_changed` before
Cell, Git, upload or catalog mutation effects. The admitted principal retains an
immutable repository constraint through routing and grant refreshes, including
local trust. Case-insensitive membership resolution also requires that constraint;
unresolved or unauthorized targets retain a hidden 404. This error has no proven-rejection
marker: an earlier attempt may have committed in the original repository. Resolve
the original outcome before starting another action. An unknown repository or
revoked membership retains the usual hidden/not-found behavior.

The embedded UI and browser clients send this precondition for discussion edits,
reviews/merge, metadata catalogs, settings, files, branches, uploads and releases.
Repository component state is keyed by UUID; a replacement cannot inherit the old
repository's draft or pending controller. Account/organization APIs do not address
repositories by these routes. Native Git and status/check integration tokens
already bind their authenticated scope to a repository UUID and do not use this
browser header. This updates the unreleased browser/API contract; no name-only
mutation fallback or storage migration is introduced.

## Understand the runtime architecture

Crab owns HTTP authentication and authorization, repository policy, the durable
catalog, Git publication, and embedded browser assets. Cellar owns native Cell
routing, leases, activation, SQLite/LTX persistence, replication, and recovery.

### Know which component owns each boundary

| Owner | Responsibility |
| --- | --- |
| `crab-server` | Product HTTP, catalog, authorization, typed repository operations, and private mTLS transport |
| Cellar 0.1.2 | Native execution domain, ownership, persistence, output durability, and peer protocol |
| Shared Crab crates | Git objects, storage mechanics, read/write publication, and metadata contracts |

### Follow one repository read

The HTTP boundary authorizes the principal against the repository catalog.
`cells/client.rs` sends the typed operation through native ingress;
`cells/application.rs` routes by repository UUID and executes a bounded SQL
transaction. The native SDK supplies the current owner and its output gate.
Read receipts bind repository identity and application revision. Serving never
creates missing application schema.

## Run the current development build

Build the frontend before Rust because `build.rs` embeds `packages/ui/dist/index.html` and rejects symlinked assets.

### Meet the prerequisites

You need:

- Node.js 22.12 or newer
- Rust and the repository's locked Cargo dependencies
- An existing S3 bucket, GCS bucket, or Azure Storage account/container
- Storage credentials in the environment
- Writable temporary space for receive packs and index sidecars
- A unique Cargo target directory on the mounted workspace volume

Set `TMPDIR` to a suitable volume when the system temporary directory is small or read-only. Follow the repository disk policy and never fall back to a local `target/` directory.

### Build the embedded application and server

This sequence installs the locked frontend dependencies, builds the React bundle, and compiles the release server:

```sh
npm ci --prefix packages/ui
npm run build --prefix packages/ui
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-server-dev" \
  cargo build -p crab-server --release --locked
```

### Configure one storage root

The process configuration selects listeners, one physical storage root, and
optional identity. Repositories are durable catalog records below that root;
they are not repeated in every pod's configuration. Shared xorbs, shards, and
their ref registry live below the same root in `.crab/`, so the configured root
is also the workload-identity and backup boundary.

```toml
listen = "127.0.0.1:8788"
management_listen = "127.0.0.1:8789"

[cells]
data_dir = "/var/lib/crab/cells"
local_disk_limit_bytes = 34359738368
peer_advertise = "https://localhost:8789"
failure_zone = "local"
peer_certificate = "/run/secrets/crab-peer/tls.crt"
peer_private_key = "/run/secrets/crab-peer/tls.key"
peer_ca = "/run/secrets/crab-peer/ca.crt"

[storage]
url = "s3://your-bucket/repositories"
```

`[cells] durability` defaults to `"fleet"`, which permits follower-fsynced
proof before an exact object root is published. For a fresh S3 deployment that
requires object-root proof before every acknowledgement, set
`durability = "object"` on every node. The object profile still retains the
follower store and log transport for recovery of older log epochs. Do not roll
an active fleet to object mode until all fleet owners have drained, every
accepted ticket has exact object coverage, and their node logs have retired.

Choose exactly one URL form per deployment:

| Provider | URL | Ambient credential source |
| --- | --- | --- |
| Amazon S3 or compatible development service | `s3://bucket/root` | AWS SDK environment/container chain |
| Google Cloud Storage | `gs://bucket/root` | Application Default Credentials or metadata server |
| Azure Blob Storage | `az://account/container/root` | Azure environment, managed identity, or workload identity |

The root prefix is required. The server derives all catalog and identity-state
keys below it, so two deployments share a repository universe only when their
normalized provider, account/container, and root prefix match.

```mermaid
flowchart TB
    Root[(provider root)]
    Catalog[.crab/http-server/v1/catalog.json]
    Auth[.crab/http-server/v1/auth/...]
    Service[your-team/service]
    Docs[your-team/docs]
    Root --> Catalog
    Root --> Auth
    Root --> Service
    Root --> Docs
```

Do not point the server at a bucket/container root. A nonempty application root
prevents catalog and session objects from colliding with unrelated workloads.

### Configure storage credentials

Production deployments should use workload identity. For local RustFS, set
these S3-compatible values in a private environment file:

```sh
AWS_ENDPOINT_URL=http://127.0.0.1:9000
AWS_ALLOW_HTTP=true
AWS_VIRTUAL_HOSTED_STYLE_REQUEST=false
AWS_ACCESS_KEY_ID=your_access_key_here
AWS_SECRET_ACCESS_KEY=your_secret_key_here
```

Do not put storage credentials in `server.toml`, source files, logs, or frontend assets.

### Create or adopt repositories

Create initializes the canonical Git repository, publishes an
`empty_cell_pending` catalog record, publishes the repository application's
initial SQLite/LTX root, then marks the record `cell_ready`. Exact retries resume
the same catalog UUID. Adopt authenticates the complete capsule-v2 view,
validates every embedded Git pack, performs a bounded walk of all reachable
refs, proves every reachable Crab pointer is represented by the authenticated
catalog, and hashes every shard/xorb body plus every reachable LFS object below
the configured root. It then records `empty_cell_pending` and publishes a new
empty application Cell. A missing, corrupt, or conflicting dependency leaves
the catalog unchanged. Adopt never converts arbitrary object prefixes, and old
collaboration application data is not imported.

The explicit initializer publishes schema using object durability, joins its
native node, and waits for its temporary lease to expire before marking the
catalog ready. Repeating create/adopt resumes the same repository identity.
Existing catalog-ready repositories require their native schema; missing state
fails startup. There is no previous-runtime storage reader or layout detector.

```sh
SERVER="$CARGO_TARGET_DIR/release/crab-server"
"$SERVER" --config /secure/server.toml repository create \
  --owner your-team --name your-project --prefix your-team/your-project \
  --default-branch main --members-file /secure/members.toml
"$SERVER" --config /secure/server.toml repository list
"$SERVER" --config /secure/server.toml cells status \
  --owner your-team --name your-project
"$SERVER" --config /secure/server.toml cells capacity --json
"$SERVER" --config /secure/server.toml cells runtime
"$SERVER" --config /secure/server.toml serve
```

`cells status` uses the running server's private mTLS management listener and
returns the native runtime version, repository UUID, application revision, and
lifecycle. `cells capacity --json --live` inspects that server's startup envelope;
without `--live`, it calculates an envelope for the current process.

Release bootstrap, pinned-backup, and node-session commands are unavailable. Cellar 0.1.2
does not supply equivalent APIs. Keep the old deployment and complete storage
root until an explicit data migration has been verified. A new storage prefix
is a separate deployment, not an import of collaboration data.

### Understand local trust mode

Without browser authentication, the server accepts loopback listeners only. This mode trusts one
local operator and exposes every cataloged repository to that principal.

The public listener exposes `GET /livez` as a storage-independent load-balancer
liveness probe. It accepts an IP-valued `Host` because ALB health checks address
tasks directly, returns no runtime state, and never substitutes for readiness.
The private management listener owns `GET /healthz`, `GET /readyz`,
`GET /integrityz`, and `GET /metrics`. The public listener does not expose management routes. Use
`healthcheck` to call readiness on the configured management address.

## Run the container

The checked-in image builds the locked React application and Rust server from
digest-pinned bases. Its build context excludes environment files, private-key
formats, repository metadata, Terraform state, dependency trees, and local
build output so those inputs cannot enter a remote BuildKit cache. The runtime
installs no packages, runs as UID/GID 10001, embeds the frontend, and uses a
dedicated temporary directory.

### Use your existing AWS bucket

The [single-container quick start](deploy/README.md#run-one-container-with-your-aws-bucket)
requires only `AWS_BUCKET` and standard AWS credentials. The no-argument image
entrypoint generates config and a stable private TLS identity, prepares Directory
through the explicit management command, and supervises the server and bundled
proxy. Configured S3 signing regions are shared by the Git/catalog store and
native Cellar, including administration through `docker exec`.

This is a local single-operator profile with host-loopback publication. Explicit
`--config` deployments retain their authentication, TLS, initialization and
orchestrator lifecycle contracts.

### Start the complete local stack

Docker Compose provides the shortest path from checkout to a usable server:

```sh
docker compose --file crates/crab-server/deploy/compose.yaml \
  up --detach --build --wait
```

The stack provisions its own private RustFS bucket and durable catalog, then
idempotently creates `demo/hello`. Open
<http://127.0.0.1:8788/demo/hello> or inspect discovery directly:

```sh
curl --fail http://127.0.0.1:8788/api/repos
```

Only Caddy's public port is published, and only on host loopback. Caddy shares
the server container's network namespace and forwards streaming bodies to
Crab's `127.0.0.1:8788` listener. Crab therefore retains the same local-trust
security rule as a direct development process; its `127.0.0.1:8789`
management listener and RustFS remain unreachable from the host network.

```text
host loopback                  shared container namespace       private network
127.0.0.1:8788 ──> Caddy :8080 ──> Crab 127.0.0.1:8788 ──> RustFS :9000
                                      │
                                      └──> management 127.0.0.1:8789
```

The named `repository-data` volume survives `docker compose down` and server
recreation. Use `down --volumes` only when intentionally deleting the local
catalog and repositories. This profile has synthetic credentials and no OIDC;
never expose it remotely. Use the orchestrator profiles below for team service.

The complete command set, image overrides, port and scratch sizing, repository
administration, and teardown behavior live in `deploy/README.md`.

### Build the image

Build from the repository root so Docker can access the workspace and frontend inputs:

```sh
docker build \
  --file crates/crab-server/deploy/Dockerfile \
  --tag crab-server:local \
  .
```

### Prepare runtime files

Copy `crates/crab-server/deploy/server.example.toml` outside the checkout.
Replace its identity and storage-root values.

Keep these inputs separate:

- `server.toml`: listeners, storage root, and browser authentication configuration
- `crab-storage.env`: private storage credentials
- `oidc-client-secret`: optional confidential-client secret
- `state-key`: at least 32 random bytes, stable across every replica and rollout
- `/var/lib/crab/tmp`: writable transient pack and index space

### Initialize and run the container

Use the [deployment profiles](deploy/README.md) to mount configuration, private
TLS identity, and writable native cache directories. The binary inside the image
uses the same `repository create`, `repository adopt`, and `serve` commands as the
local build. No release-bootstrap command or init container runs before serving.

The Compose `repository-init` service initializes application schema before its
server starts. Team deployments must initialize cataloged repositories explicitly
and provide a stable, node-specific advertised peer endpoint.

### Probe health and scrape metrics

The public listener serves `/livez`. Private mTLS management serves `/healthz`,
`/readyz`, `/capacity`, `/integrityz`, and `/metrics`. Readiness checks catalog and
native admission health plus repository read readiness. Startup verifies every
ready repository's existing native application schema before public serving.

`cells metrics` fetches the private Prometheus sample. Crab exports HTTP,
transfer, admission, catalog, and Git projection metrics; native event metrics
come from Cellar. `crab_cellar_pending_events`,
`crab_cellar_retired_events_total`, `crab_cellar_rss_bytes`, and
`crab_cellar_open_file_descriptors` use actual SDK samples. Native gauges report actual observations.

### Attribute acknowledged Cell writes

Repository mutations and their replay receipts commit atomically. Rejected
application writes roll back through a nested SDK transaction while retaining
their decision. Native response construction uses Cellar's ownership and
durability output gate. HTTP delivery may still fail after commitment: retry
with the same submission ID and payload to recover the original outcome.

Use the native event metrics, product HTTP logs, and durable process smoke test
to attribute current runtime behavior. The
[Cellar contract](CELLAR.md) describes the two durability settings and shutdown
modes. Mixed-version rollout and protected cloud fault matrices remain unqualified.

### Keep metric cardinality bounded

Metrics use bounded `method`, `outcome`, and `class` labels. They never include
repository names, paths, principals, request IDs, or storage keys. Request
duration and in-flight gauges retain ownership through the response body, so a
long Git, LFS, archive, or release stream remains visible after its handler has
returned. Body errors and client aborts have separate counters.
The `git_transfer` available-permit gauge describes the current process's
fast-path guard. The `crab_server_transfer_admission_rejections_total`
counter distinguishes deployment-wide `capacity` rejection from
`coordination` failure.

### Choose an orchestrator

Use the portable Helm chart for an EKS, GKE, or AKS team deployment. The
provider Terraform roots create dedicated versioned storage and workload
identity for an existing cluster.

| Boundary | Enforced contract |
| --- | --- |
| Image | Immutable digest only |
| Availability | At least three replicas, disruption budget, and hard node/zone spread |
| Container | Non-root, no Linux capabilities, read-only root, and bounded scratch |
| Network | Public application port only, ingress NetworkPolicy, and optional TLS ingress |
| Identity | Provider ServiceAccount integration; no static credentials or credential-source overrides in `extraEnv` |
| Storage transport | No custom provider endpoint, signature bypass, or cleartext override in `extraEnv` |
| Admission | Dedicated namespace pinned to the Restricted Pod Security policy |
| Observability | Optional autoscaling, `PodMonitor`, and alert rules; management port remains private |
| Shutdown | 15-second route-convergence delay plus a 630-second application drain budget |

`AWS_REGION` and `AWS_DEFAULT_REGION` are the only provider-prefixed
environment values accepted because EKS needs explicit routing without static
credentials. Cluster egress policy must still allow DNS, workload-identity
exchange, object storage, and OIDC endpoints.

S3 and GCS retain noncurrent versions for a configurable 90-day recovery
window and abort one-day-old incomplete multipart uploads. Azure versions
remain unexpired because its lifecycle API cannot express the same safe
noncurrent-age boundary.

The chart rejects disruption budgets that leave no minimum replica evictable
and requires hard placement across at least three nodes and two zones. Use atomic
Helm upgrades with bounded revision history so a failed readiness rollout
returns to the previous release state.

The ECS Fargate task definition remains an evaluation profile. Fargate limits a container stop timeout to 120 seconds, which can interrupt Crab operations that run for up to ten minutes. Prefer the Kubernetes chart until abrupt-process-crash qualification closes that gap.

```mermaid
flowchart LR
    EKS[EKS + Pod Identity]
    GKE[GKE + Workload Identity Federation]
    AKS[AKS + Workload ID]
    Helm[One Helm chart]
    ECS[ECS/Fargate task role]
    S3[(S3)]
    GCS[(GCS)]
    Azure[(Azure Blob)]
    EKS --> Helm --> S3
    GKE --> Helm --> GCS
    AKS --> Helm --> Azure
    ECS --> S3
```

See `deploy/README.md`, `deploy/terraform/README.md`, `deploy/helm/crab-server/README.md`, and `deploy/operations.md`. Lambda is intentionally excluded from the full data plane because Git and LFS require long streaming requests, large bodies, and bounded scratch that do not preserve the same contract through Lambda/API Gateway buffering and limits.

## Repository browser and application APIs

The browser and JavaScript Object Notation (JSON) API share the same authorization, snapshot, publication, and storage contracts. Browser links keep a selected branch for navigation and pin historical pages to a full commit OID.
The blame view links commit OIDs and subjects to immutable commit details. Its attribution and source panes are separated by a pointer- and keyboard-resizable handle; arrow keys resize incrementally, Home and End select the supported bounds, and double-click restores the default.

### Scan the browser feature map

| View | Capabilities |
| --- | --- |
| Repository root | Selected commit, file table, repository details, rendered README, request timing, and Code menu |
| Branch and tag picker | Search, keyboard navigation, default-branch state, and writer-only creation from the viewed commit |
| Branches and tags | Natural sorting, protected/default labels, copied names, immutable tips, default comparison, and guarded deletion |
| Tree and file workspace | Resizable lazy folder-first tree, preserved expansion, breadcrumbs, source, current-file code symbols, format-aware preview, blame, raw bytes, copy, download, edit, and delete |
| History and comparison | Signed pagination, first-parent path history, a persistent change tree with independently scrolling jump-linked diffs, split/unified layouts, and exact revision links |
| Go to file | Bounded full-tree fuzzy search without blob reads; `T` opens and focuses search |
| Search code | Literal UTF-8 text search on a selected branch, tag, or commit, with directory and case filters, snippets, and commit-pinned line links |
| Releases | Releases/Tags navigation, search, drafts, publication, edits, deletion, source ZIPs, and binary assets |
| Pull requests | Conversation, Commits, Checks, Files changed, reviews, shared Markdown formatting, approval state, and merge controls |
| Review requests | Versioned requests for people and teams, inbox delivery, cancellation, submitted-review fulfillment, and reviewer sidebar |
| Pull readiness | `PUT /api/repos/{owner}/{name}/pulls/{number}/readiness`; list/detail responses include `draft` |
| Issues and metadata | Searchable issues, comments, labels, assignees, Markdown formatting and preview, and conflict recovery |
| Settings | Default branch, exact protection rules, and archive state with stale-version checks |
| Appearance | System, light, and dark themes persisted across desktop and narrow layouts |

### Browse repository data

`GET /api/repos` returns the repositories visible to the current principal. Repository reads use `GET /api/repos/{owner}/{name}/{action}`.

| Action | Result |
| --- | --- |
| `refs` | Symbolic HEAD, unborn HEAD, branches, tags, peeled targets, and protection state |
| `commit` | One commit, or the latest first-parent commit that changed a path |
| `commits` | Paginated first-parent history, or the all-parent `rev - base` set |
| `tree` | Paginated raw-byte directory entries |
| `tree-attribution` | Last-change commit summaries for one exact tree page |
| `search` | Bounded fuzzy file-path search without blob-body reads |
| `code-search` | Bounded content search of a verified commit with an exclusive continuation |
| `file` | Highlightable file metadata and content |
| `blob` | Exact Git blob bytes as an attachment |
| `asset` | Signature-checked PNG, JPEG, GIF, or WebP bytes served inline |
| `changes` | Changed paths and object metadata for a comparison |
| `diff` | Split or unified file diff input |
| `blame` | First-parent blame ranges, backed by the generation-bound commit graph |

Common query parameters follow these contracts:

| Parameter | Contract |
| --- | --- |
| `rev` | Ref or full commit OID. Defaults to symbolic HEAD |
| `path` | Optional UTF-8 Git path |
| `path_hex` | Optional hexadecimal raw Git path. Use exactly one of `path` or `path_hex` |
| `q` | Required search query from 1 to 128 characters |
| `limit` | Page or search limit from 1 to 200 |
| `cursor` | Repository-bound signed cursor for directory or history pagination |
| `base` | Optional comparison base. Changes and diffs default to the first parent |

Git paths are byte strings, not filesystem paths. The server does not normalize them. Hex encoding preserves names that are not valid UTF-8.

The repository table first requests `tree` and renders that response without
waiting for history. It then requests `tree-attribution` with the same revision,
path, cursor and limit. Both successful responses include `generation`, `commit`
and `directory_oid`; the browser also compares every entry path and object ID
before adding `last_commit`. A moving branch or navigation race therefore cannot
attach metadata from a different snapshot.

When canonical generation-bound path state is ready but its SQLite projection
is still catching up, `tree-attribution` reads the exact requested paths from
that persistent index while the recurring background projection continues.
When path state itself has not been published, the endpoint returns HTTP 202
with `state: "indexing"`, `retry_after_ms`, and `Retry-After`; the browser keeps
the rendered tree and retries. Corrupt published metadata returns HTTP 503 and
starts a CAS-fenced rebuild. No case falls back to an interactive history scan.
Large initial builds checkpoint verified prefixes every 32 commits; exhausting
one bounded maintenance pass remains a 202 indexing state and the next pass
resumes that exact Git generation.

Browse-reader and background source caches share a two-second change probe of
the capsule root, a stable visible-ref capture and the bounded browse-index
record. Ref CAS can change without changing the root ETag, and graph/path
publication can change without moving a ref; both are checked. Immutable path
descriptors are verified on source changes, explicit invalidation and expiry of
the existing hourly scrub interval. Transport failures retain their source
error; missing or corrupt path metadata retains the corruption response.

One maintenance worker runs per repository. Completion is retained only when
the owned index pass verified the complete source and Cell promotion matched
it. Lease contention or a superseded pass cannot establish completion. Repair
requests and source changes reopen maintenance; unchanged completed epochs are
not repeatedly imported between scrubs. Cell reconciliation does not preload
tree rows: tree/blob responses continue through the verified remote Git reader.
Attribution batches obey both row and serialized-byte limits, preserving raw
path bytes. Reconciliation streams authenticated path-state nodes, edges and
summaries in dependency order, without assembling the complete path-state index
or reading raw Git commits. Each layer's complete hash and physical page closure
must validate before the scan finishes. Partial ingestion remains a provisional
epoch; only successful EOF and a matching source permit promotion. Maintenance
retains the current layer's component dictionary/page audit and a bounded page
cache, and performs a full streaming hash pass before its page traversal.
The commit graph still loads eagerly; bounded opening is a separate milestone.

Projection summaries include retained graph ordinals, including commits removed
by force-push. Foreground revision resolution checks visibility before consulting
those rows. Their presence never grants access to retained history.

Projection reads and ingestion use the same native repository Cell as
collaboration. Cellar owns activation, retention, eviction, and shutdown.
The canonical application schema stores commit summaries and epoch membership;
it is created only during explicit repository initialization. Serving does not
run schema migrations or recreate missing application state.
The full Next.js idle/load gates still require qualification; focused projection
tests do not establish production throughput.

Root commits compare against an empty tree. Path history follows the exact first-parent path. Commit history with `base` instead returns commits reachable from `rev` but not from `base`, across every parent.

For a PR comparison, call `changes` with `base`, `rev`, and `merge_base=true`.
The response includes `base_tip` and `commit` from a single ref capture, plus
`base`, the effective Git merge base. Pin individual `diff` requests to that
effective `base` and `commit`. Unrelated histories return 422 `no_merge_base`.
When several best ancestors exist, the read selects the lowest object ID for
stable file and review anchors. Git merge-commit generation requires a single
best ancestor and asks the author to resolve more complex histories in Git.
Per-file diffs and review anchors resolve the selected path at those commits
and read its blob-backed content. A mode change counts as a file change even
when both sides have the same blob ID. The complete change list has its own
comparison budget.

### Search repository code

**Search code** can find file contents, file paths, or symbol definitions in
one repository. Select a branch, tag, or full commit, optionally restrict the
search to an exact directory, and select **Match case** when needed. Each
excerpt links to its file and line at the commit searched.

Add qualifiers to the search text to narrow a query. For example,
`language:rust path:/src/**/*.rs symbol:parse` finds Rust definitions in
`src` and its subdirectories. Bare terms match file contents or paths by default.
**Search in** can restrict them to one field. Use `repo:owner/name` to select an exact
repository and `org:` with the organization namespace from the repository
URL or its UUID. Both selectors use current repository access. A query
containing only filters lists matching file paths.

`path:` matches a case-insensitive substring of the repository-relative path.
Use `*` or `?` within one path component and `**` across directories.
For a root-anchored directory glob, use a leading slash, as in `path:/src/**`.
Quotes keep wildcards literal and allow spaces, as in `path:"release notes"`.
Adjacent terms and qualifiers are joined by `AND`. Use uppercase `AND`,
`OR`, and `NOT`. `NOT` binds first, then `AND`, then `OR`. Parentheses
group expressions. For example, `(dial OR reconnect) NOT path:tests/` matches
either term outside the tests directory. Separate terms can match different
lines in the same file. Quote a phrase to require adjacent words in order.
For mixed queries, `content:` and `symbol:` scope the term they prefix.
`language:` uses the file extension and accepts C, C++, C#, CSS, Go, HTML,
Java, JavaScript, JSON, Kotlin, Markdown, PHP, Python, Ruby, Rust, Shell,
SQL, Swift, TOML, TypeScript, XML, and YAML. Quote text such as
`"repo:owner/name"` to search for the qualifier itself.

Wrap a pattern in slashes to search with a regular expression. For example,
`content:/phase [0-9]+\nstatus OK/` matches text across lines,
`symbol:/^parse_(file|tree)$/` matches definition names, and
`path:/^src\/.*\.rs$/` matches paths. **Search in** also applies to a bare
`/pattern/`. Escape a slash inside the pattern with `\/`. Quotes make an
expression literal. Content expressions see the full UTF-8 file, with
match fragments linked to their own lines. Larger highlights are shortened.
Match case sets the default for content and symbol terms, and an inline
flag such as `(?-i)` can override the default. The Rust regex engine limits
program size and nesting, and supports patterns without look-around or
backreferences.

`GET /api/repos/{owner}/{name}/code-search` takes `q`, optional `rev`, an
optional directory `path`, `kind=any|content|path|symbol` (default `any`),
`case_sensitive` (default `false`), `limit`
(1–40 matching files, default 20), and an opaque `cursor`. It returns
`{commit, items, scanned, skipped, next}`. Each file carries `path`,
`path_hex`, `more_lines`, and up to six excerpts with `line`, `before`,
`matched`, and `after`. Supply the same search parameters with `next`
to continue. A cursor binds the repository UUID, commit, and search filters.
Subsequent pages use the pinned commit even if the selected branch advances.
A query accepts up to 512 characters, 24 terms, 12 nested operators,
and 128 characters per literal.
Every request checks current repository access before returning any matches.

A direct page scans up to 128 files in canonical path order. Repository
search can use the index for the default revision and for symbol queries
when its tree matches the selected commit. Indexed pages open up to 128
candidates. Large blobs
above 256 KiB, binary or invalid UTF-8 data, separately stored Crab or LFS
content, and files beyond the parser budget are counted as skipped for modes
that read content. Empty pages can have a continuation. The browser retains
earlier results when a continuation needs to be retried.

**Symbols** matches literal text within definition names captured by the
Tree-sitter grammars for C, C++, Go, Java, JavaScript, Python, Rust, and
TypeScript, including TSX. Pick a result to open the definition at the pinned
line. Definitions follow the grammar's tag queries and exclude references
and plain text mentions. Unsupported file extensions are outside this mode's
scope. [GitHub symbol search](https://docs.github.com/en/search-github/github-code-search/understanding-github-code-search-syntax#symbol-qualifier)

### Search across repositories

Open **Search code** from the global header, or **Search organization code**
from an organization's repository list. Search file contents, paths, or definitions on
default branches. Use the same qualifiers, exact directory, and **Match case**
controls as repository search. A continuation binds the full query and the
current readable repository scope selected by its qualifiers. Results name
the repository and link to the exact Git commit, file, and matching line.
The workspace search URL preserves the query and filters across reloads and
can be shared. The recipient searches with their own repository access.

`GET /api/code-search` takes `q`, optional `organization` UUID and
`directory`, `kind=any|content|path|symbol` (default `any`), `case_sensitive`,
and `cursor`. It returns `groups`, `scanned` paths, `skipped` files,
`verified` content candidates checked against Git, and `next`. Groups carry
`id`, `owner`, `name`, `commit`, and matching `files`. Path matches use
the Git tree and have no excerpts. Each page examines up to four repositories
and returns up to 20 matching files. A Git scan considers up to 32 paths per
repository. An indexed content or symbol search can consider all indexed paths
and checks up to 32 candidates per repository. Follow `next` with the same
parameters, including after an empty page. A repository in progress keeps
the same commit until its scan finishes.

An indexed Boolean or regex query examines at most 4,096 path rows per
repository per page. Follow the continuation even after a page with no
matches.
For literals and regular expressions, Crab checks trigrams required by every
possible match. It derives the required byte runs from the parsed expression,
including alternation, repetition, inline flags, and Unicode case folding.
Expressions without required trigrams stay eligible for exact Git verification.
An unreadable file has unknown content, so a negated content term cannot treat
it as an absence.

The server builds a default-branch index in the background and keeps one
current published artifact per repository in object storage. It contains paths
and separate trigram filters for file text and supported definition names.
Index construction follows the verified tree and checks each blob's size
before reconstructing its contents. Eligible objects reuse the packed bytes
from the size check. Search uses the same bounded read to verify candidates.
The indexer parses each page in parallel under the shared Git CPU limit
and assembles the filters in Git path order.
Larger files get larger text filters to reduce false positives. An
index answers only for its exact Git tree. Crab verifies returned paths, text,
and definition locations against Git. It reuses unchanged blobs' filters when
their parsing language is unchanged. Short literals and other terms that the
filter cannot rule out remain candidates. The page reports `indexed_repositories`,
`scanning_repositories`, and `limited_repositories`. Crab scans Git while a
matching index is unavailable or exceeds the artifact or memory limit.
The server rebuilds the new index format in its own storage slot, so replicas
running the earlier format can continue using their existing derived index.

Search considers current readable repositories and checks them again before
returning excerpts or a cursor. A continuation binds the account, filters,
and readable repository set. A grant or address change in that scope returns
HTTP 409, and **Start over** opens a new search.

The opt-in search corpus test uses pinned snapshots of Next.js, Neovim,
ripgrep, TanStack Query, and Zod. It publishes their trees with stock Git,
checks search before and during index builds, and runs four concurrent
path, literal, regex, and Boolean content-and-symbol searches after the indexes
are ready. It exhausts result pages, opens a commit-pinned result in Chromium,
and checks continuation invalidation after grant removal. With
`agent-browser` and the built UI available, set
`CRAB_SEARCH_CORPUS_DIR` to the directory containing the source repositories
and `CARGO_TARGET_DIR` to the per-checkout build directory. Provide
`CRAB_HTTP_CELL_TEST_BUCKET`, `CRAB_HTTP_CELL_TEST_ENDPOINT`,
`CRAB_HTTP_CELL_TEST_PREFIX`, and fixture-only AWS credentials for an isolated
RustFS bucket on loopback. Run the test with
`cargo test -p crab-server --lib --locked workspace_search_indexes_and_checks_a_real_repository_corpus -- --ignored --nocapture`.
The test reports index build time, p50 and p95 page latency, observed backend
read operations, and Git content verifications. The fixture pauses unrelated
browse indexing while it measures search. An earlier local RustFS run examined
38,135 path rows across three repositories. A selective literal went from
1,235 content verifications over 38 pages to 110 over four pages.
Its regex equivalent verified 110 files,
the Boolean content-and-symbol query verified five, and the path-only query
read zero file bodies.

The five-repository run covered 42,479 paths. The first scoped search while
the index was absent took 78 and 405 ms across two pages. The largest index
built in 152 seconds after publication. Search and construction together
accounted for 57,511 observed backend reads during the build phase. Once the
indexes were ready, four concurrent query streams completed 81 pages without
an HTTP error and used 1,325 observed backend reads. Per-mode p95 page
latency was 87 ms for path, 168 ms for literal, 101 ms for regex, and 116 ms
for Boolean content and symbols. These are measurements from one unoptimized
server on arm64 macOS with loopback RustFS.

Repeating the same workload with the bounded blob read reduced observed
backend reads during search and index construction from 57,511 to 51,489.
The largest build took 153 seconds. Warm search used 1,259 backend reads
across 81 successful pages. Per-mode p95 page latency was 75 ms for path,
110 ms for literal, 60 ms for regex, and 88 ms for Boolean content and symbols.

Sampling the largest build showed time in tree-sitter and trigram generation
on a single parser worker. Using four partitions under the shared CPU budget
reduced the Next.js index build from 154 to 127 seconds and the Neovim
build from 12.8 to 8.3 seconds on the same local corpus. The concurrent
searches during construction completed eight pages at 56 ms p95.

### Create a repository in the browser

Choose **New repository** in the catalog to provision an empty private Git
repository with `main` as its default branch. The server derives the personal
namespace from the verified issuer and subject. Profile edits leave the
namespace stable. The local operator uses the `local` namespace. User
sessions can import into their own personal namespace.

`GET /api/repos/creation` returns `{owner, default_branch}`.
`POST /api/repos/creation` accepts `{request_id, name, description}` and
returns the cataloged `{id, owner, name}`. It requires a browser session and
CSRF protection. The creator receives repository Admin access. Conflicting
names are checked case-insensitively. The catalog reserves the name before Git
and Cell initialization and exposes it after Cell readiness. Retry an
interrupted request with its original UUID and fields to reuse the reservation
and repository UUID. The form retains those fields and the UUID in session
storage until the request is confirmed.

Organization owners and members allowed by repository policy can choose
**Repositories → New repository** on the organization page or select the
organization in the home catalog's owner picker. The organization owns the
new private repository, and its
URL uses `org-<organization UUID without hyphens>` as the stable namespace.
Renaming the organization leaves repository URLs and IDs stable.

Current organization owners inherit repository Admin access. This grant is
resolved from the current organization role for browser and Git requests,
including requests made with an existing Git token. Git tokens retain their
original repository and read/write limits. Ordinary organization members can
receive baseline access from organization policy. Outside collaborators
require repository-specific grants. Removing an owner
removes their inherited access. Separately assigned repository grants follow
their own lifecycle.

`GET /api/organizations/{organization}/repositories/creation` returns the
destination when current policy allows creation. `POST` accepts the same body
as personal creation. The Organization Cell checks the current role and policy
before recording or replaying the submission. A member who creates a repository
receives its direct Admin grant. Current organization owners inherit Admin.
A retry uses the same account, submission ID, and fields. Organization imports
require a current owner.
`GET /api/repos?organization={organization}` lists only repositories the
caller can access. Managed namespaces accept new repositories through the
organization creation path.

Organization owners can read and update `/api/organizations/{id}/repository-policy`
and configure it in **Settings → Member privileges**. Updates carry the saved
`version`, `member_creation`, `member_transfer`, `member_deletion`, `private_forks`, `baseline`
(`null`, `read`, `write`, or `admin`), and `default_branch` (a short Git branch name).
By default only owners can create, transfer, or delete repositories, and
members have no base access. Owners review changes to base repository permissions
before saving them in the browser. Current organization members receive those
permissions in existing and new repositories. Existing private forks retain
the base permission captured when they were published. Outside collaborators
keep their repository grants. The highest applicable organization, repository,
and group role applies subject to credential limits. The People repository
access inspector shows the baseline separately.

New organization repositories start with the branch name configured in Member
privileges, initially `main`. Once repository creation is authorized, retries
use the branch name captured for that request. Git imports use the source
repository's default branch.

Upgrade serving replicas together before starting organization-owned
creation. Existing catalog entries keep their previous ownership and grants.

### Repository visibility

The **New repository** dialog shows the visibility choices for its selected
owner. Organization owners can create public or private repositories. When
members can create repositories, **Settings → Member privileges** limits
their choices to private only, public only, or both. **Import from Git** also
offers private or public for organization and managed personal destinations.

The creation options endpoint returns `visibilities`. Send `visibility` in
the creation request. Omitting it selects private, subject to current policy.
`member_creation_visibility` in the organization policy accepts `private`,
`public`, or `any`. Organization creation checks policy under the same
lease used for policy changes and binds the choice to the exact reservation.
The catalog makes a new public repository visible when its Git and Cell setup
are ready. A creation retry retains the original visibility choice and never
overwrites a subsequent visibility change.

In **Settings → General → Danger Zone**, an organization owner can make an
existing repository public or private. Organization owners can let repository
admins change visibility in **Member privileges**. Administrators can change
visibility in a managed personal repository.

Public repositories appear in **Public repositories** at `/-/public` and
support signed-out code browsing, source archives, and HTTP Git clone and
fetch. Visitors can also browse published issues, pull requests,
conversations, attachments, and
available same-repository pull request comparisons. Repository access is still
required for writes, draft reviews, settings, checks, and Git LFS transport.
Large files linked from published code can be downloaded through the browser.
Changing back to private removes public access on subsequent requests. Copies
already downloaded remain available to their recipients.

Publishing a repository detaches its existing private forks in the same
catalog update. They keep their private visibility and current repository
grants. Pending fork jobs must settle before the repository can become public.

`GET /api/repositories/{id}/visibility` returns the current setting and
eligibility. Send `version`, `value` (`public` or `private`), and the exact
`owner/name` as `confirmation` to `PUT` on the same route. Update
`member_visibility` in the organization repository policy to delegate the
change to repository admins. Upgrade serving replicas together before using
visibility changes.

### Find a repository

The home page's **All accessible** view searches every repository you can read.
Choose **Personal** to search your own namespace, or open an organization's
**Repositories** tab to search that organization's repositories. Search treats
words case-insensitively and matches the name,
qualified owner/name, or description. Exact names and name prefixes appear
before description matches. The organization tab keeps the search scoped to
that organization. Search terms and the current page are in the URL.

`GET /api/repos?limit=32` returns `{repositories, next}`. Each result has
`id`, `owner`, `name`, `description`, and `archived`. A result includes
`can_admin: true` when the caller has current repository Admin access. Provide
optional `q` (up to 200 characters) and `organization` (UUID). Pages accept a
`limit` from 1 to 50 and return `next: null` on the last page. Send the
returned continuation as `after` with the same search and organization.
Crab binds it to the signed-in account and rechecks current repository
access. If a continuation has gone stale, start from the first page.

Exact `owner` plus `name` lookups return the repository detail needed by
repository pages and settings. Search and exact lookup options are mutually
exclusive.

`GET /api/repos/personal` searches the current account's personal namespace.
`GET /api/actors/{actor}/repositories` searches a public profile's personal
namespace. The viewer needs read access to each returned repository in either
view. The server resolves the namespace from the Directory identity and keeps
it stable across profile changes.

`GET /api/organizations/{id}/teams/{team}/repositories` requires visibility
of the team and returns readable repositories with a grant for that team. The
response includes `team_access` per row, including source grants inherited by
forks. `can_manage_team_access` appears on directly managed rows for current
organization owners. Use the existing repository Team access settings to edit
grants. The scoped endpoints share `q`, `after`, and `limit` and bind their
continuations to the viewer, search, and repository scope.

### Rename a repository

Repository administrators can change the name in **Settings → General**.
Crab keeps the repository UUID, storage placement, Git refs, collaboration
records, and access grants. Existing links and smart HTTP Git remotes
continue to resolve to the current name while their old address is available.
Crab checks current access before redirecting. Claiming a former name for a
different repository replaces its redirect. Git tokens retain their original
repository UUID and permission ceiling after a rename.

`GET /api/repositories/{repository}/name` returns the canonical `id`, `owner`,
`name`, and version to a repository administrator. To rename it, send `name`,
the captured `version`, and a UUID `request_id` with `PUT` on the same path.
Use the same input and request ID to recover a lost response. The current
name version prevents a stale submission from overwriting a newer rename.
Archived repositories must be unarchived before renaming.
If an active organization name rule protects the current name, renaming
returns HTTP 403 with `repository_name_protected` unless the administrator
has an Always bypass for every matching protection. A lost successful response
can still be recovered if the new name becomes protected.

Upgrade serving replicas together before the first rename or transfer. The
shared catalog stores the revision and former owner/name pairs.
Clients that use an old remote can update it with `git remote set-url origin`
and the current URL.

### Edit a repository description

Open **Settings → General → Edit description** with Maintain or Admin access.
The description is one line of up to 350 characters and can be cleared. The
repository page and catalog use the published description.

`GET /api/repositories/{repository}/description` returns `{description,version}`.
Send a new description with that version to `PUT` on the same route. An
outdated version returns HTTP 409 when the submitted text differs from the
current description. Retry the same text to recover a lost response while it
is still current. An archived repository requires unarchiving before editing.

### Classify a repository with topics

Open **Settings → General → Edit topics** with Maintain or Admin access.
Add up to 20 topics of 1–50 ASCII letters, digits, or hyphens each. Crab saves
lowercase names in alphabetical order and removes duplicates. Topic links on
repository pages and catalog results open a filtered repository list. Topics
are visible to people who can read the repository.

`GET /api/repositories/{repository}/topics` returns `{names,version}`.
`PUT` takes the complete topic list with its captured version. An outdated
version returns HTTP 409 for a different list. Retry the same list to recover
a lost response while it is still current. Archived repositories must be
unarchived for edits. Upgrade serving peers together before editing topics.

The home and organization catalogs accept `topic:release-engineering`,
`-topic:legacy`, and topic counts such as `topics:0`, `topics:>2`, or
`topics:1..3`. Combine them with text and other repository filters.
Continuations bind the query and the matching readable catalog scope.

### Transfer repository ownership

In **Settings → General**, a repository administrator can create an offer for a
personal account by profile handle. For an organization, choose an eligible one
by name or enter its link or ID. The requester must have creation permission
in the destination. A transfer that changes the repository name requires an
owner of the destination organization to make the request. Confirm the current
repository name before creating the offer. Share the private offer link with
the recipient. The named account or a current owner
of the destination organization sees eligible offers in **Notifications →
Repository transfers** and must sign in and accept within 24 hours. The bell
also signals a pending offer. A current source administrator can cancel a
pending offer. The recipient can
also decline it. Organization owners can allow current members with direct,
team, or group Admin access to transfer organization repositories. Crab checks
source and destination permissions when the offer is created and again at
acceptance. Tightening either policy blocks outstanding offers from members
who lose permission. Archived repositories and repositories in a private fork
network are ineligible for transfer.
If the offer changes a protected repository name, acceptance also checks
the source requester's current ruleset bypass grants.

Acceptance retains the repository UUID, storage placement, Git history,
collaboration records, and direct collaborators. Former organization team and
SSO group grants are removed. The destination organization uses its current
access policies, or the destination personal account gains administrator
access. Existing URLs redirect after checking current access until the former
address is reclaimed. Issue a fresh Git token for the destination owner after
acceptance. That token can follow the authorized redirect from the old remote.

`GET /api/repositories/{repository}/transfer` returns the current name version,
`can_transfer`, and offer to a repository administrator. Organization offers
show the confirmed destination's name.
A `POST` to that route submits `version`,
`request_id`, destination `name`, and `recipient` (`{kind:"account",handle}` or
`{kind:"organization",id}`). `GET /api/repository-transfers/{id}` reads the
private offer. `PUT` on that path takes `decision` (`accept`, `decline`, or
`cancel`). `GET /api/repository-transfers/incoming` returns up to 20
recipient-authorized pending offers and a `next` cursor to pass as `before`.
Resolved, expired, and superseded offers leave the incoming list. The bell
reports `pending_transfers: true` when there is a pending offer and no unread
discussion notification is taking priority. The browser endpoints use the session and the usual CSRF protection.
Retry a lost response with the same request ID or decision. A changed source
address, a competing catalog update, or a claimed destination blocks acceptance.

### Delete and restore an organization repository

In **Settings → General → Danger Zone**, type the full repository name to
confirm deletion. Organization policy controls whether members with
repository Admin access can delete a repository. Deleting a repository also
removes its private forks from the active catalog. Deleting a fork leaves the
upstream repository available.

Organization owners can restore an eligible repository and its private forks
for 90 days under **Settings → Deleted repositories**. Restoration keeps the
repository UUID, refs, and collaboration history. Team grants need to be
assigned again. The original repository and fork names must be available.
An individually deleted fork cannot be restored on its own. The catalog
retains a storage reservation after expiry. This workflow does not purge data
from the configured object store.

`GET /api/repositories/{repository}/deletion` returns eligibility, name
version, and the number of affected forks. Send `request_id`, `version`, and
`confirmation` to `DELETE` on the same path. Organization owners can list
recent deletions at `GET /api/organizations/{organization}/deleted-repositories`
and restore at `PUT /api/organizations/{organization}/deleted-repositories/{repository}`
with `deletion_id`. Pass the returned `next` as `before` to page through
recent deletions. Reuse the same request ID after a lost deletion response.

### Git imports

The catalog page exposes **Import from Git** for an authenticated browser
session. The workflow accepts a public or private repository from an
allowlisted Git host, creates a new Crab repository, and copies its Git history
and refs into the configured object store. It does not migrate Git LFS payloads;
use the native Git/LFS workflow for repositories whose large files must be
copied as well.

| Endpoint | Contract |
| --- | --- |
| `POST /api/imports/git` | Starts an asynchronous import. The JSON body is `{request_id?, source, owner, name, description?, visibility?, token?}`. `visibility` is `private` (default) or `public`. `source` is an HTTPS or SSH clone URL (or GitHub `owner/repository` shorthand) whose host appears in `import.allowed_hosts`. HTTPS tokens are sent as bearer headers and remain in memory. Browser requests require the session CSRF token. An exact request UUID replay returns the existing job. |
| `GET /api/imports/git/{id}` | Reads the caller's job from object storage on any serving replica. States are `queued`, `running`, `succeeded`, `failed`, and `interrupted`. The response includes the saved `visibility`, `resumable`, and the destination link after success. |
| `POST /api/imports/git/{id}/resume` | Resumes a failed or interrupted job as the original account. Send the session CSRF token and a JSON body of `{}` or `{"token":"…"}` for a private HTTPS source. An active or completed job returns its existing status. |

Organization owners use **Import from Git** in the organization's
**Repositories** tab. The scoped endpoint
`/api/organizations/{organization}/repositories/imports/git` takes the same
fields except `owner`, which is derived from the organization UUID. Its
`/{id}` and `/{id}/resume` endpoints are available to current organization
owners. One owner can share the import status link with another owner, who can
resume the same job and repository. The imported repository grants Admin
access through current organization ownership.

Public imports require a managed personal namespace or an organization
destination. An import request ID fixes visibility alongside the source and
destination. Resuming preserves that choice and accepts a replacement source
token. Public visibility takes effect in the catalog when the verified Git
snapshot and Repository Cell are ready.

Organization imports also accept `properties` keyed by property definition
UUID. The job stores and returns the chosen values to organization owners.
Resuming reuses the saved values. Crab applies them before receiving Git refs
so property-targeted rulesets see the destination's values throughout import.

Crab checks organization authority at reservation, during native Git
publication, and at the decision to expose the repository. The final decision
is recorded in the organization Cell for the exact prepared repository and
snapshot, after repository Cell preparation. When authority ends before that
decision, the destination remains private until a current owner resumes it.

Status records live under `.crab/http-server/v1/git-imports/` within the configured
root. A worker updates the record every 15 seconds. After 60 seconds without
an update, an unfinished job reports `interrupted`. The browser stores only
the last job UUID for that account in the current tab and resumes status polling
after a reload. Choose **Resume import** to continue with the same destination
and job ID. If Git publication already committed, Crab can finish without
contacting the source. Otherwise, it clones again and requires the same default
branch and ref tips if the original attempt already pinned them. Supply a
fresh source token if the clone needs one. Jobs from before resumable import
support remain readable and have `resumable: false`. Upgrade serving replicas
together before starting new imports.

Configure additional Git hosts explicitly:

```toml
[import]
allowed_hosts = ["github.com", "gitlab.com", "git.example.internal"]
```

Hosts are exact, lower-case matches. The server accepts HTTPS and SSH clone
URLs only; file paths, `git://`, embedded credentials, query strings, and
fragments are rejected. Child Git commands disable inherited credential
helpers and HTTP redirects. Keep outbound network policy enabled as a second
boundary for private deployments.

On success, the importing identity is the new repository's administrator for
account-scoped imports in an authenticated deployment. A local-trust deployment
uses the local operator. Crab pins the default branch and ref tips, reserves a
destination, and publishes through the server's atomic Git receive. The catalog exposes the repository after it
verifies all refs and finishes Cell initialization. Git browse indexes and
attribution are then rebuilt in the background. Large mirrors, including
provider pull-request refs, can take longer during verified visibility planning.
Each server allows four concurrent imports. Cloning and publication have a
30-minute command deadline. Transient publication failures retry with bounded
backoff. A job that reserved a name retains that reservation for future resumes.
The runtime image includes `git`. Each import reserves up to 8 GiB from the
Cell local-disk budget while cloning and shrinks the reservation to the measured
mirror plus a command margin. `TMPDIR` needs space for Git process scratch
files. The server needs outbound access to the configured Git host or proxy.

### Private organization forks

Private forks are disabled by default. An organization owner can enable them
in **Settings → Member privileges**. When the organization allows forks, a
repository admin can disable them for an individual source in
**Settings → General → Forking**. Repositories allow forks whenever their
organization allows them, unless an admin opts out. Changing either setting
governs future forks. Existing forks retain their current access.

When both policies allow forking, a signed-in reader can choose **Fork
repository** in the source's Code view and select **Personal**. Organization
owners can also choose the source organization as the destination. Enter an
optional description and choose whether to copy only the default branch. The
browser selects **Copy only the default branch** initially. Clearing it copies
all advertised refs. A fork writer can bring in additional branches later on
the fork’s **Branches** page with **Copy from upstream**. Crab copies the
selected history through the native Git service and keeps the source repository
UUID as the fork's lineage. Before
publishing refs, Crab copies and verifies reachable Git LFS payloads. For native
Crab Xet files, it verifies the shared shard and xorb data and writes the
selected file bindings into the fork's own catalog. If content is missing or
corrupt, repair it in the source and resume the fork job. The ready fork's
Code view links back to the source. You can clone the fork and push independent
changes with its own repository-scoped Git token.

Organization forks inherit current source members, OIDC groups, and organization
team grants. Personal forks are listed under the account's personal repositories.
They inherit source organization and group access. The fork owner has at least
Write access while they have a qualifying fork grant. Direct grants to other
individual collaborators apply to the source repository. The upstream
organization's rulesets govern private forks, including initial Git publication.

Both private fork destinations capture the organization's base repository
permission when the fork becomes available. The captured permission applies to
current organization members. Forks published before this field existed have
no captured base permission. Current team and individually applicable grants
continue to be evaluated on each request. Sync, comparison, and pull request
actions that read upstream data also require current read access to the
upstream repository.

Removing a member retires their personal private forks of that organization's
repositories. An accepted **Restore previous access** invitation brings back
eligible forks under the same UUID and address when the source is still private
and in the same ownership incarnation. **Start fresh** discards eligibility for
those archived forks. Shared organization forks stay available through current
source grants. Making an upstream repository public detaches its existing
personal forks as private repositories owned by their individual accounts.

`POST /api/repos/{owner}/{name}/forks` accepts `{request_id, name, destination,
description?, default_branch_only?}` and requires the source repository ID
header and browser CSRF token. Set `default_branch_only` to `true` to copy the
default branch and its history. Omitting it copies all advertised refs. A
retry or resume uses the original description and branch selection.
`destination` is `personal` or `organization` and defaults to `organization`.
Personal forks use the caller's `/api/imports/git/{id}` status and resume routes.
Organization forks use the organization import routes and require a current
owner. Jobs verify the pinned refs and current source access and policies
before making the fork visible.
Repository admins can update the source policy with `PUT /api/repositories/{id}/forking`
and `{version, allowed}`. The exact repository lookup returns the current
setting and the organization policy to repository admins. Deploy the updated
server on all replicas before using these policies.

On the fork's **Code** view, select a branch and choose **Sync fork**. Crab
compares it with the upstream branch of the same name. Writers can choose
**Update branch** when it can fast-forward to the previewed upstream commit.
A divergence preserves the fork's commits and can be resolved in Git. The
update uses the fork's branch protections and rulesets, and transfers the new
commits and their required LFS and Xet content.

`GET /api/repos/{owner}/{name}/fork-sync?branch=refs/heads/main` returns the
branch relationship and both commit IDs. When the branch only exists upstream,
it returns `fork_missing` and a `null` fork tip. `POST` accepts `{branch,
fork_oid, upstream_oid}` for a fast-forward. To copy a missing branch, send
`fork_oid: null` and a new `request_id` UUID. The upstream tip and the fork’s
expected state must still match before publication. Reuse the same request ID
and tip to recover a lost copy response. A later deliberate copy has its own
request ID.

Choose **Compare with upstream** on the fork's **Code** view to compare any
fork branch with an upstream branch. The file list and diffs show changes
from the common Git ancestor to the fork tip. Changes made only upstream
are excluded. Branch selection does not write refs or copy Git objects.

For the comparison API, send `GET /api/repos/{owner}/{name}/changes` with
`rev=refs/heads/{fork-branch}`, `base=refs/heads/{upstream-branch}`,
`merge_base=true`, and `upstream={source-repository-uuid}`. Crab verifies
the UUID against the fork's lineage and applies the current inherited access
rules. The response identifies the ancestor as `base`, the upstream tip as
`base_tip`, and the fork tip as `commit`. File diff requests can pin `base`
and `commit` in the fork. Both `changes` and `commits` also accept exact
commit IDs in `base` and `rev` for a repeatable comparison.

Writers can open a regular or draft pull request in the upstream repository
from the same comparison. The request stores the fork UUID, branch name and
both inspected commit IDs. The upstream owns the conversation, reviews,
CODEOWNERS and merge requirements. Files and commits are read from the fork.
The base and head may have the same branch name. API clients post to the
upstream `pulls` route with `head_repository` set to the fork UUID, and supply
`expected.base_oid` and `expected.head_oid` from the comparison.

For Crab commit statuses and check runs on a fork-only head, use the upstream
endpoint with `?pull={number}`. Crab checks that the commit is the current
head of that pull request. The records use the upstream's check provider and
permission rules. Approved changes can use the upstream's enabled merge
methods, including merge commits, squash, rebase, and fast-forward. Publication
checks both refs under leases and copies the required Git, LFS, and native Xet
content. Squash and rebase transfer only dependencies reachable from their
result, preserving the chosen upstream commit history. Fork pull requests can
also receive [GitHub Actions results](#connect-github-actions) for their exact
head through the upstream's configured connection.

### Render repository content safely

Markdown README files render beneath directory listings. Recognized file views switch between source and a format-aware preview; ordinary UTF-8 Git blobs also offer blame.

Rust, JavaScript, TypeScript, Python, Go, Java, C, and C++ source files offer a searchable current-file symbol outline. Selecting a definition jumps to its line, and selecting a captured token shows same-file definitions and references. The Tree-sitter runtime, language grammar, and tag queries load lazily and parse inside a dedicated browser worker; this feature does not create a server-side or repository-wide symbol index. Results are search-based rather than compiler-semantic, so cross-file resolution and type-aware disambiguation remain out of scope.

Preview-capable formats include raster and SVG images, PDF, common audio and video containers, CSV/TSV, JSON/JSON Lines, Jupyter notebooks, Office Open XML and OpenDocument packages, SQLite, Parquet, Arrow/Feather, NumPy/NPZ, Safetensors, GGUF, ZIP-derived packages, and TAR archives. CSV/TSV, JSON/JSON Lines, Parquet, Arrow IPC/Feather, and SQLite open in a local SQL workbench with syntax highlighting, dialect- and schema-aware completion, a searchable multi-relation schema browser, sortable and searchable paginated results, and quick charts. DuckDB queries the flat and columnar formats; SQLite runs in a dedicated browser worker against an isolated copy and exposes its user tables and views. The workbench can explain a query, generate a column profile, stop a running query, export the loaded result as CSV or JSON, and restore recent successful, failed, or stopped SQL from browser-local history. Each query returns at most 1,000 rows and renders 100 rows per page; the source can contain far more rows. Parquet uses authenticated HTTP byte ranges so projection and row-group pruning do not require a full browser download, while CSV and JSON remain full-scan formats and Arrow and SQLite load the file into browser memory. Notebooks never execute cells or HTML outputs, and model previews inspect metadata without loading or running weights. Office previews preserve document text, worksheets, and slide structure rather than promising pixel-identical desktop rendering.

Preview parsers are loaded only for the selected format and run in the signed-in browser. Crab does not send repository contents to a third-party conversion service. Workbench results and its 15-entry-per-file run history remain in that browser; result exports are generated locally. DuckDB loads its signed Parquet and JSON WebAssembly extensions on demand from the official DuckDB extension service; the content security policy permits only that external connection. Source text is embedded in JSON only through 1 MiB; recognized larger text formats fetch their exact bytes for preview. Unrecognized binary formats use a bounded universal inspector with extension hints, magic-signature detection, a safe text sample when applicable, and a 512-byte hex/ASCII view. Whole-file interactive previews, including Arrow and SQLite workbenches, are capped at 50 MiB. The range-backed Parquet workbench is exempt because it bounds query output and can scan the source incrementally. Actual source capacity remains subject to the configured repository read budget, browser WebAssembly memory, query shape, and file encoding. Expensive joins, sorts, or aggregations can still exhaust a browser even when their output is small; use filters and projections, and prefer Parquet for repeated large-data analysis. Oversized formats without a range-backed reader retain exact raw and download actions with an explicit fallback instead of attempting unbounded parsing.

Relative links stay inside the selected repository revision. Relative raster images load through the signature-checked `asset` endpoint. SVG, other local formats, and external images remain links. This prevents repository content from becoming same-origin active content or forcing signed-in browsers to contact third-party hosts.

The file view shows the logical size of Git LFS and native Crab files.
**Download Git LFS file** or **Download Crab file** streams the payload for the
displayed commit. **Raw pointer**, the ordinary `/blob` endpoint, and ZIP
archives serve the committed pointer text.

`GET /api/repos/{owner}/{name}/lfs-content?rev=COMMIT&path_hex=PATH` requires
read access, a reachable full SHA-1 commit, and a valid Git LFS pointer at
that path. The server selects the object ID and size from the verified blob
and streams through the Git LFS transfer budget and integrity checks. The
response supports single byte ranges and `If-Range` with the object ETag.

`GET /api/repos/{owner}/{name}/crab-content?rev=COMMIT&path_hex=PATH` requires
the same repository access and commit identity and a native Crab pointer.
The server checks the file hash and logical size against the captured repository
catalog. It retains that catalog against garbage collection during reconstruction
and verifies the selected chunks and output length. Full reads also verify
the file's BLAKE3 hash. This endpoint accepts single byte ranges and
`If-Range`. `HEAD` validates Git visibility and catalog metadata.

For recognized formats, choose **Preview** to read the stored bytes at the
displayed commit. **Pointer** returns to the Git representation.
Parquet uses the range-backed SQL workbench. Whole-file previews are
limited to 50 MiB, with a 1 MiB limit for Markdown and notebooks. Larger
files remain available through the download action.

### Stream a repository archive

`GET /api/repos/{owner}/{name}/archive?rev=revision_name` streams a ZIP pinned to the selected commit.

Archive generation:

- Uses bounded remote tree and blob reads
- Creates no checkout or local Git object database
- Has a 10-minute transfer budget
- Caps the encoded response at 3 GiB
- Retains repository entry and uncompressed-byte limits
- Cancels traversal and releases capacity after a disconnect

The Code menu uses this route for **Download ZIP**.

### Create, edit, and delete files

Use `POST`, `PATCH`, or `DELETE` on `/api/repos/{owner}/{name}/contents`. Every request identifies an existing branch, its exact head, a raw path, and a commit message.

```json
{
  "branch": "refs/heads/main",
  "expected_head": "0123456789012345678901234567890123456789",
  "new_branch": "docs/update-reference",
  "path_hex": "646f63732f7265666572656e63652e6d64",
  "content": "# Reference\n",
  "message": "docs: add reference"
}
```

Create and edit requests include UTF-8 `content`. Edit and delete requests also include `expected_blob`. The optional `new_branch` publishes directly to a new proposal branch and leaves the source branch unchanged.

The server rejects:

- Stale branch or blob state
- Existing or namespace-conflicting proposal branches
- Existing-path creates and missing-path edits or deletes
- Invalid paths and unsupported tree entry kinds
- Unchanged edits
- Direct writes to protected branches
- Content larger than 900 KiB
- Commit messages longer than 256 characters

The server creates Git objects in process and sends the pack through the native publication path. Deletion prunes empty trees.

### Upload multiple files atomically

Use `POST /api/repos/{owner}/{name}/uploads` with 1 to 100 `path_hex` and base64-content pairs. Each file can contain up to 900 KiB, and decoded batch content can total up to 4 MiB.

The server rejects duplicate paths, overlapping paths, and existing entries before publication. One invalid file leaves the entire batch unpublished. Binary bytes survive unchanged.

### Create and delete branches

Use `POST /api/repos/{owner}/{name}/branches` to publish an existing visible commit as a new branch:

```json
{
  "name": "feature/readable-docs",
  "source_oid": "0123456789012345678901234567890123456789"
}
```

The destination must be absent and cannot conflict with another ref namespace. The source must be a visible commit in the current snapshot. Protected names require the pull-request path.

Use `DELETE` on the same route with `name` and `expected_oid`. The server rejects deletion of the default branch, a protected branch, or a branch whose tip changed.

Both operations use native ref leases, GC fences, visibility evidence, journal publication, and cache invalidation. Branch creation uploads no Git objects.

### Change repository settings

Repository administration routes require `admin` access:

| Method and route | Request contract |
| --- | --- |
| `PATCH /api/repos/{owner}/{name}/settings/default-branch` | `name`, current `expected_head`, and target `expected_oid` |
| `PUT /api/repos/{owner}/{name}/settings/branch-protections` | `expected_version` and the complete `rules` set |
| `PUT /api/repos/{owner}/{name}/rulesets` | `expected_version` and the complete `rulesets` set |
| `PUT /api/repos/{owner}/{name}/settings/archive` | `expected_version`, `archived`, and exact `owner/name` confirmation |

Changing the default branch records a new symbolic HEAD through the ref journal. It copies no objects and applies immediately to browser defaults and native Git discovery.

Branch protection uses exact short branch names. A configuration can seed version zero; after the first administrative save, the stored object is authoritative across restarts and server instances. Set `require_conversation_resolution` to require resolved review threads.

Archived repositories remain readable and searchable. Browser writes, native pushes, and LFS uploads return a read-only error. Native Git and LFS check archive state again immediately before publication.

### Manage branch and tag rulesets

Open **Settings → Rulesets** to create, edit, disable, or remove a named ruleset.
Repository readers can open **Rulesets** from Branches or Tags to see active
rules. Pull requests list the active rulesets that match their base branch.

Choose a branch or tag target. Use short ref names and patterns such as
`release/*` for one path segment and `release/**/*` for nested names. Use
`~ALL` for every ref of the selected target and `~DEFAULT_BRANCH` for the
current default branch. A ref qualifies when any include matches and no
exclude matches. `?` matches one character within a segment. Positive bracket
classes such as `v[0-9]*` match character ranges.

Both targets can restrict creation, updates, and deletion independently.
**Restrict updates** freezes existing matching refs. An eligible **Always**
bypass can skip the rule for Git updates. A PR targeting a frozen branch
requires an explicit, authorized bypass of each applicable freeze rule.
The merge panel shows the restriction, and publication rechecks it under the
same policy lease as ruleset changes. Branch rulesets can also
require a pull request before an existing ref is updated, a minimum number of
eligible approvals, code owner review, conversation resolution, named checks, and specific connected
GitHub Actions workflows. Reviews and checks imply the pull request requirement.
Matching rulesets and classic branch protection apply together. Crab applies
the highest required approval count and includes every distinct required check
and workflow. Any applicable rule can require the head to contain the current
base commit or require the merge queue. Each setting requires at least one
required check or workflow.

**Require linear history** is available in classic branch protection and in
repository and organization rulesets. Crab checks the commits introduced by
each matching ref update, including objects already reachable from another
ref. For a new branch or commit-targeted tag, the check covers its history.
An existing merge below the previous tip can remain in place.

A PR with this rule uses squash or rebase. The merge panel shows the permitted
methods. If neither is enabled, a repository administrator can enable one in
merge settings. Queue policy uses the configured queue method. The manual CI
candidate uses the first enabled linear method and publishes the exact
tested commit. Final publication rechecks the rule and the candidate history
under the ref and policy leases.

`GET /api/repos/{owner}/{name}/rulesets` returns the version, rulesets, and
`can_manage`. Administrators can see disabled rulesets and replace the complete
collection with matching `expected_version`. Each entry contains `id` (UUID),
`name`, `target` (`branch` or `tag`), `enforcement` (`active` or `disabled`),
`include`, `exclude`, `restrict_creations`, `restrict_updates`, `restrict_deletions`,
`require_pull_request`, `required_approvals`, `required_code_owner_reviews`,
`require_conversation_resolution`, `require_up_to_date`, `require_merge_queue`, `require_linear_history`,
`required_checks`, `required_integrations`, and optional `review_dismissal`.
Branch-only settings must have
empty or zero values on tag rulesets. A stale replacement returns HTTP 409.

Rulesets govern Crab Server Git HTTP, browser Git changes, releases that create
tags, and PR merges. Deployments that rely on mandatory server policy must
limit direct object-store writes through storage IAM.

### Apply organization rulesets

Organization owners can open **Rulesets** on the organization page to manage
branch and tag policy. Target every repository, a set of repository UUIDs, or
repository names using include and exclude patterns, or repositories matching
custom property conditions. Dynamic targets also evaluate newly created repositories.

Repository readers see matching active rulesets under **Inherited from
organization**. Repository administrators can edit local policies. Organization
rulesets combine with local rulesets and classic branch protection at ref
publication, including organization Git imports. Approval counts use the
strictest requirement. Named status checks apply across repositories.
Individual GitHub Actions connections are configured per repository.
Organization rules can require the merge queue with a named status check.

`GET /api/organizations/{id}/rulesets` returns `version` and `rulesets`. To
replace the collection, send those entries with `expected_version` to `PUT`
on the same route. Each entry contains a `ruleset` and `repositories`. The
repository selector is `{"kind":"all"}`, `{"kind":"selected","ids":[...]}`,
`{"kind":"name","include":[...],"exclude":[...]}`, or
`{"kind":"properties","conditions":[{"property":"<uuid>","value":true}]}`.
For name targeting, enable **Prevent renaming of target repositories** or set
`protected: true` on the name selector. Active rules enforce this against
the current name at catalog publication, including transfers to a different
name. Each matching protection requires its own Always bypass. Upgrade all
serving peers before enabling this option.
Property conditions use all-match semantics. A multi-select condition tests
for one selected value. An `exclude: true` condition matches when the value is
absent or different. A stale write returns HTTP 409.

An explicit PR bypass references a local rule by UUID and an inherited rule
by `{"organization":"<uuid>","ruleset":"<uuid>"}`. The scopes are independent.
Organization team bypass requires active membership in the non-secret team
and current write access to the repository.

### Manage repository properties

Open **Organization → Properties** to define text, boolean, single-select, or
multi-select fields. Select up to 50 repositories across search pages and
choose **Edit properties**. The editor shows shared and mixed values. For
each field you can keep existing values, set an override, or restore defaults.
A default applies to repositories with no override, including new repositories.
To require a property for all repositories, specify a default of the property's
type. Repositories inherit that default until an override is set. Clearing the
override restores inheritance.
Enable **Require an explicit value** on a required property to require a choice
from people who can set it. This applies to new repositories, organization
Git imports and forks, transfer acceptance, and single-repository property
saves. A bulk edit can leave an explicit property alone or set an override.
Existing repositories with no override continue to inherit the default.
Previously accepted creation requests and import jobs retain their saved
choices on retry.
The **New repository** dialog shows creation-time property fields. Organization
owners can set all fields. Members can set fields delegated to repository
administrators. The organization creation options endpoint returns the eligible
`properties`. The creation request accepts initial values in a `properties`
object keyed by definition UUID. The exact request binds these values and
saves them before catalog publication.
One save applies the entire selection. A stale selection prevents every write
in the batch. Retry a lost response with the same submission to recover the
original receipt. Open the repository's **Properties** tab to view its effective
values. Public repositories publish these values to signed-out visitors.
Organization owners manage definitions and overrides. Enable **Allow repository
administrators to set this property** on a definition to delegate its value
to each repository's current administrators, including members of teams with an
Admin grant. On **Repository properties**,
**Edit properties** shows the fields the signed-in account can edit. Delegation
includes restoring defaults. Changes to a delegated value can change which
organization rulesets apply.

Select **Matching repository properties** in an organization ruleset and add
the conditions that repositories must satisfy. Built-in **Repository type**
matches source repositories or forks. **Repository name** matches a glob such
as `release-*`. Combine these with custom properties in one filter. Ruleset
checks use the cataloged repository type and name, including the reserved
destination when an import or fork is being published. In the API, built-in
conditions use `{"system":"fork","value":true}` or
`{"system":"name","value":"release-*"}`. Any condition may set `exclude`.
Upgrade serving peers together because native ruleset lookups now include the
repository type. Property changes and Git publication share the organization
policy lease. Imports hold that lease while rechecking creation restrictions and
publishing the catalog entry. A repository transfer starts a new set of explicit
overrides under the destination's property definitions. When a destination
organization owner reviews an incoming transfer, the offer response includes
the available `properties`. Send a `properties` object keyed by definition UUID
when accepting. The values become visible with the new ownership. Replay the
same values to recover a lost acceptance response.
Organization audit events record changes to definitions and repository values.

Use `GET /api/organizations/{id}/repository-properties` for the versioned
definitions and `PUT` with `expected_version` and `definitions` to replace
them. Use `GET /api/repos/{owner}/{name}/properties` for the definitions and
versioned overrides. A repository `PUT` accepts `schema_version`,
`expected_version`, and the complete `overrides` object keyed by property UUID.
`editable` lists the property UUIDs the signed-in account may edit. Definitions
use `values_editable_by` with `organization` as the default and `repository` to
allow repository administrators. A repository administrator's save must retain
the current overrides for organization-managed properties.
Required definitions use `required: true` and must have a valid `default`.
Set `require_explicit_values: true` on a required definition to enforce an
explicit selection.
Public-only readers receive `{properties:[{name,value}]}` with the effective
values of set properties. The organization schema and editing controls require
their respective signed-in access.
Organization owners can use `GET /api/organizations/{id}/repository-properties/values?repositories={uuid},{uuid}`
to capture the schema, repository IDs, ownership epochs, value versions, and
effective values. A `PUT` to the same path takes a `request_id`, `issued_at_ms`,
`schema_version`, the captured `repositories`, and a `changes` map keyed by
property UUID. Use `null` to clear an override. Reuse the same payload after
a lost response. The receipt contains the number of repositories updated.
Remove a rule's reference before deleting a definition. Retained definitions
keep their type and allow existing select values to remain valid.

### Filter organization repositories by property

Open the organization's **Repositories** tab and choose **Filter by property**
to add a qualifier to the search. Start typing `prop` in the search box to
choose a property and a suggested value. Use the arrow keys and Enter to
insert a suggestion, then submit the search. You can also type a qualifier
directly, such as `props.regulated:false` or `props.location:"US East"`. Prefix
a qualifier with `-` to exclude repositories with that value. Combine qualifiers
with name or description terms. Every qualifier must match. Property names and
boolean values ignore case. Text and choice values match exactly. A multi-select
qualifier matches one selected option. Defaults apply when no override exists.

Use `GET /api/repos?organization={id}&q={query}` to run the search. The scope is
one organization. `GET /api/organizations/{id}/repository-properties/filters`
lists the definitions available through current repository access. The catalog
checks readability before resolving properties and again before returning results.
The server considers up to 512 candidate repositories per page in batches of
128. Follow `next` even after an empty page. Continuations bind the account,
query, property revision, and readable candidates. If they change, start at the
first page.

### Filter repositories by archive and fork state

Use **Archive status** and **Fork status** on the home or organization catalog.
The controls add `archived:true`, `archived:false`, `fork:only`, or `fork:false`
to the search, and combine with text and custom-property qualifiers. `fork:true`
includes source repositories and forks, as does the unfiltered Crab catalog.
Fork rows carry a **Fork** badge.

The server reads archive state from the Repository Cell after checking repository
access. It examines up to 128 candidates per archive-filtered response. Follow
`next` even after an empty page. A cursor binds the archive revision of its
boundary repository and the readable candidates. If either changes, restart at
page one. Later pages read live archive state for their candidates.

### Filter by visibility and sort repositories

Choose **Visibility** to add `is:public` or `is:private`. **Sort repositories**
uses `sort:name-asc`, `sort:name-desc`, `sort:created-asc`, `sort:created-desc`,
`sort:updated-asc`, or `sort:updated-desc`.
Best match keeps the catalog's normal search order. Sorting runs on the server
across all matching repositories. A continuation binds visibility and sort, so
switching either starts at the first page.

Catalog registration records `created_at_ms` in the same write that reserves
the repository. Retries and later renames, transfers, and restorations retain
that date. Entries without a recorded date appear after dated repositories
when sorting by creation. The catalog leaves those dates blank.

Updated order uses `metadata_updated_at_ms`. This timestamp changes when
repository name, owner, description, topics, visibility, or fork details
change in the catalog. Repositories without a recorded edit sort by creation
date when available. Entries without either date sort last. An update can
invalidate an updated-order continuation and require restarting at page one.

### Grant ruleset bypass access

Ruleset administrators can grant bypass access to repository administrators,
repository writers, organization owners, and non-secret teams already granted
repository access. The actor must have current write access. A grant in
**Always** mode can bypass that ruleset during direct Git and browser writes.
**Pull requests only** applies to branch rulesets and requires explicit
confirmation on the PR merge panel. Other matching rulesets, classic branch
protections, repository merge methods and Git publication checks still apply.

The administrator ruleset response includes `bypass_options` for role and team
selection. Use the optional `bypass_actors` list on a ruleset with entries such
as `{"actor":{"kind":"team","id":"<uuid>"},"mode":"pull_request"}`. Role kinds
are `repository_admin`, `repository_write` and `organization_owner`. Readers
can view active rulesets without seeing bypass grants.

A PR merge may include `bypass_rulesets`, a list of eligible matching ruleset
UUIDs. The reserved merge binds those IDs to the request. Before publishing,
Crab checks the original actor's current authority and the selected rules.
Completed PRs record the selected IDs. Direct publications log the ruleset
IDs and committed Git transaction ID when bypass is used.

### Require conversation resolution

Enable **Require conversation resolution** in **Settings → Branches** or in a
branch ruleset. The merge panel shows the number of unresolved published inline
review conversations and links to the filtered list in **Files changed**.
Outdated threads count until resolved. General comments, replies and private
review drafts have no separate resolution state.

The merge route checks the count before admission and again before writing the
Git ref. During a pending merge, contributors can resolve existing threads.
New threads and reopen requests wait until the merge completes or aborts, so
the unresolved count cannot increase during publication.

### Create and manage releases

Release routes use durable request reservations and versioned records:

| Method and route | Purpose |
| --- | --- |
| `GET/POST /api/repos/{owner}/{name}/releases` | List or create releases |
| `GET /api/repos/{owner}/{name}/releases/generate-notes` | Preview generated Markdown for an exact target commit |
| `GET/PATCH/DELETE /api/repos/{owner}/{name}/releases/{number}` | Read, edit, or tombstone one release |
| `GET /api/repos/{owner}/{name}/releases/tag/{tag}` | Read by unique tag, including slash-containing tags; same draft visibility as numbered reads |
| `POST /api/repos/{owner}/{name}/releases/{number}/assets` | Upload a release asset |
| `GET/DELETE /api/repos/{owner}/{name}/releases/{number}/assets/{asset_id}` | Download or remove an asset |

Creation includes a stable universally unique identifier (UUID) in `request_id`, short `tag_name`, full target commit OID, title, Markdown body, prerelease flag, and draft state. The target must be a visible commit.

Repository writers can pick a previous tag and generate a preview in the new
release form or an existing draft. The preview includes merged pull requests
in the Git range, contributors, and a link to the commit history for that
range. Inserting the preview appends editable Markdown to the release body.

The preview route accepts a full `target_oid` and an optional `previous_tag`.
Omitting `previous_tag` selects the highest-numbered published release whose
tag still points to its recorded commit and whose commit is a strict ancestor
of the target. An empty string includes all history. An explicit tag must
resolve to the target commit or an ancestor. The response returns the pinned
`target_oid`, the selected `previous_tag` and `previous_oid` (both null for
all history), the selected `configuration_file` or null, and the Markdown
`body`. Crab checks merged pull requests in pages against the pinned Git
range. PRs outside the selected range do not count toward the 64 KiB body
limit. Git operation limits still apply to the scan.

To group release notes, commit a `.github/release.yml` or
`.github/release.yaml` file. Crab reads it from the selected target commit
and prefers the `.yml` file when both exist. API callers may pass
`configuration_file_path` to select another repository-relative file.
The preview identifies the file it used.

```yaml
changelog:
  exclude:
    labels: [skip-release-notes]
    authors: [automation]
  categories:
    - title: Features
      labels: [enhancement]
      exclude:
        labels: [internal]
    - title: Other changes
      labels: ["*"]
```

Global exclusions remove pull requests from all sections and from the
contributor list when they have no other included pull requests. The first
eligible category wins. Category exclusions let the pull request match a
later category. Use `"*"` to include changes that match no earlier category,
or omit `categories` to keep the default list. Label names match current
repository labels case-insensitively. Author selectors use current Crab
profile handles, with an optional leading `@`. The configuration is limited
to 32 KiB, 32 categories, 128 label entries, and 128 author entries.

An existing lightweight or annotated tag is accepted only when it peels to the exact target. A new published release creates a lightweight tag through canonical ref publication. Drafts reserve their release tag but do not publish a new Git tag until a version-bound edit publishes the draft. Republishing a withdrawn release verifies the existing Git tag again before notifying watchers.

Readers cannot list or open drafts. Writers can recover drafts after restart. Replaying the same request recovers the same release; changing the payload or racing for the tag returns HTTP 409.

Release limits include:

| Resource | Limit |
| --- | ---: |
| List page | 20 default, 50 maximum |
| Search query | 256 characters |
| Request body | 80 KiB |
| Markdown notes | 64 KiB |
| Tag name | 255 bytes |
| Title | 256 characters |
| Asset | 512 MiB |

Release asset uploads stream to immutable digest paths. The record stores asset ID, name, content type, byte size, SHA-256 digest, uploader, and creation time. Duplicate names return HTTP 422. Deleting a release keeps its Git tag and records a durable tombstone.

### Understand read caching and readiness

The repository cache checks the authoritative root, transaction-consistent ref
positions, and derived-index record after two seconds. An unchanged state
reuses its authenticated immutable reader; changed refs or indexes reopen it.
Git fetch always opens a fresh snapshot. Interactive reads have a five-minute
operation budget, an 8 MiB response limit, and process-wide admission for 16
concurrent requests.

Capsule maintenance publishes the verified graph before building complete path
attribution. Historical commit reads can use that graph while attribution still
reports indexing. Both publications use the same bounded derived record, exact
visible-state digest, generation ownership, GC fences and conditional updates.
A reusable attribution prefix is durably checkpointed before graph publication;
restart can resume it without exposing incomplete attribution as ready.

When a readable generation lacks its split commit graph, the server builds that derived index from bounded remote commit reads and attaches it before retrying deep blame. This process downloads no full packs and creates no checkout.

Catalog maintenance also repairs a missing standard Git pack sidecar. It downloads the canonical pack only when `.idx` or `.rev` is absent, verifies the manifest size, pack trailer, BLAKE3 identity, and object count, and rebuilds both sidecars with Gitoxide in temporary storage. The immutable store accepts the regenerated bytes only through content-addressed create semantics. Existing malformed sidecars fail closed, and pack bytes never stand in for journal-owned visibility evidence.

The management listener exposes the separate `GET /integrityz` contract. One
server replica acquires a renewable deployment-wide storage lease, runs the
background pass without blocking readiness, and publishes one bounded
CAS-protected aggregate report. Other replicas reuse that report instead of
redownloading every dependency. The pass repeats hourly, with at most two
repositories active and a twelve-minute cooperative budget per repository.
Adoption and integrity scrubbing admit up to 8 GiB of complete Git source
bodies and, separately, 8 GiB of installed pack input, matching the native
receive/import intake budget. Capsule control metadata and the uncheckpointed
frontier retain their separate 2 GiB limits.
These are independent phase limits; source framing counts toward the complete
source-body budget.
Each successful proof records the exact v2 state digest and counts for
Git-reachable Crab pointers, LFS objects, catalog files, shards, and xorbs.
Missing or corrupt dependencies return `503` from this endpoint while
`/readyz` continues to enforce only the bounded serving-readiness contract.
`202` means that no complete proof exists yet or the inspected state changed
during the pass. A failed pass retains the prior complete proof for audit.

`Server-Timing` separates repository open, read, application, and total handling time where applicable. It excludes HTTP response transmission. The browser measures its complete fetch and JSON round trip separately.

## Team sign-in

Team deployments use a provider-neutral authorization-code client with Proof Key
for Code Exchange (PKCE) and the S256 challenge method. The default `oidc`
provider discovers metadata and signing keys; the `github` provider uses
GitHub's OAuth endpoints and performs the code exchange and user lookup on the
server. The browser only receives the Crab session cookie. Neither mode stores
passwords or uses email as the authorization identifier.

### Configure the identity provider

Register this redirect URI with the provider:

```text
https://git.example.com/auth/callback
```

Use the provider's exact issuer string, including its path and trailing-slash policy.

```toml
listen = "0.0.0.0:8788"
management_listen = "0.0.0.0:8789"

[cells]
data_dir = "/var/lib/crab/cells"
local_disk_limit_bytes = 34359738368
peer_advertise = "https://node-1.internal.example:8789"
failure_zone = "us-west-2a"
peer_tls_server_name = "node-1.internal.example"
peer_certificate = "/run/secrets/crab-peer/tls.crt"
peer_private_key = "/run/secrets/crab-peer/tls.key"
peer_ca = "/run/secrets/crab-peer/ca.crt"

[storage]
url = "s3://your-bucket/repositories"

[auth]
provider = "oidc"
issuer = "https://identity.example.com/realms/team"
client_id = "crab-browser"
public_url = "https://git.example.com"
client_secret_file = "/run/secrets/crab-oidc-client-secret"
state_key_file = "/run/secrets/crab-state-key"
```

Omit `client_secret_file` for a public PKCE client. A secret file can end with one newline; other whitespace remains part of the secret.

To use a GitHub OAuth App directly, set `provider = "github"`, use
`https://github.com` as the identity namespace, and keep the same callback URI:

```toml
[auth]
provider = "github"
issuer = "https://github.com"
client_id = "1234567890abcdef"
public_url = "https://git.example.com"
client_secret_file = "/run/secrets/crab-github-client-secret"
state_key_file = "/run/secrets/crab-state-key"
```

GitHub OAuth requires a confidential client secret. The default endpoints are
`https://github.com/login/oauth/authorize`,
`https://github.com/login/oauth/access_token`, and `https://api.github.com/`.
An optional `[auth.github]` table can override all three for a provider proxy or
a loopback test service; `api_url` must end with `/`. The server sends the
short-lived GitHub access token only to GitHub's `/user` endpoint, never stores
it, and uses the stable numeric GitHub user ID as the repository membership
subject. No GitHub repository scopes are requested.

Terminate Transport Layer Security (TLS) at a reverse proxy and forward the original canonical `Host` to the private loopback listener. Forwarded headers cannot replace the configured origin.

HTTP identity endpoints are allowed only when the issuer, public URL, and listener are loopback addresses. Production identity endpoints require HTTPS.

### Define repository membership

Each catalog member record binds the provider's stable subject (`sub` for OIDC,
the numeric user ID for GitHub) to a display name and explicit grant. Supply records through `--members-file` when creating
or adopting a repository. Use `--members-file -` to read the document from
standard input, including through `kubectl exec --stdin`:

```sh
kubectl --namespace crab exec --stdin deployment/crab-server -- \
  crab-server --config /etc/crab/http-server/server.toml \
  repository set-members --owner your-team --name your-project \
  --members-file - < /secure/crab-members.toml
```

| Access | Capabilities |
| --- | --- |
| `read` | Browse, fetch, download LFS, and participate in issues, pull requests, comments, and reviews |
| `triage` | Read actions plus closing/reopening discussions, assigning existing labels, assignees and milestones, and requesting reviews; no code writes or definition management |
| `moderate` | Triage actions plus editing issue content and creating and editing label and milestone definitions |
| `write` | Moderate actions plus Git writes, LFS upload, merge, releases, label deletion, statuses, checks, and scoped write-token issuance |
| `maintain` | Write actions plus editing repository descriptions and topics and configuring ordinary PR merge settings |
| `admin` | Maintain actions plus access management, protected settings, archival, and transfer |

Subjects can contain at most 512 characters. Names can contain at most 160 characters. Subjects and case-insensitive names must be unique within a repository.

When browser authentication is configured, the repository administration CLI requires at least
one `admin` member. It rejects an empty or read/write-only membership before
touching repository storage, preventing creation of a repository that no
authenticated operator can administer. Unauthenticated loopback deployments
may omit membership.

`repository set-members` replaces the complete membership array with one
conditional catalog update. A concurrent catalog mutation returns a conflict;
inspect `repository list`, reconcile the desired membership, and issue the
command again. Every healthy replica observes the change on its next
five-second catalog poll and swaps routing only after materialization succeeds.
Requests already holding the previous repository handle finish against that
snapshot.

Administrators can make the same full replacement in **Settings → Members**.
The browser loads membership only when that section opens, presents stable
subject, display name, and `read`/`triage`/`moderate`/`write`/`maintain`/`admin` access, and sends the session
CSRF token with a single revision-guarded replacement. Add, edit, and remove
are staged locally; removal requires an explicit confirmation. A conflict keeps
the unsaved draft and requires **Reload members** before another save; it never
replays a stale array. If an OIDC administrator removes their own admin grant,
the browser confirms the accepted update and returns to the repository root.

`GET /api/repos/{owner}/{repo}/members` returns:

```json
{
  "revision": 42,
  "members": [{"subject":"alice-id","name":"Alice","access":"admin"}]
}
```

`PUT` accepts the same array with `expected_revision` and returns the accepted
replacement with its next revision. It is limited to 256 KiB and rejects
unknown fields. Missing repositories and non-administrators both receive `404`
`repository_not_found`; a stale revision or catalog ETag race is `409`
`membership_changed`; invalid fields are `422` `invalid_membership`; removing
the final administrator is `422` `administrator_required`; unavailable catalog
or audit storage is `503` `membership_unavailable`. The server does not return
the competing array on a conflict. Browser requests inherit the canonical Origin
and CSRF checks used by every other unsafe repository API.

Each changed CLI or browser replacement writes a catalog v3 membership audit
event in the same commit. Only version 3 catalogs and canonical relative audit
references are accepted; there is no automatic catalog conversion.
The bounded pending outbox flushes to
`.crab/http-server/v1/audit/membership/` and is retried after interruption.
There is no browser audit-history API yet.

An authenticated account without membership sees an empty catalog. Unauthorized
and absent repositories both return HTTP 404 after authentication. Catalog
membership changes do not invalidate the user's shared session. Every authenticated
repository request resolves the current durable catalog grant before repository
access, independently of routing refresh. Global repository lists resolve grants
from one catalog snapshot; body-scoped Git-token issuance resolves its target
explicitly. Failed authority reads fail closed. Decoded route names select a
repository, but the admitted grant and Git token bind its immutable UUID. The
provider issuer must also match; display names and public handles grant nothing.
Git publication and LFS upload refresh grants at their existing late checkpoints.
These reads do not make catalog changes and repository publications one atomic
transaction: cross-authority fencing remains unfinished. Admission adds one
bounded catalog object read per authenticated repository request; handlers with
late permission checks perform additional reads. Repository listing reads the
catalog once and indexes grants by UUID. There is no permission cache that waits
for the routing refresh interval. Explicit local-operator mode bypasses these
member lookups.

### Assign a repository to an organization

Create or adopt the repository and bootstrap the owning organization. An
operator binds the existing repository through private management. Use
`repository list` for the catalog `version` and repository `id`, and use the
organization UUID returned by bootstrap.

```sh
crab-server --config server.toml repository set-organization \
  --owner team --name project --input-file access.json
```

```json
{
  "repository_id": "10000000-0000-4000-8000-000000000001",
  "expected_revision": 42,
  "organization": {
    "id": "10000000-0000-4000-8000-000000000002",
    "teams": [{"team":"10000000-0000-4000-8000-000000000003","access":"write"}]
  }
}
```

Use an empty `teams` array to bind ownership before granting access. The binding
preserves the repository UUID, path, and existing individual and signed-group
grants. Organization owners get repository Admin access. Team grants accept
Read, Triage, Moderate, Write, Maintain, or Admin for up to 100 distinct live
team UUIDs in the owning organization. Update all serving peers before
assigning Moderate or Maintain or editing repository descriptions and topics.
Earlier servers reject the new catalog fields and role values. The operator
can replace grants for the existing owner using the same command.
Transfer to another organization requires a separate operation.

Organization owners can open **Settings → Team access** on the bound repository
to find teams by name, add grants, change roles, and remove grants. **Save team
access** applies the draft. When another administrator saves first, reload the
grants and review the current list before retrying. Remove unavailable teams
from the draft before saving. Their grants cease contributing access when the
team is deleted.

The browser reads `GET /api/repos/{owner}/{name}/team-access` and replaces the
grant list with `PUT` and `{expected_revision, teams}`. Both responses include
`team_names` keyed by the current team UUIDs. The mutation requires the session
CSRF token and repository UUID header. A stale change returns 409. Grant changes
use the catalog audit outbox. Direct member and team changes share the same
revision. Organization owners control the team grant list. Team-derived Admin
authorizes repository settings and direct collaborator management. A downgrade
removes those permissions on the next
request unless another grant provides Admin. Forks inherit access from their
source. Authenticated catalog responses include `organization_id` for visible
organization-owned repositories.

Each protected request resolves current organization and team membership.
Effective access is the highest valid grant, subject to the Git token's ceiling.
Removal from a team revokes its contribution on the next request, including
requests with existing Git and LFS credentials. A separate individual, SSO
group, or team grant can still confer access. Publication retains its late
checks. Ordering a removal atomically with an in-flight publication remains a
separate cross-authority guarantee.

Upgrade all serving peers before binding repositories. Unbound catalog rows
retain their existing serialization. Old peers reject the new catalog field
and cannot run the organization authorization read.

### Inspect an organization member’s repository access

Open the organization’s **People** page and select **View repository access**
for a current member. Organization owners can see the member's highest
configured role from direct grants, active teams, and organization ownership.
Fork rows identify the source of the grants. The view shows separately when
sign-in and user Git credentials are blocked, and when the identity provider
manages the account. Where a signed SSO claim could add access,
the row shows the highest configured SSO role. Git credentials can have a
lower scope than the account's role.

`GET /api/organizations/{id}/members/{actor}/repository-access` returns
`{account, managed, items, next}`. Pass its opaque `next` as `after` to continue.
`limit` accepts 1–50 and defaults to 30. The continuation binds the requesting
owner, inspected member, catalog revision, current teams and account
suspension version. Reload from the first page after a change. The response
omits private provider identities and group names. Upgrade Organization Cell
peers together so every owner supports the new read.

### Find outside collaborators

Organization owners can open **People → Outside collaborators** to find
people who have direct repository grants and lack an active membership in
the organization. Search by name, public username, or repository name. Expand a person's
repository list to see the role granted on each repository. **Manage access**
opens that repository's access settings to change or remove the grant.
For collaborators with a public profile, **Invite to organization** opens
the standard invitation with the person selected.

Select one or more people and choose **Remove from all repositories** to
revoke their direct grants in one change. This covers grants inherited by
private forks and grants stored in deleted repository records. Independent
grants from the identity provider can still apply. The organization audit log
records the removal and the number of people, repositories, and grants affected.

`GET /api/organizations/{id}/outside-collaborators` returns a paginated
list of people, with an opaque key and direct repository count for each.
`GET /api/organizations/{id}/outside-collaborators/{key}/repositories`
lists that person's direct grants. The listings combine the catalog and
current Organization Cell membership and expose public profiles and directly
held roles on source repositories.
An `after` token is tied to the owner, organization, search, and current
catalog and organization revisions. Restart from the first page after a
grant or membership changes. Upgrade Organization Cell peers together to
enable the member checks for this view.

`POST /api/organizations/{id}/outside-collaborators/removal-preview` accepts
up to 50 opaque `collaborators` keys from the listing. Review the returned
impact and submit its signed `plan` to
`POST /api/organizations/{id}/outside-collaborators/removals`. The owner and
targets are checked under the organization policy lease shared by membership
changes and invitation acceptance. Request a fresh preview if membership or
the catalog changes. Resubmit the same plan after a lost reply to recover
the committed result. Upgrade serving replicas together before using bulk
removal so every replica can read its audit event.

From **People → Members**, organization owners can select members and choose
**Convert to outside collaborators**. Existing direct roles and the highest current team grant
on each source repository become direct access. Organization ownership, team
membership, and baseline access end. The organization must retain an owner.
Private forks follow their source repository grants.

`POST /api/organizations/{id}/members/conversion` accepts a `members` array
of up to 50 distinct `{actor, version}` entries and returns the count in
`converted` after completion. Crab validates the whole selection before
committing the retained grants and durable conversion record in one catalog
write. The Organization Cell removes the selected memberships in one
transaction. The catalog refresh worker can resume an interrupted batch.
Membership and team changes serialize with the grant snapshot.
Upgrade serving peers together before using conversion.

### Configure protected branches

A repository can define at most 100 exact branch rules:

```toml
protected_branches = [
  { branch = "main", required_approvals = 1, require_up_to_date = true, required_checks = ["ci/test"] },
]
```

Each rule accepts 0 to 20 approvals and up to 50 unique case-insensitive check contexts. Each context contains 1 to 100 characters.

Select **Require branches to be up to date before merging** with at least one
required check or GitHub Actions workflow. The head must contain the current
base commit, and the required checks must pass on that head. If the PR falls
behind, choose **Update branch** and run the checks on the new commit. Crab
checks this policy again with both refs leased at merge publication.
[GitHub strict required checks](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/available-rules-for-rulesets#require-status-checks-to-pass-before-merging)

After a repository has a branch, native Git cannot create, update, or delete a protected name. The first branch can initialize an empty or tag-only repository. Fast-forward or two-parent pull-request merge is the supported protected-branch publication path.

### Understand session security

The sign-in callback verifies state, PKCE, nonce, signature, issuer, audience,
authorized party, expiry, issuance time, and an access-token hash when supplied.
It reloads provider keys after signature failure to handle rotation. Login flow
consumption uses object-store compare-and-swap, so a callback can land on any
replica and only one replay can exchange the code.

Identity requests reject redirects, time out after 10 seconds, and cap responses
at 1 MiB. Login transactions expire after 10 minutes. Each process admits eight
simultaneous callbacks. Sessions and Git-token records use the shared storage
root rather than pod memory.

Session cookies use `HttpOnly` and `SameSite=Lax`. HTTPS deployments also use `Secure` and the `__Host-` prefix. OIDC sessions expire at the earlier of ID-token expiry or eight hours; GitHub sessions expire after eight hours. The server does not retain refresh tokens or GitHub access tokens.

Concurrent session checks may share a batch whose durable reads begin after
every participant joins. Requests arriving after dispatch start fresh checks.
The bounded pending registry releases unfinished work when its last caller leaves.

Restarting or replacing a replica preserves sessions and current-format Git
tokens. Git-token format 2 requires the immutable repository UUID. Older tokens
are rejected and must be revoked/reissued; there is no name-only reader or
conversion. Upgrade all serving replicas together so an older replica cannot
continue accepting retired credentials. Browser sessions retain their existing
format. Revoking all Git tokens remains available even when an old token cannot
be decoded. Logout
requires the canonical `Origin` and the session's cross-site request forgery
(CSRF) token. It removes the durable session, invalidating subsequent browser
and Git-token authentication on every replica, but does not sign the account
out of the identity provider. Scope the one-day object-store lifecycle rules
to the short-lived paths in the [operations runbook](deploy/operations.md#manage-membership-and-sessions).
The broader `.crab/http-server/v1/auth/` prefix also contains the persistent
SCIM user and group directory.
Logout Token replay records under `.crab/http-server/v1/logout-replays/` live
outside that lifecycle prefix. Retain them through their signed expiry and clean
them with an expiry-aware operator job. Identity-index markers and stored session
fields are mandatory; unindexed or older session records require sign-in again.

Register the provider's Back-Channel Logout URI as
`https://git.example.com/auth/backchannel-logout` with
`backchannel_logout_session_required=false`. The endpoint accepts a form-encoded
signed Logout Token, rediscovering provider metadata and keys so normal signing
key rotation needs no restart. It returns `200` after a valid first delivery or
safe replay and `400 {"error":"invalid_request"}` for malformed, invalid, or
conflicting tokens. It validates issuer, audience, expiry, issuance time, `jti`,
the back-channel event, and a non-empty `sub`; it rejects `nonce` and
`sid`-only tokens. The raw token is never logged or stored.

Valid delivery revokes every durable browser session and derived Git token for
the exact `(issuer, subject)` across replicas. Revocation visits only the hashed
identity-session index. Sessions without a matching marker are rejected and must
sign in again. Verify delivery by
checking that both a browser request and a Git credential fail on their next
request. Without a delivered Logout Token, arbitrary provider-side revocation
still does not invalidate an issued Crab session before expiry.

The back-channel endpoint is enabled only for `provider = "oidc"`. GitHub's
OAuth flow does not issue signed OIDC Logout Tokens, so a GitHub provider-side
revocation does not immediately terminate a Crab session; use Crab logout or
wait for the eight-hour session limit.

If GitHub sign-in must retain signed back-channel logout, configure GitHub in
an OIDC broker and point Crab at the broker with `provider = "oidc"`; the UI and
membership subjects then remain provider-neutral.

Private operators can also suspend or reactivate a subject for the configured
issuer using `accounts status`, `accounts suspend`, and `accounts reactivate`.
Verification reads the current account state alongside the durable session and
its marker. Sessions and derived Git tokens issued before a change in account
revision stay invalid after reactivation. See [the operations runbook](deploy/operations.md#manage-membership-and-sessions).

`GET /api/session` returns the current account and CSRF token to the same-origin frontend. Anonymous repository APIs return HTTP 401. No cloud credential reaches the browser.

### Provision users with SCIM

Crab can require SCIM provisioning for the configured OIDC issuer. Upgrade
all serving replicas and verify that lifecycle rules preserve the SCIM directory
before enabling it. Mount the same operator-owned bearer token file on each
replica and configure it under `[auth]`. Kubernetes deployments can set
`secrets.scimTokenKey` in the [Helm chart](deploy/helm/crab-server/README.md#enable-scim-provisioning).

```toml
scim_token_file = "/run/secrets/crab-scim-token"
```

Use a dedicated, randomly generated token of 32–512 visible ASCII characters.
Enter the HTTPS server URL ending in `/scim/v2` and the token in the identity
provider. Map SCIM `externalId` to the exact, case-sensitive `sub` the provider
signs for this OIDC application. `userName`, email, and display name are
profile fields. Obtain the mapping from the provider before enabling the
integration. Provision the operator or organization owner before their next
sign-in. Enabling this setting blocks sign-in and use of session-bound Git
tokens for unprovisioned identities. Restart the serving replicas to change
the token. Protect the token as a server-wide account lifecycle credential.

Crab supports User creation, retrieval, replacement, patch, deletion, and
paged discovery with `userName eq` and `externalId eq` filters. The
case-insensitive username is unique among current User resources.
Changes can send the last returned ETag in `If-Match`. Use
`PATCH active=false` to disable a user while keeping the resource visible to
the provider. `DELETE` hides the User and retains its identity binding so a
later create for the same subject can reuse its ID. The last profile remains
in operator-owned storage. The directory holds up to 4,096 subjects and
8 MiB, including deleted users.
Deployment operators should qualify their IdP’s attribute mappings and
operations before enrolling production users.

A successful deactivation applies to the next browser or user Git
authorization. Reactivation requires a fresh sign-in and a new Git token.
Independent operator holds remain in effect after SCIM reactivation.
Disabling the SCIM endpoint preserves previously recorded provider holds. A
transient failure can happen after the change is recorded. The next User
request reconciles any such change before answering. After an uncertain
create, look up the exact `externalId` before retrying. After an uncertain
update, read the User and use its current ETag. For the protocol contract see
[SCIM Protocol (RFC 7644)](https://www.rfc-editor.org/rfc/rfc7644.html).

The RustFS CI qualification runs two independent serving processes with a
shared storage root and a fixture OIDC issuer. It uses the same browser
session and stock Git token on both replicas, deactivates the user through
one replica, and verifies both replicas reject the existing credentials
immediately and after restarting one replica. Reactivation accepts fresh
credentials. The original credentials remain invalid, and the other
managed account retains access throughout.

### Sync an organization team from SCIM

Provision a group through the existing SCIM bearer credential. The
`/scim/v2/Groups` endpoint supports create, read, replace, patch, and delete.
List groups with `startIndex` and `count`, and filter by `displayName eq` or
`externalId eq`. Membership payloads use provisioned SCIM User IDs. Group
patch accepts roster replacement, additions, and removal by
`members[value eq "USER_ID"]`. The directory supports up to 4,096 groups
within 8 MiB, 1,000 users per group, and 32 teams linked to a group. Direct
user members are supported. [SCIM Group schema](https://www.rfc-editor.org/rfc/rfc7643.html#section-4.2)

In **Organization → Teams → Settings → Identity provider group**, an
organization owner can enter the SCIM Group ID and link it. Linking replaces
the team's direct roster with the group's users who already belong to the
organization. Synced users receive the team Member role and the team's
existing repository grants. Manual roster edits are disabled while a group
owns the team. Use **Clear roster and unlink** to remove the synced grants
and resume manual management.

A successful group update or deletion commits the roster changes on each
linked team before returning. The next repository access check uses the new
roster, including for previously issued user Git tokens. If an update is
interrupted, the next group request resumes the saved projection. Deleting a
group empties its synced rosters and retains the link until the organization
owner unlinks it. An organization invitation is still required for a new
member. After accepting an invitation, resync the group to grant team access.
Unlink managed teams before moving the deployment to a different OIDC issuer.
Live provider qualification and automatic organization invitations remain
open. [GitHub team sync](https://docs.github.com/en/enterprise-cloud@latest/admin/managing-iam/provisioning-user-accounts-with-scim/managing-team-memberships-with-identity-provider-groups)

## Git HTTP reads

The Clone menu exposes `/git/{owner}/{name}.git`. Git uses protocol version 2 for ref discovery, shallow and deepen requests, object filters, and pack streaming.

### Create a scoped Git token

Signed-in members create a repository-scoped token under **Git access**. Read scope permits fetch and LFS download. Write scope also permits push, status reporting, and LFS upload when membership permits those actions.

Use `crab` as the username and the token as the password. Store the credential in a credential manager.

Git credential helpers ignore HTTP paths by default. Enable path matching before saving repository tokens:

```sh
git config --global 'credential.https://git.example.com.useHttpPath' true
```

`POST /api/git-token` accepts a JSON body up to 2 KiB:

```json
{
  "owner": "your_team",
  "repository": "your_project",
  "access": "read"
}
```

The response contains the secret, target, permission, and remaining session
lifetime. Shared storage retains only token hashes.

A write token for an organization repository can opt in to closing related
issues with Git commit messages. Select the option under **Git access** or
send `"close_related_issues": true` when generating the token. Crab checks
the account's current Write access on each issue repository before publishing
the default branch. The option grants no other access to those repositories.

Token rules:

- Scope binds one owner, repository, and `read` or `write` permission
- Effective access intersects token scope with current membership
- A read token never inherits write capability
- Each session retains at most 10 tokens
- Sign-out, replacement sign-in, and explicit revocation invalidate tokens
- Explicit revocation clears the session's token index before token object
  cleanup. Authentication rejects any token missing from the current index.
- Revocation is enforced on the next request; an already admitted request drains

### Create a deploy token for CI

In **Settings → Deploy tokens**, a signed-in repository administrator can
create a named, read-only credential that lasts 1, 7, or 30 days. Use the
username `crab` and store the displayed password in the CI secret store.
Deploy tokens allow Git fetch and Git LFS download. They cannot push, upload
LFS objects, or publish status checks. The secret appears once on creation.
If the reply is lost, reload the token list, revoke the new entry if present,
and create another.

Deploy tokens belong to a repository UUID. Signing out, changing the
creator's access, renaming the repository, or transferring ownership leaves
them active until expiry or revocation. An administrator can revoke each
token individually, including on an archived repository. Revocation applies
to new Git and LFS requests on every replica. An admitted transfer can finish.

`GET /api/repos/{owner}/{name}/settings/deploy-tokens` lists active metadata
and returns the current `version`. Use it as `expected_version` when posting
`{"edit":{"kind":"create","name":"Nightly build","days":7},"expected_version":0}`
to the same endpoint. Revoke with `edit.kind` set to `revoke` and `edit.id`
set to the token's ID. A stale version returns HTTP 409. A repository can
have up to 32 active deploy tokens.

### Fetch through smart HTTP

The server rejects Git protocol versions older than version 2. Override a conflicting client configuration with `git -c protocol.version=2`.

```sh
git -c protocol.version=2 clone \
  https://git.example.com/git/your_team/your_project.git
```

The server supports buffered, gzip-compressed, and chunked upload-pack requests. Authentication and membership checks happen before an empty authentication probe opens repository storage.

Fetch request rules:

| Resource | Limit |
| --- | ---: |
| Concurrent Git and LFS transfers | 4 per process |
| Request body deadline | 30s |
| Transmitted or decoded body | 4.25 MiB |
| Parsed command payload | 4 MiB |
| Transfer profile | 2 hours |

Unknown content encodings return HTTP 415. Invalid gzip returns HTTP 400. Oversized bodies return HTTP 413. Gzip expansion runs on a bounded blocking worker.

HTTP responses end with a flush packet and end-of-file. They do not send the response-end packet used by the standard-input remote helper.

### Repeat native fetch qualification

Configure a credential helper or `GIT_ASKPASS`, then run:

```sh
python3 crates/crab-server/tests/verify_git_transport.py \
  --url http://127.0.0.1:8788/git/your_team/your_project.git \
  --source /path/to/read_only_source \
  --revision 0123456789012345678901234567890123456789 \
  --workdir "$HOME/Workspace/Github/crab-qualification"
```

The work directory must already exist on the mounted workspace volume. The verifier leaves two native Git client repositories for inspection.

The isolated native process harness can also publish an exact-size object with
stock Git LFS, resume a byte-range download, restart the server and verify a
fresh clone's bytes:

```sh
python3 crates/crab-server/tests/qualify_cellar_process.py \
  --binary "$CARGO_TARGET_DIR/debug/crab-server" \
  --large-file-bytes 3221225472 \
  --artifacts "$CARGO_TARGET_DIR/large-file-process-proof"
```

The byte count is a qualification input, not a product limit. The harness checks
scratch capacity before generating data and uses its own disk-backed Docker
volume, which it removes on exit. The reusable container workflow accepts the
same `large_file_bytes` input and requires matching completed evidence.

## Git LFS transfers

Git discovers LFS at `/git/{owner}/{name}.git/info/lfs/objects/batch`. No separate `lfs.url` is required.

### Understand the LFS protocol

The batch API supports SHA-256 `basic` transfers. Read tokens download; write tokens upload. Every action URL uses the configured canonical origin and the same scoped token.

```mermaid
sequenceDiagram
    participant G as Git LFS client
    participant H as HTTP server
    participant T as Temporary file
    participant S as Object storage

    G->>H: POST batch request
    H-->>G: Scoped upload or download action
    G->>H: PUT object bytes
    H->>T: Stream private body
    H->>H: Verify size and SHA-256
    H->>S: Publish multipart object
    H-->>G: Success
```

Uploads stream to a private temporary file, then use `crab-lfs` for verified bounded-memory multipart publication. Full downloads hash the delivered object through successful end-of-file. Partial-range downloads verify the complete object or a matching durable verification receipt, bind that proof to the provider's strong object validator, and only then open a backpressured response. A range that spans the complete object uses the full-delivery hash contract.

Already verified objects omit upload actions. Missing or corrupt downloads return per-object errors. A successful upload needs no separate verify request. Git receive proves referenced LFS content again before publishing a commit.

### Apply LFS limits

| Resource | Limit |
| --- | ---: |
| Batch objects | 200 |
| Batch JSON | 64 KiB |
| Batch body budget | 30s |
| One LFS object | Storage-provider multipart object limit |
| Aggregate dependency content | No separate logical-byte cap; bounded receive work |
| Verification or byte transfer | 30 minutes |
| Shared Git and LFS transfers | 4 per process |

Repository capacity and individual transfers are separate. Crab does not impose a
10 GiB file limit or a TB/PB repository byte limit. LFS objects follow the selected
provider's multipart profile; S3-compatible implementations may impose a lower
ceiling than AWS S3. Xet files span bounded xorbs and retain the existing shard,
recipe and chunk-count bounds. Every pointer still receives full origin content
verification before Git publication; a matching receipt cannot replace that proof.

Large-object uploads currently reserve and spool the complete body on node-local
staging before multipart publication. A node must have sufficient admitted disk
capacity for that object, and transfer concurrency and thirty-minute operation
budgets still apply. These are resource and operation limits, not repository-size
limits. Multi-TB/PB production scale needs representative object-count, metadata,
throughput, recovery and provider qualification; passing a multi-GB transfer does
not establish that scale. Release assets retain their independent 512 MiB limit.

A started multipart operation keeps its permit and temporary file while it completes or aborts. This drain can extend beyond the thirty-minute request budget and server shutdown.

LFS object `GET` supports one RFC 9110 byte range in closed (`bytes=0-99`),
open (`bytes=100-`), or suffix (`bytes=-100`) form. A partial response returns
HTTP 206 with `Accept-Ranges`, `Content-Range`, `Content-Length`, and a strong
OID-based `ETag`. A matching `If-Range` resumes the transfer; a stale validator
returns the complete HTTP 200 representation so the client replaces its partial
copy. Malformed or unsatisfiable single byte ranges return HTTP 416 with
`Content-Range: bytes */size`. Unknown units and multi-range field values are
ignored, returning the complete HTTP 200 representation rather than creating an
unbounded multipart response.

`HEAD` describes the complete representation and ignores `Range`, as required
for methods whose range semantics are undefined.

The container gate uploads a 1 MiB LFS object, downloads an initial range through
Caddy, resumes into the same file, and compares the completed bytes with the
source. Browser blob downloads continue to return exact pointer bytes.

### Coordinate edits with LFS file locks

The server implements the Git LFS File Locking API below the repository's
automatically discovered LFS URL:

| Method and route suffix | Permission | Result |
| --- | --- | --- |
| `POST /locks` | Write | Create one exclusive repository-path lock |
| `GET /locks` | Read | List active locks, optionally filtered and paginated |
| `POST /locks/verify` | Write | Partition active locks into `ours` and `theirs` |
| `POST /locks/{id}/unlock` | Write | Release the caller's lock, or another lock with `force: true` |

```mermaid
sequenceDiagram
    participant A as Alice / git-lfs
    participant H as crab-server
    participant S as Object storage
    participant B as Bob / git-lfs

    A->>H: POST /locks {path}
    H->>S: Conditional create by path hash
    S-->>H: Stable lock ID
    H-->>A: 201 lock
    B->>H: POST /locks/verify
    H->>S: List active records
    H-->>B: theirs: Alice's lock
    Note over B: Standard pre-push hook gives early feedback
    B->>H: receive-pack changes the locked path
    H->>S: Acquire repository LFS-lock guard
    H->>S: Recheck active locks before ref commit
    H-->>B: Reject every updated ref
    A->>H: POST /locks/{id}/unlock
    H->>S: Owner + ID checked CAS tombstone
    H-->>A: 200 released lock
```

Lock ownership stores the authenticated provider subject, not a mutable display
name. Responses resolve the current repository-member name and fall back to the
subject for an old or local record. A same-owner create is idempotent. An exact
unlock retry returns the existing tombstone, while a stale ID cannot release a
replacement lock. `force: true` follows the Git LFS contract and requires write
access, not repository-administrator access.

The lock JSON body is limited to 16 KiB. Paths contain 1–4,096 UTF-8 bytes and
must be valid repository-relative Git paths. Page limits range from 1 through
100; IDs and cursors contain at most 128 bytes. One request has a 30-second
budget and shares bounded server admission. `ref` and `refspec` remain
authorization hints as defined by version 1 of the protocol; locks are not
branch-scoped.

Lock create and unlock operations share one durable repository guard with Git
publication. Receive validates every newly introduced commit, hashes its exact
raw Git paths, and rechecks as many as 10,000 active lock records immediately
before committing the ref transaction. Additions, deletions, content or mode
changes, tree/leaf replacements, and both sides of a rename count as changes.
Merge commits are compared with every parent. A change followed by a revert in
the same push still counts; comparing only the final trees would let an
intermediate locked edit bypass policy. Storage, coordination, cancellation,
and lock-limit failures reject the push rather than skipping enforcement.

When `locksverify` is unset, Git LFS probes the endpoint and may print the exact
configuration command needed to enable enforcement. Teams should set the
URL-scoped value to `true`; the pre-push hook then reports the caller's locks,
fails closed on verification errors, and halts a push that changes a path in
`theirs`:

```sh
git config lfs.https://git.example.com/git/team/project.git/info/lfs.locksverify true
```

This client setting provides early feedback, but a modified client can bypass
it. The server-side receive rule remains authoritative and rejects the same
conflicting path before ref publication. An owner's own lock does not block
that owner.

Only the server workload identity should have write access to the storage root.
A principal with direct object-store write access is an operator outside the
HTTP authorization boundary and can mutate lock records or any other
repository state.

## Native Git push

`POST /git/{owner}/{name}.git/git-receive-pack` accepts exact Git commits and atomic ref updates. [Native Git write design](DESIGN.md) documents the complete publication boundary.

### Know the supported operations

| Operation | Support |
| --- | --- |
| Initial branch push | Supported |
| Fast-forward branch update | Supported |
| Branch creation and deletion | Supported, except protected or default-branch deletion |
| Lightweight and annotated tags | Supported |
| Atomic multi-ref batch | Supported |
| Forced or non-fast-forward update | Rejected atomically |
| Protected-branch direct update | Rejected after repository initialization |
| Shallow-client push | Supported when omitted parents resolve from committed visibility |

The server preserves every submitted OID. It never synthesizes a replacement commit to make an update fit policy.

### Follow publication from request to readable ref

```mermaid
flowchart LR
    A[Authenticate, spool, and parse]
    B[Acquire ref leases and GC fences]
    C[Open and pin one snapshot]
    D[Quarantine and validate]
    E[Prepare pack and visibility]
    F[Prove dependencies and upload]
    G[Recheck policy and commit marker]
    H[Attempt read readiness]
    I[Report known status]

    A --> B --> C --> D --> E --> F --> G --> H --> I
```

The server acquires sorted ref leases, the shared namespace lease for creates or deletes, and both GC fence domains. It rechecks authorization and archive state immediately before commitment.

### Apply receive limits

| Resource | Limit |
| --- | ---: |
| Request body and prepared pack | 8 GiB |
| Cooperative receive budget | 30 minutes |
| Incoming objects | 5,000,000 |
| One Git object | 128 MiB |
| Total inflation | 64 GiB |
| Delta depth | 128 |
| Ref commands | 1,024 |
| Graph traversal steps | 100,000,000 |
| Graph object bytes read | 64 GiB |
| Existing logical objects inspected while validating receive | 5,000,000 |
| Existing object-store requests while validating receive | 6,000,000 |
| Dependency pointers | 1,024 |
| One dependency file | Pointer format and storage-provider limits |
| Aggregate dependency content | No separate logical-byte cap |
| Publication lease lifetime | 5 minutes, renewed while owned work drains |

Receive staging shares the Cell local-disk budget. Admission covers the received
body and every simultaneously live quarantine, inflated-object, decoded-object,
prepared-pack and index-sidecar file. File growth reserves capacity first;
directory cleanup releases it after owned workers drain. Overwriting or renaming
an admitted file does not charge its bytes again.

Temporary disk use can exceed the wire limit because these files overlap. A
transfer that exceeds total staging capacity returns HTTP 413; temporary shared
disk pressure returns HTTP 429. Those errors describe staging capacity rather
than the 8 GiB wire-body limit. Native receive reports them as HTTP failures,
preserving malformed-pack rejection as a separate Git protocol result.

### Handle rejection and uncertain outcomes

The server rejects the whole atomic batch for stale tips, namespace collisions, malformed commands, corrupt packs, missing bases, invalid graphs, missing pointer content, protected refs, or policy violations.

The current symbolic HEAD cannot be deleted. Git reports `deletion is prohibited`. Protected names report `protected branch requires a pull request`.

A disconnect or deadline signals cancellation. Owned workers retain transfer admission and renew GC fences until cleanup finishes. Cancellation before the active marker leaves refs unchanged.

After a marker attempt, the transaction can be committed even when the client loses the response. The server binds the exact receive wire body, repository, and authenticated subject to a deterministic publication plan. Retrying that identical wire request can resolve its durable plan receipt after a lost response or pod restart and return the normal Git success report without publishing a second transaction. An inconclusive outcome with no committed receipt still returns HTTP 503 without inventing a per-ref rejection; do not change the request and blindly replay it, because matching current refs alone do not prove which historical transaction committed.

Successful ref commitment and read readiness are distinct. The server can acknowledge a known journal commit while later catalog work remains pending. A subsequent read runs repair under generation-owner election.

### Preserve unborn HEAD behavior

Tag-only initialization keeps the configured default branch unborn. The refs API returns `head: null` and its name in `unborn_head`. Git protocol version 2 preserves that symbolic name while fetching tags.

A later branch publication establishes a resolved default. The server never substitutes a tag as HEAD. Older tagged readers from releases v1.0.1 and v1.1.0 reject a nonempty repository with unborn HEAD, so deploy updated readers and writers together.

### Repeat isolated receive qualification

Use a fresh prefix because this ignored test creates a manifest and does not overwrite one:

```sh
QUALIFICATION_BUCKET=your_test_bucket \
QUALIFICATION_PREFIX=qualification/http-receive-your_run \
TMPDIR="$HOME/Workspace/Github/crab-qualification/scratch" \
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-server-dev" \
  cargo test -p crab-server --locked \
  native_http_push_rustfs -- --ignored --nocapture
```

Create the temporary directory first. Replace the test name with `receive_faults_rustfs` and use another fresh prefix to exercise lost marker replies, rejected marker writes, cancellation boundaries, and GC fence renewal.

## Issues, pull requests and reviews

Collaboration state lives in one native repository Cell whose committed LTX roots
are authoritative in object storage. Milestones and reactions use bounded typed KV
documents in that same SQLite transaction boundary. Collaboration does not mutate
Git refs unless a pull request merge publishes a ref update.

### Prepare and use account profiles

With the server running, explicitly initialize its shared native Directory:

```sh
"$SERVER" --config /secure/server.toml cells initialize-directory
```

The command uses the configured private mTLS management listener. It is
idempotent, leaves repository state unchanged and does not enroll accounts.
`cells directory-status` and `GET /internal/directory` report `missing`,
`outdated`, `migrating`, or `ready` with the current format/progress/revision. Missing preparation returns 503 `directory_unavailable` from profile
APIs and nonempty issue/PR lists; ordinary repository readiness does not imply
Directory readiness. Prepare it before enabling the updated UI/list contract.

Directory format 2 separates immutable qualified account identities from optional
public profiles. Existing format-1 directories require an explicit upgrade:

```sh
"$SERVER" --config /secure/server.toml cells directory-status
"$SERVER" --config /secure/server.toml cells migrate-directory
```

Deploy all serving peers together before migration. Initialization does not
convert an old directory. Ordinary Directory operations return a visible
503 `schema_upgrade_required` until the upgrade finishes. The private command
copies at most 100 profiles per native transaction and persists its cursor;
rerun after interruption to resume. Existing UUIDs, names, handles, profile
versions, timestamps, enrollment reservations and command receipts are preserved.
Source table renames stop old code from creating new profile writes during the
copy, but mixed-version serving remains unsupported. After the final atomic
format switch, retired tables and migration progress are removed. Only migration
code reads format 1. The 100-row limit also fits maximum-width Unicode identity
records within the native SQL byte bound. Repository records and the discussion
index do not change.

A trusted operator can reserve stable account UUIDs before public enrollment:

```json
[
  {"issuer":"https://identity.example","subject":"opaque-provider-subject"}
]
```

Save that array as a private identity file, then run:

```sh
"$SERVER" --config /secure/server.toml cells register-accounts --identities-file accounts.json
```

The private mTLS command accepts 1–100 explicit qualified identities in a file
of at most 256 KiB and returns ordered `ids` plus the native revision. Duplicate
or repeated keys return the same UUID without another account mutation. Different
issuers with the same subject remain separate accounts. Registration creates no
public username, profile, repository, owner, organization, team or project grant.
Do not derive issuer/subject keys from names or owner labels. Profile enrollment
reuses a registered UUID; when no account exists it creates that identity and its
profile in the same native transaction. Public profile/list DTOs stay unchanged,
and accounts without a chosen profile remain absent from public handle results.
Existing assignments require the explicit actor-ID preparation below; membership
remains catalog-owned and still needs its separate native-authority cutover.

Open `/-/profile` through the header's profile icon to choose a public username
and display name. The stable profile URL is `/-/users/{actor-uuid}`; `-` is a
reserved server route owner. These pages use the shared shadcn/Geist foundation
and retain request diagnostics behind `powerbar=1`.

| Endpoint | Contract |
| --- | --- |
| `GET /api/profile` | Current account's `{profile: object or null}` |
| `POST /api/profile` | Enroll with `{request_id, handle, name}`; returns 201 |
| `PATCH /api/profile` | Edit own profile with `{version, handle, name}` |
| `GET /api/actors/{id}` | Public profile by stable actor UUID |

Profiles require an active account session or trusted loopback local-operator
mode. Repository Git credentials alone cannot read or edit them. Mutations use
the existing session, Origin and CSRF checks and a 2 KiB body limit. Inputs
reject unknown fields; callers cannot supply an authentication identity or edit
another actor. Responses expose only `id`, `handle`, `name`, `version`,
`created_at_ms` and `updated_at_ms`, visible to signed-in users of this server.
Private `(issuer, subject)` keys are unique and never inferred from matching
email or display names.

Handles are unique, 1–39 lowercase ASCII letters/digits with single internal
hyphens. Display names contain 1–100 Unicode scalar values, without surrounding
whitespace or control characters. Enrollment retains its submission identity:
an identical retry resolves the current profile; conflicting payloads fail.
Edits use the captured version. An identical immediate successor acknowledges a
lost reply without another write; other stale edits return 409. The UI retains
the draft and offers an explicit choice of saved values or rebasing that draft.
Renaming preserves the actor UUID and never changes repository permissions.

Issue/PR list and detail authors, comments, reviews, inline threads/replies,
thread resolving actors, activity actors and merge/pending-merge actors use `{name, profile}`: `name` is the recorded display-name
snapshot; `profile` is either `{id, handle, name}` from the current Directory or
null when that qualified account has not enrolled. The browser links the current
handle to the stable UUID; it never guesses a handle from the snapshot name.
The previous string shape is removed for these responses, including mutations
and submission replays. Git commit signatures and check/status
actors retain their separate contracts; reviewer invitations use the same
public author shape for reviewers and requesters. Actor-based
assignments use the separate contract below; catalog membership is unchanged.

The HTTP owner deduplicates page actors into one native Directory read
(at most 50 records, or 100 keys including thread resolving actors). Its private
`LookupActors` operation accepts at most 100 qualified keys,
returns minimal public summaries in input order and uses one Directory snapshot.
It performs indexed identity lookups rather than a catalog scan. Inputs count
Unicode scalar values, matching SQLite schema bounds and stored repository
identities. Native receipt/class checks still apply. Handlers recheck durable
repository membership after enrichment; thread reply/resolve controls use those
refreshed grants. Repository counts/cursors and
Directory display names have separate snapshots; enrichment is not a distributed
transaction or an access grant. A rename neither rewrites source records nor
requires a discussion-index rebuild. Prepare the Directory before these enriched
responses are deployed, and update serving peers and the bundled UI together
for the DTO change. No missing-schema reader or old DTO alias is supplied.
Response enrichment follows the repository transaction, so an enrichment error
does not imply a rolled-back mutation. Retry creates with the same submission
ID; reload versioned edits before retrying.

Organization/team repository grants, owner registration and remaining actor-backed
contracts and predicates remain unfinished. Choosing a handle matching a catalog
owner label does not acquire ownership.

### Use the collaboration route families

| Surface | Routes |
| --- | --- |
| Issues | `GET/POST /api/repos/{owner}/{name}/issues`, `GET/PATCH /api/repos/{owner}/{name}/issues/{number}` |
| Issue templates | `GET /api/repos/{owner}/{name}/issues/templates` |
| Issue comments | `GET/POST /api/repos/{owner}/{name}/issues/{number}/comments`, `GET/PATCH /api/repos/{owner}/{name}/issues/{number}/comments/{comment}` |
| Pull requests | `GET/POST /api/repos/{owner}/{name}/pulls`, `GET/PATCH /api/repos/{owner}/{name}/pulls/{number}` |
| Pull request templates | `GET /api/repos/{owner}/{name}/pulls/templates` |
| Pull comments | `GET/POST /api/repos/{owner}/{name}/pulls/{number}/comments`, `GET/PATCH /api/repos/{owner}/{name}/pulls/{number}/comments/{comment}` |
| Reviews | `GET/POST /api/repos/{owner}/{name}/pulls/{number}/reviews`, `GET/PATCH /api/repos/{owner}/{name}/pulls/{number}/reviews/{review}` |
| Pending review | `GET/PUT /api/repos/{owner}/{name}/pulls/{number}/pending-review`, `POST .../pending-review/submit` |
| Inline review threads | `GET/POST /api/repos/{owner}/{name}/pulls/{number}/threads`, `GET/PATCH /api/repos/{owner}/{name}/pulls/{number}/threads/{thread}`, `GET /api/repos/{owner}/{name}/pulls/{number}/threads/navigation` |
| Thread replies | `GET/POST /api/repos/{owner}/{name}/pulls/{number}/threads/{thread}/replies`, `GET/PATCH /api/repos/{owner}/{name}/pulls/{number}/threads/{thread}/replies/{reply}` |
| Apply review suggestions | `POST /api/repos/{owner}/{name}/pulls/{number}/suggestions` |
| Apply upstream review suggestions to a fork | `POST /api/repos/{fork-owner}/{fork-name}/upstream-pulls/{number}/suggestions` |
| Merge | `POST /api/repos/{owner}/{name}/pulls/{number}/merge` |
| Label search choices | `GET /api/repos/{owner}/{name}/discussion-search/labels` |
| Milestone search choices | `GET /api/repos/{owner}/{name}/discussion-search/milestones` |
| Issue type search choices | `GET /api/repos/{owner}/{name}/discussion-search/issue-types` |
| Markdown suggestions | `GET /api/repos/{owner}/{name}/discussion-search/{mentions,team-mentions,references}` |
| Team mention settings | `GET/PUT /api/repos/{owner}/{name}/mention-settings` |
| Account notification inbox | `GET /api/notifications`, `GET /api/notifications/unread`, `PUT /api/notifications/{id}/state`, `POST /api/notifications/bulk`, `POST /api/notifications/unsubscribe` |
| Account email notifications | `GET/PUT /api/notifications/email` |
| Watched repositories | `GET /api/notifications/watched`, `POST /api/notifications/watched/unwatch` |
| Repository notification preference | `GET /api/repos/{owner}/{name}/subscription`, `PUT /api/repos/{owner}/{name}/subscription` |
| Discussion subscriptions | `GET /api/notifications/subscriptions`, `POST /api/notifications/subscriptions/unsubscribe` |
| Discussion notification preference | `GET` and `PUT /api/repos/{owner}/{name}/issues/{number}/subscription` or `/pulls/{number}/subscription` |
| Stable discussion links | `GET /api/discussion-references/{repository_id}/{kind}/{number}` |
| Discussion attachments | `POST /api/repos/{owner}/{name}/discussion-attachments`, `GET /api/discussion-attachments/{repository_id}/{id}` |
| Labels | `GET/POST /api/repos/{owner}/{name}/labels`, `PATCH/DELETE /api/repos/{owner}/{name}/labels/{number}` |
| Assignees | `GET /api/repos/{owner}/{name}/assignees`; issue and pull `PATCH` requests replace assignments |
| Milestones | `GET/POST /api/repos/{owner}/{name}/milestones`, `GET/PATCH /api/repos/{owner}/{name}/milestones/{number}`, `GET /api/repos/{owner}/{name}/milestones/{number}/items`; issue/pull `GET/PUT .../{number}/milestone` |
| Reactions | `PUT /api/repos/{owner}/{name}/reactions/{target}`; discussion responses include personalized summaries |
| Conversation | `GET /api/repos/{owner}/{name}/issues/{number}/conversation`, `GET /api/repos/{owner}/{name}/pulls/{number}/conversation` |
| Conversation lock | `GET/PUT /api/repos/{owner}/{name}/issues/{number}/lock` and `GET/PUT /api/repos/{owner}/{name}/pulls/{number}/lock` |
| Comment visibility | `PUT /api/repos/{owner}/{name}/comment-visibility/{target}` |
| Edit history | `GET /api/repos/{owner}/{name}/edit-history/{target}` and `DELETE ...?revision={number}` |
| Activity | `GET /api/repos/{owner}/{name}/activity`, `GET /api/repos/{owner}/{name}/issues/{number}/activity`, `GET /api/repos/{owner}/{name}/pulls/{number}/activity` |
| Linked work | Issue/pull `GET .../{number}/links`; `GET/PUT /api/repos/{owner}/{name}/pulls/{pull}/linked-issues/{issue}` |
| Cross-repository linked work | `GET/PUT .../issues/{issue}/linked-pulls/{repository}/{pull}` |
| Development branches | `GET .../issues/{number}/links?include_branches=true`; `PUT/POST .../issues/{number}/development-branches` |
| Issue dependencies | `GET .../issues/{number}/dependencies/{blocked_by|blocking}`; `GET/PUT .../issues/{number}/dependencies/{direction}/{other}` |
| Assign or clear an issue type | `PUT .../issues/{number}/type` |

Every route prefix is `/api/repos/{owner}/{name}`. Browser mutations require the session cookie, canonical `Origin`, and CSRF token.

### Transfer an issue

Open an issue and choose **Transfer issue** in its sidebar. The picker shows
repositories under the same owner where you have Moderate access or higher.
You need that access in both repositories. The move preserves the issue,
comments, assignees, reactions, retained edit history, and discussion
attachments. Retained revisions keep their authors, timestamps, and
attachments. Removed revision text stays removed. Labels match by name.
Milestones match by title and due date. The original URL continues to work.

Current transfer support covers open issues. Crab keeps the source issue in
place when it detects linked work that the destination cannot preserve.

`GET /api/repos/{owner}/{name}/issues/{number}/transfer` lists up to 30 eligible
destinations and accepts optional `q` to filter by repository name. POST the
same route with `request_id`, the observed issue `version`, and the destination
repository UUID as `destination`. A committed move returns `202 Accepted`.
Repeat the same submission to recover a lost reply. Read the original issue
URL for the current transfer status.

### Follow a transferred issue

The original issue detail returns a `transfer` object after a committed move.
`ready` includes the current destination repository UUID and address and its
issue number. The browser opens that issue and keeps the comment anchor.
`pending` means destination publication is still in progress. `unavailable`
omits the destination when the reader lacks current access. A retry of the
original creation can return the original `number` with `transferred: true`.

### Open an issue from a repository template

Commit templates to `.github/ISSUE_TEMPLATE/` on the repository's default
branch. **New issue** shows the template picker when it finds a template or
contact link. A selected filename can be shared with
`/OWNER/REPO/issues/new?template=FILE`. Repositories without templates keep
the ordinary issue form.

- Markdown `.md` files use YAML front matter for `name`, `about`, `title`,
  `labels` and `assignees`. The title and Markdown body are editable prefills.
- YAML `.yml` and `.yaml` files use `name`, `description`, `title`, `labels`,
  `assignees` and `body`. Forms support Markdown instructions, single-line
  inputs, text areas, dropdowns and checkboxes. The browser validates required
  answers and turns them into an editable issue description when submitted.
  A text area's `render` language wraps its answer in a code block.
- `config.yml` supports `blank_issues_enabled` and HTTP(S) `contact_links`.
  Disabling blank issues in the picker reserves that option for writers and
  admins. Existing API clients can continue to create issues directly.

The picker resolves named labels and enrolled assignee handles against the
repository. Unknown defaults are skipped. A one-hour proof authorizes the
exact selected defaults for the repository and contributor, including readers
who can open issues. The native issue mutation rechecks label and assignee
eligibility and uses the existing creation receipt on retry. Template browsing
reads at most 32 files of up to 32 KiB each, plus the chooser configuration.

An organization or personal account can publish defaults in a repository
named `.github`. Store the files at the same `.github/ISSUE_TEMPLATE/` path
on that repository's default branch. A destination with a valid template or
`config.yml` uses its own chooser in full. Otherwise, the chooser can use the
shared templates, configuration and contact links together. It links to the
source repository and resolves label and assignee names in the destination.

Crab's private defaults require current Read access to both repositories.
Losing source access hides the inherited choices and invalidates outstanding
proofs for new writes. The original submission can still recover an issue
created while the proof was authorized.

### Link a branch to an issue

Open **Linked branches** in the issue sidebar and choose **Link branch**.
Select a repository and one of its existing branches. Write access to the
issue repository and Read access to the branch repository are required.
Each issue supports up to ten links. Readers see branches only while they
can read both repositories. Issue transfers retain the links and continue
to check each branch repository's access.

`GET .../issues/{number}/links?include_branches=true` returns a `branches`
object with `items`, `version`, and `can_manage`. Each item contains the
target repository address, the full ref, and `exists`. A ref that has since
been deleted remains removable and appears as unavailable. The link follows
the named branch as commits change.

Send `{branch: {repository, reference}, active, version}` to
`PUT .../issues/{number}/development-branches` using the target repository
UUID and the issue's observed branch-link version. The response includes
the committed version. A conflicting mutation returns HTTP 409.

Choose **Create a branch** to publish a new branch in the selected repository
and link it to the issue. The starting branch defaults to the selected
repository's default branch. The server verifies Write access in both
repositories and applies the destination's normal Git branch rules.
POST `{request_id, repository, name, source_oid}` to the same endpoint,
using the selected repository UUID and the exact starting commit SHA.
Keep the request ID and payload for retries. On a lost response, reload the
issue to see the linked branch or the original actor's pending request.
Recovery links only the exact Git creation, even if the new branch has moved.

When a pull request opens from a linked branch, it replaces the branch on the
issue. Both conversations show **From linked branch**. This works with fork
heads and with issues in another repository in the same organization. For a
cross-repository link, the pull request author needs Read access to the issue
repository. Merging into the default branch closes the issue when the merger
has Write access to the issue repository and the link still closes on merge.
Crab promotes up to ten matching issues per pull request in each link scope.
After an upgrade, the background worker indexes earlier branch links in batches.

### Link a pull request from another repository

Choose **Link pull request** on an issue and select a repository and pull
request within the same organization. Creating or removing the link requires
Write access to the issue repository and Read access to the pull request
repository. Either sidebar shows the relationship to readers with access
to both endpoints. The link follows the issue through transfers within the
organization.

Leave **Close issue on merge** selected to complete the issue when the pull
request merges into its repository's default branch. The merging account
needs current Write access in the issue repository. Use **Keep open** on
either sidebar to keep the relationship without automatic closure.

Use the pull request repository UUID as `{repository}` in the edit route.
GET returns the link's `active` flag and `version`. PUT accepts the desired
`active` flag, observed `version`, and optional `closes_on_merge` flag.
Omitting the flag keeps the selection on existing links and leaves new
links as related work. Repeating a completed transition returns its current
state. A stale conflicting write returns HTTP 409.
Cross-repository links appear under `cross_repository` on the first page of
each linked-work response and are limited to ten per issue or pull request.

Crab captures the authorized issue state versions before publishing the merge.
The exact Git publication receipt lets the job recover if the response is
lost or the default branch moves. An intervening closure or reopen takes
precedence over a delayed cross-repository update.

### Link an issue from a pull request description

Use `Fixes owner/repository#123`, `Closes owner/repository#123`, or
`Resolves owner/repository#123` to reference an issue in another repository
in the same organization. The pull request must target its default branch.
Crab reads up to ten distinct qualified references in Markdown prose.
Both sidebars show **From PR description** to readers with access to both
repositories. Editing the description or retargeting the pull request
updates active references. A manual link and a description reference to
the same issue share one row.

Qualified references use the same merge authorization and receipt as
manual cross-repository closure. After publication, the saved references
remain visible even if the description changes. Description-only entries
in `cross_repository` include `from_description: true` and omit `version`.

### Close a linked issue when its pull request merges

In the pull request’s **Linked issues** section, enter an issue number and leave
**Close issue when merged into the default branch** selected. Writers can change
this option on an existing link. The issue stays open until the pull request
publishes to the repository’s default branch. A completed closure records the
merging author and a link back to the pull request in the issue timeline.

The linked-issue `PUT` accepts an optional `closes_on_merge` boolean. Omission
preserves an active link’s selection. New links submitted without this field
start as related work, as do existing links from earlier server versions. A
pull request can mark up to ten linked issues for closure. Link selection and
version changes use the same request and conflict behavior as unlinking.

Crab captures the selected issues under the Git publication policy lease and
updates open issues to **Completed** in the same Cell transaction as merge
completion. A retry finishes the captured closure even after the default branch
or links change. The update preserves the reasons on already-closed issues.

In a description targeting the default branch, write `Fixes #123`,
`Closes: #123`, or `Resolves #123`. Crab accepts the singular, plural, and past
tense forms in any case, with an optional colon. Use a keyword for each issue.
Crab reads up to 32 distinct same-repository issue references in Markdown prose.
Code blocks, inline code, block quotes, Markdown links and escaped examples do
not create links. A number resolves in the issue namespace even when a pull
request has the same number.

Both sidebars show description links. Edit the description to change them
before merge. A manual link and a description link to the same issue appear in
one row. Linked-work API items sourced from the description include
`from_description: true`. Items omit `version` when there is no manual link to
edit. The `linked-issues/{issue}` edit route addresses the manual link.
Default-branch changes and PR retargeting affect the active links.
Completed merges retain the captured links and the issue's closure event.

The same keywords in a commit message close issues when the commit becomes
reachable from the default branch through Crab Server. Use `Fixes #123` in
the same repository or `Fixes owner/repository#123` for another repository
in the same organization where the publisher has Write access. This covers
Git pushes, browser commits and the commits actually published by a pull
request merge. Git credentials require the explicit token option for qualified
references. A topic branch push leaves the issue open.

The issue timeline identifies the authenticated publisher and links to the
exact closing commit for readers with access to the source repository.
Commit messages alone create no pull request relationship in either sidebar.
Crab verifies the committed ref receipt before applying closures and recovers
pending closures after an interruption. A ref publication can contain up to
100 distinct commit-message closing references across its new commits.

Before serving existing repositories, deploy all peers and run
`cells rebuild-search --owner OWNER --name REPO` to prepare discussion index
format 13. This indexes existing descriptions and preserves prior issue states.

Person mention suggestions require a matching `repository_id` UUID. Person and reference endpoints
accept an optional `search` of up to 80 characters and return up to 12 items.
Mentions search handles and display names of repository collaborators with
Crab profiles, including group-only collaborators with an unexpired, verified
OIDC session. Group candidates must match current repository grants and
suggestions expose only profile fields. Group candidate lookup is bounded,
with exact handles ranked first. References search issue and pull request titles or number
prefixes across all states. Empty searches return collaborators or recent work.
Reference items carry an explicit issue or pull request kind because the two
sequences can share a number. Both reads recheck repository access before
returning results.

Team suggestions use a matching `repository_id` and an optional `search` of up
to 129 characters. They return up to 12 configured `@namespace/team` aliases
for closed teams visible to the current organization member. Repository
administrators can read or clear the binding. Saving a binding requires the
current organization owner and its expected repository settings version.
Each binding accepts up to 24 distinct team aliases. A binding grants no access.

Discussion writes queue newly introduced mentions. Delivery checks repository
grants and rechecks current team membership and visibility for team mentions.
A recipient gets one notification per write, with a direct
mention taking priority when both kinds apply. Plain handles resolve only if
their current binding predates the write. Selecting a suggestion stores a
stable Directory or team UUID. Delivery is capped at 20 distinct mentions
per body version. When a repository has SSO group grants, addressed mentions
for accounts without explicit access are retained separately. An offline
recipient sees a retained notice when their signed-in session proves current
repository read access.

The account inbox returns up to 20 visible conversations by default.
Each account retains up to 100 recent conversations for explicit grants and
100 for addressed notices in group-backed repositories. Saved conversations
are retained independently. Source titles, links, and the unread indicator
require current repository access. See [Read the notification inbox](#read-the-notification-inbox)
for the view, cursor, and mutation contracts.

Reference lookups can resolve an independently authorized repository by name.
The response supplies its UUID. With `exact=true`, only exact issue and pull
numbers match. Selected cross-repository links persist the UUID and typed
number. The resolver checks current access before redirecting.

Account settings use `GET /api/profile/saved-replies` and versioned
`PUT/DELETE /api/profile/saved-replies/{id}`. Version zero creates a new reply.
The private Directory account owns up to 24 saved replies, independent of
public profile enrollment or handle changes.

Discussion attachment uploads accept a raw file body, a client-selected UUID
`id`, and a `name`. Files are limited to 25 MiB. Resending an identical
upload with the same ID recovers the same result. The uploader alone can read
an unpublished draft. Issue, pull request, comment, and review writes claim
linked drafts inside the same native transaction as the body. Current
repository readers can open published attachments. PNG, JPEG, GIF, and WebP
signatures allow inline display. Other files download as binary attachments.
Unclaimed uploads expire after 24 hours and the background sweep removes
their blobs. Removing a link from a published body keeps the uploaded file
accessible to repository readers.

### Separate collaboration triage from code write access

Repository membership supports `read`, `triage`, `moderate`, `write`, `maintain` and `admin` roles.
The collaboration split follows [GitHub’s repository-role reference](https://docs.github.com/en/organizations/managing-user-access-to-your-organizations-repositories/managing-repository-roles/repository-roles-for-an-organization); full role parity remains incomplete.
Triage can close/reopen other authors’ issues and PRs, apply/remove existing
labels and assignees, apply milestones, and request/cancel reviews. It cannot
push code, issue write-scoped Git tokens, merge, change draft readiness on
another author’s PR, edit another author’s title/body, create/edit label or
milestone definitions, publish releases, or administer members/settings. Moderate
and higher roles retain Triage capabilities. Authors retain their existing description
and own-item state controls; archive, pending-merge and version guards still
apply inside native commands.

Issue detail responses include `can_close` separately from `can_edit`; the
browser uses that capability for state controls. PR `can_manage` controls state
while `can_merge` still requires code write permission. Role edits use the
existing revisioned membership catalog and audit chain. Detail capabilities and
edit admission read durable membership instead of inferring it from a public
profile or delayed routing snapshot. Rechecks across awaited reads are not an
atomic cross-authority fence. Complete moderation and required-review
permission parity remain delivery work.

Moderate inherits Triage. It can edit any issue title, body, or comment, and
create and edit repository labels and milestones. The conversation records the
editor in its activity and keeps the original author on the issue and comment.
The label catalog exposes edit and delete capabilities separately. Label
deletion, Git write, merge, and required approval eligibility start at Write.

Maintain inherits Write and can edit the repository description and configure
ordinary merge methods and auto-merge. Assign it directly or through a team
or signed OIDC group. A ruleset can explicitly grant repository maintainers
bypass. Admin controls access, default branch, protections, merge queue
configuration, archival, and transfer.

Native issue/PR edit inputs carry `can_triage` instead of their earlier coarse
management flag. Update fleet peers together before assigning the new role.
No catalog, canonical repository or discussion-index schema change is needed;
existing member records remain valid. No alias accepts superseded native inputs.

### Lock a conversation

Open **Lock conversation** on an issue or pull request. Issue locks require
Moderate or higher. Pull request locks require Write or higher. Choose an optional
reason and a duration of one hour, one day, one week, or until unlocked.
The conversation displays its lock reason and expiry. The timeline records
the lock and its expiry or a manual unlock.

The posting restriction applies to comments, comment edits, reviews, inline
threads and replies, and comments submitted with a state change. People with
lock permission can continue posting. Nobody can change reactions while
locked. Authorized users can close or reopen a discussion without posting a
comment. Pending review drafts stay private and can be published when the
reviewer regains posting access.

`GET .../lock` returns `{version, locked, reason, can_manage, can_post}`.
Temporary locks include `expires_at_ms` and `expires_in_ms`. The Repository
Cell uses its clock to end the restriction at the recorded deadline. Expiry
keeps the version, and the open page refreshes the status at the deadline.
Archived repositories treat all conversations as locked. An untouched
conversation in an active repository starts unlocked at version zero.
`PUT .../lock` takes `{version, change}` with
`change: {kind:"lock", reason, duration}` or `change: {kind:"unlock"}`.
Omit `duration` for a permanent lock, or send `hour`, `day`, or `week`.
Supported reasons are `off_topic`, `too_heated`, `resolved`, and `spam`.
The lock, search entry, and timeline event commit together. An exact retry
keeps the original deadline even if the lock has since expired. Reload on
HTTP 409 before submitting a changed choice.
Blocked contributions receive HTTP 423 with code `conversation_locked`.
A previously accepted submission can recover its receipt while locked.

Update every API and Repository Cell peer before setting conversation locks.
Older collaboration writers lack the lock check, and older activity readers
lack the new event kinds. Existing format-14 indexes require bounded lock
preparation through `cells rebuild-search --owner OWNER --name REPO`.

### Hide or unhide a comment

Choose **Hide comment** and a reason on an issue comment, pull request comment,
review, or inline review comment or reply. Issue comments require Moderate.
Pull request contributions require Write. Repository readers can use **Show
comment** to expand the original content. Expanded comments offer **Unhide
comment** to current moderators. Hidden review bodies leave decisions in force.
Hidden inline comments keep their anchors, resolution controls, and replies.

A contribution's `minimization` value is `null` before the first change and
`{version, reason}` thereafter. Visibility begins at version zero. A null
reason marks a restored contribution. Write through `PUT .../comment-visibility/{target}`
with `{version, change}`. The change is `{kind:"hide", reason}` or
`{kind:"unhide"}`. Use a relative contribution address such as
`issues/42/comments/3` or `pulls/42/threads/1/replies/2`.
Reasons are `abuse`, `duplicate`, `low_quality`, `off_topic`, `outdated`,
`resolved`, and `spam`.

Visibility changes commit with their activity events. Retry a lost response
with the original version and change. If the version and desired state conflict,
reload on HTTP 409. Minimization controls the default display. Authorized API
reads, attachments, and discussion search retain the original content. Update
serving peers together before hiding comments to enable the new activity events
and batched presentation response. Existing comments need no migration.

### Mark duplicate issues and pull requests

Repository writers can post `Duplicate of #42` on its own line in a new
issue or pull request comment. The number resolves within the same repository
and discussion kind. Crab records the comment and an attributed event that
links the duplicate and canonical discussion. The same instruction works when
posting a comment with a close or reopen action. Closing remains an explicit
action.

The source conversation shows **Undo** on its current duplicate mark. An
authorized writer can remove that mark while retaining the original comment
and both events. The conversation response includes `undo_duplicate_event`
for writers. Use `PUT .../duplicates/{issue|pull}/{number}/undo` with
`{event}`. An exact retry returns the same result, and a stale event cannot
remove a newer mark.

### Track issue dependencies

Open **Issue dependencies** in the issue sidebar to see **Blocked by** and
**Blocking** relationships within the repository. Each entry links to the issue
and shows its current state. Repository readers can view relationships. Triage
and higher roles can find issues by title or number, then add or remove a
relationship from either issue.

Issue details, issue lists, and repository and organization Projects show
**Blocked** when an open issue has at least one open blocker. Closing the last
open blocker clears the badge on the next read. Reopening a blocker restores
it while the relationship is active. Project rows expose the badge only to
readers with access to the source repository. Planning status remains editable
independently of the dependency graph.
Repository Project links open directly and support browser reloads.

The two directions share one version. Use the pair's GET to obtain
`{active,version}`, then PUT the desired `active` state with that version.
Version zero describes a pair that has never changed. Repeating the last
successful change recovers a lost response. Reload after an HTTP 409.
List requests return `{items,next,can_manage}` with up to 30 rows. Use
the exclusive `after` cursor to continue. Each direction supports up to 50
direct dependencies, and Crab rejects self-links, cycles, and graphs that
exceed the validation work limit. Dependencies track work state and leave
issue completion to the normal close action.

### Classify issues with organization issue types

Organization owners open **Issue types** to manage up to 25 shared types.
New organizations start with Task, Bug, and Feature. An owner can add a type,
edit its name, description, and color, disable or enable it, or delete it.
Settings changes appear in the organization audit log.

Repository members with triage access or higher can assign or clear a type
from an issue's **Issue type** control. Readers see the type in the issue
header and issue list. Renaming a type updates existing badges when the view
reloads. Disabling hides its badges and removes it from the picker. Enabling
restores the existing badges. Deletion permanently removes that type from
the picker and existing issues. Repository transfers clear the effective
assignment because the receiving organization has its own types.

Owners read `GET /api/organizations/{id}/issue-types` for
`{version,definitions}` and send `{expected_version,definitions}` to the
same path with PUT. Each definition has an ID, name, description, color, and
enabled flag. Issue detail returns `issue_type` and includes `issue_types`
and `issue_type_version` for triage and higher roles. Send
`{expected_version,type_id}` to the issue type route to assign, with a null ID
to clear. An exact retry at the same version recovers a lost response.
Reload the latest data after HTTP 409.
Update all serving peers before using the new settings so every peer can
read the new audit events and handle issue type requests.

### Edit multiple Project items

In a repository or organization Project table, select rows with their
checkboxes. The header checkbox selects the editable items on the current
page. Choose a status, priority, planning date or custom field, then select
**Apply to selected**. A blank optional value clears that field. Organization
Project edits require access to every selected item's source work.

**Undo** restores the previous values as one new edit. If another change
advanced any of the selected item revisions, refresh and make a new selection.
After an interrupted request, use **Retry submission** to recover its decision.

The existing Project commands endpoints accept
`{kind:"bulk_fields",project,items:[{item,version,change}]}`. A selection
contains 1–50 unique item UUIDs with their current revisions. Each change sets
`status`, `priority`, `start`, `end`, or `custom` using a `field` tag and
`value`. A custom change also identifies the field by UUID in `id`.
Planning dates and custom fields use `null` to clear a value. The entire
command commits or rolls back together and keeps one payload-bound receipt.
Upgrade the serving peers and embedded UI together before using this command.

### Show issue types in Projects

In **Settings** → **Views**, turn on **Show issue type** for a saved Project
view. Table views gain a **Type** column. Board and roadmap views show the
type beside the issue title. Organization Project views show this field for
same-organization issues from repositories the viewer can read.

In table view, select a type badge or **Set type** to edit the issue. Source
repository triage access authorizes this action. Archived repositories and
Projects show the type for reference. Changes appear on the issue and in
other views of the same issue when they reload.

Saved view definitions use `show_issue_type`, which defaults to false. Project
item pages supply the enabled `issue_types` and an `issue_type` per available
issue. The item has an `issue_type_version` when the source repository permits
an edit. Submit changes through the source issue type route.

Choose **Filter by issue type** above the Project view to narrow its items.
Use **No type** to find issues without an enabled type. Set **Issue type
filter** in **Settings** → **Views** to save that filter. A saved filter uses
the type ID, so it follows a renamed type.

Both Project item endpoints accept `issue_type` with an enabled type UUID
or `none`. Organization Project filters count issues from readable
same-organization repositories. Counts and pagination use the complete
matching set. Include the returned `type_cursor` with a filtered continuation.
A stale result or changed access requires restarting from the first page.

Use **Group by** in a table or roadmap to organize work by workflow status,
issue type, or a custom Project field. Boards offer status, issue type, and
single-select fields. Save the choice for a view under **Settings** → **Views**.
Group counts cover every matching Project item across its pages. For text,
number, and date fields, each page includes counts for its displayed groups.
Issues without an enabled type use **No type**. Drafts, pull requests, and
issues whose type is unavailable to the viewer use **Other work**.

Project managers can drag a saved view by its tab handle, or use the active
tab's **Reorder view** menu to move it left or right. The selected view stays
open, and the saved order applies to the whole Project. **Overview** stays
first where available, and **History** stays last.

Both Project command endpoints accept `reorder_views` with `project`,
`version`, and `order`, a list containing every saved view UUID exactly once.
The command updates the view order and records it in Project history.

Organization owners can change **Default Project access** under organization
**Settings** → **Project policy**. It starts at **Read**. The confirmation shows
how many existing Projects inherit the default. The change also applies to new
Projects. Projects with explicit base roles keep them.

In an organization Project, **Manage access** lets Project administrators set
an explicit base role, select **Organization default**, or grant **Read**,
**Write**, or **Admin** to current members, teams, and outside collaborators.
**No access** hides the Project from members without a grant. A current direct grant takes precedence
over the base role. Active team memberships can add access. Organization
owners and the creator's original membership retain administrative access.
Write includes editing planning data and views. Admin also includes managing
access and opening or archiving the Project. Linked issues and PRs still follow
their repository permissions. Work and history from an unreadable linked
repository omit its identity, custom field values, and history field changes.

To share a Project with a guest, select **Outside collaborator** and search by
Crab username. The guest must have a current direct grant on a repository owned
by the organization. The Project appears in the guest's organization Project
library. Guests see only the Projects shared with them. They can neither create
organization Projects nor access the organization roster. Project Admin grants
let guests manage individual collaborators, with team grants reserved for
organization members.

Removing the last qualifying repository grant makes the saved Project role
inactive immediately. Restoring repository access reactivates the saved role
unless the Project grant was removed or the guest joined the organization in
the meantime. Organization membership uses its own Project access rules.

Project admins can call `GET /api/organizations/{id}/projects/{project}/access`
to read the effective base role, optional override, organization default and
its version, grants, and Project version. Post `set_access` to the organization
Project command endpoint with `project`, `version`, `target` (`{kind:"base"}`,
`{kind:"member",id}`, `{kind:"team",id}`, or `{kind:"outside",id}`) and `role`
(`none`, `read`, `write`, or `admin`). For a base role, pass `role: null` and the
displayed `default_version` to inherit the current default. `none` removes an
explicit grant.
A Project supports up to 100 explicit grants. A grant to a member expires when
that membership ends. Team grants check the active roster on every operation.

`GET /api/organizations/{id}/projects/context` returns the library name and
whether the caller can create Projects. Project administrators can call
`GET /api/organizations/{id}/projects/{project}/collaborators?q={prefix}` to
find eligible guests and their account UUIDs. With `member=true`, the search
returns current organization members matching the username prefix.

Organization owners can call `GET /api/organizations/{id}/project-policy` for
the current `role`, `version`, `affected_projects` count, and opaque `scope`.
Submit `set_default_access` with the new `role` and the displayed `version`
and `scope`. A different set of inheriting Projects or changed default requires
a fresh confirmation. A successful change appears in the organization audit log.
Upgrade all serving peers before using Project access policies or guest links.

Open **Teams** → a team → **Projects** to search its linked Projects and browse
the active and archived lists. The list requires current access to both the
team and the Project. An organization owner, or a maintainer who also has
Project Admin access, can choose **Link a project**. Linking an active Project
adds a Read grant for the team. Existing Write and Admin grants remain in
effect. Manage or remove the grant under **Manage access** on the Project.

`GET /api/organizations/{id}/projects?team={team}` scopes the Project
collection to that team. Add `linkable=true` to search unlinked active Projects
the caller can administer. Submit `link_team` to the standard Project command
endpoint with `project`, `version`, and `team`. Current team and Project roles
are checked in the same native transaction as the grant and its receipt.

In an organization-owned repository, open **Projects** → **Linked projects**
to search active and archived shared Projects owned by that organization.
The list requires current Read access to both the repository and the Project.
On the **Active** tab, choose **Link a project** to list an active Project you
can edit. The same link appears in the Project's **Repositories** panel and lets Project editors
add issues and pull requests from the repository according to source access.
Use that panel to remove the link.

`GET /api/organizations/{id}/projects?repository={repository}` scopes the
collection to links for the repository UUID. Add `linkable=true` to search
unlinked active Projects you can edit. Submit `link_repository` with
`project`, `version`, and `repository` to the organization Project command
endpoint. Publication rechecks access and ownership under the organization
policy lease.

An organization Project administrator can set **Default repository** under
**Repositories** → **Manage**. The destination must be an active, readable
repository owned by the organization. Saving the default also links the
repository and lists the Project on its **Linked projects** page. Clear or
change the default before removing that link.

In the Project's **New issue** form, the default is preselected for people
who can write to it. They can choose another writable linked repository.
If the default is unavailable, the form asks for a destination. A saved
draft or retried submission retains its chosen repository.

Submit `set_default_repository` with `project`, `version`, and a repository
UUID or `null` to the organization Project command endpoint. The server binds
the default to the repository's current ownership epoch. Project detail
returns `default_repository_configured` and the currently visible, active
`default_repository`. A Project membership grants no repository access.

With source repository triage access, drag an issue to another type group in
a table or board, or use the **Change issue type** control in any layout.
Moving to **No type** clears the assignment. The item retains its Project
workflow status and dates.

Project editors can drag work between status or custom field groups on a
table or board. Dropping into **No value** clears the custom field. These
moves use the versioned Project item command and preserve other item fields.

On a board grouped by workflow status or a single-select Project field,
select cards individually or with the checkbox in a column header. Drag any
selected card to a destination, or choose a column under **Move selected to**.
The selection covers editable cards on the current page, including selections
from different columns. The move uses one bulk Project submission and offers
**Undo** after saving. Cards already in the destination keep their revisions
and still participate in the batch's conflict checks.

In a saved board view, open a column's menu and enter an **Item limit**.
The header shows the matching item count and the limit, with a warning when
the count exceeds the limit. A limit is advisory, so moves and additions can
still put work into the column. Each saved view and grouping has its own
limits. Project managers can change or remove them.

Both Project command endpoints accept `column_limit` with `project`,
`version`, and `column: {view, group_by, value, limit}`. The limit is a positive
32-bit integer. Use `null` to remove it. Saved board views expose the optional
`column_limits` map keyed by grouping and column value. Group values are status
names, issue-type UUIDs or `none`/`other`, or single-select option names (the
empty string means **No value**). Older records without this field load with
no limits. Update all server replicas before first saving a limit so each
replica can read the updated view definition.

Choose **Add work to** on a status or custom-field group to start an item
with that value. The editor keeps the prefilled value when you switch work
sources. Unfinished items started this way have separate recovery drafts.

In a repository Project, **Create issue in** on an issue-type group opens
the new issue editor with the type selected. The issue, its type, and its
Project membership save together. This action also supports **No type**.
If a type is disabled before submission, the editor can select an enabled
type and keep the draft. The repository Project `create_issue` command
accepts an optional `type_id` with the enabled organization type UUID.

In an organization Project, choose **New issue** when adding work, or
**Create issue in** on an issue-type group. Choose a linked repository
owned by the organization where you have Write access. The editor keeps
the selected issue type and planning fields in its recovery draft.
Open a saved draft note and select **Convert to issue** to publish it in
one of those repositories. The issue replaces the draft in the same
Project row with its planning fields intact.
Organization Project `create_issue` commands take `project`, `repository`,
`item` (with no subject), and an optional `type_id`. Include `draft_version`
with the saved item's current revision to convert it. The revision stays
reserved until the destination returns a receipt. Rejection or cancellation
before publication releases the reservation and leaves the saved draft intact.
The server reserves the Project item, commits the issue and type in the
repository, and then links the issue. Retry an interrupted submission with
the same request ID to recover the original result. **Cancel if not
applied** fences a pending creation or completes the link if the issue
already exists.
After five minutes, Crab automatically finishes the link for a published
issue or cancels a submission that never published, even if the browser
has lost its saved request.

Use `group_by=status`, `group_by=issue_type`, or `group_by=field:<UUID>` with
either Project item endpoint to receive `group_counts`. Issue-type groupings
include `issue_type_group` per item and carry `type_cursor` on continuation.

### Edit pull requests and review summaries

Pull request authors can choose **Edit pull request** beside the title to update
the title and description. Review authors can choose **Edit review summary** on
a submitted review. These edits preserve the review decision, reviewed commit,
and permalink. If a newer version was saved, the editor keeps the draft and
asks the author to compare it with the current content before saving again.

### View edit history

Choose **edited** on an issue or PR description, comment, review, inline
comment, or reply. Repository readers can compare the text before and after
an edit and see its time and editor. Inline history includes suggested
replacements. History starts with the first recorded edit after upgrading.
Title, state, and metadata changes do not create a body revision.

The original captured version and the 99 newest intermediate versions are
retained. The after panel is unavailable when the adjacent version was
removed or fell outside retention. The content author or a repository
writer can remove a historical version. The timeline of edits still shows
when it was edited and who edited it, along with the removal attribution.
Removal applies to served history. Backups and object versions follow
the deployment’s retention policy.

`GET .../edit-history/{target}` returns `{items,next}`. The page lists up
to 30 edit headers newest first. Continue with the exclusive `before` cursor.
Use `?revision={number}` to receive `{revision,before,after,can_redact}`;
each available text version contains a `body` and an optional `suggestion`.
Use `DELETE .../edit-history/{target}?revision={number}` to remove that
retained version. The action is idempotent. The `has_edit_history` field
on discussion cards identifies contributions with recorded versions.

### Search issues and pull requests

List endpoints accept `q` (up to 256 characters), `limit` (1–50), `sort`,
`direction`, and an opaque exclusive `before` cursor. State belongs in the query,
for example `q=is:closed label:bug`. Use `is:locked` or `is:unlocked` to
filter by effective conversation lock. The **Conversation** menu changes the
global lock filter while preserving other conditions. A lock-filtered cursor
can expire when a temporary lock ends, and an archive state change invalidates
the cursor. Restart the search to use the current matches. An absent `q` defaults to
`is:issue is:open` for the issue workspace and `is:open` for the PR workspace.
An explicit query has no additional hidden state filter. The issue endpoint
searches both discussion kinds; the PR endpoint retains its PR scope.
Use `is:issue` or `is:pr` to include all states of that kind. The former separate
`state` parameter is rejected; milestone and release state parameters keep their
own contracts. Text terms search titles, bodies, conversation comments, submitted reviews, inline threads and replies. Spaces and `AND` require both terms; `OR` matches either; parentheses
support up to five nested groups. Quote phrases and multiword label names.

Common qualifiers include `is:open`, `is:closed`, `is:merged`, `is:unmerged`,
`is:issue`, `is:pr`, `state:open`, `state:closed`, `draft:true`, `draft:false`,
`label:name`, `milestone:"title"`, `author:username`, `author:@me`, `no:label`,
`no:assignee`, `no:milestone`, `assignee:username`, `assignee:@me`,
`assignee:*`, and `assignee:actor/UUID`. `is:closed` includes merged PRs.
Repeated label qualifiers mean AND; comma-separated label names mean OR.
Milestone titles match exactly, case-insensitively, including closed milestones.
Quote spaces, commas, colons or escaped quotation marks as literal title content.
`-milestone:"title"` excludes that milestone; `no:milestone` finds unassigned
issues and PRs. Counts use the same authoritative predicate as results.
Prefix a term or qualifier with `-` to exclude it. Missing-metadata qualifiers
cannot be negated. Unknown qualifiers and malformed syntax return 400.

For organization repositories, open **Types** above the issue list or add
`type:"Incident report"` to the search. Names match case-insensitively. Use
`no:type` to find issues without an enabled type. Disabling an issue type
hides it from search and puts its assigned issues in the **No type** results
until it is enabled again. Type filters compose with text, state, and other
qualifiers. The menu replaces top-level type filters and preserves grouped
conditions. Search suggestions include enabled type names as you type.

Counts and pages use the same indexed type filter. Type changes or assignment
changes can invalidate a continuation cursor. Return to the first page after
a stale-cursor response.

Count qualifiers `comments`, `reactions`, and `interactions` accept a nonnegative
integer, `>n`, `>=n`, `<n`, `<=n`, inclusive `n..n`, `n..*`, or `*..n`.
For example, `is:open comments:>=3 -reactions:0` finds open work with at least
three conversation comments and at least one reaction. Interactions are the sum
of conversation comments and reactions on the issue/PR itself; inline review
replies and reactions on individual comments/reviews remain separate targets.
Each bound must fit a signed 64-bit integer. Negative/fractional counts,
overflow, empty or reversed ranges return 400. Boolean grouping, exclusions,
state counts, sorting and query-bound cursors use the same native predicate.
No new schema/preparation is required: domain mutations already update these
metrics in the discussion index transaction. Completion offers numeric examples.

Author usernames match current Directory handles case-insensitively; `@me`
binds the authenticated qualified account even without a public profile.
Unknown handles match no records; exclusion, AND/OR and grouping retain their
normal meanings. Display names never resolve identities. Renames release the old
handle without rewriting discussion records; handles are not access grants.
Private native list inputs carry exactly the ordered bindings extracted from
the query. Public HTTP parameters cannot supply those bindings. Directory reads
7/8 resolve at most 64 handles or return up to 20 public actor summaries from
one indexed handle prefix. Read 8 returns `items` and `truncated`, rather than a
bare array; update all native peers together. No older-output reader is supported.
Integration-account author syntax, mention search predicates and complete
involvement predicates remain open.

The shared Author menu uses `GET .../discussion-search/authors` with `query` and
`search`. It searches registered public usernames by prefix, ignoring ASCII case
and an optional leading `@`; it does not resolve display names or infer accounts
from repository member names. The response includes public `actor` summaries,
validated query destinations, selection state, `self`/`clear` choices and an exact
truncation flag. No issuer/subject claims cross HTTP. Native query navigation
replaces global author conjunctions and exclusions while retaining predicates
inside OR groups. Maximum-length queries disable destinations that cannot fit.

Selecting a username or Yourself closes the menu, clears pagination, and preserves
sort, direction and diagnostics in the URL. Clear removes global author terms.
Keyboard navigation, focus return, reload, failed-read retry and narrow layouts
use the same shadcn picker as labels and milestones. `@me` remains useful without
a registered profile. Non-enrolled authors have no selectable username; recent
contributor ordering, integration accounts and complete participant filtering
remain unfinished. Directory reads recheck current repository access before
returning choices. No canonical schema or discussion-index rebuild is needed.

`GET .../discussion-search?query=label:he&cursor=8` returns authorized
repository suggestions and syntax feedback. `cursor`, `start`, and `end` count
browser UTF-16 units; replacement offsets enclose the current token.
Suggestions discover both kinds, draft/merge predicates, and up to 20 active
labels or public author usernames without crossing the repository authorization boundary.

Lists query a native SQLite/FTS5 projection across all matching records. Each
response includes `matching` for the exact query, `counts: {open, closed,
merged}`, `authored_by_me`, and `navigation`. `authored_by_me` identifies a
global positive `author:@me` conjunction; excluded self terms and self terms
inside OR groups do not select the personal view. The UI uses that native result
for its Created by me/Authored by me navigation. A global positive Open/Closed conjunction produces
`navigation: {kind:"state", selected, open, closed}`. Those counters describe
its destination queries; closed includes merged PRs. Other searches produce
`navigation: {kind:"results"}` and retain the exact predicate, showing the total
matching result count instead of manufacturing state tabs. This includes
unrestricted searches, merge-state predicates, exclusions, conflicting states,
and states inside OR groups. Repeated identical global state terms remain one
state facet. Non-state atom spelling, quoted literals, and boolean grouping
survive state changes. A state destination is null when it cannot fit the
grammar's size/depth/term limits; the UI disables that control and explains the
limit without truncating filters. Counts, ordered rows, selected label
names/colors, and navigation share a native snapshot. Each list item carries
`kind: "issue"` or `kind: "pull"`; PR items also include draft and branch metadata.
Clients must use kind with number for identity and destination links because
issues and PRs allocate numbers independently. Summaries omit bodies;
`comment_count` counts conversation comments, and `reaction_count` counts
reactions on the issue/PR body.

`sort` accepts `created` (default), `updated`, `comments`, `reactions`,
`thumbs_up`, `thumbs_down`, `laugh`, `hooray`, `confused`, `heart`, `rocket`,
`eyes`, and `relevance`. `updated` uses the source discussion's update timestamp;
comment/reaction writes do not fabricate edit timestamps. Relevance combines FTS5
BM25 ranks for positive scoped text terms; terms shorter than three Unicode scalars
retain literal substring matching without trigram ranking. `direction` is
`desc` (default) or `asc`. Discussion number, then kind, break ties in the same direction.
`next` is null on the last page. Cursors bind the repository, query, endpoint kind scope,
ordering, resolved author identities, generation and revision. Reassigning a
handle invalidates its old cursor even without a repository mutation. Malformed cursors return 400; changed
queries or index contents return 409 `stale_search_cursor`. Restart at the first
page to obtain a fresh snapshot. Comment pagination remains numeric.

The current discussion index format is 15. It retains qualified author identities,
milestone catalog entries and stable actor assignment keys, and adds searchable
assignment name snapshots, transactional reviewer relationships and per-source
commenter identities, transactional contribution text/FTS, latest-review/decision
references, chronological conversation references, and lock state. Older
indexes report `outdated`. Format 14 upgrades in place by copying canonical lock
records in bounded issue and pull phases. Earlier formats rebuild other derived
tables as well. Both preserve canonical records and resume through the management
CLI.

New empty repositories initialize their index. Existing native repositories
require `cells rebuild-search --owner OWNER --name NAME` against the running
private mTLS listener. This bounded operation resumes after interruption and
blocks fresh mutations and indexed reads until complete. Missing/building
indexes return 503 `discussion_index_unavailable`; detail reads and recorded
command replays remain available. The command never recreates domain records.
Other actor qualifiers and advanced metadata predicates remain delivery work. Release
search retains its separate plain-text contract.

Assignee predicates resolve public handles to stable Directory accounts; `@me`
uses the authenticated qualified account even without enrollment. Unknown accounts
match nothing. A handle change/rebinding changes cursor identity; explicit
`actor/UUID` values retain stable identity without requiring a public profile.
The Assignee menu (`GET .../discussion-search/assignees?query=...&search=...`)
combines historical snapshots, current members and public handle completion.
It returns at most 20 deduplicated `person`/query choices, `truncated`, and
`clear`, `missing`, `self`, and `any` destinations. Results recheck durable grants
after awaited reads. Definitions, grants and canonical assignments are unchanged.
Both list outputs include `assigned_to_me` for global positive self predicates;
conditional/excluded predicates retain arbitrary-search navigation.

Reviewer predicates accept public usernames or `@me`: `review-requested`,
`reviewed-by`, `review-involves`, and `user-review-requested`.
`review-requested:@me` includes outstanding requests for the teams you currently
belong to. `user-review-requested:@me` shows direct requests. A submitted review
fulfills its pending direct request and remains a participant. Cancellation
without a review removes participation. Qualified issuer/subject bindings
prevent cross-provider subject collisions.

Use `team-review-requested:<repository-owner>/<team-name>` to find PRs awaiting
review from a visible team in the repository's organization. Quote values
containing spaces. Type `team-review-requested:` in the search box to choose
a visible team with the qualified value filled in. A fulfilled or cancelled
request drops out. Team visibility and roster changes invalidate affected
continuations. Existing repositories require `cells rebuild-search` for
discussion index format 14.

Exclusion, boolean groups, exact full counts and query-bound cursors use the
same native predicate on mixed and PR-only lists. The PR workspace selects
Review requests navigation when the query includes a positive, global self
predicate. Conditional and negated self terms keep search navigation.

Named reviewer predicates use shared public-handle completion. The Reviews menu
uses the status/personal destinations described below; the former unshipped
reviewer-picker endpoint and its choice builder are removed.

`commenter:username` and `commenter:@me` match conversation comments, all submitted
reviews (including empty approvals), inline thread comments and replies. An author
with no such contributions does not match. Merely being assigned or requested to
review does not match. Public handles resolve against current Directory identities;
equal subjects under different issuers remain distinct. Exclusions, Boolean groups,
state counts and cursors share the native predicate on both list surfaces.

Format 7 stores one source identity per canonical contribution, indexed by
qualified identity and discussion kind/number. Source deletion triggers preserve
other contributions by the same actor and follow canonical cascades. Bounded
rebuild clears this projection separately and scans each canonical composite key
in frames of eight, with a durable source cursor. Private list inputs consolidate
author, reviewer and commenter names into one exact ordered `people` collection;
update and drain peers together. No old-shape decoder is supported. Commenter
completion includes public usernames and `@me`; no authentication claims cross
HTTP. Mentions and `involves` remain unsupported.

Text fields use `in:title`, `in:body`, `in:comments`, or a comma-separated
combination. Without `in:`, all three fields are searched. Repeated selectors in
the same conjunction combine; explicit inner field scopes override enclosing
fields, and OR branches retain their own scopes. For example,
`(fix in:title) OR (workaround in:comments)` preserves each branch when another
facet is selected. Negated field directives are rejected; use `-word in:comments`
to exclude a word from comments. Unknown/empty fields return 400.

Phrase matches stay within one title, description or contribution body. Separate
terms may match different contributions to the same discussion. Text comparisons
use folded literal substrings, including the existing one/two-scalar path for
trigram-ineligible terms. Author names are not searchable discussion text; use
`author:username` or `author:@me`. Best-match sorting sums positive-term header
ranks and each term's best matching contribution rank, avoiding a bonus solely
for repeating a word across many comments. This is native FTS5 ranking, not a
claim to reproduce GitHub's private ranking algorithm.

Format 8 adds a text row and contentless-delete trigram FTS entry for each
contribution. Composite foreign keys preserve identity/source ownership. Dirty-ID
triggers and bounded native flush keep inserts, edits, deletion cascades and
receipts atomic, without virtual-table writes from untrusted triggers. During
explicit preparation, header rows clear in bounded frames before their empty
shape is recreated without recorded-author text; all five contribution sources
then rebuild with their existing keyset cursor. Canonical records and versions
are unchanged. Both FTS indexes are checked before indexed readiness.

### Issue close reasons

Issue details and list summaries include nullable `state_reason`
(`completed`, `not_planned`, `reopened`) and `closed_at` (Unix milliseconds).
PATCH accepts optional `state_reason` alongside the expected `version`. Closing
without a reason chooses completed; an explicit not-planned close uses
`{"version":1,"state":"closed","state_reason":"not_planned"}`. An already
closed issue can change its reason without resetting the original close time.
Reopening clears `closed_at` and records reopened. Invalid state/reason pairs
return 400; permissions and version conflicts retain the existing contracts.
The native record, metadata, progress and events commit together.

Search supports `reason:completed`, `reason:"not planned"`, `reason:reopened`
and `no:reason`, including Boolean grouping and exclusions. Positive reason
predicates match only issues. Unknown historical reasons remain null; no-reason
also matches never-closed issues. Duplicate reasons/links and close-date search
are not implemented. Existing repositories require private explicit
`cells prepare-issue-state --owner OWNER --name REPO`; absent or partial
preparation returns 503 `schema_upgrade_required`. The command pages existing
issues/activity and recovers only proven history, without changing record
versions or original command outcomes. No discussion-index rebuild is required.

### Comment and state submission

POST `/api/repos/{owner}/{name}/issues/{number}/state` or the corresponding
`pulls/{number}/state` accepts `{request_id, version, state, reason?, body?}`.
State is `open` or `closed`. Reasons are issue-only completed/not-planned closure
reasons; omit the reason when reopening. An optional nonempty body posts a
conversation comment in the same transaction as the state change. Responses are
`{comment: object or null}` using the existing public comment DTO. Reload the
subject for its current canonical state; a recovered action can predate later
changes by another user.

Retry an uncertain action with exactly the same UUID, expected version, state,
reason and body. Recovery returns the existing comment without changing state
again, even after later archive or version changes. Altered payloads conflict.
Current repository access is always required. State permissions, archive, pending
merge and merged-state restrictions remain canonical; rejection cannot leave an
orphan comment or advance progress. Immutable-payload validation and confirmed
native domain rejection carry `X-Crab-Write-Outcome: rejected`. Context errors
(access, routing, transport, decoding) and submission-identity conflicts stay
unmarked: a previous attempt may already have committed. Preserve the original
intent until it can be recovered. A confirmed native rejection is permanent for
that UUID; archive, version or permission changes cannot turn it into a new write.
After resolving a rejection, a deliberate new action uses a fresh UUID.

Existing repositories must run both `cells prepare-issue-state` and
`cells prepare-pull-state`. These now also convert historical successful state
submissions to decision format 2, eight records per restartable frame, preserving
keys, digests, comment references and canonical history. State writes remain
unavailable until their kind is prepared. No discussion-index rebuild is needed.
The private mTLS GET/POST endpoint is
`/internal/repositories/{owner}/{name}/discussion-submissions/{kind}`; POST uses
`{version}` and returns the existing lifecycle preparation status. Kinds are
`issue`, `pull`, `bulk` and `pins`; issue preparation also prepares bulk/pins,
and pull preparation also prepares bulk. Existing state format-2 records remain
unchanged. Update private CLI clients for the renamed management route. Drain requests,
update private peers and UI together, and reload browser sessions before resuming
state actions. Historical failed actions cannot be recovered from absent records.
See [native state decisions](CELLAR.md#atomic-discussion-state-actions).

### Bulk discussion triage

Issue and PR lists expose the stable `repository_id` and `can_bulk_edit` from
current repository Triage authority.
Row checkboxes, Shift selection and Select all operate on the current page only.

GET `/api/repos/{owner}/{name}/discussions/selection?repository_id={uuid}&items=issue:1:3,pull:2:4`
checks 1–100 distinct subjects and their expected record versions in one native
snapshot. Its `{repository_id, items}` response contains `{item: {subject: {kind, number}, version},
milestone_version}` entries. Issue entries also carry `issue_type_version`.
A stale or missing subject rejects preparation;
current Triage permission is required. The UUID must match the resolved repository;
a reused owner/name cannot retarget the captured selection. The browser retains
these exact entries and UUID.

POST `/api/repos/{owner}/{name}/discussions/bulk` accepts
`{repository_id, request_id, items, action}`. Supported action shapes:

- `{kind: "state", state: "open" | "closed", reason?: "completed" | "not_planned"}`;
  explicit reasons require an issue-only closed selection. Merged PRs retain
  their state and pending merges reject state changes.
- `{kind: "labels", add: [label IDs], remove: [label IDs]}`;
  disjoint additions/removals preserve other active labels, with at most 20 per item.
- `{kind: "assignees", add: [actor UUIDs], remove: [actor UUIDs]}`;
  additions require current repository membership, with at most 10 per item.
  Historical assignees remain removable after losing access.
- `{kind: "milestone", number: milestone number or null}`;
  both source record and independent milestone assignment versions are checked.
- `{kind: "issue_type", type_id: type UUID or null}`;
  choose **Type** for an issue-only selection in an organization repository.
  The type must be enabled in that organization. **No type** clears the
  assignments. The organization policy lease protects type resolution and the
  repository transaction checks both the issue and assignment versions.

All selected changes, source versions, progress, activity, index effects and the
permanent submission receipt commit together. Any rejection rolls all items back;
there is no partial-success response. Success returns `{count}` for the selected
items processed, including items already carrying the requested value. Reload
counts/results; a changed list cursor starts again at the first page.

Uncertain retries must retain the original repository UUID, request UUID, entries
and action. Qualified
actor and payload bind the permanent receipt; name, role and eligibility snapshots
do not alter it. Recovery cannot reapply an edit after later versions, archive or
membership changes, but always requires current repository read access. The shared
`X-Crab-Write-Outcome: rejected` marker has the same commit-aware meaning as the
combined comment/state endpoint. An explicit reload permits a new selection after
a confirmed conflict. Context and repository-identity errors cannot release a
retained intent; native submission-identity conflicts also remain unmarked.
Confirmed native rejections are permanent for that UUID, including after a role,
eligibility or archive change. A deliberate revised action uses a fresh UUID.
Existing repositories must prepare the bulk decision ledger through either
issue/pull state preparation. Historical successful counts/digests/keys are
preserved in bounded frames; old failed decisions cannot be reconstructed.
No discussion-index rebuild is required. HTTP defers archive policy to the native
command so a committed submission can recover after archive.
Catalog-owned grants remain the authority. Native grant cutover, broader
cross-Cell fencing, and wider GitHub parity remain delivery work.
Coordinate private peer and UI updates before using bulk issue type assignment.

### Browse activity

Repository activity lives at `/{owner}/{name}/activity`. Its feed APIs use `limit`
(1–50, default 30) and an exclusive numeric `before` cursor, ordered newest first
by event number. Issue and PR pages use the unified chronological conversation
below; they do not render a second activity panel.

Events record new discussion changes, review decisions, inline conversations,
merge completion, and milestone assignments. They capture actor names and subject
titles at the action, without exposing private identity claims or copying bodies.
Existing history is not backfilled. Edits that change no content/state and
creation retries do not add events. Reassigning milestones records removal and
addition in the same transaction as the assignment and progress updates.

### Draft pull requests

Create with `"draft":true` to open a draft; omission opens a normal ready PR.
The creation reservation binds this mode alongside the original title, body,
branches, and author. Retrying that original payload recovers the existing PR,
including later edits and current readiness; changing the creation mode is a
submission conflict.

Authors or repository writers convert an open PR with
`PUT .../pulls/{number}/readiness`, for example `{"version":1,"draft":false}`.
Readiness and the PR version come from one native read transaction. Conversion
advances the version only when readiness changes. Stale opposite changes return 409; closed or merged PRs and pending merges reject conversion. Closing
and reopening preserves readiness. Drafts allow discussion/reviews but cannot
reserve or begin a new merge. Readiness changes invalidate old merge versions.

Fresh PR commands enforce archive transactionally, including comments, reviews,
inline conversations, and merge admission. Existing receipts remain replayable.
A retry may complete an already-published merge after archive; unpublished work
is aborted without changing Git. Archived conversations retain merge status and
allow recovery of a pending publication, with explicit refresh after an abort.
These recovery paths retain ordinary membership, writer, Origin, CSRF,
immutable-submission, and exact-ref checks.

### Request pull request reviews

The reviewer sidebar uses a searchable shadcn popover and stable actor UUIDs.
`GET /api/repos/{owner}/{name}/pulls/{number}/reviewers` returns participants,
including people who submitted a review without an invitation. Each item has
an opaque `id`, a public `reviewer` author, nullable current `candidate_id`, and
nullable `request`, `review`, and `decision` records. The request contains its
own cancellation `id`, state, public `requested_by`, and timestamps. `review`
identifies the latest submitted review; `decision` identifies the latest
approval or change request. A later comment does not replace a decision.
Summary records contain `number`, `state`, `commit_oid`, and `current`.

For an active decision on an open PR, `merge_eligible` reports whether the
reviewer has a current durable Write or Admin grant. A current eligible approval
shows **Eligible approval**. A reader's approval stays visible with **Write
access required**. An eligible change request remains active after a source push.

Freshness compares the recorded commit with the selected source ref, including
when the fork and upstream have branches with the same name. `current` is true
for the observed head, false for an older commit, and null if the head is missing.
Merged PRs use their recorded source head. A re-request remains visible alongside
the previous review. Fulfillment alone never implies approval.
Conversation review responses and their timeline use the same three-state
freshness contract. The review list envelope includes nullable `head_oid` from
that read; all loaded review cards follow this head, including older pages kept
during refresh. Storage errors remain errors rather than missing refs.

Issue and PR pages read `GET .../{issues|pulls}/{number}/conversation`. The
response contains chronological tagged `items` (`comment`, `review`, `activity`),
nullable opaque `start`/`end` cursors, `has_earlier`/`has_later`, and nullable
`head_oid`. `limit` is 1–50 (default 30); the native page is also byte bounded.
Use exactly one of `before`, `after`, or `focus=comment:N|review:N|activity:N`;
omit all three for the latest window. Focus seeks a window ending at that entry,
without walking intervening pages. Cursors bind subject, creation time, source,
number and index generation. Malformed/cross-subject inputs return 400; a
changed index generation or absent cursor boundary returns 409. Missing focused
entries return 404. Storage failures remain errors.

Native index format 10 references existing canonical records and events without
copying bodies. Creation time, source and number define order even if historical
timestamps run backwards. Opening and contribution activity snapshots do not
duplicate the description or editable cards. Inline and metadata/lifecycle events
remain compact linked entries. `cells rebuild-search` prepares historical source
rows in bounded, restartable frames; no serving scan or fallback is used.

The shared UI offers earlier/later windows and same-range retry while retaining
loaded content. Explicit refresh returns to the selected/latest window. Cached
editors retain their drafts and captured versions; stale pages cannot replace a
newer record. All review cards use the settled window's selected source head.
Comment timestamps use `#issuecomment-N`, reviews/sidebar decisions use
`#pullrequestreview-N`, and compact events use `#event-N`. Window reads support
reload/back/forward, focus the requested entry, and expose failures for recovery.
There is one whole-conversation timing widget, shown only with `powerbar=1`.

PR detail retains comment drafts, pending writes and submission identity across
tabs and metadata refreshes. Hidden panels pause conversation reads and remove
portaled controls, while cached editors and pending writes stay mounted until
the discussion actually unmounts. Successful recovery
clears the submitted draft; intentional same-text posts get a new identity.
Posting from an old window preserves navigation and offers an explicit View
comment link; posting at the end extends the window with a bounded forward read.

Pages contain at most 50 accounts, an opaque exclusive ascending `after` cursor,
current PR version, management permission, and public `candidates` in
`{id, name, profile}` form. Cancelled invitations with no submitted review are
omitted. Cancellation preserves previously submitted participation. No private
issuer or subject is exposed. The old request-only GET route and private read
are removed; there is no response alias or history-scan fallback.

`PUT .../review-requests` retains versioned selection:
`{version, change: {kind: "request", actor_id: "UUID"}}` or
`{version, change: {kind: "cancel", id: "opaque request ID"}}`.
Directory UUIDs bind immutable qualified accounts to current repository members.
Another provider's account, a removed member or an unknown UUID cannot acquire
eligibility through a name or profile. Private registration creates no grants
or handles. Public enrichment precedes the final durable membership read.
Following [GitHub’s review permissions](https://docs.github.com/en/pull-requests/reference/pull-request-reviews#requesting-and-requiring-reviews),
HTTP and native commands require triage/write authority, including for a pull
author. Read access still permits submitting reviews. Profile renames update
presentation without changing the source request or review. Historical names
remain visible when no public profile exists. Removed members cannot receive a
new request, but an authorized manager can cancel their outstanding request.

New reviews fulfill a matching request atomically. A retry cannot fulfill a
later re-request. Fulfillment and a decision advance the PR version once in the
same transaction. Requests do not add a merge gate or change approval policy.
Pending merges and archived repositories reject fresh changes. The picker
retains failed choices and requires explicit refresh after a version conflict.

Discussion index format 9 stores one latest-review reference and optional
request reference per qualified participant. The native page uses primary-key
seeks and point joins, independent of review-history length. Run
`cells rebuild-search` before serving existing indexes. Preparation clears eight
participants per frame, replaces the empty derived table, and replays review
and request sources in bounded frames. It preserves canonical records and
submission receipts. Update serving peers and bundled UI together for the new
native read and public DTO; see [reviewer status preparation](CELLAR.md#reviewer-status-preparation).

In organization repositories, the picker searches visible teams that have a
repository grant. `GET .../reviewers` returns a `teams` list with `id`, `name`,
`state`, and `can_request`. A request's state links to the review that fulfilled
it. Use `{version, change: {kind: "request_team", team_id: "UUID"}}` or
`{version, change: {kind: "cancel_team", team_id: "UUID"}}` on the same
`PUT .../review-requests` route. Request admission checks current Triage or
higher, the team grant, and team visibility. Cancellations still work after a
grant is revoked for managers who can see the team. Organization owners can
remove requests for a deleted team, shown with a null name.

An ordinary or pending review submitted by a current team member fulfills the
request in the review transaction. The author's own review and a replay of an
earlier review leave the request pending. The request sends inbox notifications
to current members other than the requester and pull author. Activity and inbox
lines omit team identity, so notifications can remain in users' history without
exposing a team name after membership changes. The current limits are 100 team
requests per PR and 256 members per team request. The Reviews filter offers
direct and team-aware personal queues. Team-specific search matches pending
requests. List rollup counts cover individual requests.

### Automatically assign team reviews

Organization owners and team maintainers can open **Teams → Settings →
Automatic review assignment** and select an assignment method and up to ten
reviewers per request. Both methods look across the organization's ready,
unarchived repositories. Round robin gives priority to the members least
recently assigned through that team, including CODEOWNERS assignments. Load
balance combines outstanding requests on open PRs with requests made in the
past 30 days. Cancelled requests are excluded. Outstanding requests count
toward the score even when they are older than 30 days. Ties prefer fewer
outstanding requests, then use the team's rotation.

New assignments record their order within the team when the PR request commits.
Earlier assignments without a comparable timestamp contribute to local
tie-breaking. Each assignment reads committed sibling history. Two simultaneous
requests in separate repositories can choose the same eligible reviewer. When
the routing history is unavailable, the review stays with the team.

Under **Never auto-assign**, select team members to skip in either assignment
method. Excluded members can still receive a direct request, fulfill a team
request, and count toward CODEOWNERS requirements if otherwise eligible.

From **Your profile → Review availability**, a reviewer can set **Busy** for
a fixed duration or until they clear it. The preference belongs to their
private account and works without a public profile. Both assignment methods
skip Busy reviewers until the saved expiry or a manual change to **Available**.
Direct requests and review eligibility continue to use repository permissions.

Crab considers current team members with durable repository Write or Admin
access. It skips the PR author, the requester, and people who have already
reviewed or received an individual request on that PR. Manual team requests
appear as individual requests when members are assigned. A request with no
available members stays on the team. CODEOWNERS keeps the team request visible
alongside any individual assignments, and any eligible code owner can satisfy
the merge requirement. Only assigned individuals receive routing notifications.

**Count existing review requests** reduces new assignments by the number of
current team members with Write or Admin access who already have a pending
individual request on the PR. Existing requests count even if their reviewers
are now Busy or excluded from automatic assignment. **Keep the team requested
after assignment** leaves an explicit team request visible alongside the
individual assignments. If automatic routing creates zero new assignments,
the request stays with the team.

### Require code owner reviews

In **Settings → Branches**, enable **Require review from code owners** on the
protected branch. The corresponding field on a protection rule is
`required_code_owner_reviews`. The gate is independent of the numeric approval
count. Direct updates to the protected branch still require the normal PR path.

Crab reads the first CODEOWNERS file it finds on the PR's exact base commit in
this order: `.github/CODEOWNERS`, `CODEOWNERS`, `docs/CODEOWNERS`. It applies
case-sensitive patterns in file order to the merge-base diff. The last matching
line determines ownership. An ownerless match clears inherited ownership.
Each matched rule requires a current-head approval from any eligible owner on
its line. Listed people use `@handle` and need a current durable Write grant.
Organization teams use `@<repository organization namespace>/<team-slug>`, with
lowercase words from the team name separated by hyphens. A team must be visible
within the organization and have its own Write or Admin repository grant. A
current member's approval can then satisfy the rule. Ambiguous slugs do not
resolve. Secret teams and email references are ineligible.

**Files changed** shows matched ownership, and the merge panel shows outstanding
approvals when required. `GET .../pulls/{number}/codeowners` reports the base and
head OIDs, source file, affected and missing path counts, and up to 30 rule
summaries. Crab re-evaluates the same policy before Git publication while the
refs are leased. A changed path whose matched line has no eligible owner
blocks a required review. Unreadable or over-budget policy also blocks the
required gate. Invalid pattern lines are skipped. Policy files must be regular
UTF-8 Git blobs under 3 MiB, with up to 4,096 rules and 1,024 distinct owner
references per comparison.

On a merged PR, **Files changed** shows the rules from the recorded base
commit and the paths they matched in the recorded comparison. The saved
report omits approval counts and eligibility fields. **View CODEOWNERS at
merge** opens the exact policy as text at
`GET .../pulls/{number}/codeowners/source`. Both reads require current access
to the PR repository and any source fork, including after source-branch
deletion.

Opening a ready PR or marking a draft ready requests its eligible code owners
even on branches without a required-owner protection rule. Draft creation
sends no requests. Requests appear in **Reviewers**, and the inbox notification
links each recipient to the PR. A recipient named both directly and through a
team receives one notification for the transition. Converting to draft keeps
existing requests and inbox history. Later readiness transitions preserve
existing or cancelled requests and may add newly eligible owners. Managers can
change the resulting user and team requests through the usual reviewer controls.
The current delivery limits are 256 members
per team, 1,024 distinct accounts per transition, and 1 MiB for the resolved
delivery plan. Invalid or unresolved owner references are skipped for delivery
and remain visible in the PR ownership report.

The file toolbar shows **Code owners** when a rule applies to the displayed
file. It lists currently eligible owners and links to the rule line in the
CODEOWNERS file at the exact commit being browsed. Historical commits use
their own policies and current repository grants. An ownerless override hides
the ownership indicator.

`GET /api/repos/{owner}/{name}/file-codeowners` accepts an exact `rev` commit
ID and the file’s `path_hex`. It returns the pinned `commit`, `path_hex`, and
the applicable `source` with policy path, line, and owner eligibility. `source`
is null when no rule assigns owners. An invalid policy includes a `problem`
and its source path. Missing file paths and inaccessible repositories return
404. Verified email owner lookup remains open. See [GitHub’s CODEOWNERS reference](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/about-code-owners).

### Read the notification inbox

Signed-in users open **Notifications** from the global header. Unread, All,
Saved, and Done support repository and reason filters. Each issue, pull
request, or release has one row, ordered by its latest activity. The link opens the latest
contribution and the strongest retained notification reason stays visible.
A personal mention takes precedence over a team mention, review request,
assignment, or watch update.

**Save** keeps a conversation beyond rolling inbox history and through later
activity. **Mark as done** reads the current activity and removes it from
Unread and All. Done shows activity completed in the last 90 days. Crab
cleans up expired completions in the background and keeps a compact activity
cutoff so delayed copies cannot bring them back. Saved conversations stay in
Saved until you unsave them. A newer contribution
reopens the conversation as unread and keeps any bookmark. **Move to inbox**
restores the current activity with its read state.

Assignments, individual and team review requests, Markdown `@handle` mentions,
and configured closed-team mentions enqueue a notification with the source
activity. The server checks current repository access before displaying it in
the recipient's private inbox. Retried delivery preserves read state. Pending
review comments enqueue mentions when the reviewer publishes the review.

A newly added assignee on an issue or pull request receives an assignment
notification. Creation, individual edits, and bulk edits use the same rule.
Unchanged assignees, removals, and self-assignment produce no new assignment
reason. Subscribers can still receive discussion updates.

`GET /api/notifications` accepts `view` (`inbox`, `saved`, or `done`),
`repository` (repository UUID), `unread` (default false), `reason` (`assigned`,
`review_requested`, `mention`, `team_mention`, `release`, or `subscribed`), `limit`
(1–50, default 20), and the `before` cursor from `next`. The default inbox
view excludes completed conversations. Rows have a stable `id`, an `updated`
stamp with `id` and `created_at_ms`, and `read`, `saved`, and `done` flags.
Issue, pull request, and release numbers address distinct inbox rows. The
unread endpoint reports accessible unread activity in the inbox view.

`PUT /api/notifications/{id}/state` accepts the row's `repository_id`, its
`updated` stamp as `expected`, and one `action` such as `{"done":true}`.
Actions set a desired `read`, `save`, or `done` flag and return the
confirmed state. The write requires CSRF protection. A missing or changed row
returns 409 so the client can refresh. Reads and writes check current repository
access and mute preferences.

Select up to 50 loaded conversations and choose a bulk action to mark them read
or unread, save or unsave them, mark them done, or move them back to the inbox.
`POST /api/notifications/bulk` accepts one `action`, a `request_id` UUID,
`issued_at_ms`, and `items` with each row's `id`, `repository_id`, and `updated`
stamp as `expected`. Selection spans loaded pages. If any row is stale, the
batch makes no state changes. Inaccessible or muted repositories reject
admission, and the server rechecks access before returning results. The
response has `items` with each row ID and its confirmed flags. Retry the same
body after a lost response to recover the stored result within the receipt
window.

Use **Unsubscribe** on an inbox row or on selected rows to stop ordinary
discussion updates and dismiss the observed activity. Saved bookmarks remain
available. A newer personal or team mention or review request can reopen the
conversation. Following the discussion again enables future activity.

`POST /api/notifications/unsubscribe` accepts `items` with each row's `id`,
`repository_id`, typed `subject`, and `updated` stamp as `expected`. Each row
returns `unsubscribed`, `changed`, or `unavailable`. The server applies
eligible rows and leaves changed and inaccessible rows alone. Retry the same
selection after a lost response. The repository remembers what was dismissed
and a replay preserves any newer follow.

#### Receive inbox notifications by email

In **Notifications → Email settings**, opt in to email delivery. Crab uses
the verified email from your OIDC sign-in or the verified primary email from
GitHub sign-in. Sign in again after the server enables email or after updating
your address with your identity provider. A changed or unverified address
turns delivery off. Opt in again to send alerts to a newly verified address.

Emails link to the inbox. Discussion titles and contents stay inside Crab,
behind the current repository access checks. The sender checks repository
access, account suspension, mute, and opt-in before sending queued activity.
Repository watches and discussion follows apply to both delivery channels.
Delivery is limited to future inbox activity and can be turned off at any time.

The same opt-in enables email for pending personal repository transfer offers.
The email links to **Notifications → Repository transfers**. Recipient
addresses come from the verified sign-in binding. Repository names and
private offer links stay inside Crab. Delivery rechecks the exact offer,
the recipient's account status, and the saved preference. Organization
offers appear in the in-app transfer list. The transfer scanner recovers from
the committed catalog, and the Directory uses the same idempotent provider
delivery and 23-hour retry window as discussion email.

`GET /api/notifications/email` returns `configured`, `address`, `enabled`,
and `version` for the signed-in account. Send `enabled` and the captured
`version` with `PUT`. A stale request for a different choice returns 409.
The verified address comes from sign-in.

Operators can enable delivery across serving peers with browser authentication
and a [Resend API key with sending access](https://resend.com/docs/dashboard/api-keys/introduction).
Mount the key as a readable secret file and use a From address on a verified
Resend domain. Use the same sender, provider account, and public URL on each
serving peer so a retried message keeps the same payload and provider
idempotency scope.

```toml
[notification_email]
from = "notifications@example.org"
api_key_file = "/run/secrets/crab-notification-email"
```

OIDC must issue both `email` and `email_verified=true` in the signed ID
token. When enabled, GitHub OAuth requests the
[`user:email` scope](https://docs.github.com/en/rest/users/emails#list-email-addresses-for-the-authenticated-user).
Omit the section to disable delivery. Deploy the serving peers together when
enabling or disabling email.

Open **Watch** in a repository to choose direct notifications, new issue
activity, new pull request activity, both kinds, or mute. Enable **New
releases** on its own or with issue or pull request watching. Watching adds
notices for activity after the preference takes effect and excludes your own
activity. A release notice is queued when publication commits. Drafts and
edits to a published release do not queue notices. Opening a release notice
uses its tag and release number. Withdrawn or deleted releases are hidden
from the inbox and unread badge. Muting clears the release watch and hides
existing repository notices until you change the preference.

`GET /api/repos/{owner}/{name}/subscription` returns the signed-in reader's
current mode and version, defaulting to `{"mode":"direct","version":0}`.
`releases:true` indicates release watching. Send the captured version and
desired mode and release choice with `PUT`. Omit `releases` or send false to
disable release watching. The allowed modes are `direct`, `issues`,
`pull_requests`, `issues_and_pull_requests`, and `muted`. A successful retry
of the same choice returns the current preference. A stale request for a
different choice returns 409. Both endpoints require current repository access. The private preference
can be changed on an archived repository.

Open **Watched repositories** from the notification inbox to see and edit your
repository watches in one place. Each row shows which updates you selected.
To stop watching, choose **Direct notifications** and turn off **New releases**.
Your discussion subscriptions and direct notices remain available.

`GET /api/notifications/watched` returns `items` with the repository address
(`id`, `owner`, `name`) and the signed-in account's current preference.
Each page scans up to `limit` currently readable repositories (1–50, default
32) in owner/name order, and includes only active watches. A page can be empty
with additional repositories left to scan. Pass `next` as `after` to continue
until `next` is null. Access is refreshed after the preference reads. A change
to the scanned portion returns 409 so the client can reload from the first
page. An optional `owner` restricts the results and cursor to that repository
namespace. Watch edits use the versioned repository subscription endpoints.

In **Watched repositories**, choose **Unwatch all**, select an owner, and
confirm the count of active watches across the owner's pages. The owner picker
lists owners from loaded rows. Load more repositories to see additional
owners. Crab stops
repository-level issue, pull request, and release updates for those watches.
Direct notices and individual discussion preferences remain available.

`POST /api/notifications/watched/unwatch` accepts the `owner` and 1–50
`items` with each repository's UUID as `id` and its previewed preference
`version`. It requires CSRF protection. Each item returns `unwatched`,
`changed`, `unavailable`, or `unconfirmed`. The server checks current
ownership and read access and edits each repository independently. Newer
watch or mute settings survive. Retry an unconfirmed item with its original
version to recover a possible completed write. The browser splits larger
selections into bounded requests.

Open **Subscriptions** from the notification inbox to manage discussions you
follow, including those with no inbox activity. Filter by repository,
discussion type, or subscription reason. Sort by most or least recently
subscribed across accessible repositories, or choose repository order. Each
row shows the discussion state, follow choice, recorded source, and repository
mute state. Earlier follows with no recorded source or date remain available
and appear last in the time sorts. Use **Notification settings**
on a row to change which updates you receive or stop following it.

`GET /api/notifications/subscriptions` accepts optional `repository` (UUID),
`kind` (`issue` or `pull`), and `reason` (`manual`, `participation`, `mention`,
`assignment`, or `unknown`) filters. It returns `items` with the repository
address, typed subject, title, state, versioned preference, repository mute flag,
and `origin` with a reason and `subscribed_at_ms` when available.
`sort=recent` and `sort=oldest` order dated subscriptions across repositories.
Default results follow catalog order, then issue and pull request numbers
within each repository. Undated subscriptions use that same order. Pass
`next` as `after` to continue with the same filters and sort. A page can be empty with more discussions to scan. The server
checks repository grants before reading preferences and refreshes them before
returning titles or continuation positions. Edits use the existing
discussion subscription endpoints.

Select up to 50 loaded rows and choose **Unsubscribe selected** to stop
following several discussions. Selection spans loaded pages and repositories.
Earlier inbox activity keeps its read and filing state.

`POST /api/notifications/subscriptions/unsubscribe` accepts `items` with
each row's `repository_id`, typed `subject`, and captured preference `version`.
It returns those addresses with `unsubscribed`, `changed`, `unavailable`, or
`unconfirmed` for each item. The request requires CSRF protection and current
repository read access. Eligible changes commit within each Repository Cell.
Newer preferences survive. Retry unconfirmed items with their original
versions to recover a possible completed write.

Use **Notification settings** in the issue or pull request sidebar for a private
override. Each discussion starts with the repository setting (`inherit`).
Choose all future discussion activity (`follow`), close, reopen and merge events
(`state_changes`), or unsubscribe from activity (`unsubscribed`). The repository
mute applies to every mode. Direct mentions, assignments, and review requests
still use the repository setting. Past inbox items keep their read state.

Use the issue or pull request subscription endpoint with the same versioned
`GET` and `PUT` contract. The endpoints require current read access and return
404 for a missing discussion. Issue and pull request numbers address distinct
preferences. Opening a discussion, publishing a comment or review, or changing
its open, closed, or merged state automatically follows that discussion for
the participating account. Participating again after unsubscribing starts a new
follow with the activity as its cutoff. New mentions and assignments also
follow the discussion when the worker confirms eligible recipients. The follow
applies to activity after the source change, including replies that arrive
before delivery runs. A newer private choice takes precedence, and repository
mute suppresses automatic following. Group-only recipients need current
signed group evidence for automatic following. Direct inbox notices retain
their existing delivery and access rules. Email delivery remains open.

### Link related work

Writers link issues from the PR sidebar and can unlink from either subject.
Readers can follow backlinks. Links are repository-local relationships; linking
does not automatically close issues or satisfy merge requirements.

Read the pair endpoint for its current `active` and `version`, then send a desired
state such as `{"active":true,"version":0}` with `PUT`. A changed relationship
increments its version; an unchanged desired state returns the current version
without another event. An opposite change with a stale version returns 409.
Unlinking retains its version, so delayed retries cannot overwrite later changes.

Both lists use `limit` (1–50, default 30) and an exclusive numeric `before` cursor
ordered by the related issue/PR number. Items include the current title, state,
subject address, and relationship version. Relationship changes update both
indexes and one canonical activity event atomically; that event appears in both
timelines and once in the repository feed. Archived repositories keep backlinks
readable and reject fresh relationship mutations.

### React to discussions

Members with read access can add or remove reactions on issue and PR bodies,
comments, reviews, inline threads, and replies. The target is the resource path
relative to the repository API, for example `issues/1/comments/2` or
`pulls/1/threads/2/replies/3`. Parent IDs are validated; equal-numbered resources
have independent reaction memberships.

Send a desired state such as `{"kind":"heart","active":true}`. Supported kinds
are `thumbs_up`, `thumbs_down`, `laugh`, `hooray`, `confused`, `heart`, `rocket`,
and `eyes`. Repeating the same desired membership does not increment counts.
Responses expose counts and the current actor's membership, without listing
identity claims. Archived repositories retain readable counts and reject fresh
reaction mutations.

Discussion detail responses include a `reactions` summary. List summaries expose
body `reaction_count` through the transactional search projection. The summary's native `revision` orders
completed writes and refreshes so delayed responses cannot replace newer browser
state. Reaction writes do not change discussion edit versions.

### Retry creations safely

Issue, pull, comment, review, label, release, status, check, and merge creations use immutable UUID request reservations. Retry a lost response with the same UUID and identical payload.

```json
{
  "request_id": "01931b9e-4b3c-7b2a-b9f0-0123456789ab",
  "title": "Document repository setup",
  "body": "Include prerequisites and one working configuration."
}
```

Reusing a UUID with changed content returns HTTP 409. A retry can finish an interrupted reservation without allocating another visible number. Number allocation can contain gaps.

Edits include the displayed `version`. A stale version returns HTTP 409 and never overwrites newer content. The browser presents saved and draft content for manual conflict resolution; neither conflict action writes until the account saves again.

### Work with issues and comments

Members can list and read issues. Members can create issues and comments. Authors can edit their own content and close or reopen their issues.

Identity uses the exact OIDC issuer and subject. Display names do not establish ownership. The loopback server uses one trusted operator identity.

Issue lists use the indexed search/count/sort/cursor contract above. Comment
lists retain exclusive numeric cursors and descending comment-number order.

Issue responses load the label catalog only when returned issues have selected
labels or an edit needs to validate nonempty label input. Repository archive
checks and label permissions still apply, including when clearing labels.
A retried creation returns the existing issue with its current labels.
Response enrichment may fail after a mutation commits; retry creations with
the original submission ID, or reload an edited issue and reconcile its version.

### Pin repository issues

`GET .../issues/pins` returns the ordered collection `version`, up to three
current issue summaries and `can_manage`. Any current reader can view the cards,
including closed issues. Current Write access is required to change pins;
Triage alone does not grant it. Archived repositories retain their visible pins.

`PUT .../issues/pins` accepts `{request_id, version, numbers}`. `numbers` is the
complete ordered set of distinct existing issue numbers, with a maximum of three;
an empty set removes all pins. The native transaction compares the collection
version and changes the whole set atomically. Pinning leaves issue edit versions
unchanged. The collection lives in native Repository KV storage, alongside its
permanent submissions; cards use at most three indexed issue reads and maintained
comment counters. Existing repositories must explicitly prepare its decision
ledger with `cells prepare-issue-state`; historical successful versions, digests
and keys remain intact. No discussion-index rebuild is needed.

A submission UUID binds its original ordered set, expected version and qualified
actor. Exact retries recover the original version after reordering, archive or
loss of Write access, provided current Read access remains. They do not restore
the old collection. Reusing a UUID for another intent returns an unmarked
submission conflict. Proven native rejections carry `X-Crab-Write-Outcome:
rejected`, including when archive middleware changes their error presentation.
Those native rejection decisions remain terminal after roles/archive change.
A deliberate new action uses a fresh UUID; uncertain errors keep the original intent. The UI reloads the current collection
after success/recovery and records pin/unpin events in each issue conversation;
pure reorder does not add a timeline event.

### Work with pull requests and reviews

After a stock Git push, the repository Code and Pull requests pages can show
**Compare & pull request** for up to three of the signed-in user's recent
branches. Same-repository comparisons start at the current default branch.
Fork links use the upstream comparison flow and its existing access rules.
The shortcut disappears while an open PR uses the branch, or after a PR merges
that exact tip into the default branch.

`GET /api/repos/{owner}/{name}/pulls/recent-pushes` returns `base_ref` and
`items` with branch `name` and `oid`. The server considers the last four
eligible pushes in 24 hours, up to eight branches per push. It checks the
committed Git receipt against the transaction that owns each current ref.
An unpublished push, deletion, or replacement by another transaction
cannot supply a shortcut. Imports and automation pushes are excluded. The
shortcut uses a fresh comparison before authoring and publication.

The pull request composer fills the description from `pull_request_template.md`
on the destination repository's default branch. Crab looks in `.github/`, the
repository root, then `docs/`. Put named Markdown templates in a
`PULL_REQUEST_TEMPLATE/` directory under any of these locations. Authors can
choose a template in the composer or open a comparison link with
`?template=release.md`. A `body` parameter supplies the initial description
when both parameters are present. Choosing a different template requires
applying it explicitly, and changing the comparison branches keeps edits.

When a repository has no local pull request templates, Crab looks in the
namespace's `.github` repository. Inheritance requires current read access
to both repositories. A local empty default template provides a blank initial
description and overrides the shared catalog. The composer lists up to 32
named templates and accepts ordinary UTF-8 Markdown files up to 32 KiB each.

The browser compares the head to the Git merge base. It sends the exact
base tip and head from that preview when creating the pull request. If either
tip changes before submission, the API rejects the request before creating a
pull, and **Review inputs** refreshes the comparison while keeping the draft.

The composer checks for an open or draft PR for the selected source repository,
head branch, and base branch. **View pull request** opens that review. The
Repository Cell repeats the same check when creating a PR, so concurrent
submissions can create only one new review for that comparison. Different base
branches and forks are independent. A comparison becomes available when it has
no open PR.

`GET .../pulls/open-comparison` accepts `base_ref`, `head_ref`, and optional
`head_repository`, and returns a nullable `pull_number`. A duplicate creation
returns HTTP 409 with error code `open_pull_exists`, the existing
`error.pull_number`, and the terminal write rejection marker. Repeating that
submission recovers its original decision, including after the other PR closes.
The browser preserves the draft and links to the existing review.

The opt-in real-change fixture takes the base, head and common-ancestor trees
from pinned Next.js and Neovim commits and publishes them as short divergent
histories. It checks every changed path, mode and object ID against stock Git,
checks the full bytes of one lazily loaded file, and moves the source branch
while the browser holds an open draft. Chromium checks that the author must
refresh and resubmit, that an inline comment appears on the expected change,
and that viewed progress survives reload. The Next.js case saves 1,178 marks
through the HTTP API and uses Chromium to save the final mark and reload the
full count. The fixture also checks that the server rejects a continuation
from the earlier snapshot. It reports cold comparison and warm concurrent
comparison and file-read latency with unrelated repository maintenance paused.

One local RustFS run recorded the following timings. Each warm p95 uses eight
requests of its type, with two comparisons and two file reads in flight per
round. Browser timing includes navigation to **Files changed**, path filtering,
and rendering the inline comment.

| Source trees | Changed paths | Cold comparison | Warm comparison p95 | Warm file p95 | Browser filter and thread |
| --- | ---: | ---: | ---: | ---: | ---: |
| Next.js | 1,179 | 4,572 ms | 148 ms | 48 ms | 830 ms |
| Neovim | 536 | 196 ms | 77 ms | 43 ms | 445 ms |

With `agent-browser` and the built UI available, set `CRAB_REVIEW_CORPUS_DIR`
to a directory holding `vercel/next.js.git` and `neovim/neovim` at the commits
pinned in the fixture. Use the per-checkout `CARGO_TARGET_DIR` and the same
isolated loopback RustFS variables as the search corpus test. Run
`cargo test -p crab-server --lib --locked real_repository_pull_review_matches_git_and_reconfirms_a_moved_head -- --ignored --nocapture`.

The **Commits** tab lists commits reachable from the pinned PR head and absent
from the pinned base. The verified ancestor index lets the server exclude shared
commits without reading the entire base history for each page. While the index
is building, the server can also exclude older history if a bounded read of
verified parent links proves that all topic paths end in the known shared
history. Inconclusive cases retain the bounded base walk. The native RustFS
fixture builds 10,050 shared commits and a topic merge with a parent below the
merge base. It checks the unindexed merge base with a ten-commit read budget and
opens a PR against the inspected tips. Two unindexed pages cover both topic
lanes, and their commit membership matches `git rev-list`. New continuations
page in stored Git parent order. While Chromium displays the commits, the
fixture publishes the graph and verifies that the existing cursor yields the
same page and that the browser retains the same lane order. Run with the
same RustFS environment and built UI using
`cargo test -p crab-server --lib --locked deep_pull_commit_pages_match_git_and_render_in_the_browser -- --ignored --nocapture`.

Pull details and summaries expose nullable `closed_at` and `merged_at` in Unix
milliseconds. A successful close records its transition time; reopening clears
it. A completed merge sets both to the native completion time, independently of
`merge.created_at` (reservation time). Later metadata edits, no-op closure changes
and merge retries preserve these dates.

List rows distinguish opened, closed and merged states and retain draft labels.
The pinned MIT-licensed `@github/relative-time-element` 5.3.1 owns date rounding,
live refresh and its default 30-day calendar-date threshold. shadcn tooltips show
localized exact dates on hover or keyboard focus. Unknown historical dates remain
explicitly unknown, including in the merge summary. See the [upstream component
contract](https://github.com/github/relative-time-element) and [documentation](https://github.github.com/relative-time-element/).

Existing native repositories require `cells prepare-pull-state --owner OWNER
--name REPO`. This bounded, restartable preparation derives metadata only from
matching immutable activity and leaves original records, versions and command
receipts unchanged. Unprepared reads/writes return 503 `schema_upgrade_required`.
No discussion-index rebuild is needed. See [native lifecycle preparation](CELLAR.md#prepare-pull-lifecycle-metadata).


Pull list rows, including PR rows returned by issue search, include `rollup` with
`number`, captured `head_oid`, `checks` and `reviews`. Check counts distinguish
success, neutral, skipped, pending and failed outcomes. Latest commit statuses
and latest check runs remain separate sources. The newest run wins within
each case-insensitive name and provider connection.
Review counts include selected-head approvals, unresolved requests for changes,
outstanding review requests and decisions anchored on earlier commits. Requested
changes remain active after a new push until the same reviewer replaces the
decision or an authorized user dismisses it. They describe recorded reviews, not merge
eligibility or current branch-protection satisfaction.

The API captures the selected source refs in one verified ref view, then one
native read compares the selected PR versions and computes the summaries in one
Cell snapshot. Concurrent source record changes return `page_changed`; failed
ref/storage reads fail the list rather than returning green or empty counts.
Missing source refs produce null head/check/review fields. Merged rows use the
recorded merged source head, including after branch deletion. Issue-only pages
open no Git ref view for this enrichment.

Discussion index format 11 adds a per-PR outstanding-request counter, maintained
by the reviewer projection's insert/update/delete triggers. It avoids scanning
historical participants. Existing Cells require explicit `cells rebuild-search`
preparation; serving never upgrades implicitly. The bounded clear/rebuild resets
aggregate rows before clearing participants, then reconstructs them from canonical
review/request records. SQL/KV records, issue/PR versions and original reviews
remain unchanged. Update serving peers and the bundled UI together.


Pull creation records exact base and head branch names plus their opening commit OIDs. Detail reads refresh both live tips without rewriting the immutable opening evidence.

Browser creation supplies `expected: {base_oid, head_oid}` from its completed
comparison. The server rechecks both refs under the same leases native Git
writes use and holds them through the Repository Cell decision. For a fork,
it leases the upstream base before the fork head. A busy ref returns HTTP 429
and the same submission can be retried. A stale or deleted ref records a
terminal 409 `comparison_changed` with the rejected write-outcome header.
Review the refreshed comparison and use
a new submission UUID. Exact retries recover the original result even if refs
have moved again. Tagged ref-only create clients retain server-side tip capture.
Inline review `base_oid` identifies the effective comparison base while PR
detail `base_oid` identifies the destination tip for merge publication. Existing
threads keep their original anchors and may appear as outdated after this change.

The Files changed tab stores Viewed progress per signed-in account and PR.
`GET /api/repos/{owner}/{name}/pulls/{number}/viewed` returns `version`, a
map of path IDs to file revisions, and a nullable `next` cursor. For another
page, send `after` with that cursor and `version` from the first page. The
server returns `409 page_changed` if a write changes the snapshot. The browser
loads the complete snapshot before showing progress or enabling **Hide viewed**.
`PUT` takes `version`, `file` (the `view_id` from the comparison), and `viewed`,
and returns the resulting `version`. A stale write requires reloading progress.
A new commit retains marks for identical diff entries and clears them for files
whose compared content or mode changed. Each account can save up to 100,000
path identities per PR across its comparisons. Existing marks remain readable.
Progress writes stay separate from PR versions, checks and activity.

Use **Filter changed files** to narrow the file tree and diffs by path on
pull requests, branch comparisons, and commits. **Hide viewed** in a pull
request uses the saved progress for the exact comparison. **Jump to diff**
reveals a filtered file and focuses its review annotation. The filter
controls retain their values so you can resume the same review scope.

**Hide whitespace** compares complete text files without spaces, tabs, or
other ASCII whitespace within each line. The diff keeps line breaks,
original text, and original line numbers. Added, deleted, binary, and truncated
files use their normal presentation. If a review comment is outside the shown
hunks, **Jump to diff** opens the original hunk without changing the preference.
The browser remembers the choice per pull request. Branch comparisons and
commit pages share a separate preference.

Use **Previous unresolved** and **Next unresolved** on **Files changed** to
step through all unresolved conversations in newest-first order. The controls
reach conversations beyond the loaded pages. Current comments open at their
diff anchor without clearing file filters. Outdated comments open in the
outdated conversations list. The review header counts all conversations for
the current comparison, and file badges indicate when their counts cover only
loaded conversations. Sign-in preserves conversation and diff anchors in
shared review links.

`GET .../threads/navigation` accepts an optional `around` thread number and
returns `base_oid`, `head_oid`, and a `summary`. The summary contains
`open` and `resolved` counts for the current comparison, `outdated` and
`outdated_unresolved` counts for earlier comparisons, and nullable
`previous` and `next` unresolved thread numbers. Without `around`, `next`
is the newest unresolved conversation. A resolved anchor can still be used
as the starting position. The endpoint requires current repository read
access.

If either branch is deleted, the conversation and original OIDs remain visible.
A completed merge retains its pre-merge base and head IDs after source deletion.
**Files changed** and **Commits** read the recorded comparison while its Git
objects remain in storage, including after squash or rebase. The server checks
current access to the PR repository and any source fork. Inline review anchors
use the same recorded comparison as the base route
`GET .../pulls/{number}/merged-comparison`.

| Path | Response |
| --- | --- |
| Base route | Changed files from the recorded comparison. |
| `/file?path_hex=...` | One changed file from that comparison. |
| `/commits?limit=50&cursor=...` | Original commits reachable from the recorded head and excluded from the recorded base, with a signed `next` cursor when another page exists. |
| `/commits/{oid}` | Commit metadata and changed files relative to its first parent. The OID must belong to the recorded range. |
| `/commits/{oid}/file?path_hex=...` | One file changed by that commit. |

Ordinary repository commit browsing requires Git reachability. The **Checks**
tab reads stored reports for the recorded merge commits independently of their
Git objects.

The active v2 pack inventory supplies saved PR comparisons. Repack preserves
the object IDs in its replacement packs. Pruning recovery history drops old
checkpoint references and capture indexes while the active inventory continues
to supply the recorded commits. Repository GC keeps the pack sources in that
active inventory. Restoring an older repository state or removing stored Git
objects can make a saved comparison unavailable.

Reviews record `commented`, `approved`, or `changes_requested` against the exact current head. Pull authors cannot approve or request changes on their own pull. Advancing the head marks older reviews as not current.

On branches with required approvals, only current explicit repository Write and
Admin grants count toward the requirement. Other participants can still review.
OIDC group claims belong to the signed-in session alone, so group-only grants
provide no durable evidence about another reviewer. The protected `review:required`
and `review:approved` filters use the same eligible reviewer scope. List rollups
and `review:changes_requested` continue to describe recorded participation.

Review submission opens a shadcn “Review changes” dialog above the diff. The
modal stays fixed while asynchronous file content loads. Comment, Approve and
Request changes use the shared radio-group component. Draft text and submission
identity stay with the pull detail, surviving dialog closure and data reloads.
The summary editor and in-flight submission persist for the lifetime of the pull
detail. Pending inline comments persist on the server across browser reloads.

Choose “Add to review” in the inline editor to save a private comment with an
optional suggested replacement. Choose “Comment” to publish that comment
immediately. The signed-in reviewer can edit, remove, or discard their pending
comments and submit them from “Review changes.”

`GET .../pending-review` returns the current account's `{version, comments}`.
`PUT .../pending-review` accepts `{expected, request_id, edit}`. The edit is
`{action:"save", comment, replace?}`, `{action:"remove", id}` or
`{action:"discard"}`. The saved comment uses the inline thread creation
shape and the same request UUID as the top-level edit. One draft holds up to
20 comments and 96 KiB of encoded draft data, pinned to one comparison.
Reload after a version conflict to see the current private draft.

`POST .../pending-review/submit` takes `{expected, request_id, body, state}`
and returns `{review, threads}` with the published record IDs. The summary
may be empty when the draft contains comments. Stale comparisons reject
submission and preserve the draft. A single repository Cell transaction
writes the review, threads, activity, and submission receipt and then clears
the draft. Retry the same submission UUID and payload to recover the resulting
IDs after a lost response, even if a new draft or comparison now exists.
Deploy the updated server peers and bundled browser UI together.

Repository writers can apply a single suggestion or select up to 20 and commit
them together. A batch keeps its selected comment versions while the writer
navigates between threads, including threads beyond the loaded page. The
commit dialog lists the selected paths and lines, allows removing suggestions,
and shows the destination branch and editable commit message. Crab uses the
writer's ordinary Git identity and credits each other suggestion author once in
`Co-authored-by` trailers.
`POST .../suggestions` takes
`{request_id, expected_head, threads: [{number, version}], message?}` and
returns `{commit}`. Use a fresh UUID for a new batch. Retry the same UUID,
expected head, selected versions, and message to recover the original commit
after a lost response or a busy reply. Recovery requires current write access
and uses the durable publication receipt, including after the branch advances.
The browser saves the original request for the signed-in account and exact PR
and destination repository in the current tab. After refreshing **Files
changed**, choose **Resume suggestion commit** to retry, even if the original
head has moved or its branch was deleted. The tab clears the saved request after
confirmation, an initial definitive rejection, or dismissal.
Crab prepares the selected comment versions and anchors in a single Repository
Cell snapshot, then verifies current file blobs and nonoverlapping line ranges
before building one commit against the expected head. The publication worker
rechecks the pull and verifies all selected comment versions together while
holding the target ref lease. Edits preserve untouched bytes and executable modes. Each
edited file is capped at 900 KiB and the resulting files at 4 MiB combined.
Archived repositories and branch protections follow existing Git publication
rules. A stale selection leaves the branch unchanged.

For a private fork PR, send the same request to the fork's
`upstream-pulls/{number}/suggestions` route and bind the repository identity
header to that fork. The fork's recorded upstream supplies the PR and review
records. Crab verifies that the PR names this fork and applies the fork's
current write permissions, branch rules, and archive state to publication.
The native Chromium qualification selects 20 suggestions across thread pages,
reloads after a lost reply, and recovers the original commit. Stock Git verifies
the result and its parent on the fork and the unchanged upstream ref.

`POST .../pulls/{number}/reviews` requires `request_id`, `body`, `state` and
`commit_oid`. The latter is the exact lowercase, nonzero SHA-1 inspected by the
browser. A fresh submission is rejected with 409 `conflict` if the live head
has advanced. Reloading preserves the draft and its old commit pin; the user
must explicitly choose “Use current comparison” after inspecting the updated
comparison. A changed pin creates a new submission identity. There is no
implicit latest-head default or old-payload decoder.

Existing submissions are verified by the native immutable ledger, including
the original body, decision and commit. Editing the current review does not
invalidate a valid retry; a mismatched payload returns `submission_conflict`.
Replays do not add another review or fulfill a newer request. Current native
permission and archival policy still applies. Review freshness after merge
uses the recorded source head, matching the PR comparison, rather than the
synthetic merge commit. Review `current` is nullable when the source ref is missing. Canonical records and native inputs are unchanged.
Deploy the API and bundled UI together for the required commit pin.

Inline threads store a raw Git path, old/new side, inclusive line range, exact base/head OIDs, and the anchored blob OIDs. Root comments publish immediately; repository members can create and reply, replies are durable child records, and every creation is idempotent by UUID. Pull authors and writers can resolve or reopen a thread, while only the thread author can edit its body or suggestion. A thread is marked outdated when the comparison OIDs or anchored blob no longer match; deleted paths remain listed with the original anchor evidence.

Suggestions are allowed only on new-side text anchors. The browser commits a
current suggestion against the exact reviewed head and file blob. Stale
branch or blob tips return a conflict.

For protected branches requiring review, only each reviewer's latest approval on the current head counts. Unresolved requests for changes also block merging when anchored on an older commit; pushing new commits cannot resolve another reviewer's decision. The same reviewer must submit a current approval, or an authorized user must dismiss the active review. This follows [GitHub's required-review behavior](https://docs.github.com/en/pull-requests/how-tos/review-pull-requests/approving-a-pull-request-with-required-reviews).

### Review status filters

Both discussion list kinds accept `review:none`, `review:required`,
`review:approved`, and `review:changes_requested`, including negation and Boolean
expressions. These predicates, matching totals, state destinations and paging
operate over the whole repository in one native snapshot. Comment-only reviews
and dismissed opinions do not count as an active decision for `review:none`;
`reviewed-by` still includes their historical participation. Changes requested
remain active across pushes. Required/approved compare selected-head approval
counts against the effective base-branch rule; unresolved changes take precedence.
An unprotected branch is never review-required, and an approved match still needs
at least one active approval. Checks and other merge conditions are independent.

Head-dependent filters bind the verified Git source token to a ready immutable
ref projection. Pending reconciliation returns 503 `review_index_pending` with
visible retry. Source changes and effective rule edits invalidate cursors.
An empty repository requires no Git manifest. Completed merges use the recorded
source head, decoded by the canonical binary codec, even after branch deletion.
Filtered rollups retain the same captured source rather than resolving newer refs.
Missing unmerged source refs have no proven latest head: rollups report null
source/check/review fields. Retaining that historical source head remains parity
work; the opening OID is not used as a freshness fallback.

`GET .../discussion-search/reviews?query=...` returns grammar-preserving native
menu destinations and selection. The shadcn Reviews menu exposes the four status
filters plus reviewed-by-you, not-reviewed-by-you and awaiting-your-review.
Existing named reviewer search/completion remains available. Team requests are
explicitly unavailable until their native model exists. Menu reads do not enroll
a profile. Diagnostics remain behind `powerbar=1`.

Discussion index format 12 adds derived source-ref/merged-head columns. Run
`cells rebuild-search --owner OWNER --name REPO` explicitly for existing Cells.
Preparation preserves canonical SQL/KV records, versions and permanent submission
ledgers. Transport receipts retain their normal bounded expiry. Serving never
installs this relation. Upgrade all peers and embedded UI together
for the changed private search/rollup input contract.

### Dismiss a pull request review

PUT `/api/repos/{owner}/{name}/pulls/{number}/reviews/{review}/dismissals`
accepts `{request_id, version, body}`. The nonempty reason uses the existing
64 KiB discussion-body contract. Repository Write permission is required.
Administrators can configure **Restrict who can dismiss pull request reviews**
in a branch ruleset with a pull request requirement. The rule's optional
`review_dismissal` array contains up to 64 `{kind: "user" | "team", id}` entries.
Each matching explicit rule must allow the actor through a named account or
current team membership. An empty array prevents manual dismissal. If no
explicit rule matches, a protected base still requires Admin permission.

Repository rulesets offer collaborators and visible teams with repository
access. Organization rulesets offer active organization members and visible
teams. Organization team membership can authorize a writer with a separate
repository grant. Repository readers see that an active restriction applies,
and repository ruleset administrators can see its local selections.

The native transaction reads current
branch protection, archive and pending merge state, verifies the review version,
records the explanation, advances review/PR versions, removes only the matching
active decision and appends one conversation event. An older dismissal cannot
erase a newer decision or revive a prior approval. Merge publication blocks
fresh dismissals until its reservation resolves.

The configured version-zero policy applies before the first administrative save,
as it does for merge and Git publication. Native dismissal receives that initial
branch classification from HTTP composition; rules saved in the native transaction
take precedence, including an explicitly empty set. Review create/edit/detail/list
and conversation capabilities use the same ruleset policy. A tracked write worker
refreshes membership inside the shared ruleset lease, which also serializes
default-branch changes. The native command verifies the evaluated base ref so a
concurrent PR retarget cannot use a decision made for its former base.

Public review details, lists and conversation entries expose effective state
`dismissed`, nullable `dismissal: {author, body, created_at}` and `can_dismiss`.
Original submitted state/body remain available to their native receipt contract;
dismissal is a separate canonical SQL relation. Public attribution resolves both
actors without exposing provider-qualified keys and rechecks current membership.
Dismissed latest reviews remain visible in the reviewer sidebar; active decision
and list/merge summaries use the canonical decision table. Search participation
still records the submitted review. A discussion-index rebuild cannot revive it.

A permanent submission UUID binds actor, review, expected version and reason.
Exact retries do not reapply the action after later reviews, archive or Write
revocation; current Read access is still required. Changed intent conflicts.
Proven rejections carry the shared write-outcome header; uncertain errors retain
the original payload. The shadcn dialog preserves the reason through cancellation
and conflicts, requires inspection of a newer version, and returns keyboard focus
to the review after success. Reasons appear with the original review; the later
conversation event links directly to it.

Run private `cells prepare-review-state --owner OWNER --name REPO` on existing
repositories. Upgrade peers and embedded UI together for the private dismissal
command contract. This
workflow follows [GitHub review dismissal](https://docs.github.com/en/pull-requests/how-tos/review-pull-requests/dismissing-a-pull-request-review)
and its [protected-branch permission contract](https://docs.github.com/en/rest/pulls/reviews#dismiss-a-review-for-a-pull-request), checked 2026-10-22.

### Change a pull request's base branch

An author or repository writer can select **Change base** on an open pull
request. Choose a branch in the upstream repository and inspect the merge-base
comparison before confirming. This also works when the head is in a private
organization fork. The new base and the inspected head must still match the
preview, and the pull request must be free of a pending merge.

Changing the base writes a conversation event and dismisses active approvals.
Requests for changes stay active. Review threads retain their original anchors
and may become outdated. The new target's branch policy and CODEOWNERS rules
apply immediately. Required checks continue to use the exact head commit.

`POST /api/repos/{owner}/{name}/pulls/{number}/base` takes `request_id`, `version`,
`base_ref`, `base_oid` and `head_oid`. Use the selected branch tip as `base_oid`
and the compared head tip as `head_oid`. A successful retry with the same
inputs returns `saved: true`. The pull request detail returns the current
`base_revision`. Include it when submitting reviews or pending review batches
after a base change.
[GitHub base branch changes](https://docs.github.com/en/pull-requests/how-tos/create-pull-requests/changing-the-base-branch-of-a-pull-request)

### Update the head branch from the base

Open a pull request's **Conversation** tab and choose **Update branch** when
the head lacks commits from the base. Crab creates a merge commit on the head
branch. The first parent is the head you reviewed and the second is the base
shown beside the action. The pull request stays open at the new head so it can
receive fresh approvals and checks.

The action requires Write access to the head repository and works for
same-repository and private organization fork pull requests. Crab verifies both
tips and applies the head branch's protections, rulesets and file locks before
publication. If the tips changed, refresh the comparison. If the changes
conflict, choose **Resolve conflicts**, edit each file, and **Mark as resolved**.
Choose **Commit merge** to publish the merge commit on the head branch. The
browser editor supports overlapping changes in existing UTF-8 text files.
Resolve binary and structural conflicts with Git. Head branch policy still
applies when Crab publishes a browser resolution.

To publish a resolution when the head branch is protected, select **Create a
branch and update this pull request** in the conflict editor. Enter an unused
branch name in the head repository. Crab applies branch creation rules,
publishes the resolved commit, and then switches the existing pull request to
the new head. The switch records an activity event, expires prior approvals,
and starts a new review comparison. If the PR or a captured tip changes before
the switch, Crab shows a link to the branch and leaves the PR on its current head.

On a closed or merged pull request, **Delete branch** removes the recorded
head branch from the source repository. The current tip must match the saved
head, and no open pull request can still use the branch as its head.
**Restore branch** recreates a missing head at that same commit if its Git
objects are still present. Merged pull requests use the original head recorded
at merge. Closed pull requests use the head saved in their comparison. Both
actions require current Write access and follow the source repository's branch
protection and rulesets. The repository's default branch cannot be deleted.

When the merged pull request's head and base are in the same repository,
**Delete branch** on the pull request and automatic deletion move eligible open
pull requests from that branch to the merged pull request's base. The confirmation
lists the affected pull requests and the new base. Manual and automatic deletion lease the relevant
refs, including any dependent fork heads, and save the new comparisons as one
repository transaction before publishing the deletion. Base changes use the
same review invalidation and CODEOWNERS behavior as a manual base change.
The branch is kept if the batch would leave an invalid comparison or contains
more than 16 dependent pull requests. For a closed unmerged pull request or
a merged pull request from a fork, a branch used as a base is kept.

From the **Branches** page, the delete confirmation shows the same impact
when merged pull requests for the exact branch and commit agree on the new base.
Stock Git can delete these branches with `git push origin --delete <branch>`.
One atomic push can remove several independent branches and move up to 16
dependent pull requests in a single repository transaction. Push changes to a
replacement base or dependent head separately. If matching merges point to
different bases, use **Delete branch** on the intended merged pull request.

Repository administrators can enable **Automatically delete head branches**
in **Settings** under **Pull request merge methods**. Use the setting in the
source repository, including a fork. Crab records the opt-in and the source
branch's Git transaction during publication, then queues deletion when the
merge completes. Direct merges, auto-merge, and the merge queue use the same
cleanup. The actor who publishes the merge needs current Write access in the
source repository. Cleanup skips archived repositories, the default branch,
protected branches, ruleset violations, branches with open pull requests that
cannot be moved, and branches that moved or were recreated. Changing the setting
invalidates pending cleanup from the previous setting. Restore an automatically
deleted branch the same way as a manually deleted branch.

`PUT .../settings/merge-methods` accepts the optional administrator-only
`delete_head_branches` boolean. It defaults to `false`.

`GET /api/repos/{owner}/{name}/pulls/{number}/head-branch` returns the available
`branch` action with its pull version, source repository UUID, and commit.
When dependent pull requests can move, `branch.retarget` lists their numbers
in `pulls`, their new `base_ref`, and a `token` for confirming that exact set.
`cleanup_pending: true` indicates that automatic deletion is queued for a
branch that still offers the delete action.
`POST` accepts `action` (`delete` or `restore`), `request_id`, `version`,
`repository_id`, and `head_oid`. Include `retarget_token` when confirming a
delete action with dependent pull requests. Publication checks the exact tip or its absence
under the Git ref lease. Retrying a recorded request recovers its result
without repeating the ref edit or retargeting the same pull requests again.
For a private fork, use the fork's repository route and include the PR's
repository UUID as `upstream` in the query string.

`GET /api/repos/{owner}/{name}/branches/deletion?name=…&expected_oid=…`
returns `retarget: null` for an ordinary deletion or the matched pull request
number, version, destination, dependent numbers, and confirmation token. Submit
a stacked deletion through the returned pull request's `head-branch` action.

Reopening a closed pull request requires both branch names to exist, including
the exact source repository for a fork head. Conversation, bulk, and direct
state edits keep the selected refs leased through the native state decision.
A missing ref returns HTTP 409 `pull_branches_unavailable`. A combined comment
or bulk edit rolls back if reopening fails. Exact retries recover recorded
state submissions. Once the branch is restored, start a fresh reopen action.

Choose **Update with rebase** from the branch update menu to replay the head
commits on top of the captured base. Confirm **Rebase branch** to replace the
head history with the new commits. Crab preserves authors and commit messages,
checks the final tree against a three-way merge, and applies the head branch's
protections, rulesets and file locks. A rebase that cannot reproduce the merge
result is rejected. Stock Git receive still rejects non-fast-forward pushes.

`GET /api/repos/{owner}/{name}/pulls/{number}/update-branch` returns `update`
with `request_id`, `version`, `base_oid` and `head_oid` when the head can be
compared and lacks the base commit. Submit that object with `POST` to the same
route. The `method` defaults to `merge_commit`. Set it to `rebase` to replay
the commits. `resolutions` are valid for `merge_commit`. For a fork, use
`/api/repos/{fork-owner}/{fork-name}/upstream-pulls/{number}/update-branch`
and bind the write to the fork repository ID. Retrying the same submission can
recover the recorded commit after an interrupted response.

Add `new_branch` with a short branch name to create a replacement head. The
reply includes the fully qualified `branch` and `pull_updated`. A false
`pull_updated` acknowledges Git publication and reports an incomplete PR switch.
Retrying the identical request can recover both results from their receipts.

Pass `?conflicts=true` to preview the marked text of conflicting files. The
response includes `conflicts` with each file's `path`, `path_hex` and `text`.
Submit the preview's `update` with a `resolutions` array of `path_hex` and
replacement `text` for every conflict. The preview and resolution support at
most eight conflicting files, 128 KiB per file and 512 KiB total. Crab rechecks
the exact tips and conflict paths before publishing. Retrying the same
submission recovers the original merge commit if publication already finished.

### Merge a pull request

Writers call `POST /api/repos/{owner}/{name}/pulls/{number}/merge` with a UUID
`request_id`, pull `version`, `method`, exact base and head OIDs, and an optional
message for methods that create a single commit.

| Method | Contract |
| --- | --- |
| `fast_forward` | Moves the base when it is an ancestor of the head |
| `merge_commit` | Creates a two-parent commit after bounded tree and text merging |
| `squash` | Creates one commit with the same merged tree and the base as its only parent |
| `rebase` | Replays the topic changes as separate linear commits with original authors and messages and new committer metadata |

The browser exposes enabled methods in the pull request. The confirmation
lets writers edit the merge or squash message. Repository maintainers use
**Settings → General → Pull request merge methods** to choose at least one
method. `GET /api/repos/{owner}/{name}/settings/merge-methods` returns
`{version,enabled}`. The `PUT` accepts `{expected_version,enabled}` and rejects
stale writes. Merge queue fields in that request require Admin. Repositories
initially enable all four methods. Pull request
detail returns the enabled list in `merge_methods`.

Direct PR merges reject binary, type, delete/modify, and overlapping text
conflicts. Use [Update the head branch from the base](#update-the-head-branch-from-the-base)
to resolve supported text conflicts before merging. Individual text blobs can
contain at most 8 MiB. Rebase skips
empty and merge commits and checks the final tree against the three-way
merge result. It refuses a history that would lose a merge-only edit or
produce a different result. Rebase is bounded to 256 topic commits and a
bounded set of generated Git objects. Signature headers are dropped from
rewritten commits. Both methods leave the source branch tip intact.

The server acquires source and destination ref leases and both GC fences. It
revalidates tips, grants, enabled merge methods, approvals, required checks,
pointer dependencies and visibility before publishing the destination. New reservations bind the exact
ref edit to a durable Git publication plan. A retry checks the plan receipt
before reading current tips, so it can finish recorded PR state after a
subsequent destination advance. Ambiguous publication keeps the exact original
intent pending. Reservations from the tagged alpha retain their original retry
semantics because they predate plan receipts.

### Automatically merge a pull request

Repository administrators enable **Allow auto-merge** in **Settings → General
→ Pull request merge methods**. It is disabled by default. Writers can then
select **Enable auto-merge** on a blocked pull request, choose an enabled
method, and confirm the head commit and optional commit message. The PR author
and current writers can disable a waiting request.

Crab persists the request in the repository Cell. The background worker finds
waiting requests after startup and rechecks current approvals, conversations,
check results, branch policy, and the scheduler's durable Write grant. The
head commit and both branch names are pinned. The base tip can advance on the
selected branch. A new head commit requires fresh consent, including a push by
a writer. Session-only OIDC group grants require a durable repository or team
grant before the user can schedule a merge. Automatic merges use the standard
leased publication and receipt path, with no ruleset bypass.

`GET .../pulls/{number}/auto-merge` returns the current request and actions.
`PUT` takes `request_id`, `version`, `base_oid`, `head_oid`, `method`, and an
optional `message`. `DELETE` takes the active `request_id`. The current request
reports why it stopped. `PUT .../settings/merge-methods` accepts an optional
`auto_merge` boolean alongside the enabled methods and `expected_version`.

### Test a prospective merge

On an open, ready pull request, writers can select **Prepare merge candidate**
on the **Checks** tab. Crab combines the captured base and head in a two-parent
commit by default. A branch that requires linear history uses the first enabled
squash or rebase method. Crab publishes the tip at `refs/crab/merge-queue/<pull-number>` under
both source ref leases. Authorized Git clients can fetch that ref. Crab owns
its namespace and applies the normal repository and artifact access checks.
The protected branch advances only through its regular merge workflow.

Use **Candidate checks** to inspect CI for the prepared SHA. To run a connected
GitHub Actions workflow, fetch the candidate and push the displayed mirror
refspec to the configured GitHub repository. Configure the workflow's `push`
branch filter to include `crab/merge-queue/**`. Each mirror branch includes the
Crab repository UUID, PR number, and candidate SHA. Signed workflow deliveries
must match that branch and the current candidate. A change to either source tip
or the PR comparison makes the candidate stale. Writers can prepare a new one
or remove the stale ref. Candidate checks appear separately from head checks.
Crab retains the current candidate ref until it is replaced or removed.

Select **Merge tested candidate** under **Candidate checks** to publish that
exact SHA. The base branch must require at least one check, the candidate's
method must be enabled, and the pull request and candidate must both satisfy the
base branch policy. This includes head checks, reviews, conversations, and an
up-to-date head when required. Crab rechecks current write access, both source
tips, the candidate ref, file locks, and the current policy before publication.
The merged pull request records the tested SHA.

After merge, **Checks** opens the recorded reviewed head. If Crab published a
tested candidate, **Tested merge checks** shows reports for that separately
recorded commit. Both views list stored commit statuses and check runs,
including after squash or rebase and source branch deletion. These are the
reports currently stored for the displayed SHA.

`GET .../pulls/{number}/merge-candidate` returns `preview` and `candidate`.
Submit `preview` with `POST` to prepare the recorded comparison. It contains
the request ID, PR version, exact tip OIDs, prior candidate OID, and the
`method` when the branch requires linear history. `DELETE`
accepts the candidate's `commit` to remove that exact ref. Retrying a completed
preparation returns its recorded commit, including after the ref is removed.
The candidate's `promotion` field describes tested-merge eligibility.
Submit `POST .../pulls/{number}/merge` with the candidate's `method`,
`tested_candidate_oid`, `request_id`, `version`, `base_oid`, and `head_oid`
to publish the candidate. Retry an interrupted response with the same request.

### Merge a pull request through the queue

Enable **Require merge queue** on an exact branch in **Settings → Branches**
to make the queue the branch's merge path. The branch rule must name a
required status check or GitHub Actions workflow. A required branch uses the queue even when optional
queues are disabled. Native pushes, direct PR merges, ordinary auto-merge,
and manual candidate promotion cannot publish to that branch. Crab still
recovers any merge whose Git publication committed before the rule changed.

The branch protection API accepts `require_merge_queue: true` in the rule.

To cover matching branches across repositories, enable **Require merge queue**
in a repository or organization branch ruleset. The rule can match the current
default branch, exact branches, or include and exclude patterns. A matching
rule protects existing branches from updates and
deletion through Git HTTP. Branch creation follows the ruleset's creation
policy. An explicit ruleset bypass applies only to the selected rule. Classic
protection and every other matching ruleset still apply.

To offer the queue on other branches, enable **Allow merge queue** under
**Settings → General → Pull request merge methods**. It is disabled by default. On a ready pull request with passing
head checks and reviews, select **Add to merge queue**. The target branch must
require at least one check.

When the branch requires the queue and the pull request is still blocked by
reviews or checks, a writer can select **Merge when ready**. The request
survives reload and worker restart. Crab gives it a queue position once the
exact head satisfies current requirements, then builds and checks a candidate
against the live base and earlier queue entries. A new head needs fresh
confirmation. Writers can cancel from the pull request before it enters the
queue.

In the same settings section, **Queue merge method** chooses the repository
default of merge commit, squash, or rebase. It is independent of the enabled
direct merge methods and defaults to merge commit. **Branch queue overrides**
let administrators override the method, build limit, and check timeout
independently for up to 100 exact branch names. Each unset field inherits the
repository default. Branch protection and rulesets determine where a queue is
required.

Changing the effective queue method, adding a method override, or removing
one invalidates waiting entries on the affected branches. Re-add those entries
to build and test candidates with the current method. Squash produces one commit on the base.
Rebase uses the existing bounded history replay and preserves the replayed
commit messages. Crab tests and publishes the tip of that exact history.

Crab keeps the admission order separately for each target branch. Set
**Concurrent queue builds** to a value from 1 to 100 in the repository's merge
settings or on an exact branch override. The repository default is 1.
The first entry tests the branch's latest tip.
Each following entry within the build limit tests a candidate that includes
the changes ahead of it. The server resolves the ordered prefix from one
Cell snapshot and reads its candidate commits together from one pinned Git
snapshot. If the base advances unexpectedly or an earlier
entry is removed, Crab rebuilds the affected candidates and requires checks on
their new SHAs. When an earlier candidate becomes the base exactly as tested,
its successor keeps its existing check results.
Writers can inspect that commit under **Candidate checks**. Connections
configured for [automatic GitHub Actions dispatch](#start-github-actions-for-a-merge-queue-candidate)
show the launched run and its status. Other required checks can report on the
candidate Git ref or use the configured GitHub Actions mirror. Crab publishes
each tested SHA automatically, in queue order, after the required checks pass.
Later results cannot advance the branch while earlier entries are waiting.
Strict head freshness is satisfied through the tested candidate. The PR head
must still pass its own checks and meet current review and conversation policy.

A failed check, unresolved conflict, changed head, revoked Write grant, or
expiration of the configured **Queue check timeout** removes the entry and
advances the queue. Set the timeout from 1 to 720 minutes. It defaults to 60
minutes. For a candidate that includes earlier entries, Crab evaluates failures
against the live base when the entry reaches the front. PR authors and writers
can remove an entry before publication is
pending. A PR in the queue can only merge through the queue.

Open **Pull requests → Merge queue** or follow **View merge queue** from a
queued PR to see the entries for one target branch in merge order. The queue
shows the requested head, whether a candidate is scheduled, and whether
publication is pending. Open **Candidate checks** for the current result and
tested commit. Readers can inspect the queue. Writers and PR authors can
remove an entry by its exact request before publication is pending.
Repository admins can select **Move to front** for an urgent pull request.
Crab keeps the entry and its recorded requester and rebuilds candidates for
the reordered branch. Checks on earlier candidate commits cannot satisfy the
new order. Once the front entry has begun publication, the queue accepts
priority changes again after that publication finishes.

`PUT /api/repos/{owner}/{name}/pulls/{number}/merge-queue/priority` accepts
`queue_id` from the queue row and a new UUID as `request_id`. Retrying the
same accepted request returns its acknowledgement without moving the entry
a second time. The pull request conversation records the move.

`GET /api/repos/{owner}/{name}/pulls/queue` takes the full target ref as
`branch` (for example, `refs/heads/main`). Pass the returned `next` cursor
as `before` to continue on the same branch. Completed and removed requests
leave the queue.

The settings `PUT` accepts optional `merge_queue` (boolean),
`merge_queue_method` (`merge_commit`, `squash`, or `rebase`),
`merge_queue_builds` (1–100), and `merge_queue_timeout_minutes` (1–720).
`merge_queue_branches` replaces the override collection when supplied. Each
entry has a short `branch` name and at least one of `method`, `builds`, or
`timeout_minutes`. Send an empty array to restore inheritance for every branch.
The settings response omits default-valued queue settings and an empty
override list.
Use `PUT .../auto-merge` with `queue: true`, the `queue_method` returned by
the state endpoint as `method`, the exact comparison and a `request_id` to
enqueue. The state response gives the effective method, build limit, and
timeout for the target branch. It identifies the first entry and shows
whether candidate preparation has started. Disabling
optional queues invalidates optional entries, including across off/on changes.
Branches that require the queue keep their entries.
On a required branch, add `when_ready: true` to record consent and wait for the
head requirements. The request reports `waiting_for_queue: true` until it joins
the queue with the same ID. A waiting request takes no position or build slot.

### Manage labels and assignees

Assignments store stable Directory account UUIDs and explicit display-name
snapshots. `GET .../assignees` returns `{items:[{id,name,profile}],can_manage}`;
`profile` is `{id,handle,name}` or null. Issue/PR edits accept
`{version,assignee_ids:[UUID]}`. The old provider-subject request shape is removed.
Current membership determines eligibility and triage permission after awaited
Directory reads. Account registration creates no profile or grant. Historical
assignments remain visible after membership removal and link to a chosen public
profile when one exists.

The shadcn picker loads candidates when opened, searches names/handles, supports
keyboard focus/Escape, and permits removing unavailable historical selections
without re-adding them. It submits the record version captured when editing
starts; conflicts retain choices and require a refreshed record. This is a
metadata picker. The separate Assignee list menu supports native public-handle,
current-account, historical actor-ID and assigned/unassigned filtering.

Existing repositories need private `cells assignment-inventory`,
`cells migrate-assignments` and then `cells rebuild-search`. Nonempty legacy
assignments require explicit per-record qualified-identity JSON Lines mappings;
empty records convert without a file. Migration uses eight records per native
transaction, durable progress, conflict receipts and rename/update operations
that avoid whole-table rewrites. See [assignment preparation](CELLAR.md#assignment-preparation)
for the exact format, prerequisites, interruption and fleet upgrade procedure.


Label constraints:

- 1 to 50 characters per name
- Case-insensitive unique names
- Six hexadecimal color digits without `#`
- Descriptions up to 100 characters
- At most 20 labels per issue or pull
- At most 500 allocated label IDs during repository lifetime

The Labels menu uses `GET .../discussion-search/labels` with `query` and a
bounded `search` substring. It searches the complete canonical active-label
catalog and returns up to 20 choices, exact matches first, with a truncation
indicator. Colors and descriptions come from validated label records. Native
include, exclude, no-label and clear destinations retain the current query's
other terms and boolean grouping. Selection describes global label clauses;
labels inside OR remain conditional predicates rather than global selections.

Selecting several labels adds separate conjunctions. Clicking a selected label
removes it; Alt+click or Alt+Enter toggles exclusion. Editing a name within a
comma-OR clause retains its other members. Choosing no labels replaces global
label constraints; clearing the final constraint spells an unrestricted query
rather than resetting to the default open list. Oversized destinations are
disabled. Recent personal suggestions are not yet implemented.

The shared filter picker remains open during multi-selection and exposes
loading, unavailable, retry, selection and excluded states. Issue/PR result
ownership is shared: controls survive a refresh while counts and rows clear,
and one `powerbar=1` boundary encloses the complete list. A failed request shows
unavailable counts rather than a permanent loading message.
Issue/PR lists share a collapsible icon navigation rail and comfortable/compact
rows. These are browser-local preferences, retained across reload and list
navigation without changing query parameters, native cursors or data requests.
Compact rows preserve the displayed metadata. When browser storage is denied,
the current component's selection still works in memory. The sidebar measures
available viewport height with diagnostics present, and wraps on narrow screens.
Detail/create routes keep full-width layouts. The repository navigation opens
personal author, assignment, and review request filters for that repository.
Count/loading rows and the optional diagnostic header retain their height during
refresh so a completed request cannot move an open menu under a pressed pointer.
Before a measurement exists, the diagnostic header shows a dash and explains
that no request timing has completed; normal pages have no diagnostic header.

The Milestone dropdown on issue and PR lists searches the complete native
milestone catalog and switches between open/closed milestones. Its choice
endpoint accepts the current `query`, a bounded `search` substring and optional
`state=open|closed`. It returns at most 20 choices, an explicit truncation flag,
and native query destinations/selection for each choice, no milestone and
clearing the global facet. Exact title matches sort first. Refine the search to
reach additional matches; filtering never uses only a browser-loaded page.
Both search metadata endpoints recheck durable repository membership after the
awaited native read, so a stale materialized grant cannot expose revoked choices.

Choosing a milestone replaces global milestone conjunctions while retaining
other filters and boolean groups. Milestone predicates inside OR remain intact;
the chosen facet adds a global conjunction. The UI preserves sort/direction and
`powerbar=1`, resets pagination, and disables destinations that exceed query
bounds rather than truncating other filters. Keyboard selection, Escape/focus
return, empty/loading, request failure and retry are supported.

Assignments contain at most 10 distinct stable account UUIDs. Current repository
triage permission controls changes to label or assignee sets.

Stored assignments retain stable IDs and name snapshots after grant removal.
Public profiles provide current presentation without granting access. Label
renames appear immediately; deleted labels disappear without rewriting discussions.

### Find your work across repositories

Open **My work** in the global navigation. **Pull requests** and **Issues**
each offer **Created by me** and **Assigned to me**. **Review requests** includes
open pull requests requested from you or your current teams. Created and
assigned work can be filtered by Open, Closed, or All states. Closed pull
requests include both merged and unmerged work. Rows are newest first by
creation time, with repository-qualified links and a draft or lifecycle label.

`GET /api/personal-work` requires a browser account. It accepts
`kind=pull|issue`, `relation=authored|assigned|review_requested`,
`state=open|closed|all`, `limit` from 1 to 50, and `after` from the previous
result. The default is 30 open authored pull requests. Review requests require
`kind=pull` and `state=open`. The response contains `items` and nullable `next`.

Upgrade server peers and the embedded UI together to expose **My work**. Each
readable repository supplies bounded summaries from its existing discussion index.
The server merges those ordered pages with at most eight repository reads in
flight. It rechecks the readable repositories and applicable team membership
before returning results. Continuations bind the account, filters, and this
access scope. A changed scope returns HTTP 409 and the browser offers **First
page**. Refresh the first page to see new work created ahead of a continuation.

### Search issues and pull requests across repositories

Open **Search issues and PRs** from My work or the user menu. Use the same
text, Boolean expressions, and discussion qualifiers as the repository lists.
For example, `is:open label:"bug" deployment` finds open work that matches
both the label and text. The repository scope accepts an owner namespace or
an exact `owner/repository`. Search both types together or choose Issues or
Pull requests, and sort by creation time or recent activity. The browser URL
keeps the query and filters and opens each result by its type and repository
UUID.

`GET /api/discussion-search` requires a browser account, plus `q` or `scope`.
It accepts optional `kind=issue|pull`, `sort=created|updated`, `limit` from 1
to 50, and `after` from the previous response. It returns `items` and nullable
`next`. Crab merges bounded pages from each readable repository's discussion
index. The account, search parameters, and readable scope bind the cursor.
Current repository access is checked again before results are returned. A
changed scope returns HTTP 409. An unavailable repository index returns its
normal preparation error so an incomplete search can be retried.

### Understand pagination and storage

Issue, pull, comment, and review lists default to 30 items and accept 1 to 50. Indexed discussion lists return matching summaries and use the opaque cursor contract above. Comment/review lists scan at most 200 allocated numbers; a page can be empty and still return `next`. Label lists return the complete bounded catalog of at most 500 active labels.

Titles accept 1 to 256 characters. Markdown bodies accept 64 KiB. Collaboration requests use an 80 KiB body limit, eight concurrent application slots, and a 30-second handler deadline.

Serving data currently uses these roots:

| Root | Content |
| --- | --- |
| Directory Cell/LTX namespace | Stable qualified accounts, optional public profiles, enrollment submissions, explicit migration progress and command receipts |
| Repository Cell/LTX namespace | Issues, comments, labels, commit statuses, check runs, branch protections, lifecycle, Pulls/reviews, Releases/asset references, counters, publication intent, and permanent submission ledgers |
| `app/v1/issues` | Retired issue documents; serving ignores them and operators delete them at hard cutover |
| `app/v1/pulls` | Retired Pull documents; serving ignores them and operators delete them at hard cutover |
| `app/v1/labels` | Retired Label documents; serving ignores them and operators delete them at hard cutover |
| `app/v1/releases` | Retired Release documents and asset bodies; serving ignores them and operators delete them at hard cutover |
| `release-assets/v1/sha256` | New immutable content-addressed Release asset bodies referenced by Cell metadata |
| `app/v1/statuses` | Retired status documents; serving ignores them and operators delete them at hard cutover |
| `app/v1/check-runs` | Retired check-run documents; serving ignores them and operators delete them at hard cutover |
| `app/v1/settings` | Retired policy documents; serving ignores them and operators delete them at hard cutover |

There is no remaining collaboration JSON serving backend. Preserve the Cell/LTX
namespace and immutable asset bodies in backups. Restoring visible records
without counters, claims, request ledgers,
control and immutable roots loses numbering, ownership and retry guarantees.

Discussion deletion and broader activity and email delivery qualification
remain pending. The isolated RustFS process qualification
restores version-selected product roots, including release asset bodies.
Cross-provider export and restore on a production deployment remain pending.

## Commit statuses and required checks

CI systems can report compact commit statuses or detailed check runs. The
direct reporting API requires a reachable commit and repository write access.
GitHub Actions uses the separate signed workflow endpoint below.

### Report a commit status

Use Basic authentication with a scoped write token:

```http
POST /api/repos/your_team/your_project/statuses/0123456789012345678901234567890123456789
Content-Type: application/json
Authorization: Basic base64_encoded_crab_and_token
```

```json
{
  "request_id": "01931b9e-4b3c-7b2a-b9f0-0123456789ab",
  "context": "ci/test",
  "state": "success",
  "description": "All checks passed",
  "target_url": "https://ci.example.com/runs/1234567890123"
}
```

State is `pending`, `success`, `failure`, or `error`. Context matching is case-insensitive. The newest report replaces the visible result for that context; replaying an older request returns its original record without replacing newer state.

`GET /api/repos/{owner}/{name}/commits/{sha}/status` returns the latest context results and combined state.
For a completed pull request, pass `?pull={number}` to read the exact recorded
source head or the published tested SHA. The same read scope applies to the
check run list and detail. Repository access and commit admission for reporting
still apply to writes.

Status limits include 1,000 submissions per commit, 128 retained contexts, 140 description characters, a 2 KiB target URL, and an 8 KiB request body. Target URLs require HTTPS, except loopback HTTP during local development.

### Create and update a check run

| Method and route | Purpose |
| --- | --- |
| `POST /api/repos/{owner}/{name}/check-runs` | Create a run for a reachable commit |
| `GET /api/repos/{owner}/{name}/commits/{sha}/check-runs` | List runs |
| `GET /api/repos/{owner}/{name}/commits/{sha}/check-runs/{id}` | Read one run |
| `PATCH /api/repos/{owner}/{name}/commits/{sha}/check-runs/{id}` | Update a run created by the same identity |

Runs progress from `queued` to `in_progress` to `completed`. Completed runs require one conclusion: `success`, `neutral`, `skipped`, `failure`, `cancelled`, `timed_out`, or `action_required`. Completed runs cannot reopen.

Each output can contain:

- A required title
- Markdown summary and optional Markdown text
- Up to 50 steps with bounded plain-text logs
- Up to 50 file and line annotations

Encoded output can contain at most 192 KiB. Requests can contain at most 256 KiB. A commit retains at most 100 runs. Lists default to 30 and accept 1 to 50 with an exclusive `before` cursor.

### Send repository webhooks

Add up to 20 distinct approved receivers per repository UUID in the server configuration. Set
`auth.state_key_file` to the same file on every serving peer. The destination
must use HTTPS with no URL credentials, query, or fragment. The sender verifies
TLS and does not follow redirects.

```toml
[[outbound_webhooks]]
repository = "01900000-0000-7000-8000-000000000101"
url = "https://ci.example.test/events"

[[outbound_webhooks]]
repository = "01900000-0000-7000-8000-000000000101"
url = "https://audit.example.test/events"
```

A repository administrator opens **Settings → Webhooks**, selects a destination,
chooses events, and copies the signing secret
to that receiver. Each endpoint has its own subscriptions, secret, and
delivery history. Saving creates a new secret for that endpoint and cancels
its outstanding deliveries. Removing an approved endpoint stops delivery to
it as peers take up the new configuration. Roll out consistent receiver
assignments across the fleet before enabling subscriptions.

The settings read lists destinations with opaque IDs and includes the selected
destination's subscription and recent deliveries. Use `?destination=<id>`
with reads and mutations. A mutation can omit the selector when the repository
has exactly one approved receiver.

Crab POSTs a JSON object with `repository` (the stable UUID), `delivery` (the
delivery UUID), `occurred_at_ms`, and `data`. `X-Crab-Event` is `push`,
`pull_request`, `issues`, `issue_comment`, or `pull_request_review`.
`X-Crab-Delivery` is the delivery UUID. Verify the raw bytes
with the configured secret and `X-Hub-Signature-256` (`sha256=` plus the
hex-encoded HMAC-SHA256), then deduplicate by delivery UUID before processing.
[Webhook signature guidance](https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries)

- `push` contains `data.updates` for one committed branch or tag transaction.
  It covers smart HTTP Git and Crab browser writes, including file commits,
  branch creation and deletion, release tags, fork synchronization, and PR
  merges. Each update contains `ref`, `before`, and `after`. Creation and
  deletion use `null` for the missing side. Private merge-candidate refs and
  Git import traffic stay outside the subscription.
- `pull_request` contains `data.action`, PR number, title, draft state, version,
  base and head refs and SHAs, `head_repository`, and `merge_commit`. Actions
  include `opened`, `edited`, `closed`, `reopened`, `ready_for_review`,
  `converted_to_draft`, and `synchronize`. A base retarget uses `edited` and
  a merged PR uses `closed` with `merge_commit`. A `synchronize` delivery
  caused by a committed branch update includes `head_change.before` and
  `head_change.after`. The latter is also `head_sha`, the exact new head to
  check. A recreated branch has a null `before`. The `base_sha` describes
  the PR's recorded comparison. A browser head-branch switch emits the
  recorded new comparison without `head_change`.
- `issues` contains `data.action`, `number`, `title`, `state`, `state_reason`,
  and `version`. Actions are `opened`, `edited` for title or description
  changes, `closed`, and `reopened`. Closures from merges and commits use the
  same `closed` event as an interactive change.
- `issue_comment` contains `data.action` (`created` or `edited`),
  `subject` (`kind` of `issue` or `pull` and its `number`), `comment` (the
  comment number), and its `version`. This covers ordinary conversation
  comments and comments submitted with a close or reopen. Use the subject and
  comment revision to reconcile current state through the authorized API.
  Discussion bodies and private account identifiers are excluded from payloads.
- `pull_request_review` contains `data.action` (`submitted`, `edited`, or
  `dismissed`), the pull and review numbers, the review `version`, the exact
  reviewed `commit_oid`, and the original `decision`. Subscribe using **Pull
  request reviews**. The event covers published standalone and batched reviews,
  summary edits, and explicit dismissals. Draft saves and retries of a committed
  submission do not enqueue deliveries. Base and head changes that expire
  approvals are reported through the corresponding `pull_request` event.
  The payload excludes review text and dismissal reasons. Delivery history
  shows the reviewed commit and links directly to the review.
  [GitHub review event](https://docs.github.com/en/webhooks/webhook-events-and-payloads#pull_request_review)

Existing subscriptions keep their event selection. The settings command
accepts the optional booleans `issues`, `issue_comment`, and
`pull_request_review`, each defaulting to `false`. Upgrade all server replicas
before enabling a newly supported event.

A branch update emits one synchronization event per subscribed receiver for
each open PR already tracking that repository and branch when publication
reserves delivery. A private fork's update uses the upstream PR repository's subscriptions
even when the fork has no webhook. The worker skips a PR closed, reopened,
or rerouted before it expands the publication into deliveries. Deleting a
branch emits a push event when subscribed. Delivery is asynchronous, so check the current PR
head before scheduling work that would supersede a newer run.

Publication and delivery are separate. The worker resolves the committed Git
plan before queueing or sending a ref event. An unconfirmed plan is cancelled
after an hour. Delivery retries reuse the UUID and payload. A 2xx response confirms
delivery. Other responses and connection failures retry after 10 and 20
seconds, then remain visible as failed. **Recent deliveries** shows the selected
receiver's entries within the repository's latest 50 deliveries and the last
ten HTTP outcomes per entry. An administrator can redeliver completed deliveries
for that receiver's current subscription. Disabling a webhook prevents
new delivery attempts and cannot recall a request that already reached the
receiver.

### Connect GitHub Actions

Crab accepts signed GitHub.com workflow-run deliveries. An operator can use
one private GitHub App for multiple workflows, or a repository administrator
can use a dedicated push-check connection. Use a GitHub source repository that has the same commits as the Crab repository
and an Actions workflow triggered by pushes to the branches under review.
The Crab public URL must be reachable by GitHub. All serving replicas need
the same `auth.state_key_file`.

1. Open **Settings → Integrations** as a repository administrator. Enter
   the GitHub repository ID and workflow ID, and choose the check name.
   Use `gh api repos/OWNER/REPO --jq .id` and
   `gh api repos/OWNER/REPO/actions/workflows --jq '.workflows[] | [.id, .path] | @tsv'`
   to find the IDs.
   [Repository API](https://docs.github.com/en/rest/repos/repos#get-a-repository)
   · [Workflows API](https://docs.github.com/en/rest/actions/workflows#list-repository-workflows)
2. For a dedicated connection, configure a private GitHub App with the
   displayed webhook URL and secret, read-only Actions repository permission,
   and the **Workflow run** event. Install the app on the source repository.
   A workflow marked **Managed by the server GitHub App** uses the shared
   endpoint and operator-managed signing credential described below.
   [GitHub App registration](https://docs.github.com/en/apps/creating-github-apps/registering-a-github-app/registering-a-github-app)
   · [GitHub workflow-run event](https://docs.github.com/en/webhooks/webhook-events-and-payloads#workflow_run)
3. Push the same commit to Crab and the GitHub source. Open the Crab pull
   request's **Checks** tab to see the workflow and its GitHub logs link.
   If GitHub sends the result before the commit reaches Crab, ensure that
   the commit is reachable on Crab and redeliver from the app's recent deliveries.
   [GitHub redelivery](https://docs.github.com/en/webhooks/testing-and-troubleshooting-webhooks/redelivering-webhooks)
4. To enforce it, edit the target branch rule and select the workflow under
   **Required GitHub Actions workflows**. Each selection binds the rule to the
   connection ID for that workflow. A passed manual or differently scoped
   check cannot stand in for it.

The webhook verifies the signature over the raw request before parsing the
workflow. For dedicated connections, Crab derives the secret from the shared
state key and returns it to a repository administrator on connect or rotate.
Rotate that secret in Crab and update the dedicated GitHub App. Rotation retains
accepted runs. Disconnecting blocks new deliveries and leaves referring branch
rules blocked until edited.
Incoming deliveries are limited to 1 MiB and a repository can connect up to
16 workflows. The pull request shows the workflow's exact head SHA and opens
the corresponding GitHub run. Redelivery is idempotent and older runs and
attempts cannot overwrite the selected result. GitHub `pull_request` workflows
can run against a synthetic merge commit. Head checks use the `push` workflow
result for the matching Crab head.
[GitHub webhook signatures](https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries)
· [GitHub workflow triggers](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#pull_request)

#### Inspect accepted workflow deliveries

In **Settings → Integrations**, select **Recent deliveries** for a connected
workflow. Repository administrators can see its latest 50 accepted reports,
newest first. Each entry shows the receipt ID, Crab commit, GitHub run and
attempt, and whether the report changed the check. A report that already
matches the stored check or arrives after a newer result is marked **Check
unchanged**. Sending the same receipt again leaves one entry.

`GET /api/repos/{owner}/{name}/settings/integrations/{connection}/deliveries`
returns the same summaries. History starts with reports accepted after all
serving peers receive this update. It omits unrelated workflow events and
rejected requests. Use the GitHub App's delivery log to diagnose or retry a
failed request. Secret rotation retains history, and disconnecting deletes the
connection's history. [GitHub App delivery log](https://docs.github.com/en/webhooks/testing-and-troubleshooting-webhooks/viewing-webhook-deliveries#viewing-webhook-deliveries-for-github-apps)

#### Configure the server GitHub App

Configure one private GitHub App with **Actions: Read and write** permission
and subscribe to **Workflow run**. Set its webhook URL to
`https://git.example.com/webhooks/github-actions/app` using Crab's public HTTPS
origin. Save the same webhook secret in the App and in a private file mounted
on every Crab replica. Use a randomly generated secret of 32–512 visible
ASCII characters. Install the App on the source repositories. Mount its
private key on each replica and configure the exact repository, installation,
workflow and trusted branch bindings:

```toml
[github_actions]
app_id = 7
private_key_file = "/run/secrets/github-actions.pem"
webhook_secret_file = "/run/secrets/github-actions-webhook"

[[github_actions.bindings]]
crab_repository = "01900000-0000-7000-8000-000000000001"
installation = 42
github_repository = 123
workflow = 456
branch = "main"
```

Each installation, GitHub repository and workflow tuple can route to one Crab
repository. Add another binding for each workflow the App should report.
Crab validates the App signature before looking up that tuple. The signature
authenticates the installation claims, and required checks still use the exact
Crab commit and connection. The App's signing secret stays with the operator.
To rotate it, update GitHub and the mounted files, then restart all serving
replicas. Redeliver any reports interrupted by the rollout.

Upgrade all replicas together and point the App at the shared URL. Existing
queue-dispatch connections use the App route after upgrade. A standalone
connection can switch by setting up queue dispatch after the operator adds its
binding, or by disconnecting and connecting it again. Switching to the App
requires a fresh signed head result. Clearing queue dispatch retains App
signing for head reports. Use **Disconnect** to revoke a connection and block
its use in branch rules.

The RustFS CI gate runs two standalone servers with an App signing fixture.
It verifies that a manual check with the same name, an unbound installation,
an older commit, and a downgraded reviewer cannot authorize a protected merge.
It uses authenticated scale-down to transfer ownership, retries an unread
merge reply through the surviving replica, and fetches the exact checked
commit using stock Git. A live App installation exercises delivery from
GitHub and remains part of deployment qualification.

#### Start GitHub Actions for a merge queue candidate

In Crab, select **Set up queue dispatch** on an authorized connection. The
server requests an installation token scoped to that repository and to Actions.
Use a private Actions repository to test private Crab source. Define the
workflow on the default branch and the bound branch. Store Crab's canonical
HTTPS origin without a trailing slash in the Actions variable `CRAB_ORIGIN`.
Save the displayed checkout token as the Actions secret
`CRAB_QUEUE_READ_TOKEN`. Continue reporting signed head checks through the same
required connection. Use a separate Actions secret name in each workflow if
several connections share the Actions repository. Rotate the Actions secret
when changing dispatch settings or rotating the queue checkout token.

Add the dispatch trigger and job below to the connected workflow. Keep its
existing push trigger and run the push jobs only on push events. Add your
project's test steps after checkout so the checkout credential is out of scope
when project code executes. The endpoint serves Git objects reachable from the
active candidate. Git LFS and Xet hydration require separate access.

```yaml
run-name: ${{ github.event_name == 'workflow_dispatch' && format('Crab queue {0} {1}', inputs.crab_commit, inputs.crab_dispatch) || github.event.head_commit.message }}
'on':
  workflow_dispatch:
    inputs:
      crab_commit:
        required: true
        type: string
      crab_dispatch:
        required: true
        type: string
      crab_source:
        required: true
        type: string
jobs:
  queue:
    if: github.event_name == 'workflow_dispatch'
    permissions: {}
    runs-on: ubuntu-latest
    steps:
      - name: Check out the exact Crab candidate
        shell: bash
        env:
          CRAB_ORIGIN: ${{ vars.CRAB_ORIGIN }}
          CRAB_SOURCE: ${{ inputs.crab_source }}
          CRAB_COMMIT: ${{ inputs.crab_commit }}
          CRAB_QUEUE_READ_TOKEN: ${{ secrets.CRAB_QUEUE_READ_TOKEN }}
        run: |
          set -euo pipefail
          [[ "$CRAB_ORIGIN" == https://* ]]
          [[ "$CRAB_SOURCE" == "$CRAB_ORIGIN"/ci/queue/* ]]
          [[ "$CRAB_COMMIT" =~ ^[0-9a-f]{40}$ ]]
          auth=$(printf 'crab-ci:%s' "$CRAB_QUEUE_READ_TOKEN" | base64 | tr -d '\r\n')
          echo "::add-mask::$auth"
          git init .
          GIT_CONFIG_COUNT=2 GIT_CONFIG_KEY_0=http.extraHeader \
            GIT_CONFIG_VALUE_0="Authorization: Basic $auth" \
            GIT_CONFIG_KEY_1=http.followRedirects GIT_CONFIG_VALUE_1=false \
            git -c protocol.version=2 fetch --depth=1 "$CRAB_SOURCE" "$CRAB_COMMIT"
          git checkout --detach FETCH_HEAD
          test "$(git rev-parse HEAD)" = "$CRAB_COMMIT"
```

Crab binds the candidate to the returned GitHub run ID. It can recover a lost
response from the dispatching App's run title or a matching signed delivery.
Candidate checks link to the bound run. The GitHub run's `head_sha` describes
the workflow definition branch. Crab uses the dispatch's candidate SHA as the
tested commit. A superseded or published candidate loses checkout access.
[GitHub workflow dispatch API](https://docs.github.com/en/rest/actions/workflows?apiVersion=2026-03-10#create-a-workflow-dispatch-event)
· [GitHub App installation authentication](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/authenticating-as-a-github-app-installation)

For a private organization fork, configure the connection on the upstream Crab
repository. Include `crab/pulls/**` in the GitHub workflow's `push` branches and
test the pushed revision. Copy the **GitHub Actions mirror refspec** from the
open Crab pull request's **Checks** tab. Fetch the fork commit locally, then run
`git push <github-remote> <copied-refspec>` for the connected GitHub repository.
Restrict writes to the mirror and review workflow changes before pushing a fork
commit. The GitHub repository and workflow are trusted to report the result.

The mirror branch includes the upstream Crab repository UUID and pull request
number. Crab verifies that the signed delivery comes from the configured GitHub
repository and that the SHA matches the fork's current head or its recorded
merged head. A new result for an outdated head is rejected. Retries of an
already accepted delivery remain idempotent. After the fork branch moves, copy
the updated refspec and push the new commit to run its checks. If the fork
branch was rebased, fetch the current mirror branch before updating it with
`--force-with-lease`.
[GitHub push workflow trigger](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#push)

### Evaluate required checks

For each named Crab context, the latest commit status and latest Crab check run must
both pass when both exist. A single existing report can satisfy the rule. The
merge panel shows both reports, their source, reporter and outcome. The panel
identifies the exact PR head or merge candidate that must pass, including while
results are missing. On fork PRs the head link opens the fork's commit. A
required GitHub Actions connection needs a completed successful
result from that active connection for the exact same head. Neutral, skipped,
pending, disconnected, and missing workflow results block merge.

| Latest result | Required-check state |
| --- | --- |
| Status `pending`, or run `queued` or `in_progress` | Pending and blocks merge |
| Status `success` | Success |
| Named Crab check run conclusion `success`, `neutral`, or `skipped` | Success |
| Required GitHub Actions workflow conclusion `success` | Success |
| Missing, `failure`, `error`, or any other completed run conclusion | Unsuccessful and blocks merge |

Each `merge_requirements.checks` entry retains the required name's summary fields
and tested `head_sha`. When a name has both report types, the top-level fields
describe the strongest blocker and `additional_report` contains the other
report. A missing named result accepts a Crab commit status or check run.
Candidate requirements use the same report shape.

The server reads the named statuses, check runs, and provider-bound results for
the head and any tested candidate in one Repository Cell transaction. A new head
starts with missing checks. Publication refreshes required checks immediately
before attempting the ref edit. A recorded receipt proves a previous publication
even if policy or check results have since changed.

Update the repository Cell peers and bundled UI together for the check snapshot
read. Rules containing only named Crab contexts retain their existing storage
representation.

## Resource and lifecycle limits

Product HTTP admission remains bounded. Cellar uses its bounded runtime defaults
and a resident-cell limit derived from process memory and file descriptors.
`cells capacity --json` reports the startup resource and admission envelope; it
is not a throughput or production-scale qualification.

Browser blob reads allow up to 256 MiB per Git object and raw/inline-asset
response; JSON responses retain their separate 8 MiB cap. The `file-preview`
endpoint reads metadata first and returns `preview.state: omitted_size` above
1 MiB without guessing text encoding. The first-party viewer uses this endpoint.
The tagged `file` endpoint retains full inspection within its operation budget:
`text_truncated` is true only for inspected UTF-8 omitted because of size.
Raw/range requests still verify the Git object before returning bytes; this is
not an unbounded server-side streaming reader.
CSV/TSV workbenches above their 50 MiB normalization budget keep HTTP scans;
whole-file browser parsers retain their independent 50 MiB limit.

`local_disk_limit_bytes` bounds Crab's staging envelope. Native cache usage is
managed by Cellar; use a filesystem quota for a hard cache-volume bound. Staging
still checks physical free space and accounts for retained transfer sessions.

Normal termination drains HTTP and Git work, then stops the native node with
`Preserve`. The private scale-down endpoint requests `Handoff`, which requires a
successor. Transport stays alive through the native drain. Incomplete shutdown
returns an error; it does not claim durable completion. See [CELLAR.md](CELLAR.md).

## Current verification

| Proof | Executable surface |
| --- | --- |
| Native restart, duplicate identity, archive/write ordering | `cells::native_tests` |
| Signed peer routing over mTLS, owner handoff | `server::peer_e2e_tests` |
| Real RustFS object publication and handoff | Ignored `rustfs_cellar_routes_over_mutual_tls_and_recovers_after_handoff` test with isolated test settings |
| HTTP collaboration, authorization, CSRF, replay | `api::<feature>::tests` and `auth::<feature>::tests` use shared `test_support/http` fixtures with actual native nodes |
| Missing schema and membership installation | `api::members::tests` |
| Retained staging capacity and restart | `local_disk::tests` |
| Creation, Git push, issue replay, process restart | `tests/qualify_cellar_process.py` |

### Read the executable evidence map

The architecture workflow runs native recovery and RustFS handoff proof plus the
real-process smoke test. The smoke test writes a JSON receipt only after all
assertions pass and keeps diagnostic logs locally. Historical runtime receipts cannot qualify this implementation.

### Understand what has been qualified

Local proof covers actual native nodes and an isolated RustFS instance, including
successful Git push and state visible after restart. It does not prove live
Azure/GCS failure behavior, production scale, backup restore, or mixed-version
upgrades. Container CI runs native process smoke; release publication remains disabled
until the broader native qualification contract is implemented and verified.

### Keep qualification evidence honest

Bind future release evidence to the exact source, image, SDK version, and provider
configuration exercised. Qualification must exercise the native implementation.

## Completion requirements

The server is complete only when a real account can perform the workflow and observe its durable result. A green component test or visible placeholder does not satisfy that bar.

| Surface | Required evidence | Status |
| --- | --- | --- |
| Multi-replica deployment | One Rust binary, durable CAS catalog and identity state, private management probes, graceful drain, hardened Helm profile, reproducible container, and portable cross-replica gate | Native mTLS routing and handoff tested locally; protected provider qualification must be migrated and rerun |
| Repository browsing | Refs, byte-preserving paths, history, files, blame, downloads, freshness, and empty/error states against real repositories | In progress |
| Diff and tree interface | Pierre Trees and Diffs, correct modes and binary handling, bounded large-repository behavior, and keyboard navigation | In progress |
| GitHub-quality design | Themes, responsive layouts, accessible controls, navigation, and loading/error behavior across workflows | In progress |
| Team identity and authorization | OIDC, sessions, membership, permissions, isolation, revocation, CSRF, and administration | In progress; browser and CLI membership replacement are audited, and back-channel logout revokes delivered exact-identity sessions |
| Git hosting | Authenticated fetch and push, exact branch/tag lifecycle, protection, publication, and independent-client proof | In progress; additional crash phases and coexistence qualification remain |
| Collaboration | Durable issues, pulls, comments, reviews, labels, assignees, merge, checks, activity, and notifications | In progress; discussion deletion and email delivery qualification remain |
| Repository management | CLI create/adopt/list, archive, settings, search, import, and audited administration | In progress; browser creation/import and audit history remain |
| Production operation | Durable concurrency, restart and crash recovery, backup restore, observability, upgrades, and operator guidance | Native process restart, owner handoff, and version-selected RustFS restore tested. Native metrics and shutdown wiring implemented. Deployment restore, crash matrices, upgrades, and provider qualification remain |
| Quality gates | API, UI, accessibility, realistic repositories, security, package smoke, and measured performance | In progress |

### Track known operational gaps

The remaining production gaps include:

- Broader abrupt-crash coverage beyond the qualified in-flight native-push boundary, plus a portable client recovery token (native Git can only recover an identical wire request through the server-side plan receipt)
- Journal or visibility-receipt reconstruction when verified evidence is missing; missing standard Git `.idx` and `.rev` sidecars are repaired from the verified canonical pack during catalog maintenance
- Hosted-provider and sustained-load qualification of the bounded background dependency scrub, including exact request/byte accounting and provider-level lease-loss behavior. Deterministic local fault injection proves that renewal loss during a paused dependency scan cancels the old owner before report publication and preserves the successor lease. Deployment-wide owner election prevents healthy replicas from repeating deep reads; the 10-second readiness probe intentionally validates Git serving rather than redownloading every large external object
- Protected-view writer coexistence with shared namespace guarantees
- Production throughput and provider-level admission qualification
- Membership audit history and immediate arbitrary provider revocation when no Logout Token is delivered
- Repository creation and adoption exist in the CLI; browser import remains
- Extend the version-selected RustFS product-root restore proof to production storage providers and authenticated deployments
- Manual assistive-technology audits and broader workflow coverage
- Successful EKS, GKE, and AKS live-workflow receipts; rollback, alert-tuning, and disaster-recovery qualification
- First tagged server image/chart publication and registry-attestation verification

The first tagged publication will also exercise the permanent operator
handoff: a non-latest GitHub Release containing the packaged chart, a JSON
record binding the source commit to the image/chart digests and package
checksum, and an offline GitHub attestation bundle for that record.

## Ownership

Object storage is the repository authority. Runtime caches and temporary files are disposable.

New collaboration data needs a versioned storage and concurrency contract under the repository prefix. It must not reuse Git object, metadata, or lock namespaces.

The service account needs object reads plus conditional writes and deletes for:

- Repository metadata, manifests, refs, packs, visibility, and locator state
- Ref-journal transactions, prepared heads, and cleanup
- Per-ref, namespace, generation-owner, and GC coordination keys
- The complete `app/v1` application namespace
- LFS objects, lock records, and multipart lifecycle
- The server catalog and shared OIDC/session/Git-token namespace

Preserve source errors across crate boundaries. Map them at the HTTP boundary only when the status code or client action changes.

## Integration sources

The interface follows these upstream contracts:

- [Git pack protocol](https://git-scm.com/docs/pack-protocol)
- [Git credential contexts](https://git-scm.com/docs/gitcredentials#_configuration_options)
- [Git LFS File Locking API](https://github.com/git-lfs/git-lfs/blob/main/docs/api/locking.md)
- [Git LFS extensions](https://github.com/git-lfs/git-lfs/blob/main/docs/extensions.md)
- [RFC 9110 HTTP range semantics](https://www.rfc-editor.org/rfc/rfc9110.html#section-14)
- [Pierre Diffs documentation](https://diffs.com/docs)
- [Pierre Trees documentation](https://trees.software/docs)
- [Primer React guidance](https://primer.style/product/getting-started/react/)
- [GitHub default-branch administration](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-branches-in-your-repository/changing-the-default-branch)
- [GitHub repository rulesets](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/about-rulesets)
- [GitHub classic branch protection](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/managing-a-branch-protection-rule)
- [GitHub repository archive behavior](https://docs.github.com/en/repositories/archiving-a-github-repository/archiving-repositories)

The OIDC dependency includes `rsa` for public-key signature verification. [RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071.html) concerns private-key timing leakage. This relying party holds no RSA signing keys and decrypts no RSA ciphertext. The test issuer uses Ed25519 only in the test harness. No advisory suppression or dependency override is present; a complete dependency audit remains part of production qualification.


## Organization authority foundation

The `Organization` native class uses schema 3 under `cells/organization/` and
shares the Cellar transaction, receipt and output gate. Explicit private
`cells bootstrap-organization --organization UUID --input-file JSON` resolves the
first owner's exact `{issuer, subject}` through Directory without profile
enrollment. It returns `/-/organizations/UUID`. Public HTTP has no bootstrap route.
Use fresh Organization Cell UUIDs for schema 3.

`/api/organizations/{id}` reads/edits metadata; `/members` lists bounded current
members, and `/members/{actor}` changes/removes active grants with expected
membership versions. `/invitations` lists bounded pending/expired offers;
`/invitations/candidates` searches public usernames;
`/invitations/{recipient}` reads, creates/edits/retries or cancels an offer.
`/invitation` reads/decides only the authenticated recipient's offer. Bodies deny
unknown fields. Repository Git tokens cannot access these account features.
Native authorization precedes receipt replay and mutations; last-owner guards,
invitation decisions, membership tombstones and audit append are transactional.

Removing a member or converting them to an outside collaborator also cancels
any pending invitations they sent. A pending edit keeps the original sender.
Cancelled invitations disappear from the recipient's invitation inbox and
remain visible in invitation history. The membership audit event records how
many invitations were cancelled.

Removing a member also revokes their direct grants on the organization's live
and deleted source repositories. Crab records those grants against the removal
version and repository ownership epoch. Conversion to outside collaborator
preserves repository access through direct grants.

In **People → Members**, select members on the current page, open the selection
menu, and choose **Remove from organization**. Review the members and the
access changes before confirming. `POST /api/organizations/{id}/members/removal`
accepts `{members: [{actor, version}]}` for 1–50 distinct current members and
returns `{removed}`. Crab validates the whole selection and requires a surviving
owner before committing any repository or membership changes.

For 90 days after a member is removed or converted to an outside collaborator,
an owner can invite them from **People → Invite member** and choose **Restore
previous access** or **Start fresh**. Restoring access uses the former
organization role and restores manual memberships in surviving teams on
acceptance. The teams' current repository permissions apply. Eligible archived
direct grants also return where the repository still has the same ownership
epoch. Newer direct grants take precedence. **Start fresh** discards the
archived grants when accepted. An offer issued within the eligibility window
keeps the normal seven-day invitation deadline.

The catalog records pending departure and rejoin operations so the refresh
worker can complete interrupted changes. Grant edits and ownership transfers
can conflict until that pending operation finishes.

The owner-only invitation view includes `reinstatement` with the eligible
membership `version` and former `role`. Send that version as
`reinstate_version` alongside the role and invitation version to make a
reinstatement offer. Ordinary offers omit `reinstate_version`. Reinstatement
offers report `reinstates: true`. Stale membership versions conflict. Upgrade
serving and Organization Cell peers together before issuing these offers.

Current organization owners inherit Admin on their organization’s repositories.
Other organization members can inherit roles through repository team grants.
The account menu's **Your organizations** page and **Notifications →
Organization invitations** use `GET /api/organizations` with an optional
`after` continuation. The Directory index locates candidate organizations.
Current membership and pending offers are checked by each native Organization
Cell, including for organizations without repositories. Readable browser
links include the display-name slug and stable UUID. Organization invitation
emails use verified opt-in delivery and link to the inbox.

Discovery starts with fresh organization state. Bootstrap owners and invitees
are indexed automatically. People searches accept `q`, `role`, and an optional visible
`team` UUID. Invitation searches use `q`, `role`, and `status` for owners.
Search cursors bind the account, filters, and current organization revision.
Visible teams can choose a parent when created or be moved from **Team →
Settings**. Owners can move any team. Maintainers can move teams they manage
under visible parents they also manage. The Organization Cell rejects cycles,
secret parents, and a visibility change that would make a nested team secret.
A team's direct memberships control its administration. Its members inherit
repository grants from all ancestors. Descendant members receive parent team
mentions and appear in the parent roster without editable direct roles.

Maintainers who manage one side can propose the move from **Team → Settings**
or **Team → Child teams**. The other team's maintainers can approve or decline
under **Teams → Team requests**. The requester can withdraw their own request.
Approval rechecks both actors' current authority and the child version, then
applies the same cycle and visibility checks as a direct move.

`GET /api/organizations/{id}/teams/hierarchy-requests` lists the caller's pending
requests and those they are authorized to review. It accepts optional `team`,
`request`, `after`, and `limit`. `propose_hierarchy` takes the child `team` and `version`,
the proposed `parent`, and the requester's `side` of `child` or `parent`.
`decide_hierarchy` takes the child `team`, exact `request` UUID and an `approve`,
`decline`, or `withdraw` decision. The proposal submission UUID identifies the
request. Proposing, declining, and withdrawing leave repository access unchanged.

**Notifications → Team requests** lists requests the caller can review across
current organizations. `GET /api/notifications/team-requests` accepts an opaque
`after` continuation. Each row links to the exact pending request, with review
authority checked against current membership in the owning Organization Cell.

Use `GET /api/organizations/{id}/teams?parent={team}` for a paginated child
team list. A `reparent` team command takes the team's current `version` and a
`parent` UUID or `null`. A parent-team delete requires an organization owner
and the `hierarchy_version` returned as `tree_version` by the team detail.
Deleting a parent removes its entire subtree. Team **Repositories** distinguishes
roles inherited from ancestors from direct grants.

Cross-authority repository and project fencing remains separate work. See `CELLAR.md`
for preparation, expiry and coordinated deployment contracts. Native tests live
in `cells/organization/tests/`, HTTP tests
in `api/organizations/tests/`, and browser fixtures in
`packages/ui/tests/browser/organizations.e2e.ts`. The process qualifier also covers
private bootstrap, metadata persistence, profile enrichment and last-owner denial.

The manual `api::organizations::tests::browser` qualification uses two actual
browser/OIDC sessions and the embedded bundle. With `agent-browser` available,
build the UI, set the checkout's mounted `CARGO_TARGET_DIR`, and run
`cargo test -p crab-server --locked --lib api::organizations::tests::browser -- --ignored --nocapture`.
It creates a public profile, creates/accepts an invitation, observes the native
membership and checks that removal hides current metadata while retaining only
the recipient's historical decision. Browser cleanup is bounded and scoped to
its own sessions. The normal suite ignores it because it needs an external browser.


### Manage organization automation credentials

Open **Automation** as a signed-in organization owner. Create a named
identity, choose repositories, and assign Read or Write to each. Read supports
Git checkout and Git LFS download. Write additionally supports Git pushes and
Git LFS upload. Branch protection, rulesets and archive checks apply to
automation. Automation credentials are scoped to Git and Git LFS.

Issue a token for 1 hour, 1 day, 7 days or 30 days. Save the displayed token
in the CI secret store and use it as the password for Git username `crab`.
The server stores only a token digest. If the issuance reply is lost, refresh
the list, revoke the newly created token, and issue a replacement. Each
organization can have up to 32 identities, each with up to 50 repository
grants and four active tokens.

A token's grants are bounded by the identity's selected access at issuance.
Grant removals and downgrades reduce existing tokens. A new token is required
to use added or upgraded grants. Disabling the identity revokes all its
tokens. An ordinary rename retains access by repository UUID. A transfer
ends the grant, including when the repository later returns. Reapprove the
repository and issue a new token. Automation changes appear in **Audit log**.

`GET /api/organizations/{id}/automation` lists the current revision, identities,
grants and token metadata. `POST` uses `{expected_version, edit}`, where
`edit.kind` is `create`, `configure`, `enabled`, `delete`, `issue`, or
`revoke`. The browser session and CSRF checks govern writes. A stale revision
returns 409. Upgrade all serving replicas before using automation or
transferring repositories with the new ownership epoch.

The RustFS CI gate runs two `crab-server` processes against shared object
storage. It revokes a token through one replica while the other holds an
admitted Git push, verifies that neither replica serves or advances the
branch using the old token, and verifies that a replacement token can
still publish after restart.

### Review and export organization audit history

**Audit log** in the organization displays recorded organization, invitation,
membership, team, project, ruleset, and repository access events for current
organization owners. Records show the action, timestamp in UTC, actor and
affected account, team, project, or repository where recorded. Names from
account profiles reflect the current public profile. Historical repository
names reflect the event's own organization at the time it was recorded.

Filter by **Identity provider group sync** to see committed SCIM team links,
roster synchronizations, and unlinks. Events identify the team, Crab SCIM group
ID, group revision, and counts of roster entries added or updated and removed.
Link and unlink events identify the organization owner. Automatic roster
synchronization is attributed to SCIM. Replaying an already committed group
revision does not append another event. Upgrade all serving replicas before
recording the new event type.

`GET /api/organizations/{id}/audit-log` accepts an exact `action` (for
example `team_group_sync`), actor and affected `account` UUIDs,
`repository` name, inclusive `since_ms`, exclusive `until_ms`, and
`limit` from 1 to 50. The actor filter parameter is `actor`. Continue with
the returned `next` as `after` and the original filters, even after an
empty page. The signed continuation binds the owner and organization and
walks a fixed prefix of both event histories. Each source keeps its
append order. The combined view compares timestamps at the heads of both
sources.

Use **Export JSON** or **Export CSV** to save the current search. The API
accepts `export=json|csv` with the same filters. An export response contains
up to 500 matching rows and scans up to 512 events. A response with
`X-Crab-Audit-Next` has more history. Follow it as `after`, or use
**Export next part** in the browser. `X-Crab-Audit-Count` counts rows in the
response. The next part remains available after an empty response.
Organization ownership is checked on each request. Downloads omit the raw
provider identities, group names, and access digests in private storage.

Native Organization events are visible from the existing durable history.
Repository registration, access changes and transfers appear from the first
write that records an organization scope. Older repository records without an
event-time organization remain in the private catalog audit chain. Each side
of a transfer receives its own name and event, with access to its own preceding
history. Coordinate this catalog change across all serving replicas before
accepting new writes. Keep a pre-upgrade backup for rollback.

### Native standalone teams

`cells/organization/teams/` and `api/organizations/teams/` own direct teams,
current-incarnation memberships, visible/secret reads, maintainer controls,
creation policy, bounded pages and permanent submission cancellation/recovery.
Organization membership transitions update the same native authority. Fresh
private bootstrap prepares teams; existing organizations require
`cells prepare-organization-teams --organization UUID --actor OWNER_ACTOR_UUID`.
Read [the team preparation and API contract](CELLAR.md#standalone-teams).

The UI owner is `packages/ui/src/features/organizations/teams/`. Its account-bound
submission controller lives above application routing, so lost acknowledgements
can recover after navigation or organization removal without retargeting the
write. New native/HTTP tests live beside their team owners; responsive,
accessibility, conflict and recovery scenarios live in
`packages/ui/tests/browser/teams.e2e.ts`. Team Project listings and linking
reuse the organization Project collection and access policy.

Current team qualification: 13 native/HTTP team checks, all 476 native regressions
(six existing opt-in qualifications ignored), 13 CLI checks, 164 browser scenarios,
107 UI unit checks, strict all-target Clippy and architecture/format/build checks.
Real RustFS/two-account OIDC browser runs cover native side effects, a lost edit
reply followed by revocation, non-restoring rejoin, permanent rejected/cancelled
submissions, restart and team name reuse. Desktop and actual mobile dark screens
were inspected. The local preview preserves all 18 public repository resources
and 39 canonical tables per repository Cell. These results qualify the standalone
team slice.

Initial issue/PR metadata is owned by `cells/repository/collaboration/creation/`
and composed by `api/discussion_creation/`. Public create bodies optionally include
`metadata: {label_ids, assignee_ids, milestone}`. Initial values commit atomically at
record version one. Applied UUIDs recover current records without resetting later
assignments; metadata rejections remain terminal and permit explicit input review.
The private PR submission read returns original comparison tips for both outcomes.
See `CELLAR.md` for coordinated peer DTO deployment and native storage contracts.


SSO repository access uses exact signed OIDC group IDs alongside individual
members. `auth/oidc/` validates claims, `auth/principal/` resolves the current role
union/token ceiling, and `catalog/` commits complete access-list changes with their
audit outbox. The Members settings page edits both sources. Group-backed sessions
have a fifteen-minute ceiling; IdP synchronization and native organization/team
authority remain separate. See `README.md` for provider setup and `CELLAR.md` for
fleet deployment, proof and limits.
