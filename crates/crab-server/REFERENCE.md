# Operate and integrate `crab-server`

The server embeds Cellar 0.1.0. [CELLAR.md](CELLAR.md) defines its native
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
| Configure OpenID Connect (OIDC) | [Team sign-in](#team-sign-in) |
| Clone or fetch with native Git | [Git HTTP reads](#git-http-reads) |
| Transfer LFS objects | [Git LFS transfers](#git-lfs-transfers) |
| Push branches and tags | [Native Git push](#native-git-push) |
| Use issues, pulls, reviews, labels, or assignees | [Issues, pull requests and reviews](#issues-pull-requests-and-reviews) |
| Report continuous integration (CI) results | [Commit statuses and required checks](#commit-statuses-and-required-checks) |
| Inspect limits and failure behavior | [Resource and lifecycle limits](#resource-and-lifecycle-limits) |
| Run verification | [Current verification](#current-verification) |
| Assess release readiness | [Completion requirements](#completion-requirements) |

## Understand the runtime architecture

Crab owns HTTP authentication and authorization, repository policy, the durable
catalog, Git publication, and embedded browser assets. Cellar owns native Cell
routing, leases, activation, SQLite/LTX persistence, replication, and recovery.

### Know which component owns each boundary

| Owner | Responsibility |
| --- | --- |
| `crab-server` | Product HTTP, catalog, authorization, typed repository operations, and private mTLS transport |
| Cellar 0.1.0 | Native execution domain, ownership, persistence, output durability, and peer protocol |
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

Release bootstrap, pinned-backup, and node-session commands are unavailable. Cellar 0.1.0
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
| Releases | Releases/Tags navigation, search, drafts, publication, edits, deletion, source ZIPs, and binary assets |
| Pull requests | Conversation, Commits, Checks, Files changed, reviews, shared Markdown formatting, approval state, and merge controls |
| Review requests | Versioned member requests, cancellation, submitted-review fulfillment, and reviewer sidebar |
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

### Git imports

The catalog page exposes **Import from Git** for an authenticated browser
session. The workflow accepts a public or private repository from an
allowlisted Git host, creates a new Crab repository, and copies its Git history
and refs into the configured object store. It does not migrate Git LFS payloads;
use the native Git/LFS workflow for repositories whose large files must be
copied as well.

| Endpoint | Contract |
| --- | --- |
| `POST /api/imports/git` | Starts an asynchronous import. The JSON body is `{source, owner, name, description?, token?}`. `source` is an HTTPS or SSH clone URL (or GitHub `owner/repository` shorthand) whose host appears in `import.allowed_hosts`; HTTPS tokens are sent as a bearer header and never persisted. Browser requests require the session CSRF token. |
| `GET /api/imports/git/{id}` | Returns `queued`, `running`, `succeeded`, or `failed` status for the caller's own job. Job records are in-memory and retained for one hour after completion. |

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

On success, the importing identity is the new repository's administrator in an
OIDC deployment. A local-trust deployment uses the local operator. Imports are
published only after the repository Cell has been initialized, its root verified,
and its control record released to idle. The catalog keeps the repository out of
serving indexes until that readiness gate passes; transient Cell activation
errors are retried by request paths. The import job becomes `succeeded` once all
refs are durably committed; Git browse indexes and attribution are then rebuilt
in the background. A mirror preserves every advertised ref (including provider
pull-request refs), so large GitHub mirrors can take longer during verified
visibility planning even though the repository is already durable. Jobs are
bounded to four concurrent imports; cloning and publication have a 30-minute
deadline, publication uses the server's atomic receive capability, and failed
publications retry with bounded backoff. The runtime image includes `git`; each
import reserves up to 8 GiB from the Cell local-disk budget while cloning, then
shrinks that reservation to the measured mirror plus a command margin. `TMPDIR`
must still have room for Git's process scratch files.
The server also needs outbound access to the configured Git host (or the
configured network proxy). Transient push failures are retried at the Git
publication boundary; a terminal failure leaves the destination name reserved until
an operator removes the repository that was created before the failure.

### Render repository content safely

Markdown README files render beneath directory listings. Recognized file views switch between source and a format-aware preview; ordinary UTF-8 Git blobs also offer blame.

Rust, JavaScript, TypeScript, Python, Go, Java, C, and C++ source files offer a searchable current-file symbol outline. Selecting a definition jumps to its line, and selecting a captured token shows same-file definitions and references. The Tree-sitter runtime, language grammar, and tag queries load lazily and parse inside a dedicated browser worker; this feature does not create a server-side or repository-wide symbol index. Results are search-based rather than compiler-semantic, so cross-file resolution and type-aware disambiguation remain out of scope.

Preview-capable formats include raster and SVG images, PDF, common audio and video containers, CSV/TSV, JSON/JSON Lines, Jupyter notebooks, Office Open XML and OpenDocument packages, SQLite, Parquet, Arrow/Feather, NumPy/NPZ, Safetensors, GGUF, ZIP-derived packages, and TAR archives. CSV/TSV, JSON/JSON Lines, Parquet, Arrow IPC/Feather, and SQLite open in a local SQL workbench with syntax highlighting, dialect- and schema-aware completion, a searchable multi-relation schema browser, sortable and searchable paginated results, and quick charts. DuckDB queries the flat and columnar formats; SQLite runs in a dedicated browser worker against an isolated copy and exposes its user tables and views. The workbench can explain a query, generate a column profile, stop a running query, export the loaded result as CSV or JSON, and restore recent successful, failed, or stopped SQL from browser-local history. Each query returns at most 1,000 rows and renders 100 rows per page; the source can contain far more rows. Parquet uses authenticated HTTP byte ranges so projection and row-group pruning do not require a full browser download, while CSV and JSON remain full-scan formats and Arrow and SQLite load the file into browser memory. Notebooks never execute cells or HTML outputs, and model previews inspect metadata without loading or running weights. Office previews preserve document text, worksheets, and slide structure rather than promising pixel-identical desktop rendering.

Preview parsers are loaded only for the selected format and run in the signed-in browser. Crab does not send repository contents to a third-party conversion service. Workbench results and its 15-entry-per-file run history remain in that browser; result exports are generated locally. DuckDB loads its signed Parquet and JSON WebAssembly extensions on demand from the official DuckDB extension service; the content security policy permits only that external connection. Source text is embedded in JSON only through 1 MiB; recognized larger text formats fetch their exact bytes for preview. Unrecognized binary formats use a bounded universal inspector with extension hints, magic-signature detection, a safe text sample when applicable, and a 512-byte hex/ASCII view. Whole-file interactive previews, including Arrow and SQLite workbenches, are capped at 50 MiB. The range-backed Parquet workbench is exempt because it bounds query output and can scan the source incrementally. Actual source capacity remains subject to the configured repository read budget, browser WebAssembly memory, query shape, and file encoding. Expensive joins, sorts, or aggregations can still exhaust a browser even when their output is small; use filters and projections, and prefer Parquet for repeated large-data analysis. Oversized formats without a range-backed reader retain exact raw and download actions with an explicit fallback instead of attempting unbounded parsing.

Relative links stay inside the selected repository revision. Relative raster images load through the signature-checked `asset` endpoint. SVG, other local formats, and external images remain links. This prevents repository content from becoming same-origin active content or forcing signed-in browsers to contact third-party hosts.

Crab and LFS pointer blobs download as the exact Git pointer bytes. The HTTP application does not hydrate pointer-backed artifacts in browser file views.

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
| `PUT /api/repos/{owner}/{name}/settings/archive` | `expected_version`, `archived`, and exact `owner/name` confirmation |

Changing the default branch records a new symbolic HEAD through the ref journal. It copies no objects and applies immediately to browser defaults and native Git discovery.

Branch protection uses exact short branch names. A configuration can seed version zero; after the first administrative save, the stored object is authoritative across restarts and server instances.

Archived repositories remain readable and searchable. Browser writes, native pushes, and LFS uploads return a read-only error. Native Git and LFS check archive state again immediately before publication.

### Create and manage releases

Release routes use durable request reservations and versioned records:

| Method and route | Purpose |
| --- | --- |
| `GET/POST /api/repos/{owner}/{name}/releases` | List or create releases |
| `GET/PATCH/DELETE /api/repos/{owner}/{name}/releases/{number}` | Read, edit, or tombstone one release |
| `GET /api/repos/{owner}/{name}/releases/tag/{tag}` | Read by unique tag, including slash-containing tags; same draft visibility as numbered reads |
| `POST /api/repos/{owner}/{name}/releases/{number}/assets` | Upload a release asset |
| `GET/DELETE /api/repos/{owner}/{name}/releases/{number}/assets/{asset_id}` | Download or remove an asset |

Creation includes a stable universally unique identifier (UUID) in `request_id`, short `tag_name`, full target commit OID, title, Markdown body, prerelease flag, and draft state. The target must be a visible commit.

An existing lightweight or annotated tag is accepted only when it peels to the exact target. A new published release creates a lightweight tag through canonical ref publication. Drafts reserve their release tag but do not publish a new Git tag until a version-bound edit publishes the draft.

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
| `write` | All Triage actions plus Git writes, LFS upload, merge, releases, labels, assignees, statuses, checks, and scoped write-token issuance |
| `admin` | All write actions plus repository settings and branch protections |

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
subject, display name, and `read`/`triage`/`write`/`admin` access, and sends the session
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

### Configure protected branches

A repository can define at most 100 exact branch rules:

```toml
protected_branches = [
  { branch = "main", required_approvals = 1, required_checks = ["ci/test"] },
]
```

Each rule accepts 0 to 20 approvals and up to 50 unique case-insensitive check contexts. Each context contains 1 to 100 characters.

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
out of the identity provider. Configure an object-store lifecycle rule that
deletes objects below `.crab/http-server/v1/auth/` after 24 hours. Active state
expires within eight hours; the rule only collects consumed flows, expired
sessions, and token records left after session invalidation.
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

`GET /api/session` returns the current account and CSRF token to the same-origin frontend. Anonymous repository APIs return HTTP 401. No cloud credential reaches the browser.

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

Token rules:

- Scope binds one owner, repository, and `read` or `write` permission
- Effective access intersects token scope with current membership
- A read token never inherits write capability
- Each session retains at most 10 tokens
- Sign-out, replacement sign-in, and explicit revocation invalidate tokens
- Revocation is enforced on the next request; an already admitted request drains

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
| One object | 512 MiB |
| Dependencies in one push | 2 GiB |
| Verification or byte transfer | 5 minutes |
| Shared Git and LFS transfers | 4 per process |

A started multipart operation keeps its permit and temporary file while it completes or aborts. This drain can extend beyond the five-minute request budget and server shutdown.

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
| One dependency file | 512 MiB |
| Aggregate dependency content | 2 GiB |
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

Organization/team authority, owner registration and remaining actor-backed
contracts and predicates remain unfinished. Choosing a handle matching a catalog
owner label does not acquire ownership.

### Use the collaboration route families

| Surface | Routes |
| --- | --- |
| Issues | `GET/POST /api/repos/{owner}/{name}/issues`, `GET/PATCH /api/repos/{owner}/{name}/issues/{number}` |
| Issue comments | `GET/POST /api/repos/{owner}/{name}/issues/{number}/comments`, `GET/PATCH /api/repos/{owner}/{name}/issues/{number}/comments/{comment}` |
| Pull requests | `GET/POST /api/repos/{owner}/{name}/pulls`, `GET/PATCH /api/repos/{owner}/{name}/pulls/{number}` |
| Pull comments | `GET/POST /api/repos/{owner}/{name}/pulls/{number}/comments`, `GET/PATCH /api/repos/{owner}/{name}/pulls/{number}/comments/{comment}` |
| Reviews | `GET/POST /api/repos/{owner}/{name}/pulls/{number}/reviews`, `GET/PATCH /api/repos/{owner}/{name}/pulls/{number}/reviews/{review}` |
| Inline review threads | `GET/POST /api/repos/{owner}/{name}/pulls/{number}/threads`, `GET/PATCH /api/repos/{owner}/{name}/pulls/{number}/threads/{thread}` |
| Thread replies | `GET/POST /api/repos/{owner}/{name}/pulls/{number}/threads/{thread}/replies`, `GET/PATCH /api/repos/{owner}/{name}/pulls/{number}/threads/{thread}/replies/{reply}` |
| Merge | `POST /api/repos/{owner}/{name}/pulls/{number}/merge` |
| Label search choices | `GET /api/repos/{owner}/{name}/discussion-search/labels` |
| Milestone search choices | `GET /api/repos/{owner}/{name}/discussion-search/milestones` |
| Labels | `GET/POST /api/repos/{owner}/{name}/labels`, `PATCH/DELETE /api/repos/{owner}/{name}/labels/{number}` |
| Assignees | `GET /api/repos/{owner}/{name}/assignees`; issue and pull `PATCH` requests replace assignments |
| Milestones | `GET/POST /api/repos/{owner}/{name}/milestones`, `GET/PATCH /api/repos/{owner}/{name}/milestones/{number}`, `GET /api/repos/{owner}/{name}/milestones/{number}/items`; issue/pull `GET/PUT .../{number}/milestone` |
| Reactions | `PUT /api/repos/{owner}/{name}/reactions/{target}`; discussion responses include personalized summaries |
| Conversation | `GET /api/repos/{owner}/{name}/issues/{number}/conversation`, `GET /api/repos/{owner}/{name}/pulls/{number}/conversation` |
| Activity | `GET /api/repos/{owner}/{name}/activity`, `GET /api/repos/{owner}/{name}/issues/{number}/activity`, `GET /api/repos/{owner}/{name}/pulls/{number}/activity` |
| Linked work | Issue/pull `GET .../{number}/links`; `GET/PUT /api/repos/{owner}/{name}/pulls/{pull}/linked-issues/{issue}` |

Every route prefix is `/api/repos/{owner}/{name}`. Browser mutations require the session cookie, canonical `Origin`, and CSRF token.

### Separate collaboration triage from code write access

Repository membership supports `read`, `triage`, `write` and `admin` roles.
The collaboration split follows [GitHub’s repository-role reference](https://docs.github.com/en/organizations/managing-user-access-to-your-organizations-repositories/managing-repository-roles/repository-roles-for-an-organization); full role parity remains incomplete.
Triage can close/reopen other authors’ issues and PRs, apply/remove existing
labels and assignees, apply milestones, and request/cancel reviews. It cannot
push code, issue write-scoped Git tokens, merge, change draft readiness on
another author’s PR, edit another author’s title/body, create/edit label or
milestone definitions, publish releases, or administer members/settings. Write
and Admin retain Triage capabilities. Authors retain their existing description
and own-item state controls; archive, pending-merge and version guards still
apply inside native commands.

Issue detail responses include `can_close` separately from `can_edit`; the
browser uses that capability for state controls. PR `can_manage` controls state
while `can_merge` still requires code write permission. Role edits use the
existing revisioned membership catalog and audit chain. Detail capabilities and
edit admission read durable membership instead of inferring it from a public
profile or delayed routing snapshot. Rechecks across awaited reads are not an
atomic cross-authority fence: the planned native grant cutover remains required.
Maintain, owner/organization/team grants, complete moderation and required-review
permission parity remain delivery work.

Native issue/PR edit inputs carry `can_triage` instead of their earlier coarse
management flag. Update fleet peers together before assigning the new role.
No catalog, canonical repository or discussion-index schema change is needed;
existing member records remain valid. No alias accepts superseded native inputs.

### Search issues and pull requests

List endpoints accept `q` (up to 256 characters), `limit` (1–50), `sort`,
`direction`, and an opaque exclusive `before` cursor. State belongs in the query,
for example `q=is:closed label:bug`. An absent `q` defaults to
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
Integration-account author syntax, mentions and complete involvement predicates remain open.

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

The current discussion index format is 10. It retains qualified author identities,
milestone catalog entries and stable actor assignment keys, and adds searchable
assignment name snapshots, transactional reviewer relationships and per-source
commenter identities, transactional contribution text/FTS, latest-review/decision
references and chronological conversation references. Older indexes report
`outdated`; run the explicit rebuild below before admitting indexed traffic.
It rebuilds only derived tables and preserves canonical milestone and assignment
records. Clearing, milestone catalog preparation and discussion projection are
separate bounded phases, with restart/resume through the existing management CLI.

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
`reviewed-by`, `review-involves`, and `user-review-requested`. The last qualifier
selects direct requests; team relationships are not implemented. A submitted
review fulfills its pending direct request and remains a participant; cancellation
without a review removes participation. Qualified issuer/subject bindings prevent
cross-provider subject collisions. Exclusion, boolean groups, exact full counts
and query-bound cursors use the same native predicate on mixed and PR-only lists.
The PR workspace adds Review requests navigation from the native
`review_requested_from_me` result flag. Negated/conditional self terms do not
select that personal view.

`GET .../discussion-search/reviewers?query=...&search=...&relation=...` returns
bounded public reviewer/query choices with `self`, `clear`, and truncation.
Relations are `review_requested`, `reviewed_by`, `review_involves`, and
`user_review_requested`. The shared shadcn Reviews menu exposes the first three,
search, keyboard selection, focus return, failure/retry and mobile/dark support.
Choosing a relationship replaces only global reviewer terms; conditional OR
groups and unrelated facets keep their meaning. Sort/direction and diagnostics
persist, while pagination resets. Review-status predicates remain unsupported
until connected to current Git head and branch protection policy.

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

Freshness compares the recorded commit with the actual source ref. `current`
is true for the observed head, false for an older commit, and null if the head
is missing. Merged PRs use their recorded source head. Only a current decision
gets a positive or negative status color; older decisions show “Outdated review,”
and an absent head shows “Head unavailable.” A re-request remains visible
alongside the previous review. Fulfillment alone never implies approval.
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
Pending/dismissed reviews, teams, code-owner suggestions, and notifications
remain separate delivery work.

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

### Work with pull requests and reviews

Pull creation records exact base and head branch names plus their opening commit OIDs. Detail reads refresh both live tips without rewriting the immutable opening evidence.

If either branch is deleted, the conversation and original OIDs remain visible while live comparison becomes unavailable. A completed merge retains its pre-merge comparison after source deletion.

Reviews record `commented`, `approved`, or `changes_requested` against the exact current head. Pull authors cannot approve or request changes on their own pull. Advancing the head marks older reviews as not current.

Review submission opens a shadcn “Review changes” dialog above the diff. The
modal stays fixed while asynchronous file content loads. Comment, Approve and
Request changes use the shared radio-group component. Draft text and submission
identity stay with the pull detail, surviving dialog closure and data reloads.
They are discarded when leaving the pull; no browser storage is used.

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

Suggestions are allowed only on new-side text anchors. The browser applies a current suggestion through the existing exact-head/exact-blob contents update contract, so it creates the normal Git commit and stale branch or blob tips return a conflict rather than silently changing a different file.

For protected branches, only each reviewer's latest decision on the current head counts. A current request for changes blocks merge until the same reviewer submits a current approval.

### Merge a pull request

Writers call `POST /api/repos/{owner}/{name}/pulls/{number}/merge` with a UUID `request_id`, pull `version`, `method`, exact base and head OIDs, and commit message.

| Method | Contract |
| --- | --- |
| `fast_forward` | Moves the base only when it is an ancestor of the head |
| `merge_commit` | Creates a two-parent commit after bounded recursive tree and text merging |

Merge-commit construction rejects binary conflicts, type conflicts, delete/modify conflicts, and overlapping text conflicts. Individual text blobs can contain at most 8 MiB during merge construction.

The server revalidates live refs, approvals, required checks, pointer dependencies, and visibility under the base-ref lease and both GC fences. A durable merge marker supports recovery after response loss or restart.

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
Detail/create routes keep full-width layouts. These controls do not implement
saved views or personal author/assignment/review navigation.
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

Discussion deletion, moderation, edit history, activity feeds, and notifications remain unimplemented. Native backup restore, cross-provider export, and complete product-root restore qualification remain pending.

## Commit statuses and required checks

CI systems can report compact commit statuses or detailed check runs. Both surfaces require a reachable commit and repository write access.

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

### Evaluate required checks

For each required context, the newest report wins across status and check-run records. A check run wins a same-millisecond tie.

| Latest result | Required-check state |
| --- | --- |
| Status `pending`, or run `queued` or `in_progress` | Pending and blocks merge |
| Status `success` | Success |
| Run conclusion `success`, `neutral`, or `skipped` | Success |
| Missing, `failure`, `error`, or any other completed run conclusion | Unsuccessful and blocks merge |

Required checks bind to the pull request's exact current head. A new head starts with missing checks. Merge admission uses one latest-status snapshot; later updates apply to later merge attempts.

## Resource and lifecycle limits

Product HTTP admission remains bounded. Cellar uses its bounded runtime defaults
and a resident-cell limit derived from process memory and file descriptors.
`cells capacity --json` reports the startup resource and admission envelope; it
is not a throughput or production-scale qualification.

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
| Collaboration | Durable issues, pulls, comments, reviews, labels, assignees, merge, checks, activity, and notifications | In progress; activity, moderation, history, and notifications remain |
| Repository management | CLI create/adopt/list, archive, settings, search, import, and audited administration | In progress; browser creation/import and audit history remain |
| Production operation | Durable concurrency, restart and crash recovery, backup restore, observability, upgrades, and operator guidance | Native process restart and owner handoff tested; native metrics and shutdown wiring implemented. Backup restore, crash matrices, upgrades, and provider qualification remain |
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
- Version-selected complete product-root restore qualification for Git, shared identity state, native Cellar metadata, immutable LTX graphs, and release assets across providers
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
- [GitHub classic branch protection](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/managing-a-branch-protection-rule)
- [GitHub repository archive behavior](https://docs.github.com/en/repositories/archiving-a-github-repository/archiving-repositories)

The OIDC dependency includes `rsa` for public-key signature verification. [RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071.html) concerns private-key timing leakage. This relying party holds no RSA signing keys and decrypts no RSA ciphertext. The test issuer uses Ed25519 only in the test harness. No advisory suppression or dependency override is present; a complete dependency audit remains part of production qualification.


## Organization authority foundation

The new `Organization` native class uses schema 1 under `cells/organization/` and
shares the Cellar transaction, receipt and output gate. Explicit private
`cells bootstrap-organization --organization UUID --input-file JSON` resolves the
first owner's exact `{issuer, subject}` through Directory without profile
enrollment. It returns `/-/organizations/UUID`. Public HTTP has no bootstrap route.

`/api/organizations/{id}` reads/edits metadata; `/members` lists bounded current
members, and `/members/{actor}` changes/removes active grants with expected
membership versions. `/invitations` lists bounded pending/expired offers;
`/invitations/candidates` searches public usernames;
`/invitations/{recipient}` reads, creates/edits/retries or cancels an offer.
`/invitation` reads/decides only the authenticated recipient's offer. Bodies deny
unknown fields. Repository Git tokens cannot access these account features.
Native authorization precedes receipt replay and mutations; last-owner guards,
invitation decisions, membership tombstones and audit append are transactional.

Organization authority grants no catalog repository access. Registered-account
invitations currently use manually shared acceptance links; email delivery,
owner-name discovery, teams, owner registry and cross-authority repository/project
fencing remain unfinished. See `CELLAR.md` for preparation, expiry and coordinated
deployment contracts. Native tests live in `cells/organization/tests/`, HTTP tests
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
