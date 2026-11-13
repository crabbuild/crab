# crab-git

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-git/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns Git discovery, refs, pointers, packs, and bounded protocol mechanics. Transport authorization and publication policy belong to callers; LFS payload storage belongs to crab-lfs.

## Read first

1. `crates/crab-git/src/lib.rs` — public mechanics and optional facade.
2. `crates/crab-git/src/discover.rs` — `discover_git_dir_from / resolve_common_dir`: Git directory and common-directory discovery.
3. `crates/crab-git/src/receive_plan.rs` — `validate / GraphSource`: ref comparisons and graph validation.
4. `crates/crab-git/src/pack.rs` — `verify_pack_sha1 / install_pack_file_from_path`: pack/index integrity and installation.
5. `crates/crab-git/src/refname.rs` — `validate_push_refname / validate_ref_namespace`: individual names versus final namespace validation.

Trace one path: `crates/crab-server/src/git/receive/validate.rs` → `validate` in
`crates/crab-git/src/receive_plan.rs` → `validate_ref_namespace` in
`crates/crab-git/src/refname.rs`. Publication remains above this chain.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Native receive validation | `crates/crab-git/src/receive_plan.rs` | `crates/crab-server/src/git/receive/validate.rs` |
| Pack decoding | `crates/crab-git/src/incoming_pack.rs`, `crates/crab-git/src/delta.rs` | `crates/crab-remote-git/src/reader.rs` |
| Streaming pack construction | `crates/crab-git/src/pack_writer.rs` | `crates/crab-server/src/git/receive/mod.rs` |
| Pointer classification | `crates/crab-git/src/pointer_detect.rs` | `crates/crab-types/src/pointer.rs` |

## Invariants

- Validate the final ref namespace after edits; validate individual names separately. A parent ref cannot coexist with its slash-delimited descendants.
  Source: `crates/crab-git/src/refname.rs`.
- A receive plan is not publication authority: callers supply policy, verify pointer dependencies, and recheck the captured base under writer leases.
  Source: `crates/crab-git/src/receive_plan.rs`.
- Read `GIT_DIR` with `var_os`; an explicit OS path must not disappear because
  it is non-Unicode. Check the feature-gated CLI config resolver sibling too.
  Sources: `crates/crab-git/src/discover.rs`, `crab/src/core/config_resolver.rs`.
- Keep common-directory discovery distinct from the current working tree; discovery has an explicit .git result outside repositories.
  Source: `crates/crab-git/src/discover.rs`.

- Worktree porcelain framing must not trim NUL-delimited field contents. Line
  mode and NUL mode have different delimiter contracts; native Git path coverage
  lives in `worktree::tests`.

- Immutable-object consumers use `raw_object_command` so local replacement refs,
  grafts and shallow markers cannot change object kinds or ancestry. Fetch-specific
  repacking retains its deliberately selected shallow frontier.
  Source: `crates/crab-git/src/lib.rs`.

- Batch verification, small-object visits and whole-blob copies share the same
  checksum/framing owner. Copy output remains untrusted until successful EOF;
  callers discard failed output and drain their process workers before cleanup.
  SHA-1 uses the same hardware-accelerated collision detector as generated
  objects, preserving Gitoxide's collision error and unmodified digest.
  Source: `crates/crab-git/src/batch.rs`.
- Loose-object copies stream zlib input through the same body verifier without
  mapping the compressed object. Require the declared header, size, Git checksum,
  zlib StreamEnd and physical EOF. Alternates preserve native Git path bytes,
  quoting, relative bases, cycles and recursion limits within a metadata budget.
  Source: `batch::open_loose_blob / copy_loose_blob`; desktop range and file-copy
  requests share this descriptor/worker owner.
- Local packed copies read bounded index windows and collect delta ancestry
  iteratively. One admitted anonymous program file and seekable bases/results
  avoid whole-object RAM and recursive stack/descriptor growth. Reference bases
  prefer their source pack. Final target checksums authorize delivery; callers
  supply scratch policy and retain joined I/O through cancellation.
  Sources: `batch/packed.rs`, `batch/pack_index.rs`, `delta::copy`.
- Captured local Git ranges retain admitted anonymous bytes with block proofs
  authenticated by the complete Git checksum. Origin metadata may invalidate
  reuse but never authorize returned bytes. Callers own idle eviction, shared
  operational disk admission and draining before close. Storage decline uses
  the same complete physical verifier; source corruption must remain visible.
  Sources: `batch/capture.rs`; desktop `content/git_capture.rs`.
- Built-in checkout streams linear EOL conversion through the raw EOF verifier.
  Ident and auto text classification use authenticated capture windows; long
  ident candidates replay owned seekable input instead of growing a token buffer.
  Auto text classification follows ident expansion. Cancellation must not be
  retried as ordinary interrupted I/O. Callers retain encoding/filter and
  publication ownership; converted output is untrusted until successful EOF.
  Source: `batch/capture/checkout.rs`.

## Features and platform

`facade` enables the optional high-level gix dependency. `process` enables the shared `process::OwnedChild` group/job lifetime owner used by CLI and desktop. Neither is enabled by default. Git-spawning tests need Git on PATH; inspect locked dependency source before altering validation or process contracts.

`index_stat` also uses `facade`. CLI and desktop publication share its locked
stat-cache writer. Retain the stat captured during verification and fence native
updates with the expected blob; never stamp a later external edit as clean.
Large verified blobs supply their OID instead of retaining complete bytes.
Checkout may refresh advisory stats while preserving staged IDs/modes/siblings.

## Verification

`process::tests` covers Darwin exit/permission races. Process callers must stop
before joining pipes on errors; Unix exit polling retains the leader until the
group signal. CLI cancellation/grace policy stays in `crab/src/git/process.rs`;
desktop owns its finite output/deadline policy in
`packages/desktop/agent/src/repository/content/command.rs`.

Read inline discovery/ref tests and `crates/crab-git/src/receive_plan/tests.rs`; pack quarantine regressions live in `crates/crab-git/src/incoming_pack/tests.rs`.

Delta instruction validation and reconstruction share a private walker. Run
`cargo test -p crab-git --locked --lib delta::tests` with the external target
directory configured; include incoming-pack and remote-reader consumer tests
when changing instruction semantics.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-git --locked --lib receive_plan::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-git --locked --lib discover::tests
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-git/README.md` — usage and detailed contracts.
- `crates/crab-git/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
