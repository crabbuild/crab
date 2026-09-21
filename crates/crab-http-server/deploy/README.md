# Deployment profiles

`crab-http-server` has one provider-neutral runtime contract, a one-command local stack, and checked-in deployment profiles for three cloud storage providers:

```mermaid
flowchart LR
    Client[Browser / Git / LFS] --> Edge[TLS load balancer]
    Edge --> Replicas[3+ server replicas]
    Replicas --> Catalog[Durable CAS catalog]
    Replicas --> State[OIDC sessions and Git tokens]
    Replicas --> Repos[Crab repositories]
    Catalog & State & Repos --> Root[(One object-storage root)]
```

| Target | Deployment asset | Workload identity | Status |
| --- | --- | --- | --- |
| Local Docker | `compose.yaml` | Synthetic local credentials | Qualified in container CI |
| EKS | `helm/crab-http-server` | EKS Pod Identity association | Recommended team profile; live qualification required |
| GKE | `helm/crab-http-server` | GKE Workload Identity Federation | Recommended team profile; live qualification required |
| AKS | `helm/crab-http-server` | AKS Workload ID | Recommended team profile; live qualification required |
| ECS/Fargate | `ecs/task-definition.example.json` | ECS task role | Evaluation profile; replacement grace is too short |

The Kubernetes chart generates configuration from typed values, runs three or more replicas,
private probes and Prometheus metrics, an optional Prometheus Operator
`PodMonitor` and alert rules, a disruption budget, ingress isolation, optional
Transport Layer Security (TLS) ingress, and optional autoscaling. A provider is
release-qualified only after its live test matrix passes.

The disruption budget protects ready replicas while allowing an unhealthy pod
to be evicted during a node drain, so a broken process cannot indefinitely
block cluster maintenance.

S3 and GCS Terraform roots bound noncurrent repository versions to a configurable
90-day recovery window and remove abandoned multipart uploads after one day.
Azure versions remain unexpired because its available lifecycle condition cannot
measure age since becoming noncurrent safely. Provider cost alerts and the
restore runbook remain operator responsibilities.

Container CI stops the local writers, copies the complete storage root to an
isolated object prefix, compares every key and object body, then verifies the
restored catalog through native Git, issue, and LFS clients. This proves the
portable recovery shape, but provider version selection and regional recovery
still require a live drill.

The same container path uploads a 1 MiB LFS object, interrupts its logical
download after an initial byte range, resumes the remaining range through
Caddy, and requires the reconstructed file to match byte-for-byte.
It also drives the stock Git LFS client through lock creation, listing,
verify-on-push, and unlock against durable RustFS lock records.

The server image also includes the system `git` executable for the browser's
Git import workflow. That workflow copies Git history and refs only; it does
not include Git LFS payload migration. Configure `[import].allowed_hosts` and
allow the server workload outbound HTTPS/SSH to those hosts (or through the
configured proxy) when this workflow is enabled.

Server release tags have their own contract, independent of the Crab CLI. An
annotated `crab-http-server-vX.Y.Z` tag matching the server crate publishes a
qualified AMD64/ARM64 image to GHCR with immutable version and source-commit
tags, an OCI Helm chart, an SBOM, and provenance attestations. Exact-source
qualification rejects fixable HIGH or CRITICAL image vulnerabilities before
publication. Kubernetes deployments still pin the resulting image manifest
digest. A GitHub Release retains the packaged chart and a signed JSON deployment
record that binds the tag and source commit to both registry digests and the
chart package checksum. The publisher never overwrites an existing image tag
or chart version.

## Start locally with Docker Compose

Docker Engine with Compose v2 is the only prerequisite. From the repository
root, run:

```sh
docker compose --file crates/crab-http-server/deploy/compose.yaml \
  up --detach --build --wait
```

Open <http://127.0.0.1:8788/demo/hello>. The first start builds the locked
server image, starts a private RustFS object store, creates its bucket, creates
the `demo/hello` Crab repository, and waits until the complete stack is ready.
Later starts are idempotent.

```mermaid
flowchart LR
    Browser[Browser / Git] -->|127.0.0.1:8788| Proxy[Caddy stream proxy]
    Proxy -->|shared loopback| Server[crab-http-server]
    Server -->|private Compose network| RustFS[(persistent RustFS volume)]
    Server -. readiness .-> Management[127.0.0.1:8789]
```

