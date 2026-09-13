# Deployment profiles

`crab-http-server` has one provider-neutral runtime contract, a one-command local stack, and checked-in deployment profiles for three cloud storage providers:

```mermaid
flowchart LR
    Client[Browser / Git / LFS] --> Edge[TLS load balancer]
    Edge --> Replicas[2+ server replicas]
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

The Kubernetes chart generates configuration from typed values, runs two or more replicas,
private probes and Prometheus metrics, an optional Prometheus Operator
`PodMonitor` and alert rules, a disruption budget, ingress isolation, optional
Transport Layer Security (TLS) ingress, and optional autoscaling. A provider is
release-qualified only after its live test matrix passes.

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

Server release tags have their own contract, independent of the Crab CLI. An
annotated `crab-http-server-vX.Y.Z` tag matching the server crate publishes a
qualified AMD64/ARM64 image to GHCR with immutable version and source-commit
tags, an OCI Helm chart, an SBOM, and provenance attestations. Exact-source
qualification rejects fixable HIGH or CRITICAL image vulnerabilities before
publication. Kubernetes deployments still pin the resulting image manifest
digest. The publisher never overwrites an existing image tag or chart version.

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
not published to the host. Compose waits until the catalog is valid and every
repository can open its current Git view. This profile is for local development
and evaluation, not remote or multi-user service.

### Operate the local stack

Use the one-shot repository service to manage the durable catalog:

```sh
docker compose --file crates/crab-http-server/deploy/compose.yaml run --rm \
  repository-init --config /etc/crab/server.toml repository create \
  --owner my-team --name my-project --prefix my-team/my-project

docker compose --file crates/crab-http-server/deploy/compose.yaml run --rm \
  repository-init --config /etc/crab/server.toml repository list
```

Inspect or stop the stack without deleting repositories:

```sh
docker compose --file crates/crab-http-server/deploy/compose.yaml logs --follow server proxy
docker compose --file crates/crab-http-server/deploy/compose.yaml down
```

`docker compose down --volumes` permanently removes the local RustFS
volume, including the catalog and every repository. The defaults need no
`.env` file. These optional environment variables customize local operation:

| Variable | Default | Purpose |
|---|---|---|
| `CRAB_HTTP_SERVER_PORT` | `8788` | Localhost port published by Docker |
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

## Deploy for a team

Use the Helm chart on Amazon Elastic Kubernetes Service (EKS), Google Kubernetes Engine (GKE), or Azure Kubernetes Service (AKS). One chart preserves the server runtime contract across providers.

```mermaid
flowchart LR
    Provider[Generated provider values] --> Helm[Helm release]
    Team[Team image, OIDC, and ingress values] --> Helm
    Secret[OIDC secret and stable state key] --> Helm
    Identity[Cloud workload identity] --> Pods[Two or more Crab pods]
    Helm --> Pods
    Pods --> Root[(One storage root)]
```

Complete the setup in this order:

1. Create versioned storage and provider workload identity with `terraform/aws`, `terraform/gcp`, or `terraform/azure`.
2. Grant the workload identity access only to the dedicated storage boundary.
3. Register the OpenID Connect (OIDC) callback `https://git.example.com/auth/callback`.
4. Export Terraform's generated provider values and edit the provider-neutral team values file.
5. Create the Kubernetes Secret and install the chart.
6. Create the first repository through a running pod.
7. Run the live qualification gates before admitting critical repositories.

[The infrastructure bootstrap guide](terraform/README.md) creates storage and identity. [The Kubernetes deployment guide](helm/crab-http-server/README.md) contains install commands. [The operations runbook](operations.md) covers rollout, rollback, rotation, incidents, and restore qualification.

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

crab-http-server --config server.toml repository list
```

`create` initializes canonical Crab layout and manifest objects before its CAS
catalog publish. `adopt` requires those objects to exist already. Every running
replica refreshes the catalog and begins routing a successful change within
five seconds; in-flight requests retain the previous repository handle.

## Why Lambda is excluded

Lambda is not a supported full data-plane target. Native receive, upload-pack,
LFS, archive downloads, and maintenance can stream for minutes and use large
bounded scratch space. API Gateway and Lambda buffering, payload, duration,
and ephemeral-runtime constraints change those semantics. A future Lambda
adapter may host bounded control-plane operations, but it must not be described
as the same server deployment.
