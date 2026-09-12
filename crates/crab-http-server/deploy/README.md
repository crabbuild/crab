# Deployment profiles

`crab-http-server` has one provider-neutral runtime contract and two checked-in
deployment profiles:

```mermaid
flowchart LR
    Client[Browser / Git / LFS] --> Edge[TLS load balancer]
    Edge --> Replicas[2+ server replicas]
    Replicas --> Catalog[Durable CAS catalog]
    Replicas --> State[OIDC sessions and Git tokens]
    Replicas --> Repos[Crab repositories]
    Catalog & State & Repos --> Root[(One object-storage root)]
```

| Target | Deployment asset | Workload identity | Storage URLs |
|---|---|---|---|
| EKS | `helm/crab-http-server` | EKS Pod Identity association | `s3://bucket/root` |
| GKE | `helm/crab-http-server` | GKE Workload Identity Federation | `gs://bucket/root` |
| AKS | `helm/crab-http-server` | AKS Workload ID | `az://account/container/root` |
| ECS/Fargate | `ecs/task-definition.example.json` | ECS task role | `s3://bucket/root` |

These assets are portable implementation evidence. A provider is only
release-qualified after its live test matrix passes; the chart or task
definition alone is not that claim.

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