The proxy shares the server's network namespace. It is the only process bound
to Docker's published port; Crab still binds to loopback and keeps its
unauthenticated local-trust invariant. The management listener and RustFS are
not published to the host. Compose creates the peer CA and leaf once in a
persistent identity volume, so ordinary container recreation remains in the
same Cell fleet. A partial identity volume fails closed instead of silently
creating a different fleet. A one-shot `release-init` service converges concurrent
first-install callers on the exact Cell descriptor and image before repository
initialization or server startup. It resumes only its own bootstrap operation,
admits an operator-prepared candidate without activating it, and never replaces
a different desired release.
Compose then waits until the catalog is valid and every repository can open its
current Git view. This profile is for local development and evaluation, not
remote or multi-user service. Caddy preserves the validated external loopback
authority, so Git LFS action URLs also follow a custom `CRAB_HTTP_SERVER_PORT`.

### Operate the local stack

Use the one-shot repository service to manage the durable catalog:

```sh
docker compose --file crates/crab-http-server/deploy/compose.yaml run --rm \
  repository-init --config /etc/crab/server.toml repository create \
  --owner my-team --name my-project --prefix my-team/my-project

docker compose --file crates/crab-http-server/deploy/compose.yaml run --rm \
  repository-init --config /etc/crab/server.toml repository list
```

Create returns only after the initial repository SQLite/LTX root has been
published, restored, identity-checked and marked `cell_ready`. Adopted
repositories follow the same empty-Cell initialization and readiness transition;
old collaboration application data is not imported.

Inspect or stop the stack without deleting repositories:

```sh
docker compose --file crates/crab-http-server/deploy/compose.yaml exec server \
  crab-http-server --config /etc/crab/server.toml cells capacity --json --live
docker compose --file crates/crab-http-server/deploy/compose.yaml exec server \
  crab-http-server --config /etc/crab/server.toml cells backup create \
  --pin 11112222333344445555666677778888
docker compose --file crates/crab-http-server/deploy/compose.yaml exec server \
  crab-http-server --config /etc/crab/server.toml cells backup verify \
  --pin 11112222333344445555666677778888
docker compose --file crates/crab-http-server/deploy/compose.yaml exec server \
  crab-http-server --config /etc/crab/server.toml cells backup restore \
  --pin 11112222333344445555666677778888 \
  --destination-prefix recovery/compose-restore
docker compose --file crates/crab-http-server/deploy/compose.yaml logs --follow server proxy
docker compose --file crates/crab-http-server/deploy/compose.yaml down
```

The capacity report is the server's resource-derived admission envelope, not a
benchmark result. It reports the configured local-disk limit and current
filesystem free space separately, plus effective capacity as the smaller of
that limit and the backing filesystem total. The configured limit is the
admission ceiling even when the host filesystem is larger. Record it beside
live RSS, file-descriptor, latency and RustFS measurements when qualifying a
node profile.

The backup commands operate on object-store state, not the disposable Cell
tmpfs. Reusing the same nonzero lowercase pin ID is idempotent and re-verifies
the existing release, catalog, controls, and reachable LTX graph. Restore uses
conditional same-bucket copies and publishes the destination release and pin
only after the copied graph verifies. Keep the destination offline during the
operation. Cross-provider export and product data outside `cells/v1` remain
separate operator work.

`docker compose down --volumes` permanently removes the local RustFS and peer
identity volumes, including the catalog and every repository. The next start
therefore creates a new, empty Cell fleet. The defaults need no
`.env` file. These optional environment variables customize local operation:

| Variable | Default | Purpose |
|---|---|---|
| `CRAB_HTTP_SERVER_PORT` | `8788` | Localhost port published by Docker |
| `CRAB_HTTP_SERVER_RELEASE_IMAGE` | `sha256:` plus 64 `1` digits | Synthetic nonzero image identity recorded by local release bootstrap |
| `CRAB_TMP_SIZE` | `2g` | Bounded receive-pack and index scratch space |
| `CRAB_HTTP_SERVER_IMAGE` | `crab-http-server:local` | Server image name or prebuilt image reference |

The dependency images are version- and digest-pinned. `RUSTFS_IMAGE`,
`AWS_CLI_IMAGE`, and `CADDY_IMAGE` exist for controlled mirrors; keep them
pinned when overriding. To use a prebuilt server image, set
`CRAB_HTTP_SERVER_IMAGE` to its manifest digest and add `--no-build` to `up`:

