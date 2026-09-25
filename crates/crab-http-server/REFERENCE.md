# Operate and integrate `crab-http-server`

This reference explains how to configure, run, call, and qualify Crab's repository HTTP application. It covers the browser API, native Git transport, Git Large File Storage (LFS), authentication, collaboration data, publication, storage ownership, and current delivery gaps.

> **Development status:** The server supports real repository reads and writes, but production qualification remains incomplete. Read [Completion requirements](#completion-requirements) before deploying it beyond a controlled environment.

Start with [the crate README](README.md) if you need only the build command and source entry points. Use this document when you need an operator or protocol contract.

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

One Rust process serves the embedded React application, application programming interface (API), native Git smart HTTP, and LFS. Object storage remains authoritative; local disk holds only bounded temporary artifacts.

```mermaid
flowchart LR
    Browser[Browser and React app]
    Git[Native Git client]
    CI[CI integration]
    Server[crab-http-server]
    Auth[OIDC or GitHub provider]
    Temp[Temporary pack and index files]
    Store[(S3, GCS, or Azure Blob root)]

    Browser -->|web pages, API data, ZIP| Server
    Git -->|protocol v2, receive-pack, LFS| Server
    CI -->|status and check APIs| Server
    Server -->|sign-in and key discovery| Auth
    Server -->|bounded preparation| Temp
    Server -->|Git, metadata, app state| Store
```

Normal serving paths do not create a server checkout, run the Git executable, or
maintain a local Git object database. The browser Git import is an explicit
background workflow: it runs `git clone --mirror` in Cell-managed staging, then
pushes Git history and refs through the server's receive-pack endpoint. The
integration tests may use Git as an independent protocol oracle.

### Know which component owns each boundary

| Component | Responsibility |
| --- | --- |
| `crab-http-server` | Routing, HTTP policy, authentication, authorization, durable sessions, catalog refresh, versioned application documents, and response contracts |
| `crab-remote-git` | Immutable, bounded Git reads from committed object locators and packs |
| `crab-read` | Snapshot-bound dependency selection and verified Crab pointer reconstruction |
| `crab-lfs` | LFS object layout, streaming transfer, size checks, and SHA-256 verification |
| `crab-remote` | Reusable pack preparation, lease orchestration, and publication composition |
| `crab-write` | Ref-journal commit, compaction, catalog publication, and read readiness |
| `crab-storage` | Provider construction, object locations, and conditional storage operations |
| `crab-metadata` | Manifests, refs, visibility evidence, indexes, receipts, and publication formats |
| `crab-coordination` | Per-ref leases, namespace serialization, and garbage collection (GC) fences |
| `packages/ui` | React interface, URL state, design tokens, Pierre Trees and Diffs, and accessible interactions |

### Follow one repository read

The read path pins every response to one repository snapshot. Historical links use full commit object IDs (OIDs) so later branch movement does not change their content.

```mermaid
sequenceDiagram
    participant C as Client
    participant H as HTTP boundary
    participant R as Remote reader
    participant M as Metadata
    participant O as Object storage

    C->>H: GET repository resource
    H->>H: Validate Host and membership
    H->>R: Open bounded snapshot
    R->>M: Read manifest, refs, and catalog
    R->>O: Read exact pack ranges
    R-->>H: Generation-pinned result
    H-->>C: JSON, bytes, or stream
```

If committed refs await indexing, the server runs the shared read-readiness pass and retries the open. The server never invents visibility evidence or rolls back a committed ref.

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
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-http-server-dev" \
  cargo build -p crab-http-server --release --locked
```

### Configure one storage root

The process configuration selects listeners, one physical storage root, and
optional identity. Repositories are durable catalog records below that root;
they are not repeated in every pod's configuration.

```toml
listen = "127.0.0.1:8788"
management_listen = "127.0.0.1:8789"

[cells]
data_dir = "/var/lib/crab/cells"
local_disk_limit_bytes = 34359738368
peer_advertise = "https://localhost:8789"
failure_zone = "local"
failure_host = "development-node"
peer_certificate = "/run/secrets/crab-peer/tls.crt"
peer_private_key = "/run/secrets/crab-peer/tls.key"
peer_ca = "/run/secrets/crab-peer/ca.crt"

[storage]
url = "s3://your-bucket/repositories"
```

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
the same catalog UUID. Adopt validates an existing layout and manifest, records
`empty_cell_pending`, publishes a new empty application Cell and never converts
arbitrary object prefixes. Old collaboration application data is not imported.

```sh
SERVER="$HOME/Workspace/crabbuild-target/crab-http-server-dev/release/crab-http-server"
"$SERVER" --config /secure/server.toml repository create \
  --owner your-team \
  --name your-project \
  --prefix your-team/your-project \
  --default-branch main \
  --description "Your project" \
  --members-file /secure/members.toml

"$SERVER" --config /secure/server.toml repository list
"$SERVER" --config /secure/server.toml cells status \
  --owner your-team --name your-project
"$SERVER" --config /secure/server.toml cells capacity --json --live
"$SERVER" --config /secure/server.toml cells backup create \
  --pin 11112222333344445555666677778888
"$SERVER" --config /secure/server.toml cells backup verify \
  --pin 11112222333344445555666677778888
"$SERVER" --config /secure/server.toml cells backup restore \
  --pin 11112222333344445555666677778888 \
  --destination-prefix recovery/restore-2026-09-16
"$SERVER" --config /secure/server.toml cells release activate \
  --expected-revision 8 --strategy maintenance \
  --retention-grace-hours 168 --retention-max-deletes 10000
"$SERVER" --config /secure/server.toml repository set-members \
  --owner your-team --name your-project \
  --members-file /secure/members.toml
"$SERVER" --config /secure/server.toml storage-probe
"$SERVER" --config /secure/server.toml serve
```

`storage-probe` validates the complete runtime storage contract: catalog read,
bounded list, holder-checked conditional create/update, ordinary object write,
delete, and a post-delete not-found read. `serve` runs the same preflight
before binding either listener. The unique delete-test object is created below
`.crab/http-server/v1/auth/preflight/`; normal completion removes it, while the
provider profile's one-day lifecycle bounds residue after an interrupted
probe.

`cells status` is a read-only operator view of the durable control record. Its
versioned JSON includes the Cell and incarnation IDs, lifecycle state, epoch,
revision, serving session and endpoint, exact LTX root, code/schema pair, and
next scheduler deadline. It reads object-store authority directly; it does not
open SQLite, acquire ownership, or extend a lease.

`cells capacity --json --live` is a read-only mTLS request to the running
process. It reports that process's retained startup memory limit, free Cell
volume bytes, configured Cell-volume limit, available file descriptors,
CPU-derived job credits, and the resulting active-Cell, retained-byte,
local-disk, scratch, blocking-job, dirty-job and full-recovery admission
limits. The full-recovery limit stays at two even when the node can run more
blocking or capture jobs. Omitting `--live` calculates a preflight envelope for
the command process. Both are qualification inputs, not measured performance
evidence.

`cells backup create` observes all 256 catalog heads before traversing their
immutable pages. It binds the selected release record and descriptors, the
exact catalog revisions, one canonical control per entry, and every verified
LTX dependency into a strict-created pin. Repeating an existing pin ID verifies
and returns the original boundary. A signed zero-capacity advertisement covers
the complete create operation; maintenance drains it before collecting objects,
while a creator that advertises after the release fence fails its second Ready
check. `cells backup verify` rereads the complete
graph and fails closed when any content-addressed dependency is absent or
corrupt. `cells backup restore` requires a ready pinned release and a canonical
destination prefix in the configured bucket. It verifies the complete source
graph before creating the destination identity, uses conditional provider-side
copies for immutable objects, independently verifies destination descriptors,
catalog pages and LTX roots, strict-creates unowned `Idle` controls, and writes
the release and pin pointers only after their dependencies. An exact interrupted
restore is resumable while an already-used or divergent destination fails
closed. Cross-provider archive export and server-owned configuration outside
`cells/v1` remain separate operator work.

Maintenance retention is optional. With `--retention-grace-hours`, the sole
maintenance executor verifies the exact release, revision-pinned catalog,
unowned current controls, all retained pins, and their LTX graphs before the
first delete. It stores reachability in bounded local SQLite scratch, streams
the application prefix, and considers only recognized V1 immutable paths older
than the provider-time grace. Unknown layouts and mutable authority are skipped.
The deletion bound defaults to 10,000 and cannot exceed 100,000. Reaching it
returns an incomplete-retention error and keeps the release in `Maintenance`;
repeat the same activation and expected revision until it returns `Ready`.

Membership is supplied separately so the shared server configuration stays
small and secret-independent:

```toml
members = [
  { subject = "provider-subject", name = "Alice", access = "admin" },
]
```

Use `repository adopt` with the same identity flags for an existing canonical
Crab prefix. Owner and name are URL-safe identifiers of at most 100 bytes;
`api`, `assets`, `auth`, and `git` are reserved owner names. Prefixes and names
are unique in the catalog, and the relative `.crab` namespace is reserved for
server state. Successful changes become visible to every replica within five
seconds without a restart.

### Understand local trust mode

Without browser authentication, the server accepts loopback listeners only. This mode trusts one
local operator and exposes every cataloged repository to that principal.

The public listener exposes `GET /livez` as a storage-independent load-balancer
liveness probe. It accepts an IP-valued `Host` because ALB health checks address
tasks directly, returns no runtime state, and never substitutes for readiness.
The private management listener owns `GET /healthz`, `GET /readyz`, and
`GET /metrics`. The public listener does not expose management routes. Use
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
docker compose --file crates/crab-http-server/deploy/compose.yaml \
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
  --file crates/crab-http-server/deploy/Dockerfile \
  --tag crab-http-server:local \
  .
```

### Prepare runtime files

Copy `crates/crab-http-server/deploy/server.example.toml` outside the checkout.
Replace its identity and storage-root values.

Keep these inputs separate:

- `server.toml`: listeners, storage root, and browser authentication configuration
- `crab-storage.env`: private storage credentials
- `oidc-client-secret`: optional confidential-client secret
- `state-key`: at least 32 random bytes, stable across every replica and rollout
- `/var/lib/crab/tmp`: writable transient pack and index space

### Initialize and run the container

Run the initialization command once with the production mounts, credentials, and configuration. Then start the service behind an HTTPS reverse proxy:

```sh
docker run --rm --name crab-http-server \
  --publish 127.0.0.1:8788:8788 \
  --env-file /secure/crab-storage.env \
  --mount type=bind,src=/secure/server.toml,dst=/etc/crab/server.toml,readonly \
  --mount type=bind,src=/secure/oidc-client-secret,dst=/run/secrets/crab-oidc-client-secret,readonly \
  --mount type=bind,src=/secure/state-key,dst=/run/secrets/crab-state-key,readonly \
  crab-http-server:local
```

Run `repository create` or `repository adopt` as a one-shot container with the
same mounts. Creation and adoption initialize a new empty Cell before returning;
adoption intentionally does not preserve old collaboration data. A running
server rejects changed catalog versions containing pending or rootless repositories and marks
readiness unhealthy; it swaps in ready changes without a restart.

The binary's `healthcheck` command calls `/readyz` on the management listener.
`SIGTERM` and Ctrl-C start the same graceful drain. Repository, catalog,
identity, and application state remain in object storage; `/var/lib/crab/tmp`
contains only bounded transient files.

### Probe health and scrape metrics

The two probe routes answer different operator questions:

| Route | Success means | Failure contract |
| --- | --- | --- |
| `GET /healthz` | The HTTP process can answer | It does not inspect repository storage |
| `GET /readyz` | The durable catalog is valid and every cataloged repository can open its current Git view within 10 seconds | HTTP 503 with `Retry-After: 5` |
| `GET /metrics` | Prometheus text exposes request/body lifetime, admission, catalog, repository, receive-worker, and drain signals | It does not perform a storage probe |

Only the management listener serves probes. Every public request retains strict
canonical `Host` validation.

Before starting either listener, the process also claims and releases one of
16 dedicated startup-probe slots in the shared transfer-admission namespace.
Invalid conditional-write permissions therefore fail startup instead of
leaving a read-ready server that rejects its first transfer. Probe slots are
separate from the four live-transfer slots.

Every public response includes a server-generated `x-request-id`. The completion log records the same identifier with the method, path, status, and elapsed milliseconds. Set the standard `RUST_LOG` environment variable to adjust tracing filters; the default level is `info`.

Metrics use bounded `method`, `outcome`, and `class` labels. They never include
repository names, paths, principals, request IDs, or storage keys. Request
duration and in-flight gauges retain ownership through the response body, so a
long Git, LFS, archive, or release stream remains visible after its handler has
returned. Body errors and client aborts have separate counters.
The `git_transfer` available-permit gauge describes the current process's
fast-path guard. The `crab_http_server_transfer_admission_rejections_total`
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

See `deploy/README.md`, `deploy/terraform/README.md`, `deploy/helm/crab-http-server/README.md`, and `deploy/operations.md`. Lambda is intentionally excluded from the full data plane because Git and LFS require long streaming requests, large bodies, and bounded scratch that do not preserve the same contract through Lambda/API Gateway buffering and limits.

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

The repository cache reopens authoritative state after two seconds. Git fetch always opens a fresh snapshot. Interactive reads have a two-minute operation budget, an 8 MiB response limit, and process-wide admission for 16 concurrent requests.

When a readable generation lacks its split commit graph, the server builds that derived index from bounded remote commit reads and attaches it before retrying deep blame. This process downloads no full packs and creates no checkout.

Catalog maintenance also repairs a missing standard Git pack sidecar. It downloads the canonical pack only when `.idx` or `.rev` is absent, verifies the manifest size, pack trailer, BLAKE3 identity, and object count, and rebuilds both sidecars with Gitoxide in temporary storage. The immutable store accepts the regenerated bytes only through content-addressed create semantics. Existing malformed sidecars fail closed, and pack bytes never stand in for journal-owned visibility evidence.

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
failure_host = "worker-17"
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
kubectl --namespace crab exec --stdin deployment/crab-http-server -- \
  crab-http-server --config /etc/crab/http-server/server.toml \
  repository set-members --owner your-team --name your-project \
  --members-file - < /secure/crab-members.toml
```

| Access | Capabilities |
| --- | --- |
| `read` | Browse, fetch, download LFS, and participate in issues, pull requests, comments, and reviews |
| `write` | All member actions plus Git writes, LFS upload, merge, releases, labels, assignees, statuses, checks, and scoped write-token issuance |
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
subject, display name, and `read`/`write`/`admin` access, and sends the session
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
event in the same commit. Version 2 catalogs remain readable, but the first
write becomes v3; rolling back to a pre-v3 server afterward is unsupported.
The bounded pending outbox flushes to
`.crab/http-server/v1/audit/membership/` and is retried after interruption.
There is no browser audit-history API yet.

An authenticated account without membership sees an empty catalog. Unauthorized
and absent repositories both return HTTP 404 after authentication. Catalog
membership changes do not invalidate the user's shared session and become
effective on every replica after refresh.

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

Restarting or replacing a replica preserves sessions and Git tokens. Logout
requires the canonical `Origin` and the session's cross-site request forgery
(CSRF) token. It removes the durable session, invalidating subsequent browser
and Git-token authentication on every replica, but does not sign the account
out of the identity provider. Configure an object-store lifecycle rule that
deletes objects below `.crab/http-server/v1/auth/` after 24 hours. Active state
expires within eight hours; the rule only collects consumed flows, expired
sessions, and token records left after session invalidation.
The identity-index migration cutoff at
`.crab/http-server/v1/identity-index-migration.json` and Logout Token replay
records under `.crab/http-server/v1/logout-replays/` intentionally live outside
that lifecycle prefix. Keep the cutoff until rollout completes and retain replay
records at least through their signed token expiry; clean them with an
operator-controlled, expiry-aware job rather than applying the 24-hour auth
rule to these coordination records.

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
the exact `(issuer, subject)` across replicas. The hashed identity-session index
has an eight-hour compatibility window that also scans pre-index sessions; roll
out all replicas inside that maximum session lifetime. Verify delivery by
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
python3 crates/crab-http-server/tests/verify_git_transport.py \
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
    participant H as crab-http-server
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

Temporary disk use can exceed the wire limit because quarantine, normalized pack output, and index sidecars can overlap.

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
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-http-server-dev" \
  cargo test -p crab-http-server --locked \
  native_http_push_rustfs -- --ignored --nocapture
```

Create the temporary directory first. Replace the test name with `receive_faults_rustfs` and use another fresh prefix to exercise lost marker replies, rejected marker writes, cancellation boundaries, and GC fence renewal.

## Issues, pull requests and reviews

Issue, issue-comment and label state use one repository SQLite Cell whose committed
LTX roots are authoritative in object storage. Remaining collaboration domains
still use versioned JSON documents under the repository prefix. Collaboration
does not mutate Git refs unless a pull request merge publishes a ref update.

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
| Labels | `GET/POST /api/repos/{owner}/{name}/labels`, `PATCH/DELETE /api/repos/{owner}/{name}/labels/{number}` |
| Assignees | `GET /api/repos/{owner}/{name}/assignees`; issue and pull `PATCH` requests replace assignments |

Every route prefix is `/api/repos/{owner}/{name}`. Browser mutations require the session cookie, canonical `Origin`, and CSRF token.

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

Lists accept `state=open|closed|all`, a case-insensitive `q`, `limit`, and exclusive numeric `before`. They return newest items first.

### Work with pull requests and reviews

Pull creation records exact base and head branch names plus their opening commit OIDs. Detail reads refresh both live tips without rewriting the immutable opening evidence.

If either branch is deleted, the conversation and original OIDs remain visible while live comparison becomes unavailable. A completed merge retains its pre-merge comparison after source deletion.

Reviews record `commented`, `approved`, or `changes_requested` against the exact current head. Pull authors cannot approve or request changes on their own pull. Advancing the head marks older reviews as not current.

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

Label constraints:

- 1 to 50 characters per name
- Case-insensitive unique names
- Six hexadecimal color digits without `#`
- Descriptions up to 100 characters
- At most 20 labels per issue or pull
- At most 500 allocated label IDs during repository lifetime

Assignments contain at most 10 distinct configured member subjects. Only repository writers can change label or assignee sets.

Stored assignments use stable IDs or subjects. Label renames appear immediately. Deleted labels disappear without rewriting every discussion. Removed members disappear from resolved assignment responses.

### Understand pagination and storage

Issue, pull, comment, and review lists default to 30 items and accept 1 to 50. Each page scans at most 200 allocated numbers. A filtered page can be empty and still return `next`; clients must follow that cursor. Label lists return the complete bounded catalog of at most 500 active labels.

Titles accept 1 to 256 characters. Markdown bodies accept 64 KiB. Collaboration requests use an 80 KiB body limit, eight concurrent application slots, and a 30-second handler deadline.

Serving data currently uses these roots:

| Root | Content |
| --- | --- |
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

Discussion deletion, moderation, edit history, activity feeds, and notifications remain unimplemented. Cell backup pin creation, verification, and same-bucket isolated-prefix restore are implemented; cross-provider export and complete product-root restore qualification remain pending.

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

This table collects process and transport limits that otherwise span several route families.

| Boundary | Limit | Ownership |
| --- | ---: | --- |
| Interactive repository reads | 16 concurrent, 5 minutes, 4 GiB fetched/inflated, 8 MiB response | `server.rs` and `crab-remote-git` |
| Collaboration handlers | 8 concurrent, 30s | `app.rs` and route middleware |
| Git fetch, push, LFS, archives, and release assets | 4 concurrent across the deployment | Process-local fast-path semaphore plus renewable object-store CAS slots under `.crab/http-server/v1/admission` |
| Read-readiness publication | 2 repositories concurrently, 3-minute cooperative budget | `maintenance.rs` |
| OIDC callbacks | 8 concurrent, 10s per provider request | `auth.rs` |
| Archive transfer | 10 minutes, 3 GiB encoded response | `archive.rs` |
| Git receive | 30-minute cooperative budget, 8 GiB body | `receive.rs` |
| Browser existing-object ref update | 30s | `receive.rs` |
| LFS transfer | 5 minutes, 512 MiB object | `lfs.rs` and `crab-lfs` |

Handler completion and response-body completion are different lifecycle events. Archive, release-asset, Git, and LFS streams retain their own permits and cancellation guards until the body finishes or fails.

Server shutdown follows this order:

1. Stop accepting new work after Ctrl-C or `SIGTERM`
2. Cancel request-scoped work
3. Drain Axum connections
4. Close and drain receive workers
5. Release and drain deployment-wide transfer leases
6. Drain retained maintenance jobs and lease cleanup
7. Shut down the shared remote-read runtime

Stateful publication futures are drained instead of aborted. This prevents a dropped handler from abandoning a marker attempt or releasing a GC fence before cleanup finishes.

## Current verification

Run focused checks from the repository root. Use a target directory unique to this checkout:

```sh
npm test --prefix packages/ui
npm run typecheck --prefix packages/ui
npm run format:check --prefix packages/ui
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-http-server-dev" \
  cargo test -p crab-http-server --locked
```

Run the read-only live verifier against a revision with a parent and `README.md`:

```sh
python3 crates/crab-http-server/tests/verify_live.py \
  --url http://127.0.0.1:8788 \
  --repository your_team/your_project \
  --source /path/to/read_only_source \
  --revision 0123456789012345678901234567890123456789
```

For an authenticated server, add `--cookies /path/to/private_cookies.txt` with a private Netscape-format cookie file. Never commit that file.

Run repeatable concurrent HTTP qualification with the Rust load generator:

For durable command throughput, create a JSON template containing a top-level
`"request_id":"{{request_id}}"`. Run mutations only against a disposable
repository: the generator assigns a fresh UUIDv7 and creates real state for
every request.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-load-generator" \
  cargo run -p crab-http-server --release --example qualify_http_load --locked -- \
  --base-url http://127.0.0.1:8788 \
  --target 'refs=4@/api/repos/team/project/refs' \
  --target 'commits=8@/api/repos/team/project/commits?rev=main&limit=20' \
  --mutation 'issues=8@/api/repos/team/disposable-load/issues|/secure/new-issue.json' \
  --aggregate-requests-per-second 1000 \
  --duration-seconds 60 \
  --warmup-seconds 5 \
  > http-load.json
```

Each read or mutation target declares its own concurrency and all targets run together. The
versioned JSON receipt includes the configured aggregate rate, its minimum
successful-response count, successful responses, HTTP 429 admission
rejections, unexpected responses, response bytes, throughput, and p50/p95/p99
latency. A fixed-rate run fails qualification when it delivers less than 95%
of the configured successful request count; admission rejections therefore do
not masquerade as sustained target throughput. The generator fully consumes every body, bounds response bytes, checks
`/livez` before and after traffic, and exits unsuccessfully on 5xx, unexpected
non-429 status, transport/body-limit failure, or unhealthy liveness. Use
`--header-file /secure/load-headers` for one private HTTP header per line; the
tool neither prints nor stores those values.
For direct-node qualification through a loopback port-forward, pass
`--authority git.example.com`. The option accepts only one DNS name or IP
address without a port and changes the HTTP Host header without changing the
connection destination.

### Read the executable evidence map

| Contract | Primary source | Executable evidence |
| --- | --- | --- |
| Route composition, Host checks, request correlation, readiness, metrics, and shutdown | `src/server.rs`, `src/metrics.rs` | Server, metrics, authentication, and maintenance tests |
| OIDC, membership, sessions, tokens, CSRF, and back-channel logout | `src/auth.rs`, `src/members.rs` | `src/auth_tests.rs`, `src/auth_tests/members.rs`, `src/auth_tests/backchannel_logout.rs`, and `src/auth_tests/git_tokens.rs` |
| Repository reads and raw paths | `src/api.rs` | `tests/verify_live.py` and frontend navigation tests |
| HTTP capacity and overload behavior | server admission and public routes | `examples/qualify_http_load/main.rs`, its self-hosted tests, and retained JSON receipts |
| Git protocol version 2 fetch | `src/git.rs` | `tests/verify_git_transport.py` and protocol CI |
| Native receive, changed-path validation, and recovery | `src/receive.rs`, `src/receive/publish.rs`, `crab-git::receive_plan` | `src/receive_tests.rs` and `src/receive_fault_tests.rs` |
| LFS transfer, range-resume, file-lock, and authoritative receive contracts | `src/lfs.rs`, `src/receive/publish.rs` | `src/lfs_tests.rs`, `src/receive_tests.rs`, `src/auth_tests/git_tokens.rs`, `tests/qualify_lfs_range_resume.sh`, and `tests/qualify_lfs_locking.sh` |
| Browser Git writes and settings | `src/contents.rs`, `src/branches.rs`, `src/members.rs` | `src/auth_tests/branches.rs`, membership API tests, and browser settings tests |
| Issues, labels, and assignees | `src/issues.rs`, `src/cells/repository.rs`, `src/cells/router.rs`, `src/labels.rs`, `src/assignees.rs` | Scoped authenticated Cell publication and source-loss tests |
| Pulls, reviews, checks, and merge | `src/pulls/`, `src/statuses.rs`, `src/checks.rs` | `src/pulls_tests.rs` and `src/auth_tests/pulls.rs` |
| Releases and assets | `src/releases.rs` | `src/auth_tests/releases.rs` |
| Container and multi-cloud deployment contracts | `deploy/Dockerfile`, `deploy/helm/crab-http-server`, `deploy/terraform` | `.github/workflows/http-server-container.yml`, `.github/workflows/http-server-release.yml`, `.github/workflows/http-server-kubernetes-live.yml`, the Helm storage test, and `deploy/helm/crab-http-server/qualification/qualify-kubernetes.sh` |

### Understand what has been qualified

Current local and CI evidence includes:

- Kubernetes-scale protocol version 2 discovery, partial clone, deepening, large request batches, path search, history, diff, and deep blame
- Exact commit, tree, blob, archive, LFS, branch, tag, release, pull, merge, status, and check data compared with independent Git clients
- Native initial pushes, fast-forward updates, branch and tag lifecycle, atomic rejection, fault injection, response loss, cooperative restart recovery, and container `SIGKILL` during an in-flight push
- OIDC redirects and signed-token validation, key rotation, membership isolation, token scope, identity revocation, Origin checks, CSRF rejection, and back-channel logout validation
- Browser light, dark, desktop, narrow-screen, keyboard, conflict, and automated Web Content Accessibility Guidelines (WCAG) A/AA checks
- Container build, non-root identity, stop signal, health command, storage-aware repository readiness, private metrics scrape, Prometheus-validated baseline alerts, runtime inspection, strict Helm lint, and Kubernetes schema validation
- Complete-root RustFS cold copy into an isolated prefix, exact key/size comparison, byte hashing of every object, and independent restored Git, issue, and LFS reads
- LFS partial download and byte-identical range resume through the Compose Caddy/server/RustFS stack, including safe full-response fallback for multiple ranges
- Stock Git LFS lock, list, verify-on-push, and unlock against the Compose Caddy/server/RustFS stack
- Native Git rejection when another subject owns a changed path, including a change-and-revert history whose final tree matches the original

These runs use local RustFS, in-memory stores, shared caches, and controlled fixtures. Recorded timings are diagnostic observations, not throughput or production latency guarantees. The container crash test proves one in-flight native-push boundary and accepts only the exact old or new ref before a byte-identical retry or clone. The cold-restore test proves the complete fixture root can move to an isolated object prefix without flattening its key namespace and remain readable through independent protocols. In-memory multi-instance tests prove that independent server admission gates share and reuse fixed storage slots. These tests do not establish every crash phase, provider-scale performance, version-selected cloud recovery, or complete manual accessibility.

The packaged Kubernetes gate makes live evidence repeatable, but its existence
is not provider qualification. Only a successful EKS, GKE, or AKS run and its
workflow-attested JSON receipt establish that release's cross-replica rollout
result. The gate also launches a short-lived Restricted peer pod and fails if
the cluster CNI permits it to reach the private management listener. The
protected workflow first verifies the deployed image provenance against the
exact server release tag and source commit. It now derives the expected image
and chart from the tag's signed release record. The live gate also rejects a
mismatched installed chart version or missing provider-native workload identity
injection on either the original or replacement pods.

The chart's disruption budget protects healthy capacity with `minAvailable`
and marks unhealthy pods `AlwaysAllow` for eviction. This preserves voluntary
availability without letting a permanently unready pod block a node drain.

### Keep qualification evidence honest

Use the following interpretation:

| Evidence | What it proves | What it does not prove |
| --- | --- | --- |
| Unit test | Local contract and rejection behavior | HTTP composition or real storage |
| In-memory HTTP test | Routing, authorization, and response semantics | Provider behavior or restart durability |
| RustFS integration | Real object-store persistence and independent-client result | Production load, region failure, or Internet latency |
| Browser regression | Rendered behavior and automated accessibility rules | Storage durability or manual assistive technology |
| Container CI | Reproducible image/runtime metadata, one in-flight `SIGKILL` outcome, and one isolated complete-root cold restore | Cloud orchestration, version-selected provider recovery, every crash phase, or upgrade safety |

## Completion requirements

The server is complete only when a real account can perform the workflow and observe its durable result. A green component test or visible placeholder does not satisfy that bar.

| Surface | Required evidence | Status |
| --- | --- | --- |
| Multi-replica deployment | One Rust binary, durable CAS catalog and identity state, private management probes, graceful drain, hardened Helm profile, reproducible container, and portable cross-replica gate | Implemented with a protected GitHub OIDC evidence workflow; each provider release still requires a successful recorded live run |
| Repository browsing | Refs, byte-preserving paths, history, files, blame, downloads, freshness, and empty/error states against real repositories | In progress |
| Diff and tree interface | Pierre Trees and Diffs, correct modes and binary handling, bounded large-repository behavior, and keyboard navigation | In progress |
| GitHub-quality design | Themes, responsive layouts, accessible controls, navigation, and loading/error behavior across workflows | In progress |
| Team identity and authorization | OIDC, sessions, membership, permissions, isolation, revocation, CSRF, and administration | In progress; browser and CLI membership replacement are audited, and back-channel logout revokes delivered exact-identity sessions |
| Git hosting | Authenticated fetch and push, exact branch/tag lifecycle, protection, publication, and independent-client proof | In progress; additional crash phases and coexistence qualification remain |
| Collaboration | Durable issues, pulls, comments, reviews, labels, assignees, merge, checks, activity, and notifications | In progress; activity, moderation, history, and notifications remain |
| Repository management | CLI create/adopt/list, archive, settings, search, import, and audited administration | In progress; browser creation/import and audit history remain |
| Production operation | Durable concurrency, restart and crash recovery, backup restore, observability, upgrades, and operator guidance | Kubernetes controls, one RustFS in-flight `SIGKILL` path, one complete-root cold restore, verified Cell backup pins and same-bucket isolated restore, bounded Prometheus metrics, request correlation, and runbook implemented; cross-provider export and broader live qualification remain |
| Quality gates | API, UI, accessibility, realistic repositories, security, package smoke, and measured performance | In progress |

### Track known operational gaps

The remaining production gaps include:

- Broader abrupt-crash coverage beyond the qualified in-flight native-push boundary, plus a portable client recovery token (native Git can only recover an identical wire request through the server-side plan receipt)
- Journal or visibility-receipt reconstruction when verified evidence is missing; missing standard Git `.idx` and `.rev` sidecars are repaired from the verified canonical pack during catalog maintenance
- Protected-view writer coexistence with shared namespace guarantees
- Production throughput and provider-level admission qualification
- Membership audit history and immediate arbitrary provider revocation when no Logout Token is delivered
- Repository creation and adoption exist in the CLI; browser import remains
- Version-selected complete product-root restore qualification for Git, shared identity state, Cell pins, immutable LTX graphs, and release assets across providers
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
