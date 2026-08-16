# Changelog

All notable Crab changes that affect public SDK, Python, agent, or data
integration surfaces should be recorded here before release.

## Unreleased

### CLI Add And Push

- Unified native and remote-helper push on one fail-closed dependency pipeline
  with exact per-ref outcomes, generation-pinned Git/file acceleration, and a
  manifest-CAS commit receipt.
- Made large-file add record one versioned CDC recipe and unique local payloads
  without an eager second full-xorb write. Push packs only origin-missing unique
  chunks through a bounded, backpressured pack/upload pipeline.
- Added batch/path recipe leases, immutable push snapshots, legacy staging
  quarantine, cache-to-origin repair, pre-CAS GC-root union, registry repair,
  and manifest-scoped metadata/Git membership rebuild diagnostics.

### Rust SDK

- Documented the `crab-sdk` package boundary, pre-1.0 compatibility policy,
  public API inventory, deferred methods, and adoption roadmap in
  `crab/docs/guides/sdk-platform.md`.
- Clarified that the SDK is the read-first programmatic surface. Repository
  mutation, push locking, GC, and protected publishing remain CLI/Git remote
  helper responsibilities.
- Added URL-opened `CrabRepository::log` support through the SDK-managed remote
  pack cache.
- Added URL-opened `CrabRepository::diff` support through the SDK-managed
  remote pack cache and shared metadata resolver.
- Added structured SDK errors for invalid caller input and intentionally
  unsupported modes, replacing generic internal errors for malformed globs,
  inverted byte ranges, and URL-opened prefetch-profile loading.
- Added public Rust SDK examples for URL opens, snapshot reads, traversal,
  range streaming, diff, prefetch progress, auth status, and health.

### Python Data Access

- Documented the Python package boundary for stable SDK read wrappers,
  structured exceptions, file-like readers, async APIs, and optional
  integration extras.
- Recorded that dataframe, fsspec-style, ML, and notebook dependencies should
  stay optional rather than becoming base package requirements.

### Agent And Data Integrations

- Documented the planned read-only agent/tool package boundary, including
  bounded reads, policy controls, audit events, and evidence bundles.
- Documented the initial ETL/data-platform integration direction: pinned input
  resolution, prefetch, lineage manifests, table-format coexistence, catalog
  export, and candidate-branch publish flows.
