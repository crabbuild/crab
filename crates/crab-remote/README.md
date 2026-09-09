# crab-remote

Internal orchestration shared by Crab SDK, CLI, HTTP, and managed-service
callers. Authorization and product policy remain at those caller boundaries.

The default feature has no runtime dependencies. `publication` owns immutable
artifact preparation, operation and ref leases, ordered GC fences, journal
outcome mapping, durable plan attribution, historical reconciliation, readiness,
and protected-push request binding. CLI and HTTP callers use these mechanics
without delegating their authorization decisions.

`objects` owns bounded tree reads, deterministic tree edits and encoding,
commit encoding, and Git object identities. `prepare` owns normalized packs and
hydrated content artifacts used by remote commits and recovery. File bodies and
caller admission policy stay outside the object helpers.

The `local` feature owns explicit Git and Crab subprocesses, capabilities
handshake, bounded I/O, cancellation, hook isolation, repository leases, filter
configuration, and exact command-path quoting. `transfer` owns direct clone and
fetch snapshots used by the SDK while local ref, shallow, and worktree state
remain under Git locks.

Every publication callback retains its terminal outcome while leases and
heartbeats drain. Missing historical proof stays indeterminate; current ref
equality never invents attribution. Protected receive remains server-authorized
and cannot be invoked as a client-authorized commit.
