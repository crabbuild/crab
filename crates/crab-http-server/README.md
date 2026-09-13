# crab-http-server

Crab's repository HTTP application: one Rust server embeds the React UI and
serves repositories from the operator's object storage.

**Development status:** production qualification is incomplete. Read the
[completion matrix](REFERENCE.md#completion-requirements) before deployment.

## Architecture

```text
Browser                    Native Git / Git LFS
   │                              │
   └──────────────┬───────────────┘
                  ▼
          crab-http-server
       auth · routes · lifecycle
                  │
       remote-git / read / write
                  │
             object storage
```

The server owns HTTP policy and application workflows. Shared crates own Git
reading, validation, publication, and storage mechanics. The server uses
writable temporary space for pack/index preparation; it creates no Git checkout
or local Git object database.

## Build and run

For the fastest local start, use Docker Engine with Compose v2:

```sh
docker compose --file crates/crab-http-server/deploy/compose.yaml \
  up --detach --build --wait
```

Then open <http://127.0.0.1:8788/demo/hello>. The stack creates a persistent
local object store and demo repository automatically. See the
[Compose operations guide](deploy/README.md#start-locally-with-docker-compose)
for repository commands, configuration overrides, and safe teardown.

To build outside Docker, run from the repository root. You need Node 22.12+,
Rust, an existing bucket, and storage credentials. Build the frontend first:
`build.rs` embeds its output.

Choose a unique Cargo target directory for this checkout on the mounted workspace
volume, following root `AGENTS.md`. This example uses `/Volumes/Workspace`:

```sh
export CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-http-server-dev
npm ci --prefix packages/repository
npm run build --prefix packages/repository
cargo build -p crab-http-server --release --locked
```

Create a configuration using the
[development example](REFERENCE.md#run-the-current-development-build). Select
one `s3://`, `gs://`, or `az://` storage root and configure ambient workload
credentials. Then create a cataloged repository and start:

```sh
"$CARGO_TARGET_DIR/release/crab-http-server" --config /path/to/server.toml \
  repository create --owner team --name project --prefix team/project
"$CARGO_TARGET_DIR/release/crab-http-server" --config /path/to/server.toml \
  storage-probe
"$CARGO_TARGET_DIR/release/crab-http-server" --config /path/to/server.toml serve
```

`storage-probe` fails unless the workload can read and list the configured
root, perform conditional coordination writes, create and delete an object,
and observe that deletion. `serve` runs the same preflight before binding its
listeners.

The bucket or container must already exist. `repository adopt` can publish an
existing canonical repository; it does not convert arbitrary objects into a
Crab repository. Every replica discovers catalog changes without a restart.
Without authentication, the server accepts only loopback listeners and trusts
the local operator. Team deployments need [OIDC configuration](REFERENCE.md#team-sign-in)
and a canonical HTTPS origin. For container deployment, use the
[deployment profiles](deploy/README.md),
[container instructions](REFERENCE.md#run-the-container), and
[example configuration](deploy/server.example.toml).

## Find the relevant contract

| Task | Read |
| --- | --- |
| Configure catalog, storage root, temporary space, and credentials | [Development setup](REFERENCE.md#run-the-current-development-build) |
| Start locally with Docker Compose | [Local Compose stack](deploy/README.md#start-locally-with-docker-compose) |
| Deploy on EKS, GKE, AKS, or ECS | [Deployment profiles](deploy/README.md) |
| Probe readiness and drain the service | [Container operation](REFERENCE.md#run-the-container) |
| Scrape metrics and define alerts | [Operations runbook](deploy/operations.md#observe-requests-and-capacity) |
| Understand browser APIs and publication ownership | [Application behavior](REFERENCE.md#repository-browser-and-application-apis) |
| Configure identity, membership, sessions, and protection | [Team sign-in](REFERENCE.md#team-sign-in) |
| Clone and fetch with scoped Git tokens | [Git HTTP reads](REFERENCE.md#git-http-reads) |
| Transfer LFS objects | [Git LFS](REFERENCE.md#git-lfs-transfers) |
| Push, handle uncertain publication, and inspect limits | [Native push](REFERENCE.md#native-git-push), [write design](DESIGN.md) |
| Work on issues, pull requests, reviews, or merges | [Collaboration](REFERENCE.md#issues-pull-requests-and-reviews) |
| Report CI results or enforce merge checks | [Statuses and checks](REFERENCE.md#commit-statuses-and-required-checks) |

## Trace issue creation

Use `POST /api/repos/{owner}/{name}/issues` to follow a collaboration request.
Read these sources in order:

| Source | Responsibility |
| --- | --- |
| [server.rs](src/server.rs) | Route composition, host/session checks, and mutation protection |
| [issues.rs](src/issues.rs) | Route body limit, request extraction, and issue handler |
| [app.rs](src/app.rs) | Admission timeout, repository access, input validation, and HTTP error mapping |
| [issues/storage.rs](src/issues/storage.rs) | Submission reservation and visible issue creation |
| [app_storage.rs](src/app_storage.rs) | Storage reads, conditional creation, and number allocation |

Example JSON body, subject to the server's authentication and mutation checks:

```json
{
  "request_id": "01931b9e-4b3c-7b2a-b9f0-0123456789ab",
  "title": "Document the repository setup",
  "body": "Include prerequisites and a minimal configuration."
}
```

Generate a fresh submission ID for a new issue. If the response is lost, retry
with the same ID and original title/body: the reservation preserves the issue
number. Reusing that ID with different content or another author is a conflict.

The handler also loads presentation data after issue creation. A failed response
therefore does not prove that the write failed; keep retry behavior aligned with
the storage reservation contract. JSON extractor rejections retain their HTTP
status through `app::Error`, while internal storage failures use a separate
response classification.

## Admission ownership

A handler returning a response and a client finishing its response body are
different lifecycle events:

| Surface | Ownership |
| --- | --- |
| Collaboration middleware in [app.rs](src/app.rs) | Holds an application slot while producing the response; applies a 30-second handler timeout |
| Release download in [releases.rs](src/releases.rs) | Moves a separate transfer permit and cancellation guard into the response stream |

Keep body-stream resources with the stream. The middleware timeout cannot stand
in for transfer deadlines or worker cleanup after a response has been returned.
The production server initializes eight process-local application slots and
four deployment-wide Git/LFS/archive/release transfer slots in
[server.rs](src/server.rs); test fixtures use smaller limits.

Archive downloads in [archive.rs](src/archive.rs) use a channel-backed ZIP body.
Traversal cancellation must fail that body: finalizing ZIP state for cleanup
must not turn an incomplete archive into a successful download.

## Work on this crate

Start with [AGENTS.md](AGENTS.md) for entry points, ownership, and invariants.
The main source path is `src/main.rs` → `src/server.rs` → request handlers;
publication continues into `crab-write`. The frontend lives in
[`packages/repository`](../../packages/repository).

Build the frontend before Rust checks. Focused HTTP authentication coverage:

```sh
cargo test -p crab-http-server --locked --lib server::auth_tests
cargo clippy -p crab-http-server --locked --all-targets -- -D warnings
```

Keep `CARGO_TARGET_DIR` set to the checkout's external directory for each shell
invocation. Broader protocol, browser, live-storage, and container qualification
belongs in CI or a dedicated test environment. See the
[verification commands and evidence](REFERENCE.md#current-verification);
component tests alone do not establish production readiness.
