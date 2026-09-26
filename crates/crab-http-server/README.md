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
              ┌───┴────────────────┐
              ▼                    ▼
     repository Cell router   remote-git/read/write
              │                    │
        SQLite + crab-ltx           │
              └─────────┬──────────┘
                        ▼
                   object storage
```

The server owns HTTP policy and application workflows. Shared crates own Git
reading, validation, publication, Cell execution, and storage mechanics. The
owner-resolving peer HTTP client and pinned mTLS listener live in
`crab-cell-peer-http`; this server supplies TLS configuration and its private
authenticated receiver. Issues,
comments, Labels, commit statuses, check runs, branch protections, repository
lifecycle, Pull requests, reviews, and Release metadata use typed Rust commands
against one SQLite/LTX Cell per repository. The remote-owner path is verified
from the public HTTP listener through the private mTLS listener to an advanced
LTX root. No serving route reads or writes the retired collaboration object
trees. Normal serving paths create no Git checkout or local Git object database;
the explicit browser Git import workflow runs a time-bounded
`git clone --mirror` in Cell-managed staging and pushes the result through the
native receive-pack path. Sources are limited to the operator's configured Git
host allowlist.

Authenticated repository administrators manage the complete member list in
**Settings → Members** or through the conditional CLI replacement command.
Both paths commit the same catalog revision and durable membership audit event.
OIDC providers can also revoke an identity's Crab browser sessions and derived
Git tokens through the documented back-channel logout endpoint. GitHub OAuth
uses the same server-side browser session contract but does not provide signed
OIDC logout tokens. The exact API, catalog migration, and rollout contracts are in
[Team sign-in](REFERENCE.md#team-sign-in).

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
npm ci --prefix packages/ui
npm run build --prefix packages/ui
cargo build -p crab-http-server --release --locked
```

