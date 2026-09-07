# Plan 001: Add agent guides to all 21 shared crates

> Executor: read this plan and the inherited repository instructions before work.
> Implement all six batches. This plan needs no prior conversation context.
> Update `advisor-plans/README.md` when implementation is complete; do not mark
> it done merely because the plan exists.

## Status

- Priority: P1
- Effort: M–L; documentation research across 21 crates
- Risk: low runtime risk; inaccurate or stale instructions are the main risk
- Category: docs / developer experience
- Depends on: none
- Planned at: `ebd0e40d14c`, 2026-09-06
- Delivery: 21 `AGENTS.md` files and 21 relative `CLAUDE.md` symlinks

## Why this matters

Agents currently have a workspace-level subsystem map but no individual crate
reading guides. Each task rediscovers local entry points, consumers, feature
requirements and lifecycle constraints. Add concise, source-backed navigation
so agents can reach the right code and verification without duplicating global
rules or turning these files into design histories.

## Current state and evidence

- `Cargo.toml` declares 21 `crates/crab-*` members plus the `crab` product crate.
  Only the 21 shared/server crates are in this plan; `crab/` is excluded.
- All 21 have a README. None has a crate-local AGENTS.md or CLAUDE.md at the
  planned commit. Check again before writing; do not overwrite new guidance.
- `crates/AGENTS.md` provides the subsystem and dependency ownership map.
  Its opening explicitly says: `Root AGENTS.md also applies.`
- Root `AGENTS.md` requires: `New AGENTS.md: add sibling CLAUDE.md symlink;
  edit AGENTS.md only.` Use `CLAUDE.md -> AGENTS.md`, not a second text file.
- `crates/crab-write/src/lib.rs:1` says:
  `//! Shared publication mechanics; authentication and product policy stay with callers.`
  Its `journal.rs` API documents leases, publication and cancellation. This is
  a useful ownership example: guide readers to the contract rather than copying it.
- `crates/crab-staging/src/lib.rs:17` starts concurrency documentation;
  `tests/unit/` contains property tests. Test discovery must include inline
  tests and path-mounted unit modules, not only top-level integration tests.
- `CONTEXT.md` distinguishes a worktree (Git checkout and its metadata) from
  a working tree (checked-out files). Preserve the distinction in guides.
- Existing CI already includes split-crate interface, behavior, Clippy and
  test checks in `.github/workflows/architecture.yml`. Do not propose or add
  duplicate gates as part of this documentation work.

## Scope and exclusions

Only create `crates/<name>/AGENTS.md` and `crates/<name>/CLAUDE.md` for the
21 names listed below. Update this plan's status in `advisor-plans/README.md`.
No source, README, Cargo manifest/lockfile, CI, root/scoped guide, baseline,
example or test changes. Do not introduce new policy, compatibility shims or
runtime behavior. If a source defect is noticed, report it separately.

Use the existing worktree. If a branch is needed, use `codex/crate-agent-guides`.
Do not commit, push or create a PR unless requested. Preserve unrelated changes.

## Guide shape

Use these headings consistently. Aim for roughly 80–140 lines; this is an
editing target, not a quota. Small crates should be shorter. Avoid nested lists
and long paragraphs. Use repository-root-relative file references inside guides.

```markdown
# <crate-name>

Root AGENTS.md and crates/AGENTS.md apply. Read README.md for crate usage.

## Purpose and ownership
<One short paragraph; owned responsibilities and neighboring owners.>

## Read first
<Ordered route of 3–6 source files, each with a reason to read it.>

## Common changes
| Task | Start here | Also inspect |
| ... | real source path | caller, callee or sibling path |

## Invariants
<3–6 local constraints with source references; link detailed contracts.>

## Features and platform
<Exact feature names from Cargo.toml; defaults, optional paths and OS needs.>

## Verification
<Focused, copyable commands; distinguish local tests from dedicated/live CI.>

## Related documentation
<Existing README, relevant docs/tests and when to update this guide.>
```

For crates without explicit Cargo features, say so rather than inventing
`default` or `all` features. Distinguish dependency-enabled capabilities from
crate features. Server entry points may be binaries; their caller map should
start at that binary and router rather than inventing a library consumer.