```sh
CRAB_HTTP_SERVER_IMAGE=ghcr.io/crabbuild/crab-http-server@sha256:qualified_digest_here \
  docker compose --file crates/crab-http-server/deploy/compose.yaml \
    up --detach --no-build --wait
```

### Qualify three local Cell processes

The cluster overlay runs four independent server containers and one real
RustFS origin. Each server has its own Cell tmpfs. They share a dedicated,
long-lived Docker network-namespace sidecar so every unauthenticated listener
can remain on loopback without coupling peer restarts to node A; this keeps the
same local-trust boundary as the one-node profile.

```mermaid
flowchart LR
    Client[Qualification client] --> LB[Caddy round-robin :18880]
    LB --> A[Node A\n127.0.0.1:8788]
    LB --> B[Node B\n127.0.0.1:8888]
    LB --> C[Node C\n127.0.0.1:8988]
    LB --> D[Node D\n127.0.0.1:9188]
    A & B & C & D --> Origin[(RustFS)]
    B -. first SIGKILL .-> LostB[Owner B local SQLite removed]
    A -. pause past lease .-> Reenroll[C replaces follower A with B]
    C -. second SIGKILL .-> LostC[Owner C local SQLite removed]
    B -->|verified follower tail| C
    C -->|verified replacement tail| B
    B -. third SIGKILL .-> LostB2[All B log followers unavailable]
    D -->|bounded any-node fallback| Recovered[Exact RustFS root restored]
```

Run the repeatable owner-loss qualification from the repository root:

```sh
crates/crab-http-server/tests/qualify_compose_cluster.sh
```

The script builds the current source by default, creates a uniquely named
Compose project, and then:

1. Records the live admission envelope from all four processes.
2. Sends a durable issue mutation directly to node B and proves A, C, D, and the
   round-robin endpoint route to B over the private mTLS peer protocol.
3. Blocks immutable Cell-object writes, commits through follower fsync, sends
   `SIGKILL` to B, and destroys its local SQLite files.
4. Waits until B's exact signed boot-session advertisement expires.
5. Reads through C and requires a higher epoch, a different session, and the
   same complete LTX root: digest, transaction ID, checksum, and commit sequence.
6. Writes a second issue through C and requires a different digest plus higher
   transaction ID and commit sequence.
7. Restarts B with empty local Cell storage and proves it routes to C.
8. Pauses follower A past its signed lease, writes through object coverage,
   and requires C to recruit B into a higher node-log epoch.
9. Blocks immutable objects again and commits through the replacement follower.
10. Sends `SIGKILL` to C, destroys its local SQLite files, and requires B to
    recover both follower-only commits before serving further reads.
11. Restarts the needed local processes, publishes an object-covered label, and
    records B's exact follower membership before the fallback fault.
12. Stops every original member of B's log, keeps a non-member process live,
    then sends `SIGKILL` to B and requires the non-member to recover the exact
    RustFS root and all labels.

Success prints a JSON receipt containing all three failovers' sessions, epochs
and complete roots, the original member sets, follower replacement evidence,
the non-member fallback identity, all follower-only/object-covered responses,
and each process's admission envelope. The trap clears an injected bucket
policy and removes only the uniquely named qualification project and its
volumes. Set
`CRAB_HTTP_CLUSTER_BUILD=false` to reuse an already-built
`CRAB_HTTP_SERVER_IMAGE`.

This is real process-loss, source-loss, peer-routing, follower replacement,
follower-affine recovery, and bounded non-member fallback evidence. The
qualification fixture uses a dedicated, long-lived network-namespace sidecar,
so node A or D can self-fence without preventing another node from restarting;
B and C are independently killed and lose their tmpfs. The gate is not a
production multi-Pod partition test.
Lost control-CAS responses are covered by a deterministic scheduler regression.

## Deploy for a team

Use the Helm chart on Amazon Elastic Kubernetes Service (EKS), Google Kubernetes Engine (GKE), or Azure Kubernetes Service (AKS). One chart preserves the server runtime contract across providers.

The shortest supported team path is:

| Owner | One-time setup | Per release |
| --- | --- | --- |
| Platform team | Apply one provider Terraform root; configure DNS, TLS, ingress, and workload identity | Review infrastructure drift |
| Identity team | Register the OIDC callback and store the client secret | Rotate under the runbook |
| Crab service owner | Create the namespace, stable state-key Secret, and team values | Verify the signed release record, change one image digest, and run `helm upgrade` plus `helm test` |

No repository list belongs in the server configuration. Create or adopt a
repository once through the management CLI; every replica discovers the shared
catalog update from object storage.

