# Changelog

All notable Crab changes that affect public SDK, Python, agent, or data
integration surfaces should be recorded here before release.

## Unreleased

## 1.0.0 - 2026-08-31

### Open Source Launch

- Established `crabbuild/crab` as the canonical open-source repository and
  reset the public release line to 1.0.0. Pre-launch development snapshots are
  recorded below by date only.
- Shipped the serverless Git remote helper, direct object-storage large-file
  workflow, partial-clone support, protected publication, and safe repository
  maintenance as the open-source launch baseline.
- Published native ARM64 and x86_64 archives for macOS, Linux, and Windows with
  checksums, build-provenance attestations, public installers, self-update
  support, and the `crabbuild/tap/crab` Homebrew formula.

## 2026-08-27

### Git And Large Files

- Made Crab LFS a direct object-storage workflow with bounded transfers,
  publication coordination, lock ownership, and fail-closed push behavior.
- Hardened partial clone, fetch, push, and remote-pack maintenance for large
  repositories, including generation-aware visibility and bounded locator work.
- Added safer mixed LFS/Xet conversion and filtering, with expanded regression
  coverage across checkout, fetch, transfer-agent, and concurrent publication
  paths.

### Storage And Performance

- Hardened repository and bucket garbage collection so referenced content and
  grace-period objects remain protected while remote collections scale.
- Added geometric remote-pack compaction and reduced lock, listing, and
  publication amplification on hot repositories.
- Moved xorb maintenance under `crab optimize` and expanded RustFS performance
  qualification for repository creation and large-repository workflows.

### Release And Distribution

- Consolidated release publishing into the open-source repository's tag-driven
  GitHub Actions workflow; local tooling can build but cannot publish or replace
  release assets.
- Added native ARM64 and x86_64 archives for macOS, Linux, and Windows,
  checksums, packaged-binary smoke tests, and GitHub build-provenance
  attestations.
- Made the installer, self-updater, release badge, and Homebrew formula consume
  releases from `crabbuild/crab`.

### Documentation And Website

- Launched the learning library and file-backed blog with improved navigation,
  structured SEO data, and responsive architecture and workflow diagrams.
- Added direct-storage Crab LFS onboarding and expanded large-file, GC,
  optimization, and Continuity architecture guidance.

## 2026-08-22

### Release Automation

- Build the macOS, Linux, and Windows release archives natively on x86_64 and
  ARM64 GitHub-hosted runners, execute each packaged CLI before publishing,
  and attach build-provenance attestations to all six archives.

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