Create a configuration using the
[development example](REFERENCE.md#run-the-current-development-build). Select
one `s3://`, `gs://`, or `az://` storage root and configure ambient workload
credentials. Then create a cataloged repository and start:

```sh
"$CARGO_TARGET_DIR/release/crab-http-server" --config /path/to/server.toml \
  cells release bootstrap --image sha256:IMAGE_MANIFEST_DIGEST
"$CARGO_TARGET_DIR/release/crab-http-server" --config /path/to/server.toml \
  repository create --owner team --name project --prefix team/project
"$CARGO_TARGET_DIR/release/crab-http-server" --config /path/to/server.toml \
  storage-probe
"$CARGO_TARGET_DIR/release/crab-http-server" --config /path/to/server.toml \
  cells capacity --json
"$CARGO_TARGET_DIR/release/crab-http-server" --config /path/to/server.toml \
  cells backup create --pin 11112222333344445555666677778888
"$CARGO_TARGET_DIR/release/crab-http-server" --config /path/to/server.toml \
  cells backup verify --pin 11112222333344445555666677778888
"$CARGO_TARGET_DIR/release/crab-http-server" --config /path/to/server.toml \
  cells backup restore --pin 11112222333344445555666677778888 \
  --destination-prefix recovery/team-2026-09-16
"$CARGO_TARGET_DIR/release/crab-http-server" --config /path/to/server.toml \
  cells release activate --expected-revision 8 --strategy maintenance \
  --retention-grace-hours 168 --retention-max-deletes 10000
"$CARGO_TARGET_DIR/release/crab-http-server" --config /path/to/server.toml serve
```

`cells release bootstrap` converges concurrent first-install callers on one
exact descriptor/image and resumes only the activation operation it created.
For an operator-prepared upgrade it validates and admits the candidate without
completing activation; it refuses to replace another desired release. `serve`
rejects startup before binding either listener unless the compiled descriptor is
the selected ready release or the selected compatible rollout candidate.
`repository create` also publishes the repository's initial SQLite/LTX Cell and
does not return until its catalog state is ready. `repository adopt` records an
existing canonical Git repository, publishes a new empty application Cell and
does not return until that Cell is ready. It does not preserve old collaboration
application data.
The server refuses startup for pending, missing or rootless repository Cells.
`storage-probe` fails unless the workload can read and list the configured
root, perform conditional coordination writes, create and delete an object,
and observe that deletion. `serve` runs the same preflight before binding its
listeners. `cells capacity --json` reports the resource-derived Cell admission
envelope without claiming that the node has met a throughput target.

`cells backup create` snapshots all catalog heads, requires one exact control
per catalog entry, verifies the selected release and every reachable LTX
dependency, and strict-creates the pin pointer last. Reusing a pin ID verifies
and returns the existing boundary. Creation advertises a signed zero-capacity
worker for its complete operation, so maintenance either waits for an in-flight
pin or fences it before publication. `cells backup verify` independently reopens
the pin and fails closed on a missing or corrupt dependency. `cells backup
restore` conditionally copies the verified graph to a canonical isolated prefix
in the configured bucket, publishes unowned `Idle` controls, then publishes the
catalog, release, and pin commit points. Repeating an offline restore adopts
only exact existing state; divergent destination state fails closed.

Maintenance retention is opt-in. Supplying `--retention-grace-hours` verifies
current controls and every backup pin before streaming the Cell application
prefix and deleting recognized V1 immutable objects older than the grace.
`--retention-max-deletes` accepts 1 through 100,000 and defaults to 10,000.
Reaching the bound leaves the release in `Maintenance`; repeat the same
activation until it returns `Ready`.

The bucket or container must already exist. `repository adopt` can publish an
existing canonical repository; it does not convert arbitrary objects into a
Crab repository. Every replica discovers catalog changes without a restart.
Without authentication, the server accepts only loopback listeners and trusts
the local operator. Team deployments need [OIDC or GitHub OAuth configuration](REFERENCE.md#team-sign-in)
and a canonical HTTPS origin. For container deployment, use the
[deployment profiles](deploy/README.md),
[container instructions](REFERENCE.md#run-the-container), and
[example configuration](deploy/server.example.toml).

## Find the relevant contract

For the proposed SQLite/LTX storage and multi-node ownership architecture, read
[the next-generation design index](next-architecture/README.md). Focused topics under
`next-architecture/` cover [ownership and load balancing](next-architecture/ownership-and-load-balancing.md),
[SQLite/LTX publication](next-architecture/storage-protocol.md),
[Kubernetes operation](next-architecture/deployment-and-operations.md), and the
[hard cutover](next-architecture/hard-cutover.md). These describe intended
behavior and acceptance gates separately from the current implementation.

| Task | Read |
| --- | --- |
| Configure catalog, storage root, temporary space, and credentials | [Development setup](REFERENCE.md#run-the-current-development-build) |
| Start locally with Docker Compose | [Local Compose stack](deploy/README.md#start-locally-with-docker-compose) |
| Simulate 3 → 5 → 10 → 20 one-GiB Cell service nodes | [Compose KV reference service](examples/compose_kv_service/README.md) |
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

Pull request Files changed views support durable inline review threads in
addition to the existing summary review. Members can anchor a comment to an
old or new line range, reply and edit their own messages, resolve or reopen a
conversation, and keep exact-commit conversations visible when a later push
makes them outdated. New-side suggestions use the existing exact-head
`/contents` mutation, so applying one creates the normal Git commit and keeps
branch/blob conflicts authoritative.

## Trace issue creation

Use `POST /api/repos/{owner}/{name}/issues` to follow a collaboration request.
Read these sources in order:

| Source | Responsibility |
| --- | --- |
| [server.rs](src/server.rs) | Route composition, host/session checks, and mutation protection |
| [issues.rs](src/issues.rs) | Route body limit, request extraction, HTTP contract, and typed command/query invocation |
| [app.rs](src/app.rs) | Admission timeout, repository access, input validation, and HTTP error mapping |
| [cells/router.rs](src/cells/router.rs) | Published-root validation plus local-owner restore or authenticated peer dispatch |
| [cells/repository.rs](src/cells/repository.rs) | SQLite transaction, submission reservation, number allocation, and typed outcome |
| [crab-ltx](../crab-ltx/README.md) | Verified immutable publication and exact source-loss restore |

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

The Cell command commits and publishes before the handler loads label and
assignee presentation data. A lost or failed HTTP response therefore does not
prove that the write failed. Retry with the same submission ID and payload; a
new ID represents a new mutation. Unknown Cell outcomes remain distinguishable
from unavailable routing and contract failures in the HTTP error mapping.

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
[`packages/ui`](../../packages/ui).

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
