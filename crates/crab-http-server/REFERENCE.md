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
    Auth[OIDC provider]
    Temp[Temporary pack and index files]
    Store[(S3, GCS, or Azure Blob root)]

    Browser -->|web pages, API data, ZIP| Server
    Git -->|protocol v2, receive-pack, LFS| Server
    CI -->|status and check APIs| Server
    Server -->|sign-in and key discovery| Auth
    Server -->|bounded preparation| Temp
    Server -->|Git, metadata, app state| Store
```

The process does not create a server checkout, clone a repository, run the Git executable, or maintain a local Git object database. The integration tests may use Git as an independent protocol oracle.

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
| `packages/repository` | React interface, URL state, design tokens, Pierre Trees and Diffs, and accessible interactions |

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

Build the frontend before Rust because `build.rs` embeds `packages/repository/dist/index.html` and rejects symlinked assets.

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
npm ci --prefix packages/repository
npm run build --prefix packages/repository
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

Create initializes the canonical repository first and then publishes its
catalog record with compare-and-swap. Adopt validates an existing layout and
manifest and never converts arbitrary object prefixes.

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
"$SERVER" --config /secure/server.toml serve
```

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

Without OIDC, the server accepts loopback listeners only. This mode trusts one
local operator and exposes every cataloged repository to that principal.

The private management listener owns `GET /healthz` and `GET /readyz`. The
public listener does not expose probes. Use `healthcheck` to call readiness on
the configured management address.

## Run the container

The checked-in image builds the locked React application and Rust server from digest-pinned bases. The runtime installs no packages, runs as UID/GID 10001, embeds the frontend, and uses a dedicated temporary directory.

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

- `server.toml`: listeners, storage root, and OIDC configuration
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
same mounts before or after starting the service. Catalog changes are discovered
without restarting the long-running container.

The binary's `healthcheck` command calls `/readyz` on the management listener.
`SIGTERM` and Ctrl-C start the same graceful drain. Repository, catalog,
identity, and application state remain in object storage; `/var/lib/crab/tmp`
contains only bounded transient files.

### Probe liveness and readiness

The two probe routes answer different operator questions:

| Route | Success means | Failure contract |
| --- | --- | --- |
| `GET /healthz` | The HTTP process can answer | It does not inspect repository storage |
| `GET /readyz` | The durable catalog can be read and validated within 10 seconds | HTTP 503 with `Retry-After: 5` |

Only the management listener serves probes. Every public request retains strict
canonical `Host` validation.

### Choose an orchestrator

Use the portable Helm chart for EKS, GKE, or AKS and the Fargate task profile
for ECS. Both run at least two replicas, expose only the public port, pin an
image digest, drop Linux capabilities, use a read-only root filesystem, and
mount bounded disposable scratch space.

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

See `deploy/README.md`, `deploy/helm/crab-http-server/README.md`, and
`deploy/ecs/README.md`. Lambda is intentionally excluded from the full data
plane because Git and LFS require long streaming requests, large bodies, and
bounded scratch that do not preserve the same contract through Lambda/API
Gateway buffering and limits.

## Repository browser and application APIs

The browser and JavaScript Object Notation (JSON) API share the same authorization, snapshot, publication, and storage contracts. Browser links keep a selected branch for navigation and pin historical pages to a full commit OID.
The blame view links commit OIDs and subjects to immutable commit details. Its attribution and source panes are separated by a pointer- and keyboard-resizable handle; arrow keys resize incrementally, Home and End select the supported bounds, and double-click restores the default.

### Scan the browser feature map

| View | Capabilities |
| --- | --- |
| Repository root | Selected commit, file table, repository details, rendered README, request timing, and Code menu |
| Branch and tag picker | Search, keyboard navigation, default-branch state, and writer-only creation from the viewed commit |
| Branches and tags | Natural sorting, protected/default labels, copied names, immutable tips, default comparison, and guarded deletion |
| Tree and file workspace | Lazy folder-first tree, preserved expansion, breadcrumbs, source, preview, blame, raw bytes, copy, download, edit, and delete |
| History and comparison | Signed pagination, first-parent path history, commit changes, split/unified diffs, and exact revision links |
| Go to file | Bounded full-tree fuzzy search without blob reads; `T` opens and focuses search |
| Releases | Releases/Tags navigation, search, drafts, publication, edits, deletion, source ZIPs, and binary assets |
| Pull requests | Conversation, Commits, Checks, Files changed, reviews, approval state, and merge controls |
| Issues and metadata | Searchable issues, comments, labels, assignees, Markdown preview, and conflict recovery |
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

