# Artifact registry contract

The top-level artifacts declaration in crab.yaml is a validated catalog. It
does not execute a stage and it does not infer a release version. A declaration
has a canonical name, repository-relative path, kind, optional description,
labels, and bounded scalar metadata.

crab artifacts version create hashes the current clean output with Crab's
canonical BLAKE3 identity and stores an immutable manifest under Crab-owned
per-worktree state. The manifest records declaration identity, source commit,
content identity, size, creation time, and annotations. Creation rejects a
missing or dirty path and never reuses a different manifest identity.

Promotion is metadata-only. A candidate, staging, or production label points
to one immutable version and is updated with compare-and-swap. The expected
version must be supplied for concurrent automation; conflicts report the
current and expected values. Promotion never copies artifact bytes.

The CLI keeps a local mirror for recovery, but a configured primary `crab://`
remote is canonical: list, show, get, version create, promote, and history
read and publish the remote manifest, payload, stage, and promotion records.
Remote downloads stream into a verified, non-overwriting destination and
preserve directory trees and modes. Shared GC reachability and automatic old-
version retention are deliberately not enabled until the artifact GC gate
passes.