A reading guide is not an API inventory. Every path and rule must help a future
agent make a concrete decision. Do not paste root style rules, entire READMEs,
feature dependency trees, transient line counts, benchmark numbers or plan IDs.
Use a small diagram only when a lifecycle is clearer than an ordered route.

## Crate-specific investigation routes

Paths below are relative to `crates/<crate>/`. Every listed route existed at
planning time. These are research starting points, not a claim that every
function or dependency contract has already been audited. Read the relevant
whole functions, callers, callees, sibling paths and tests before writing
behavioral instructions. Verify focus statements against current source.

| Crate | Read in order | Guide emphasis |
|---|---|---|
| crab-types | `src/lib.rs` → `src/pointer.rs` → `src/storage.rs` → `src/error.rs` | Serialized shapes and validation; distinguish worktree from working tree; inspect consumers before changing wire contracts. |
| crab-git | `src/lib.rs` → `src/discover.rs` → `src/receive_plan.rs` → `src/pack.rs` → `src/refname.rs` | Git mechanics versus command policy; protocol parsing, ref namespaces and pack integrity; facade is optional. |
| crab-diff | `src/lib.rs` → `src/types.rs` → `src/chunk_sequence.rs` → `src/chunk_comparator.rs` → `src/pointer_pairs.rs` | Pure comparisons and chunk ordering; no storage or hydration orchestration. |
| crab-xet | `src/lib.rs` → `src/hash.rs` → `src/xorb/builder.rs` → `src/reconstruction.rs` → `src/shard.rs` | Hashes, serialized Xorbs and complete term coverage; trace upstream Xet contracts before changing formats. |
| crab-storage | `src/lib.rs` → `src/layout.rs` → `src/store.rs` → `src/cas.rs` → `src/error_map.rs` | Storage keys, conditional writes and retry classification; inspect object_store contract and distinguish transport from caller policy. |
| crab-metadata | `src/lib.rs` → `src/manifests.rs` → `src/manifest_store.rs` → `src/ref_journal.rs` → `src/git_visibility.rs` | Payload-only versus persistence features; publication visibility, codecs and explicit writer close; follow git_object_locator/ for catalog tasks. |
| crab-staging | `src/lib.rs` → `src/index.rs` → `src/segment.rs` → `src/recovery.rs` → `src/push_plan.rs` | Durability, complete file-version chunks, locking and recovery; read recipe.rs and multipart_resume.rs for those tasks. |
| crab-coordination | `src/lib.rs` → `src/push_lock.rs` → `src/lease_operation.rs` → `src/gc_fence.rs` → `src/write_coordinator.rs` | Per-ref serialization, lease renewal/release and GC fencing; distinguish object-store and active-active backends. |
| crab-write | `src/lib.rs` → `src/journal.rs` → `src/namespace.rs` → `src/generation.rs` → `src/catalog.rs` | Caller-held leases and snapshots; commit uncertainty, publication versus read readiness, cancellation draining and cleanup. |
| crab-cache | `src/lib.rs` → `src/key.rs` → `src/path_class.rs` → `src/local_cache.rs` → `src/lifecycle.rs` | Cache identity, admission and lifecycle ownership; remote contracts in cache_client.rs; keep routing policy boundaries explicit. |
| crab-cache-store | `src/lib.rs` → `src/xorb_read.rs` | Canonical cache/origin composition; verified reads, error-source preservation and sibling caller behavior. |
| crab-read | `src/lib.rs` → `src/selection.rs` → `src/hydrator.rs` → `src/term_resolver.rs` → `src/store_client.rs` | Selection and verified hydration; branch to upload_pack.rs and fetch_admission.rs for Git transport tasks. |
| crab-remote-git | `src/lib.rs` → `src/repository.rs` → `src/snapshot.rs` → `src/operation.rs` → `src/reader.rs` | Bounded filesystem-free reads, snapshot/operation lifecycle and immutable pack evidence; inspect pack.rs for range reads. |
| crab-lfs | `src/lib.rs` → `src/object_store.rs` → `src/lock.rs` | LFS object identity, integrity and locks; pointer parsing belongs to crab-git. |
| crab-auth | `src/lib.rs` → `src/credentials.rs` → `src/credential_provider.rs` → `src/token_cache.rs` → `src/protected_push.rs` | Credential and scope contracts, secret handling and optional clients; use managed/ for managed-service API tasks. |
| crab-auth-store | `src/lib.rs` → `src/refreshing_store.rs` → `src/gateway_store.rs` → `src/managed_repository.rs` | Resolved credentials to storage; refresh and gateway lifecycles; separate credential policy from storage mechanics. |
| crab-auth-server | `src/lib.rs` → `src/receive.rs` → `src/receive/git_workspace.rs` → `src/view.rs` → `src/doctor.rs` | Protected receive and view composition; authorization boundary, workspace cleanup and binary entry points from Cargo.toml. |
| crab-cache-server | `src/lib.rs` → `src/server.rs` → `src/state.rs` → `src/handlers.rs` → `src/cache_store.rs` | Request/auth/origin flow, persistence and eviction; config.rs and preflight.rs for operations; service lifecycle ownership. |
| crab-http-server | `src/main.rs` → `src/server.rs` → `src/app.rs` → `src/auth.rs` → `src/receive.rs` | HTTP routing, auth and native Git receive; branch to contents.rs, git.rs, lfs.rs and maintenance.rs; embedded frontend build prerequisites. |
| crab-vfs | `src/lib.rs` → `src/pipeline.rs` → `src/engine.rs` → `src/hydration.rs` → `src/mount_runtime.rs` | Mount admission and teardown, leases, IPC and worker shutdown; separate FUSE/NFS adapters from canonical read orchestration. |
| crab-workflow | `src/lib.rs` → `src/yaml.rs` → `src/graph.rs` → `src/scheduler.rs` → `src/executor.rs` | Strict parse, deterministic plan and execution; lockfile.rs, journal.rs and resume.rs for persisted state; CLI owns product invocation. |