Root commits compare against an empty tree. Path history follows the exact first-parent path. Commit history with `base` instead returns commits reachable from `rev` but not from `base`, across every parent.

### Render repository content safely

Markdown README files render beneath directory listings. Markdown file views switch between source, preview, and blame.

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

`Server-Timing` separates repository open, read, application, and total handling time where applicable. It excludes HTTP response transmission. The browser measures its complete fetch and JSON round trip separately.

## Team sign-in

Team deployments use an OIDC authorization-code client with Proof Key for Code Exchange (PKCE) and the S256 challenge method. The server discovers provider metadata and signing keys; it stores no passwords and never uses email as the authorization identifier.

### Configure the identity provider

Register this redirect URI with the provider:

```text
https://git.example.com/auth/callback
```

Use the provider's exact issuer string, including its path and trailing-slash policy.

```toml
listen = "0.0.0.0:8788"
management_listen = "0.0.0.0:8789"

[storage]
url = "s3://your-bucket/repositories"

[auth]
issuer = "https://identity.example.com/realms/team"
client_id = "crab-browser"
public_url = "https://git.example.com"
client_secret_file = "/run/secrets/crab-oidc-client-secret"
state_key_file = "/run/secrets/crab-state-key"
```

Omit `client_secret_file` for a public PKCE client. A secret file can end with one newline; other whitespace remains part of the secret.

Terminate Transport Layer Security (TLS) at a reverse proxy and forward the original canonical `Host` to the private loopback listener. Forwarded headers cannot replace the configured origin.

HTTP identity endpoints are allowed only when the issuer, public URL, and listener are loopback addresses. Production identity endpoints require HTTPS.

### Define repository membership

Each catalog member record binds the provider's stable `sub` claim to a display
name and explicit grant. Supply records through `--members-file` when creating
or adopting a repository:

| Access | Capabilities |
| --- | --- |
| `read` | Browse, fetch, download LFS, and participate in issues, pull requests, comments, and reviews |
| `write` | All member actions plus Git writes, LFS upload, merge, releases, labels, assignees, statuses, checks, and scoped write-token issuance |
| `admin` | All write actions plus repository settings and branch protections |

Subjects can contain at most 512 characters. Names can contain at most 160 characters. Subjects and case-insensitive names must be unique within a repository.

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

Session cookies use `HttpOnly` and `SameSite=Lax`. HTTPS deployments also use `Secure` and the `__Host-` prefix. Sessions expire at the earlier of ID-token expiry or eight hours. The server does not retain refresh tokens.

Restarting or replacing a replica preserves sessions and Git tokens. Logout
requires the canonical `Origin` and the session's cross-site request forgery
(CSRF) token. It removes the durable session, invalidating subsequent browser
and Git-token authentication on every replica, but does not sign the account
out of the identity provider. Configure an object-store lifecycle rule that
deletes objects below `.crab/http-server/v1/auth/` after 24 hours. Active state
expires within eight hours; the rule only collects consumed flows, expired
sessions, and token records left after session invalidation.

Provider-side account revocation does not invalidate an issued Crab session
before expiry. Back-channel logout remains unimplemented.

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

Uploads stream to a private temporary file, then use `crab-lfs` for verified bounded-memory multipart publication. Downloads verify size and SHA-256 before opening a backpressured response.

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

Downloads restart from byte zero; range resume is not implemented. The optional LFS locking API returns HTTP 501. Browser blob downloads continue to return exact pointer bytes.

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
| Request body and prepared pack | 2 GiB |
| Cooperative receive budget | 5 minutes |
| Incoming objects | 1,000,000 |
| One Git object | 64 MiB |
| Total inflation | 8 GiB |
| Delta depth | 128 |
| Ref commands | 1,024 |
| Graph traversal steps | 1,000,000 |
| Dependency pointers | 1,024 |
| One dependency file | 512 MiB |
| Aggregate dependency content | 2 GiB |
| Publication lease lifetime | 5 minutes, renewed while owned work drains |

