# Plan 010: Qualify providers and hard-cut to canonical v1

> **Executor instructions**: Qualify one canonical Crab-owned v1 layout and
> replace all pre-cutover development state. Do not implement migration,
> rollback, dual read/write, aliases, or compatibility fallbacks. Run every
> gate and update `plans/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat 1f9dae74..HEAD -- crab/src/cmd/init.rs crab/src/main.rs crab/src/core/remote_layout.rs crates/crab-metadata/src crates/crab-storage/src crab/docs crab/scripts/e2e .github/workflows`
> If any Crab-owned version field, provider write contract, or repository init
> path changed, reconcile the canonical v1 inventory before editing.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/006-streaming-dedup-and-push.md` and provider-relevant
  gates from `plans/008-inventory-gc-and-fsck.md`; Plan 007 only if cache is a
  release requirement
- **Category**: release, tests, tech-debt
- **Planned at**: commit `1f9dae74`, reconciled 2026-08-29
- **Delivery status**: BLOCKED — the canonical v1 implementation, full Rust
  test target, qualification-script suite, docs links, and 115/115-check local
  RustFS canary pass. Retained real AWS, GCS, and Azure evidence is absent;
  strict repo-wide Clippy reports 449 warnings-as-errors across the monolithic
  CLI, including both touched and unrelated surfaces.

## Why this matters

Crab has no users or stored compatibility obligation. Migration and rollback
machinery would create multiple writers, readers, and failure modes before any
contract needs preservation. The correct release path is one canonical v1,
destructive reinitialization of development repositories, and independent
proof that each advertised object-store provider implements the required
conditional-write, multipart, range-read, and receipt semantics.

## Current state

- Plan 002 owns the canonical v1 layout descriptor and fail-closed open path.
- Existing production-shaped RustFS and concurrent-push scripts are useful
  qualification starting points, but no complete retained provider matrix
  proves the final v1 contract.
- `crab/docs/architecture/git-integration.md:55` identifies S3-compatible
  behavior as implemented/qualified; that does not prove GCS or Azure
  conditional-write and identity semantics.
- Existing pre-cutover repositories, staging databases, manifests, shards, and
  push plans are disposable development state. No runtime code may translate
  or infer them.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Full Rust tests | `cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main make test` | all pass |
| Clippy | `cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main make clippy` | exit 0 |
| Qualification and scale scripts | `python3 -m unittest discover -s crab/scripts/e2e -p 'test_*.py'` | all pass |

Run provider E2E only in isolated accounts/prefixes with explicit safe cleanup.
Never run bucket-wide GC. Use the required external Cargo target.

## Scope

**In scope**:

- canonical v1 init/open/version owners under `crates/crab-metadata/src`,
  `crates/crab-storage/src`, and `crab/src/core/remote_layout.rs`
- `crab/src/cmd/init.rs` and `crab/src/main.rs`
- provider qualification scripts/tests under `crab/scripts/e2e/`
- `.github/workflows/pb-provider-qualification.yml`
- architecture, provider, and operational docs under `crab/docs/`

**Out of scope**:

- Any migration command or state machine.
- Old-layout retention, reverse migration, rollback window, or dual write.
- Descriptor-less/open-by-probing behavior.
- Automatic deletion of production/user data; the premise is that none exists.
- Advertising a provider without completed retained evidence.

## Git workflow

- Branch: `advisor/010-provider-v1-cutover`
- Commits: delete obsolete format paths, provider harness, canary/docs.
- Do not push, deploy, or touch shared provider prefixes without instruction.

## Steps

### Step 1: Inventory and collapse every Crab-owned version contract

Search serialized public/local contracts for numeric versions and enumerate
their single v1 writer/reader: layout descriptor, manifest, pointer, shard,
staging schema, prepared push plan, receipts, and any CLI JSON schema. For each
contract touched by Plans 001-009:

- keep exactly one v1 serializer and one strict v1 reader;
- replace higher numeric labels with v1 in place;
- delete legacy/version dispatch, conversion, alias, migration, and dual-write
  code plus obsolete fixtures/tests;
- fail unknown/non-v1 input with an actionable reinitialize/restage error;
- do not change external protocol names such as Git protocol v2.

**Verify**: add a maintained contract inventory test that asserts the expected
v1 constants and rejects non-v1 fixtures. `rg` may identify external protocol
or dependency versions; those are not Crab format violations.

### Step 2: Make destructive development reinitialization explicit

Document and test one supported cutover:

1. discard pre-cutover local staging/cache state;
2. delete/recreate isolated development repository storage;
3. run canonical v1 init;
4. re-add and push source files;
5. fresh clone/hydrate and verify byte identity.

Do not put destructive automatic deletion in normal open. A mismatched remote
fails closed and tells the operator to run the explicit development reset
workflow against the exact repository scope.

**Verify**: an isolated E2E starts with a non-v1 fixture, proves normal open
refuses it, runs explicit reset, and completes v1 add→push→clone→digest.

### Step 3: Qualify provider contracts independently

For S3-compatible, GCS, and Azure, retain evidence for create-only and
match-token writes, ETag/version identity, multipart completion/abort,
file-backed staged multipart, exact range reads, pagination used outside GC,
retry/error mapping, cancellation, and origin receipts. Mark a provider
unsupported until every required row passes on the required real service or
explicitly approved emulator.

**Verify**: CI artifacts include provider, region/emulator, SDK/object_store
version, Crab commit, commands, object/request metrics, and pass/fail matrix.

### Step 4: Run the canonical v1 canary matrix

Exercise first add/push, duplicate add/push, restage before push, skip then
ordinary add, fresh clone/hydrate/digest, narrow range, concurrent disjoint and
same-ref push, cache outage, multipart failure, crash boundaries, protected
push, and non-atomic CAS replan. Include multi-TB materialized internal evidence
and bounded larger metadata simulation; distinguish payload throughput from
control-plane proof.

**Verify**: every run emits the Plan 001 evidence schema and passes exact
byte-identity plus the add/push release gates in `plans/README.md`.

### Step 5: Change defaults only after retained proof

Once every required provider and canary row passes, make canonical v1 the only
init/open path and delete any temporary development selector. Update CLI help,
architecture docs, provider runbooks, incident reset instructions, and docs
links. Do not preserve the previous default behind a flag.

**Verify**: full Rust tests, clippy, script tests, and docs link checks pass;
`rg` finds no retired Crab-owned format reader/writer or migration command.

## Test plan

- Contract inventory and strict non-v1 rejection.
- Explicit isolated destructive reset and byte-identical reconstruction.
- Provider-specific conditional-write/multipart/range/receipt matrix.
- Add/push failure injection and concurrency canary.
- No-migration assertion: CLI help and source contain no layout migration,
  rollback, dual-write, or compatibility fallback surface.

## Done criteria

- [x] Every Crab-owned serialized contract has one canonical v1 reader/writer.
- [x] Unknown/non-v1 state fails closed with explicit reset/restage guidance.
- [x] No migration, rollback, dual-read/write, or compatibility code remains.
- [ ] Every advertised provider has retained qualification evidence.
- [x] Canonical v1 add→push→fresh hydrate is byte-identical under the local
  RustFS canary matrix.
- [x] Full Rust tests, qualification-script tests, and docs links pass.
- [ ] Strict repo-wide Clippy passes; the focused staging, storage, xet, and
  auth-server crates changed by add/push hardening pass their strict gates.

## STOP conditions

Stop and report if:

- Any real user-owned or production data requiring preservation is discovered.
- An external protocol/dependency forces a version name that cannot be hidden
  behind Crab's canonical v1 contract.
- Provider qualification lacks isolated credentials/scope or safe cleanup.
- Hard cutover would delete data outside an explicitly named development repo.
- A change proposes compatibility code “temporarily.”

## Maintenance notes

The no-compatibility premise expires when Crab has real users or retained
production data. At that point, stop hard-cutting formats and establish an
explicit compatibility policy. Until then, reviewers should reject version
bumps and demand deletion of replaced code.