## Feature inventory at the planned commit

Use this to detect drift; regenerate from Cargo.toml before documenting.
An omitted `default` key does not imply the absence of optional features.

| Crate | Declared features |
|---|---|
| crab-types | None declared |
| crab-git | `facade` |
| crab-diff | None declared |
| crab-xet | `default`, `chunker`, `upload-concurrency` |
| crab-storage | `default`, `test-support` |
| crab-metadata | `default`, `file-index-reader`, `local-index`, `remote-index`, `storage` |
| crab-staging | None declared |
| crab-coordination | `default`, `object-store-lock`, `coordinator-dynamodb`, `coordinator-spanner`, `coordinator-cosmosdb` |
| crab-write | None declared |
| crab-cache | `default`, `active-probe`, `local-cache`, `remote-client`, `xet-chunk-cache` |
| crab-cache-store | `default`, `remote-client` |
| crab-read | None declared |
| crab-remote-git | None declared |
| crab-lfs | None declared |
| crab-auth | `default`, `aws-oidc-client`, `azure-entra-client`, `crab-auth-client`, `gcp-workload-identity-client`, `oidc-client` |
| crab-auth-store | `default`, `refreshing-store`, `managed-service` |
| crab-auth-server | None declared |
| crab-cache-server | None declared |
| crab-http-server | None declared |
| crab-vfs | `default`, `fuse`, `nfs`, `gix-facade` |
| crab-workflow | `default`, `crash-injection`, `gix-facade`, `testing`, `watch` |

## Implementation steps

### 1. Establish baseline and inspect inherited guidance

Run from repository root:

```sh
git status --short
git diff --stat ebd0e40d14c..HEAD -- AGENTS.md CONTEXT.md crates Cargo.toml
cat AGENTS.md crates/AGENTS.md CONTEXT.md
```

Record pre-existing changes. Read each crate's README, Cargo.toml and lib.rs
before its batch. Inspect binary declarations and build scripts for servers.
If source moved, refresh the route from current source; if an ownership rule
conflicts with code, report the conflict instead of publishing it as fact.

Verify the matrix contains exactly the current `crates/` workspace members:

```sh
python3 - <<'PYVERIFY'
import tomllib
from pathlib import Path
workspace = tomllib.loads(Path('Cargo.toml').read_text())['workspace']['members']
crates = sorted(p for p in workspace if p.startswith('crates/'))
assert len(crates) == 21, crates
assert all((Path(p) / 'README.md').is_file() for p in crates)
print('21 crate members; all README entry points exist')
PYVERIFY
```

