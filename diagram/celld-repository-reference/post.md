# X post draft

Your application servers + one S3 bucket.

That can be the shared infrastructure foundation for a distributed Git hosting service. Celld offers a useful architecture to borrow: partition state into cells, give each cell one owner, and make its state recoverable when that owner changes.

Consider one cell per repository:

• `acme/api` → a SQLite database for its issues, PRs, reviews and checks.
• `acme/web` → another independent cell and database.
• Each server owns many cells; requests route to the current owner.
• SQLite changes become LTX replication files. S3 stores recovery data and ownership records; Git packs and large objects live in separate prefixes.

For this reference design, a mutation succeeds only after its LTX data and durable head are published to the bucket. If a server dies, another acquires ownership and restores the published state. Add servers to distribute more repositories. Activate cells on demand so idle repositories do not each need a running database process.

The useful idea: S3 can be the only shared persistence and coordination service. No separate database server or coordination cluster is required for this design. You still run compute, networking and a correct replication/ownership protocol; the bucket must support the required conditional writes and consistent reads.

The same boundary can be a tenant, chat room or workflow instance. Choose a unit whose transactions mostly stay local. Cross-cell operations need explicit coordination, and adding nodes does not remove one hot cell's single-writer limit.

Celld inspires the pattern; this diagram shows a proposed repository service, not a deployed benchmark. Celld also supports follower-backed durability, while the illustrated design waits for bucket publication.

https://github.com/denoland/celld

## Diagram

Attach `reference@2x.png`. `reference.svg` is the editable source.

Alt text: A load balancer connects developers and CI to three application servers. Each server owns multiple repository cells with independent SQLite collaboration databases. Owner-aware routing sends requests to the correct server. One S3 bucket holds ownership records, snapshots and LTX changes, and Git and large objects. A replacement server acquires ownership and restores published state. Writes succeed after bucket publication. Compute and network infrastructure remain required.

## Source notes

- [Celld architecture](https://github.com/denoland/celld): per-cell SQLite, owner routing, object storage, and node/follower durability.
- [Celld guarantees](https://github.com/denoland/celld/blob/main/docs/guarantees.md): required bucket semantics, fencing, response gating and recovery.
- [Celld limitations](https://github.com/denoland/celld/blob/main/docs/limitations.md): cell balancing and operating constraints.
- [Crab reference design](../../crates/crab-http-server/next-architecture/README.md): proposed per-repository architecture, not current runtime support.
