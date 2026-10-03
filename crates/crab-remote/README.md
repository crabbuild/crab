# crab-remote

`browse_indexes::ensure` is optional background v2 indexing. It captures a
snapshot under renewed generation ownership and GC fences, reuses the shared
Git index builders, and publishes `v2/browse-indexes` only after checking a fresh
root and visible ref positions. Conditional record publication cannot overwrite
a newer maintenance result; readers independently reject a stale binding.
Ordinary push/clone/fetch do not load this derived record.

Internal orchestration shared by Crab SDK, CLI, HTTP, and managed-service
callers. Authorization and product policy remain at those caller boundaries.

The default feature has no runtime dependencies. `publication` owns immutable
artifact preparation, operation and ref leases, ordered GC fences, journal
outcome mapping, durable plan attribution, historical reconciliation, readiness,
and protected-push request binding. CLI and HTTP callers use these mechanics
without delegating their authorization decisions.

`checkpoint` separates metadata-first logical publication from physical pack
maintenance using the layered format only. Foreground admission checkpoints the
authenticated source directory;
only the hard 64-source limit can force a minimal suffix roll-up. Background
owners then pin the published checkpoint for geometric repacking without folding
newer per-ref heads. The shared maintenance pass retains the successful root-CAS
receipt and complete checkpoint between phases; it does not reread its own
publication. The metadata owner and HTTP server use this two-phase pass, with
separate CAS and cancellation boundaries. Explicit CLI repack consolidates the
selected suffix and checkpoints its pinned view with one root CAS. Small
frontiers reuse already authenticated capsule bodies; large frontiers retain
bounded control and range reads.
A stale root loses its CAS without changing visible refs or
history. Suffix installation is restricted to selected physical sources, and
stable-prefix descriptors and member positions remain unchanged even when the
suffix contains identical pack bytes. Verified external delta bases are installed
only in private scratch object databases, where native Git repairs source copies
before metadata queries. Loose bases alone do not make raw thin packs readable
by Git. Those copies and bases are not added to the selected or replacement
object universe.
If logical publication loses its CAS, maintenance stops without attempting a
physical rewrite of that known-stale root. Already-completed logical work stays
accounted. Physical debt below the logical threshold still runs against its
captured checkpoint, excluding newer ref heads.

Maintenance returns publication status separately from logical pack-body work.
The input count is the deduplicated installed suffix, not the whole inventory;
the output count is the replacement body submitted to verified immutable
publication, excluding its sidecars and envelope. A losing root CAS retains
both counts. Retrying the same source can repeat this work even when the
immutable output already exists. These are not transport counters: readback,
retries, external-base resolution, metadata and range overread are separate.
Metadata-only and no-op passes return zero body work. Callers combine logical
and physical outcomes so a forced source-limit roll-up is not omitted.
The pinned-view outcome also reports the last inventory published by that pass,
or the input inventory if neither phase published. It deliberately excludes
later ref-head updates instead of rescanning them for CLI statistics.

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
