# Next-generation crab-http-server: repository SQLite cells and LTX durability

Status: proposed technical design; no SQLite/LTX runtime is implemented by this
document. Prepared 2026-09-13 against Crab commit
`f67181e0dcdc69a766b14a8b441e9119d3684f33`; deployment and integration notes were
updated when rebasing onto `2cb79f1cdb56eb824c607195635615a6f9c4a23f`.

Audience: implementers of the HTTP application, storage and publication owners,
operators, and reviewers of correctness and migration evidence.

This design replaces collaboration JSON documents with one SQLite database per
cataloged repository. A Rust subsystem captures SQLite WAL changes into LTX
files. Object storage holds the authoritative recovery graph. Multiple HTTP
servers route collaboration requests to the repository's current owner, while
the existing Git data plane continues to use its shared publication contracts.

Accepted deployment decision: hard cutover from the current architecture. A
maintenance window stops the old fleet, imports and verifies existing application
data, and starts the new fleet. The new runtime supports only SQLite/LTX
application storage; no old/new mixed fleet or legacy backend is required.

The existing [system and write design](DESIGN.md) describes current behavior.
The [reference](REFERENCE.md) remains the authority for implemented APIs and
qualification status. This proposal defines intended behavior, implementation
boundaries, and acceptance gates; examples of new configuration, commands, SQL,
and Rust interfaces are design examples, not currently available APIs.

## Reading guide

The design is organized by subsystem under `next-architecture/`. Each topic
owns its detailed contract; links connect the publication, routing, Git, and
deployment boundaries. This entry point replaces the original numbered chapter
list.

| Topic | Covers |
| --- | --- |
| [Architecture, scope, and guarantees](next-architecture/overview.md) | System diagram, data ownership, safety properties, and design decisions. |
| [Current implementation and evidence](next-architecture/current-implementation.md) | Existing HTTP, Git, application storage, deployment, and test boundaries. |
| [Celld architecture and Rust integration](next-architecture/celld-and-rust.md) | Per-cell LTX mechanics, differences from Celld, dependency strategy, and Rust ownership. |
| [Object storage and commit publication](next-architecture/storage-protocol.md) | Control record, immutable recovery graph, publication CAS, and response gating. |
| [Ownership, placement, and load balancing](next-architecture/ownership-and-load-balancing.md) | Leases, activation, capacity admission, idle handoff, and fleet balancing. |
| [SQLite runtime and application data model](next-architecture/sqlite-and-data-model.md) | WAL capture, checkpoints, read consistency, schema, transactions, and retries. |
| [HTTP routing, peer protocol, and security](next-architecture/routing-and-security.md) | Route classification, authenticated proxying, bounded retries, and authorization. |
| [Git and application workflows](next-architecture/git-workflows.md) | Durable outbox, merge/tag publication, uncertain outcomes, and release assets. |
| [Recovery, compaction, and backups](next-architecture/recovery-and-retention.md) | Exact restore, failure matrix, takeover races, retention, and backup roots. |
| [Deployment, lifecycle, and operations](next-architecture/deployment-and-operations.md) | Kubernetes topology, drain, resource budgets, capacity, metrics, and runbooks. |
| [Hard cutover and future upgrades](next-architecture/hard-cutover.md) | Offline import, verification, fleet transition, failure recovery, and format upgrades. |
| [Validation, delivery, and worked examples](next-architecture/validation-and-delivery.md) | Protocol tests, real repositories/UI acceptance, delivery gates, examples, and sources. |

Start with [architecture and data ownership](next-architecture/overview.md#architecture-and-data-ownership).
For the multi-node design, read [ownership and load balancing](next-architecture/ownership-and-load-balancing.md),
then [peer routing](next-architecture/routing-and-security.md) and
[Kubernetes deployment](next-architecture/deployment-and-operations.md).
For storage implementation, read [Celld and Rust integration](next-architecture/celld-and-rust.md),
[commit publication](next-architecture/storage-protocol.md), and
[SQLite execution](next-architecture/sqlite-and-data-model.md).

The accepted baseline remains one active AppCell owner per repository UUID,
one combined owner/head control CAS, object-store publication before success,
and fleet-wide hard cutover. The public load balancer chooses an entry node;
repository control state determines where collaboration executes. Git operations
continue through the existing shared Git publication and read boundaries.

Examples, diagrams, proposed ports, and delivery gates describe the target
architecture. [Current implementation evidence](next-architecture/current-implementation.md)
and [REFERENCE.md](REFERENCE.md) distinguish it from today's server.
