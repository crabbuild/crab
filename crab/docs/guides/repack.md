# crab repack

Consolidate the physical Git pack sources selected by one captured Capsule v2 view.

## Synopsis

```bash
crab repack [--dry-run] [--json | --jsonl]
crab optimize packs [--dry-run] [--json | --jsonl]
```

The configured repository must have valid Capsule authority and visible refs.
Missing or corrupt root is an error; there is no manifest-v1 repack path.

## Operation

The CLI obtains its selected store and captured root through repository
authentication. It opens the complete repack view from that root and delegates
to the shared Capsule checkpoint/repack machinery. Compressed source sizes
select a geometric suffix; format admission can also require consolidation.
Stable-prefix source descriptors and positions remain intact. Small frontiers
can reuse authenticated capsule bodies; larger frontiers use bounded control
and range reads.

Selected physical sources are consolidated with verified Git pack mechanics.
Publication includes current catalogs and visibility, and uses compare-and-swap
against the captured root. It does not write a unified manifest, SlateDB locator,
legacy generation receipt, or a second sidecar repair path.

A competing root replacement leaves this pass unpublished. The summary retains
its input inventory for that no-op and reports actual work performed; it does
not attribute a later writer's inventory to this pass. A GC fence produces a
retryable conflict. Uncertain publication retains its typed evidence and does
not become a blind retry. Cooperative cancellation is awaited through the
publication boundary.

Recovery history continues to retain superseded sources. Repack reduces the
active read inventory; it does not itself reclaim all old storage. Preview an
explicit retained-history boundary before repository GC when reclamation is
needed.

## Preview and verify

```bash
crab repack --dry-run --json
crab repack --json
```

Dry-run reports captured pack count and bytes without publishing or reading pack
bodies. `packs_after` and `bytes_after` remain equal to their input values;
`bytes_read` and `bytes_written` are zero. It does not predict compressed output
size.

After apply, verify refs and perform an independent clone, strict Git integrity
check, and representative hydration. These checks prove readable content in
addition to the maintenance summary.

## Structured output

Both output modes contain `packs_before`, `packs_after`, `bytes_before`,
`bytes_after`, `bytes_read`, `bytes_written`, and `elapsed_ms`. Body-work counters
exclude sidecars and transport overhead. `--json` emits a `repack` result
envelope; `--jsonl` emits the terminal `repack.event` result. The CLI does not
emit per-pack progress events.

The shared Capsule machinery owns current maintenance policy. Removed
manifest-only Rust configuration fields never controlled the current Capsule
path; there are no corresponding repack CLI options to migrate.

See [Repacking Objects](https://crab.build/docs/cli/storage/repacking-objects)
and [command reference](https://crab.build/docs/cli/reference/crab-repack).
