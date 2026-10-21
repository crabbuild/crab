# crab-remote

`browse_indexes::ensure` is optional background v2 indexing. It captures a
snapshot under renewed generation ownership and GC fences, reuses the shared
Git index builders, and publishes `v2/browse-indexes` only after checking a fresh
root and visible ref positions. Conditional record publication cannot overwrite
a newer maintenance result; readers independently reject a stale binding.

Physical OID and pack-offset pages are built from authenticated complete source sidecars,
verified in full before readiness, and carried through final path publication.

The elected owner repairs missing/corrupt derived physical pages; foreground
reads propagate those failures rather than probing whole pack indexes.
Ordinary push/clone/fetch do not load this derived record.

Internal orchestration shared by Crab SDK, CLI, HTTP, and managed-service
callers. Authorization and product policy remain at those caller boundaries.

The default feature has no runtime dependencies. `publication` owns immutable
artifact preparation, operation and ref leases, ordered GC fences, journal
outcome mapping, durable plan attribution, historical reconciliation, readiness,
and protected-push request binding. CLI and HTTP callers use these mechanics
without delegating their authorization decisions.

## Checkpoint publication and maintenance

`checkpoint::prepare_capsule_catalog_from_view` prepares durable catalog pages
without making a checkpoint visible. Checkpoint publication and historical
restore use this same captured-delta folding owner under caller-held GC admission.
Ref-only preparation retains the existing root without catalog I/O.

`publish_capsule_checkpoint_with_catalog_root_from_view` consumes that prepared
root through the same publication owner, avoiding duplicate delta preparation.

`checkpoint` separates metadata-first logical publication from physical pack
maintenance using the layered format only. Foreground admission checkpoints the
authenticated source directory;
only the hard 64-source limit can force a minimal suffix roll-up.

Background owners then pin the published checkpoint for geometric repacking without folding
newer per-ref heads. The shared maintenance pass retains the successful root-CAS
receipt and complete checkpoint between phases; it does not reread its own
publication. The metadata owner and HTTP server use this two-phase pass, with
separate CAS and cancellation boundaries.

Explicit CLI repack consolidates the
selected suffix and checkpoints its pinned view with one root CAS. Small
frontiers reuse already authenticated capsule bodies; large frontiers retain
bounded control and range reads.

A stale root loses its CAS without changing visible refs or
history. Suffix installation is restricted to selected physical sources, and
stable-prefix descriptors and member positions remain unchanged even when the
suffix contains identical pack bytes.

Verified external delta bases are installed
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
immutable output already exists.

These are not transport counters: readback,
retries, external-base resolution, metadata and range overread are separate.

Metadata-only and no-op passes return zero body work. Callers combine logical
and physical outcomes so a forced source-limit roll-up is not omitted.

The pinned-view outcome also reports the last inventory published by that pass,
or the input inventory if neither phase published. It deliberately excludes
later ref-head updates instead of rescanning them for CLI statistics.

## Capsule preparation and publication

Capsule preparation resolves outgoing pointer files and only the prepared
shard/xorb identities against a captured paged catalog. Ordinary Git and LFS-only
uploads do not read historical Xet catalog pages. Selected dependencies retain
complete closure, size and immutable-descriptor conflict checks.

Scratch ownership does not freeze artifact bytes. Bounded reads revalidate the
inspected raw Blake3 body identity before upload; Xet protocol identities retain
their distinct word-ordered representation.

Capsule commitment retains exact
verified create/read identities with their source catalog, publishes rebuildable
chunk hints only after proven ref visibility, and preserves a committed outcome
if hint-index maintenance fails. Uncertain publication does not publish hints.

Caller-owned ref leases and GC fences cover this complete operation.

Preparation rechecks caller cancellation and namespace renewal immediately before
visibility. Cancellation during immutable preparation leaves only unadvertised
objects; cancellation after visibility retains the known committed result while
all publication and lease cleanup drains.

New refs and explicit cross-ref visibility bases publish complete replacement
closures from the captured proof. Only same-ref updates retain additive evidence;
a different ref's later deletion must not invalidate an acknowledged branch.