Expected: the printed success line and exit 0. A changed membership requires
reconciling scope before claiming all-crate coverage.

### 2. Write and verify six batches

1. Contracts/mechanics: types, git, diff, xet.
2. Persistence/publication: storage, metadata, staging, coordination, write.
3. Read/cache: cache, cache-store, read, remote-git, lfs.
4. Credentials: auth, auth-store.
5. Services/mounts: auth-server, cache-server, http-server, vfs.
6. Workflow: workflow.

All names have the `crab-` prefix. Within each batch, for each crate:

1. Read the route and the whole relevant functions. Search direct consumers
   using the Cargo package name and Rust crate identifier, including aliases
   and re-exports. Follow at least one real entry-point → owner → callee path.
2. Build a temporary evidence table: local responsibility, source symbol,
   caller/entry point, callee, sibling, test and feature gate. Cover each
   invariant proposed for the guide. Missing evidence means narrow the claim;
   do not fill a cell with a guess. Trace dependency-backed behavior to the
   locked dependency's types/source/docs when making a claim about it.
3. Write the guide with the shape above. Include at least two common-change
   routes, one caller/entry-point reference and one callee/neighbor reference.
   Name actual test modules/files and a narrow test command for the important
   contract. Do not claim the tests passed merely because their source exists.
4. Create the relative symlink only if no file/symlink already occupies it:
   `ln -s AGENTS.md crates/<crate>/CLAUDE.md`. Substitute the actual crate name.
   Never force-overwrite a pre-existing file.
5. Check both local paths and paths into sibling crates. Read the generated
   guide from top to bottom and remove duplication or unsupported guarantees.

Useful searches (substitute a real crate or symbol):

```sh
rg -n 'crab_staging::|crab-staging' crab/src crates Cargo.toml
rg -n '#\[test\]|#\[tokio::test|proptest!|#\[path' crates/crab-staging
rg -n 'StagingArea|flush_pending' crates/crab-staging crab/src
```

Expected: actual callers and test source for the selected route. Search results
are navigation, not proof; inspect their enclosing functions. Use equivalent
queries for every crate, not just staging.

After each batch run `git diff --check` (expected exit 0) and the structural
verification below with `GUIDE_CHECK_PARTIAL=1`. Newly created untracked files
must also pass the Python check; Git diff alone does not inspect them.

### 3. Verify commands without unnecessary builds

This task changes documentation only. Do not run all crate suites or cloud
qualification just to add guides. Validate command package names, test target
names and feature flags against manifests, test declarations and existing CI.
If a command cannot be established statically, label its validation limitation
in the handoff; do not invent a passing result.

Source command recipes from:

- `crab/scripts/check-crate-behavior.py` — focused test filters.
- `crab/scripts/check-crate-interface-builds.py` — interface/feature slices.
- `crab/Makefile` — split-crate Clippy/test targets.
- `.github/workflows/architecture.yml` — dedicated runner prerequisites.
- Relevant live/platform workflows (NFS, cache-service, HTTP, Git protocol).

A complete example for future storage changes, backed by the existing behavior
check script:

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c"   cargo test -p crab-storage --locked provider_store
```

On another worktree choose a unique target directory for that worktree. Before
any compilation, verify `$HOME/Workspace` resolves to the mounted workspace
volume and the chosen target is writable. Never silently use local `target/`
or share another worktree's build directory. Respect the inherited install
policy if a dependency is missing. Do not run installation for this docs task.

Commands included in final guides should be runnable from repository root and
carry their CARGO_TARGET_DIR assignment on every compilation invocation.
Distinguish platform-only or live-service tests, prerequisites and side effects.
Do not put real credentials, cloud resource identifiers or destructive GC
commands into guides. Do not prescribe broad `--all-features` where exact
feature slices are required.

### 4. Check coverage, references, headings and symlinks

Run the following from repository root after all batches. For intermediate
batches prefix the command with `GUIDE_CHECK_PARTIAL=1`.

```sh
python3 - <<'PYVERIFY'
import os
import re
import tomllib
from pathlib import Path