Temporary disk use can exceed the wire limit because quarantine, normalized pack output, and index sidecars can overlap.

### Handle rejection and uncertain outcomes

The server rejects the whole atomic batch for stale tips, namespace collisions, malformed commands, corrupt packs, missing bases, invalid graphs, missing pointer content, protected refs, or policy violations.

The current symbolic HEAD cannot be deleted. Git reports `deletion is prohibited`. Protected names report `protected branch requires a pull request`.

A disconnect or deadline signals cancellation. Owned workers retain transfer admission and renew GC fences until cleanup finishes. Cancellation before the active marker leaves refs unchanged.

After a marker attempt, the transaction can be committed even when the client loses the response. An inconclusive outcome returns HTTP 503 without inventing a per-ref rejection. Inspect remote refs before retrying. Matching current refs alone do not prove which historical transaction committed.

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

Application collaboration data uses versioned JSON documents under the repository prefix. It does not mutate Git refs unless a pull request merge publishes a ref update.

### Use the collaboration route families

| Surface | Routes |
| --- | --- |
| Issues | `GET/POST /api/repos/{owner}/{name}/issues`, `GET/PATCH /api/repos/{owner}/{name}/issues/{number}` |
| Issue comments | `GET/POST /api/repos/{owner}/{name}/issues/{number}/comments`, `GET/PATCH /api/repos/{owner}/{name}/issues/{number}/comments/{comment}` |
| Pull requests | `GET/POST /api/repos/{owner}/{name}/pulls`, `GET/PATCH /api/repos/{owner}/{name}/pulls/{number}` |
| Pull comments | `GET/POST /api/repos/{owner}/{name}/pulls/{number}/comments`, `GET/PATCH /api/repos/{owner}/{name}/pulls/{number}/comments/{comment}` |
| Reviews | `GET/POST /api/repos/{owner}/{name}/pulls/{number}/reviews`, `GET/PATCH /api/repos/{owner}/{name}/pulls/{number}/reviews/{review}` |
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

Issue, pull, comment, and review lists default to 30 items and accept 1 to 50. Each page scans at most 200 allocated numbers. A filtered page can be empty and still return `next`; clients must follow that cursor.

Titles accept 1 to 256 characters. Markdown bodies accept 64 KiB. Collaboration requests use an 80 KiB body limit, eight concurrent application slots, and a 30-second handler deadline.

Data uses these versioned roots:

| Root | Content |
| --- | --- |
| `app/v1/issues` | Issues, comments, counters, and request reservations |
| `app/v1/pulls` | Pulls, comments, reviews, merge state, counters, and reservations |
| `app/v1/labels` | Label catalog, claims, reservations, and tombstones |
| `app/v1/releases` | Releases, tags, assets, reservations, and tombstones |
| `app/v1/statuses` | Commit statuses and immutable requests |
| `app/v1/check-runs` | Check catalogs, versioned output, and requests |
| `app/v1/settings` | Branch protection and repository lifecycle records |

Every JSON document uses `schema_version: 1`; unknown versions fail closed. Preserve the complete `app/v1` tree in backups. Restoring visible documents without counters, claims, and request reservations loses numbering and retry guarantees.

Discussion deletion, moderation, edit history, activity feeds, and notifications remain unimplemented. Backup and restore qualification remains pending.

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
| Interactive repository reads | 16 concurrent, 2 minutes, 8 MiB response | `server.rs` and `crab-remote-git` |
| Collaboration handlers | 8 concurrent, 30s | `app.rs` and route middleware |
| Git fetch, push, and LFS transfers | 4 concurrent | Shared `git_admission` semaphore |
| Read-readiness publication | 2 repositories concurrently, 3-minute cooperative budget | `maintenance.rs` |
| OIDC callbacks | 8 concurrent, 10s per provider request | `auth.rs` |
| Archive transfer | 10 minutes, 3 GiB encoded response | `archive.rs` |
| Git receive | 5-minute cooperative budget, 2 GiB body | `receive.rs` |
| Browser existing-object ref update | 30s | `receive.rs` |
| LFS transfer | 5 minutes, 512 MiB object | `lfs.rs` and `crab-lfs` |