## Shared helpers and lifecycle

`objects` owns bounded tree reads, deterministic tree edits and encoding,
commit encoding, and Git object identities. `prepare` owns normalized packs and
hydrated content artifacts used by remote commits and recovery. File bodies and
caller admission policy stay outside the object helpers.

Capsule publication preparation queries only outgoing file bindings and
candidate shard/xorb identities from its captured catalog roots. All three key
sets and their dependency pages share one query budget. Candidate identities
still need collision checks when a new file reuses historical artifacts; they
cannot be inferred only from existing file bindings.

The caller retains its ref leases and GC fences through lookup, payload verification and commitment.
This does not change physical content-proof limits or authorize a newer view.
Full catalog inspection remains an explicit maintenance operation.

The `local` feature owns explicit Git and Crab subprocesses, capabilities
handshake, bounded I/O, cancellation, hook isolation, repository leases, filter
configuration, and exact command-path quoting. `transfer` owns direct clone and
fetch snapshots used by the SDK while local ref, shallow, and worktree state
remain under Git locks.

Every publication callback retains its terminal outcome while leases and
heartbeats drain. Missing historical proof stays indeterminate; current ref
equality never invents attribution. Protected receive remains server-authorized
and cannot be invoked as a client-authorized commit.

## Read qualification

The `publication` feature exposes `qualify_remote` and `qualify_browse` examples.
They compose capsule capture, read APIs and explicit browse-index maintenance at
this orchestration boundary. The low-level remote Git reader retains its narrow
Git/storage/metadata dependency boundary. See the
[indexed-read qualification contract](../../crab/docs/architecture/indexed-remote-reads.md)
for commands, native Git oracles and cold/warm measurement semantics.

Foreground qualification uses `open_git_view_from_root`; browse-index builders
use the complete metadata entry point. Pack payloads stay in authenticated origin
ranges before and after checkpoints. Checkpoint catalog/visibility decoding and
ref capture remain measured opening work, not a completed bounded-opening gate.

`capture-browse` captures the immutable view and readiness before its stdin
handshake, then constructs the repository after external GC. Its separate
`reader-open` measurement includes normal tip validation, keeping graph, physical
and path pages cold until the external lifecycle transition completes.

`capture-catalog` takes a word-ordered file hash instead of a text path. It holds
the canonical current-state file-index session across the same stdin handshake,
then measures cold/warm duplicate lookups before closing and running the bulk
oracle. This tests catalog source lifetime without requiring browse indexes.

`qualify_remote <bucket> <prefix> <text-path> graph-pages` measures sampled
ordinal/parent records and duplicate OID batches through `CommitGraphReader`.
It verifies the derived graph's captured state and Git identity before reads.
Control capture remains in the reported opening measurements; this component
workload does not claim bounded foreground opening or replace ref visibility.

`graph-ancestry` adds membership checks against the HEAD tip from the same captured
ref view. The native oracle uses `git merge-base --is-ancestor`, treating only
exit status 1 as a valid negative answer. Foreground graph conversion is a separate
gate; this component qualification does not bypass repository visibility.

`checkpoint` publishes through the real logical-checkpoint API, then verifies
the advertised checkpoint pointer and authenticated footer. It mutates the
chosen repository; use a dedicated qualification prefix. Run `build-indexes`
afterward, then repeat the same native-Git query oracles. This separates
checkpoint maintenance costs from foreground opening and prevents a no-op
maintenance command from standing in for checkpoint qualification.

`qualify_remote <bucket> <prefix> <file-hash-hex> catalog` measures selected file
and chunk dependency lookup against a captured paged Git view, cold and warm.
The hash argument uses Xet's word-ordered catalog-key hex, as produced by
`crab_xet::hash::merkle_hex_from_bytes` from the pointer's raw hash bytes.
Its complete-catalog maintenance oracle runs outside measured lookup work.
`pointer_catalog_pages` reports parsed checkpoint catalog pages separately;
opening still includes eager publication deltas and ref/visibility metadata.
