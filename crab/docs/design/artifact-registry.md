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

The local lifecycle is intentionally conservative. list, show, get, version
create, promote, and history have text, JSON, and JSONL contracts. get selects
one immutable version or stage and refuses to overwrite an existing path.
Remote ref publication, clean-clone enumeration, shared GC reachability, and
canonical hydration integration are not advertised until the remote artifact
gate passes.