root = Path.cwd()
members = tomllib.loads((root / 'Cargo.toml').read_text())['workspace']['members']
crates = sorted(p for p in members if p.startswith('crates/'))
assert len(crates) == 21, crates
partial = os.environ.get('GUIDE_CHECK_PARTIAL') == '1'
headings = [
    'Purpose and ownership', 'Read first', 'Common changes', 'Invariants',
    'Features and platform', 'Verification', 'Related documentation',
]
count = 0
for member in crates:
    directory = root / member
    guide = directory / 'AGENTS.md'
    alias = directory / 'CLAUDE.md'
    if partial and not guide.exists():
        assert not alias.exists() and not alias.is_symlink(), str(alias)
        continue
    assert guide.is_file() and not guide.is_symlink(), str(guide)
    assert alias.is_symlink(), str(alias)
    assert os.readlink(alias) == 'AGENTS.md', str(alias)
    assert alias.resolve() == guide.resolve(), str(alias)
    body = guide.read_text()
    assert body.startswith('# ' + directory.name + '\n'), str(guide)
    for heading in headings:
        assert f'## {heading}' in body, (member, heading)
    assert 'crates/AGENTS.md' in body, member
    assert 'CARGO_TARGET_DIR=' in body, member
    assert not any(line.rstrip() != line for line in body.splitlines()), member
    assert body.endswith('\n'), member
    # Root-relative references only; strip an optional source line suffix.
    refs = re.findall(r'`((?:crates|crab|packages|\.github)/[^`\s]+)`', body)
    assert len(set(refs)) >= 3, (member, 'too few concrete source references')
    for ref in refs:
        target = re.sub(r':\d+(?:-\d+)?$', '', ref)
        assert (root / target).exists(), (member, ref)
    count += 1
assert count > 0
assert partial or count == 21
print(f'{count}/21 guides checked; headings, source paths and symlinks valid')
PYVERIFY
git diff --check
git status --short
```

Expected final result: `21/21 guides checked; headings, source paths and
symlinks valid`; Git diff check exits 0. The checker validates structure,
not factual accuracy, test execution, arbitrary Markdown links or test filters.
Review those manually against source.

### 5. Review factual quality and hand off

Review every guide against its temporary evidence table. Check feature names
and defaults against the live manifest, cleanup rules against implementation,
and test commands against the actual test declarations. Ask: is this the best
navigation guide for the crate, or merely a plausible module list?

Confirm exactly 42 guide/symlink additions and the permitted status update,
adjusting only for pre-existing changes recorded at baseline. Use `git status
--short --untracked-files=all` because new files are not shown by ordinary diff.
Mark the plan DONE only once all 21 guides exist and checks pass. Report the
structural checks run and any command validation gaps; no runtime change or
runtime qualification is implied.

## Done criteria

- [ ] All 21 workspace crate members have a substantive AGENTS.md.
- [ ] All 21 CLAUDE.md files are relative symlinks to their sibling AGENTS.md.
- [ ] Structural checker prints 21/21 and exits 0; `git diff --check` exits 0.
- [ ] Every guide has source-backed ownership, reading routes, invariants,
      feature/platform notes and focused verification commands.
- [ ] Every stated contract has source/test/consumer evidence; no invented APIs,
      feature names, passing-test claims or blanket copied instructions.
- [ ] No source, READMEs, manifests, CI, baselines or existing root guides changed.
- [ ] Plan status updated in advisor-plans/README.md; final handoff states limits.

## Stop conditions

- A pre-existing guide or alias would be overwritten: preserve it and reconcile
  its instructions first; report a blocking conflict rather than forcing a write.
- Workspace crate membership differs from 21: report the scope difference.
- A lifecycle or ownership assertion conflicts with source or inherited rules:
  omit the unsupported claim, record the gap, and seek resolution if it prevents
  a useful guide. Continue independent crates.
- Accurate guidance requires source fixes, new policy or out-of-scope edits:
  record follow-up work; do not implement it in this task.
- Workspace volume is unavailable when compilation is actually needed:
  stop that verification and report the missing prerequisite.

## Maintenance

Update the local guide when public entry points, ownership, feature flags,
cleanup responsibilities or verification routes change. Keep stable source
paths and symbol names rather than fragile line-number inventories. Deep API
contracts remain in rustdoc/source; detailed usage remains in READMEs. README
rewrites, executable examples, code refactors and permanent CI doc gates are
separate follow-up work.