Handler completion and response-body completion are different lifecycle events. Archive, release-asset, Git, and LFS streams retain their own permits and cancellation guards until the body finishes or fails.

Server shutdown follows this order:

1. Stop accepting new work after Ctrl-C or `SIGTERM`
2. Cancel request-scoped work
3. Drain Axum connections
4. Close and drain receive workers
5. Drain retained maintenance jobs and lease cleanup
6. Shut down the shared remote-read runtime

Stateful publication futures are drained instead of aborted. This prevents a dropped handler from abandoning a marker attempt or releasing a GC fence before cleanup finishes.

## Current verification

Run focused checks from the repository root. Use a target directory unique to this checkout:

```sh
npm test --prefix packages/repository
npm run typecheck --prefix packages/repository
npm run format:check --prefix packages/repository
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

### Read the executable evidence map

| Contract | Primary source | Executable evidence |
| --- | --- | --- |
| Route composition, Host checks, readiness, and shutdown | `src/server.rs` | Server, authentication, and maintenance tests |
| OIDC, membership, sessions, tokens, and CSRF | `src/auth.rs` | `src/auth_tests.rs` and `src/auth_tests/git_tokens.rs` |
| Repository reads and raw paths | `src/api.rs` | `tests/verify_live.py` and frontend navigation tests |
| Git protocol version 2 fetch | `src/git.rs` | `tests/verify_git_transport.py` and protocol CI |
| Native receive and recovery | `src/receive.rs` | `src/receive_tests.rs` and `src/receive_fault_tests.rs` |
| LFS upload and download integrity | `src/lfs.rs` | `src/lfs_tests.rs` |
| Browser Git writes and settings | `src/contents.rs`, `src/branches.rs` | `src/auth_tests/branches.rs` |
| Issues, labels, and assignees | `src/issues.rs`, `src/labels.rs`, `src/assignees.rs` | Scoped authenticated tests |
| Pulls, reviews, checks, and merge | `src/pulls/`, `src/statuses.rs`, `src/checks.rs` | `src/pulls_tests.rs` and `src/auth_tests/pulls.rs` |
| Releases and assets | `src/releases.rs` | `src/auth_tests/releases.rs` |
| Container identity and health contract | `deploy/Dockerfile` | `.github/workflows/http-server-container.yml` |

### Understand what has been qualified

Current local and CI evidence includes:

- Kubernetes-scale protocol version 2 discovery, partial clone, deepening, large request batches, path search, history, diff, and deep blame
- Exact commit, tree, blob, archive, LFS, branch, tag, release, pull, merge, status, and check data compared with independent Git clients
- Native initial pushes, fast-forward updates, branch and tag lifecycle, atomic rejection, fault injection, response loss, and cooperative restart recovery
- OIDC redirects and signed-token validation, key rotation, membership isolation, token scope, revocation, Origin checks, and CSRF rejection
- Browser light, dark, desktop, narrow-screen, keyboard, conflict, and automated Web Content Accessibility Guidelines (WCAG) A/AA checks
- Container build, non-root identity, stop signal, health command, and runtime inspection

These runs use local RustFS, in-memory stores, shared caches, and controlled fixtures. Recorded timings are diagnostic observations, not throughput or production latency guarantees. The tests do not establish abrupt process-crash safety, multi-instance global admission, provider-scale performance, backup recovery, or complete manual accessibility.

### Keep qualification evidence honest

Use the following interpretation:

| Evidence | What it proves | What it does not prove |
| --- | --- | --- |
| Unit test | Local contract and rejection behavior | HTTP composition or real storage |
| In-memory HTTP test | Routing, authorization, and response semantics | Provider behavior or restart durability |
| RustFS integration | Real object-store persistence and independent-client result | Production load, region failure, or Internet latency |
| Browser regression | Rendered behavior and automated accessibility rules | Storage durability or manual assistive technology |
| Container CI | Reproducible image and runtime metadata | Production orchestration or upgrade safety |

## Completion requirements

The server is complete only when a real account can perform the workflow and observe its durable result. A green component test or visible placeholder does not satisfy that bar.

| Surface | Required evidence | Status |
| --- | --- | --- |
| Multi-replica deployment | One Rust binary, durable CAS catalog and identity state, private management probes, graceful drain, Helm, ECS profile, and reproducible container | Implemented; live rollout qualification remains |
| Repository browsing | Refs, byte-preserving paths, history, files, blame, downloads, freshness, and empty/error states against real repositories | In progress |
| Diff and tree interface | Pierre Trees and Diffs, correct modes and binary handling, bounded large-repository behavior, and keyboard navigation | In progress |
| GitHub-quality design | Themes, responsive layouts, accessible controls, navigation, and loading/error behavior across workflows | In progress |
| Team identity and authorization | OIDC, sessions, membership, permissions, isolation, revocation, CSRF, and administration | In progress; membership administration and provider revocation remain |
| Git hosting | Authenticated fetch and push, exact branch/tag lifecycle, protection, publication, and independent-client proof | In progress; crash and coexistence qualification remain |
| Collaboration | Durable issues, pulls, comments, reviews, labels, assignees, merge, checks, activity, and notifications | In progress; activity, moderation, history, and notifications remain |
| Repository management | CLI create/adopt/list, archive, settings, search, import, and audited administration | In progress; browser creation/import and audit history remain |
| Production operation | Durable concurrency, restart and crash recovery, backup restore, observability, upgrades, and operator guidance | Static EKS/GKE/AKS/ECS profiles implemented; live qualification pending |
| Quality gates | API, UI, accessibility, realistic repositories, security, package smoke, and measured performance | In progress |

### Track known operational gaps

The remaining production gaps include:

- Durable application-level push receipts and full abrupt-crash recovery
- Index receipts and restart reconstruction when verified visibility evidence is missing
- Protected-view writer coexistence with shared namespace guarantees
- Multi-instance global admission and production throughput qualification
- LFS locking and resumed range downloads
- Membership administration, provider back-channel logout, and immediate provider revocation
- Repository creation and adoption exist in the CLI; browser import remains
- Backup and restore qualification for Git and the complete `app/v1` namespace
- Manual assistive-technology audits and broader workflow coverage
- Production observability, upgrade, rollback, and disaster-recovery procedures

## Ownership

Object storage is the repository authority. Runtime caches and temporary files are disposable.

New collaboration data needs a versioned storage and concurrency contract under the repository prefix. It must not reuse Git object, metadata, or lock namespaces.

The service account needs object reads plus conditional writes and deletes for:

- Repository metadata, manifests, refs, packs, visibility, and locator state
- Ref-journal transactions, prepared heads, and cleanup
- Per-ref, namespace, generation-owner, and GC coordination keys
- The complete `app/v1` application namespace
- LFS objects and multipart lifecycle
- The server catalog and shared OIDC/session/Git-token namespace

Preserve source errors across crate boundaries. Map them at the HTTP boundary only when the status code or client action changes.

## Integration sources

The interface follows these upstream contracts:

- [Git pack protocol](https://git-scm.com/docs/pack-protocol)
- [Git credential contexts](https://git-scm.com/docs/gitcredentials#_configuration_options)
- [Git LFS extensions](https://github.com/git-lfs/git-lfs/blob/main/docs/extensions.md)
- [Pierre Diffs documentation](https://diffs.com/docs)
- [Pierre Trees documentation](https://trees.software/docs)
- [Primer React guidance](https://primer.style/product/getting-started/react/)
- [GitHub default-branch administration](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-branches-in-your-repository/changing-the-default-branch)
- [GitHub classic branch protection](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/managing-a-branch-protection-rule)
- [GitHub repository archive behavior](https://docs.github.com/en/repositories/archiving-a-github-repository/archiving-repositories)

The OIDC dependency includes `rsa` for public-key signature verification. [RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071.html) concerns private-key timing leakage. This relying party holds no RSA signing keys and decrypts no RSA ciphertext. The test issuer uses Ed25519 only in the test harness. No advisory suppression or dependency override is present; a complete dependency audit remains part of production qualification.
