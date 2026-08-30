# Plan 002: Select one remote layout through an authoritative descriptor

> **Executor instructions**: Implement only descriptor creation, validation,
> and runtime dispatch. Do not implement partitioned databases yet. Run all
> gates and update `plans/README.md`.
>
> **Hard-cutover rule**: the original plan preserved descriptor-less and
> manifest-v2 behavior. The reconciled plan below replaces it with canonical v1
> descriptor/manifest behavior, no compatibility reader, and no migration.
>
> **Drift check (run first)**:
> `git diff --stat 1f9dae74..HEAD -- crates/crab-metadata/src crates/crab-storage/src crab/src/cmd/init.rs crab/src/main.rs crab/src/git crab/tests`

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: HIGH
- **Depends on**: `plans/001-pb-contract-and-baselines.md`
- **Category**: direction, tech-debt
- **Planned at**: commit `1f9dae74`, 2026-08-19

## Why this matters

The current path router has one implicit layout. PB support needs a serialized,
authoritative choice so every client opens identical paths and unknown formats
fail closed. Selection must not rely on local config or directory probing,
which would create split-brain writes and permanent fallback stacks.

## Current state

- `crates/crab-storage/src/layout.rs:8-120` routes xorbs/shards globally and
  mutable state under the repo prefix; no layout descriptor exists.
- `crates/crab-metadata/src/manifests.rs:16-57` uses manifest version 2. This is
  disposable pre-user state to replace with canonical manifest v1.
- `crab/src/cmd/init.rs:612-650` creates the initial unified manifest.
- `crab/src/main.rs:72-92` exposes init storage/provider options but no layout
  selector.
- There is no compatibility rule to preserve. Add one canonical path and
  delete aliases, absence defaults, implicit probes, and higher-version readers.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Metadata/storage tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-metadata -p crab-storage --locked` | all pass |
| CLI tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --locked init -- --nocapture` | all matching tests pass |

Before Cargo, require `test -w /Volumes/Workspace/crabbuild-target`; stop if it
fails. Never fall back to a local `target/`.

## Scope

**In scope**:

- `crates/crab-metadata/src/layout_descriptor.rs` (create)
- `crates/crab-metadata/src/lib.rs`
- `crates/crab-metadata/src/manifests.rs`
- `crates/crab-storage/src/layout.rs`
- `crab/src/cmd/init.rs`
- `crab/src/main.rs`
- `crab/src/core/remote_layout.rs` (create)
- `crab/src/core/mod.rs`
- Current remote/layout open call sites: `crab/src/cmd/add_push_plan.rs`,
  `crab/src/cmd/clone.rs`, `crab/src/cmd/diff.rs`,
  `crab/src/cmd/diff_driver.rs`, `crab/src/cmd/exp.rs`,
  `crab/src/cmd/fetch.rs`, `crab/src/cmd/fsck_store.rs`,
  `crab/src/cmd/gc/bucket.rs`, `crab/src/cmd/gc/mod.rs`,
  `crab/src/cmd/history_recovery.rs`, `crab/src/cmd/hydrate.rs`,
  `crab/src/cmd/lfs/store_setup.rs`, `crab/src/cmd/metadb.rs`,
  `crab/src/cmd/mount.rs`, `crab/src/cmd/push.rs`,
  `crab/src/cmd/repack.rs`, `crab/src/cmd/restripe.rs`,
  `crab/src/git/clean.rs`, `crab/src/git/filter_process.rs`,
  `crab/src/git/protected_push.rs`, `crab/src/git/push.rs`,
  `crab/src/git/push_native.rs`, `crab/src/git/remote_helper.rs`,
  `crab/src/git/store_client.rs`, `crab/src/git/upload_pack_wire.rs`,
  `crab/src/import/publish.rs`, `crab/src/metadata/metadb/guard.rs`,
  `crab/src/metadata/metadb/mod.rs`, `crab/src/metadata/shard_sync.rs`,
  `crab/src/read/mod.rs`, and `crab/src/replication/mod.rs`
- focused unit/integration tests under those modules and `crab/tests/`
- `crab/docs/architecture/pb-scale-repositories.md`

**Out of scope**:

- Opening partitioned SlateDB handles.
- Migration of an existing repository; pre-cutover repositories are disposable.
- Environment variables or hidden config fallback.
- Any v2+ Crab-owned schema, fallback reader, or dual-write path.

## Git workflow

- Branch: `advisor/002-layout-descriptor`
- Commits: `feat: add authoritative remote layout descriptor`, then focused
  wiring/tests if useful.
- Do not push without instruction.

## Steps

### Step 1: Add the serialized contract

Define a `#[serde(deny_unknown_fields)]` descriptor with schema version 1,
`RemoteLayout::{Unified, Partitioned}`, chunk/file/receipt partition
bits, recipe page limits, and a content digest or validation function. Use
validated numeric ranges documented by Plan 001. The descriptor path is
`{repo}/layout`; add only a typed `StoreLayout::layout_descriptor_path()` path
builder. Any version other than 1, missing required fields, impossible fanout, or
digest mismatch are corruption/configuration errors.

**Verify**: metadata/storage test command → round-trip, unknown-field,
unknown-version, invalid-range, and corrupt-digest tests pass.

### Step 2: Make init publish layout before the empty manifest

Add an explicit init argument such as `--layout unified|partitioned`; keep
`unified` as the default until rollout changes it. Create `{repo}/layout` with
create-only semantics, read it back and validate it, then create the generation
0 manifest. A retry with the identical descriptor is idempotent; a conflicting
descriptor fails without overwriting either object.

**Verify**: CLI init tests demonstrate default unified, explicit partitioned,
idempotent retry, and conflict rejection.

### Step 3: Dispatch every repository open once

Create one `remote_layout` opener at the composition boundary. Update every
listed current call site to read and validate the descriptor before opening
metadata or mutable paths and pass the resulting enum down; do not let callers
independently probe paths. Before editing, rerun
`rg -l "MetaDb::new|StoreLayout::new" crab/src | sort`; if its result differs
from the in-scope list, STOP and reconcile the plan. Missing, unknown, or
corrupt descriptors fail closed without writes. There is no descriptor-less
default and no path probing; development repositories must be reinitialized.
Record the selected layout in one structured log field without credentials.

**Verify**: an integration test opens each layout, rejects an unknown layout,
and proves no metadata objects are created on descriptor failure.

## Test plan

- Serialization stability and strict decoding.
- Create-only/idempotent init.
- Conflicting init cannot change layout.
- Open chooses one enum and never falls through.
- Descriptor and manifest schemas are canonical v1 for both layouts.

## Done criteria

- [ ] One authoritative `{repo}/layout` contract exists.
- [ ] All opens dispatch from it before metadata writes.
- [ ] Descriptor-less and pre-cutover repositories fail with reinitialize guidance.
- [ ] Unknown/corrupt descriptors fail closed without writes.
- [ ] Format and scoped tests pass; no local `target/` exists.

## STOP conditions

- A real user-owned repository requiring preservation is discovered; the
  no-user hard-cutover premise must be revisited explicitly.
- Implementing dispatch requires duplicating complete push/read pipelines.
- Any code attempts layout detection by listing or probing several prefixes.
- The target volume is unavailable or unwritable.

## Maintenance notes

The descriptor is the sole v1 data contract. Before users, improve it through
hard replacement and delete old code. After real users exist, stop and define a
new compatibility policy rather than prebuilding migration machinery.