```mermaid
flowchart LR
    Provider[Generated provider values] --> Helm[Helm release]
    Team[Team image, OIDC, and ingress values] --> Helm
    Secret[OIDC secret and stable state key] --> Helm
    Identity[Cloud workload identity] --> Pods[Three or more Crab pods]
    Helm --> Pods
    Pods --> Root[(One storage root)]
```

Complete the setup in this order:

1. Create versioned storage and provider workload identity with `terraform/aws`, `terraform/gcp`, or `terraform/azure`.
2. Grant the workload identity access only to the dedicated storage boundary.
3. Register the OpenID Connect (OIDC) callback `https://git.example.com/auth/callback`
   and Back-Channel Logout URI `https://git.example.com/auth/backchannel-logout`.
   Set `backchannel_logout_session_required=false`; Crab currently supports
   subject-scoped Logout Tokens, not `sid`-only delivery.
4. Export Terraform's generated provider values and edit the provider-neutral team values file.
5. Create a dedicated namespace with the Restricted Pod Security policy, create
   the Kubernetes Secret, and install the chart.
6. Run `helm test` to prove a fresh workload can read, list, write, conditionally update, and delete through its cloud workload identity.
7. Create the first repository through a running pod.
8. Run the portable multi-replica qualification locally or through the protected GitHub Actions workflow before admitting critical repositories.

Do not grant team members direct write credentials for the storage root. Git,
LFS, browser, and administration traffic must pass through the server so its
authorization, locking, and atomic publication rules remain authoritative.
The production chart also rejects static cloud credentials, credential-source
overrides, unsigned or cleartext storage modes, and custom provider endpoints
in `extraEnv`; the provider ServiceAccount is the only supported credential
path.

[The infrastructure bootstrap guide](terraform/README.md) creates storage and
identity. [The Kubernetes deployment guide](helm/crab-http-server/README.md)
contains install commands. [The operations runbook](operations.md) covers
rollout, rollback, rotation, incidents, and restore qualification.

## Repository lifecycle

The server discovers repositories from a bounded, versioned catalog below the
configured storage root. It never scans a bucket and never treats GC metadata
as an application registry.

```sh
crab-http-server --config server.toml repository create \
  --owner my-team --name my-project --prefix my-team/my-project \
  --members-file members.toml

crab-http-server --config server.toml repository adopt \
  --owner my-team --name existing --prefix imports/existing \
  --members-file members.toml

crab-http-server --config server.toml repository set-members \
  --owner my-team --name my-project --members-file members.toml

crab-http-server --config server.toml repository list
```

Pass `--members-file -` to read the TOML membership document from standard
input, which is useful with `kubectl exec --stdin`. Authenticated deployments
require at least one `admin` member when creating or adopting a repository;
unauthenticated loopback deployments may omit membership.

`create` initializes canonical Crab layout and manifest objects, CAS-publishes
`empty_cell_pending`, provisions and verifies the initial SQLite/LTX root, then
CASes `cell_ready`. Exact retries retain the catalog UUID and restore the
published root before completing. `adopt` requires canonical Git objects,
publishes `empty_cell_pending`, initializes a new empty application Cell and
then publishes `cell_ready`. `set-members` uses one conditional catalog update
and reports a conflict instead of replaying a stale decision over a concurrent
change. Browser Settings → Members uses the same revision contract and CSRF
protection. Both paths create one durable membership audit event with the
accepted catalog change. Version 2 catalogs remain readable, but the first
write upgrades the catalog to v3; do not roll a v3 storage root back to an older
server binary. Every running replica checks the catalog every five seconds and
swaps routing only after all records pass Cell readiness validation; in-flight
requests retain the previous repository handle.

Back-channel delivery invalidates all browser sessions and derived Git tokens
for the Logout Token's exact issuer and subject. The identity index has an
eight-hour compatibility window for sessions created by the pre-index binary.
Complete the rollout inside that maximum session lifetime; do not leave old and
new server binaries coexisting beyond it. A valid or replayed delivery returns
200, while malformed or invalid tokens return 400.

## Why Lambda is excluded

Lambda is not a supported full data-plane target. Native receive, upload-pack,
LFS, archive downloads, and maintenance can stream for minutes and use large
bounded scratch space. API Gateway and Lambda buffering, payload, duration,
and ephemeral-runtime constraints change those semantics. A future Lambda
adapter may host bounded control-plane operations, but it must not be described
as the same server deployment.
