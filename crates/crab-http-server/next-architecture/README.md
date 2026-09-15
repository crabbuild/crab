# Next-generation crab-http-server: repository SQLite cells and LTX durability

Status: target server architecture. Local and optional remote `crab-ltx`
mechanics are implemented, and the server now has a statically registered
repository issue/comment module proven through local LTX publication and
source-loss restore. The HTTP composition root now starts that native runtime,
withdraws readiness when it drains and joins its SQL workers during ordinary
server shutdown. The release CLI now provides resumable exact-compatible
activation and publishes a verified descriptor as current. Private routing now
includes mandatory mTLS ingress, live enrollment, authoritative outbound owner
lookup and one bounded stale-owner retry. Explicit release activation now also
requires one live exact fleet/image/release/module-compatible candidate. The
maintenance importer now covers the legacy issue/comment tree, including
counters, visible edits and incomplete reservations, with two-pass source
verification, immutable evidence and crash-resumable LTX publication. The
repository router now distinguishes a live signed remote owner from an absent or
expired session and uses the runtime's unchanged-control observation before
takeover. Remaining collaboration-domain import, old-version migration/configured
multi-node quorum and the remaining product-domain route cuts are not yet
integrated. New repository creation explicitly publishes an empty Cell and marks
the catalog ready; adoption remains blocked until verified import. Startup
rejects every missing or rootless repository Cell. The issue/comment HTTP group
now uses the release-aware router, which reuses local handles, selects
authenticated remote owners and restores idle Cells without request-time empty
bootstrap.
Prepared 2026-09-13
against Crab commit
`f67181e0dcdc69a766b14a8b441e9119d3684f33`; deployment and integration notes were
updated when rebasing onto `2cb79f1cdb56eb824c607195635615a6f9c4a23f`.
The local crate implementation was added against `c6a64db0c28` and its API,
limits and qualification status are recorded in [crab-ltx](crab-ltx.md).

Audience: implementers of the HTTP application, storage and publication owners,
operators, and reviewers of correctness and migration evidence.

The [embedded Rust Cell runtime specification](../../../crab/docs/architecture/platform/README.md)
now owns the low-level shared runtime APIs, Cell formats, primitive contracts and
compiled-release lifecycle. It narrows delivery to Rust handlers embedded in this
server; no standalone multi-language platform is planned. For overlapping runtime
details, use that specification; this folder retains the repository data model,
Git integration and hard-cutover requirements.

Native application code may be organized in private Rust workspace crates, but
this server remains the sole registry composition root, executable and deployable
unit. Kubernetes balances complete Crab nodes; it does not schedule application
modules independently. Product HTTP/Git routes remain the only public API, while
Cell and primitive capabilities remain private Rust contracts.

This design replaces collaboration JSON documents with one SQLite database per
cataloged repository. A Rust subsystem captures SQLite WAL changes into LTX
files. Object storage holds the authoritative recovery graph. Multiple HTTP
servers route collaboration requests to the repository's current owner, while
the existing Git data plane continues to use its shared publication contracts.

Accepted deployment decision: hard cutover from the current architecture. A
maintenance window stops the old fleet, imports and verifies existing application
data, and starts the new fleet. The new runtime supports only SQLite/LTX
application storage; no old/new mixed fleet or legacy backend is required.

The existing [system and write design](../DESIGN.md) describes current behavior.
The [reference](../REFERENCE.md) remains the authority for implemented APIs and
qualification status. This proposal defines intended behavior, implementation
boundaries, and acceptance gates; examples of new configuration, commands, SQL,
and server-layer Rust interfaces remain design examples. The callable
[crab-ltx API and current limits](crab-ltx.md#implemented-library-api) are explicitly
marked as implemented; library/RustFS proof is not HTTP owner-publication or UI proof.

## Reading guide

This folder contains the next-generation design, organized by subsystem. Each
topic owns its detailed contract; links connect the publication, routing, Git,
and deployment boundaries.

| Topic | Covers |
| --- | --- |
| [Architecture, scope, and guarantees](overview.md) | System diagram, data ownership, safety properties, and design decisions. |
| [Current implementation and evidence](current-implementation.md) | Existing HTTP, Git, application storage, deployment, and test boundaries. |
| [Celld architecture and Rust integration](celld-and-rust.md) | Per-cell LTX mechanics, differences from Celld, dependency strategy, and Rust ownership. |
| [crab-ltx source integration and crate design](crab-ltx.md) | Local/remote APIs, sparse SQL, epoch inheritance, bundles, compaction, Celld provenance and qualification. |
| [Object storage and commit publication](storage-protocol.md) | Control record, immutable recovery graph, publication CAS, and response gating. |
| [Ownership, placement, and load balancing](ownership-and-load-balancing.md) | Leases, activation, capacity admission, idle handoff, and fleet balancing. |
| [SQLite runtime and application data model](sqlite-and-data-model.md) | WAL capture, checkpoints, read consistency, schema, transactions, and retries. |
| [HTTP routing, peer protocol, and security](routing-and-security.md) | Route classification, authenticated proxying, bounded retries, and authorization. |
| [Git and application workflows](git-workflows.md) | Durable outbox, merge/tag publication, uncertain outcomes, and release assets. |
| [Recovery, compaction, and backups](recovery-and-retention.md) | Exact restore, failure matrix, takeover races, retention, and backup roots. |
| [Deployment, lifecycle, and operations](deployment-and-operations.md) | Kubernetes topology, drain, resource budgets, capacity, metrics, and runbooks. |
| [Hard cutover and future upgrades](hard-cutover.md) | Offline import, verification, fleet transition, failure recovery, and format upgrades. |
| [Validation, delivery, and worked examples](validation-and-delivery.md) | Protocol tests, real repositories/UI acceptance, delivery gates, examples, and sources. |

Start with [architecture and data ownership](overview.md#architecture-and-data-ownership).
For the multi-node design, read [ownership and load balancing](ownership-and-load-balancing.md),
then [peer routing](routing-and-security.md) and
[Kubernetes deployment](deployment-and-operations.md).
For storage implementation, read [Celld and Rust integration](celld-and-rust.md),
[the crab-ltx design](crab-ltx.md),
[commit publication](storage-protocol.md), and
[SQLite execution](sqlite-and-data-model.md).

The accepted baseline remains one active AppCell owner per repository UUID,
one combined owner/head control CAS, object-store publication before success,
and fleet-wide hard cutover. The public load balancer chooses an entry node;
repository control state determines where collaboration executes. Git operations
continue through the existing shared Git publication and read boundaries.

Examples, diagrams, proposed ports, and delivery gates describe the target
architecture. [Current implementation evidence](current-implementation.md)
and [REFERENCE.md](../REFERENCE.md) distinguish it from today's server.
