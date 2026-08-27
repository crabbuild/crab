# Phase 0: Define the LFS compatibility contract and baseline

> **Executor instructions**: Follow every step and verification gate. Update the Phase 0 row in `advisor-plans/lfs/README.md` when complete. Do not claim broader compatibility than the tests prove.
>
> **Drift check (run first)**: `git diff --stat 2cbd0d92..HEAD -- crab/src/cmd/lfs crab/src/lfs crab/docs packages/web/content/docs .github/workflows`
> Compare the current-state facts below with live code. Stop on a contract mismatch.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none
- **Category**: direction, tests, docs
- **Planned at**: commit `2cbd0d92`, 2026-08-25

## Why this matters

The code and docs currently use “full Git LFS compatibility” for a proven standalone custom-agent integration that deliberately bypasses the standard HTTP API. This phase establishes an honest support matrix, preserves the working behavior with transcript tests, and creates repeatable performance evidence before deeper refactors.

## Current state

- `crab/src/cmd/lfs/install.rs:18` configures `lfs.standalonetransferagent=crab`, so Git LFS bypasses its server API.
- `crab/src/lfs/transfer_agent.rs:141` implements the JSON-lines custom-agent protocol.
- `crab/docs/design/lfs.md:90` and `crab/docs/guides/lfs.md:13` say “full” or “drop-in” compatibility.
- `crates/crab-auth-server/README.md:1` explicitly says that crate is not a long-running HTTP server.
- `crab/src/cmd/lfs/store_setup.rs:61` rejects managed LFS writes outside protected-push authorization, so managed standalone uploads are not currently a passing profile.
- `crab/src/cmd/lfs/filter_process.rs:32` discards remote-resolution errors and non-lazy smudge can return the pointer successfully; this is not valid required-filter behavior unless skip policy is explicit.
- `crab/src/lfs/config.rs:72` manually reads selected config files and gives tracked `.lfsconfig` precedence over Git config, unlike the official Git LFS contract.
- The official protocol says standalone agents bypass the API and current transfers are serial per process: `docs/custom-transfers.md` in the Git LFS repository.
- Existing live evidence: Git LFS 3.7.1 completed upload, push, clone, selective fetch/checkout, Crab fsck, and Git LFS fsck for 10,900 distinct 1 MiB objects. Preserve this as historical evidence, not a release gate.

## Commands you will need

Use a unique target path for this worktree in every Cargo invocation.

| Purpose | Command | Expected |
|---------|---------|----------|
| LFS crate | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo test -p crab-lfs --locked` | 22+ tests pass |
| CLI LFS tests | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo test -p crab --lib lfs --locked --no-default-features` | all filtered tests pass |
| Docs links | `cd packages/web && npm run check:links` | exit 0 |

## Scope

**In scope**:
- `crab/docs/design/lfs.md`
- `crab/docs/architecture/lfs-compatibility.md`
- `crab/docs/guides/lfs.md`
- relevant `packages/web/content/docs/` LFS pages found by search
- transfer-agent transcript tests beside `crab/src/lfs/transfer_agent.rs`
- a new deterministic LFS compatibility harness under `crab/scripts/e2e/`

**Out of scope**:
- transfer mechanics or storage layout changes
- an HTTP server
- live credential values or committed `.env` files
- changing CLI output solely to mimic Git LFS where no public contract exists

## Git workflow

- Branch: `advisor/lfs-phase-0-contract`
- Conventional commits, for example `test(lfs): define compatibility profiles` and `docs(lfs): state supported integration modes`.
- Do not push unless instructed.

## Steps

### Step 1: Add a versioned support matrix

Define three named profiles in the design and user docs: `crab-native`, `git-lfs-standalone-direct`, and `git-lfs-standalone-managed`. For every profile state requirements, supported operations, auth model, locking model, tested Git/Git LFS versions, and release status. Mark managed upload as unsupported until Phase 6 and remove unconditional “drop-in” claims. Record standard HTTP discovery as an external-server integration, not a Crab product profile.

Document current installation scope explicitly: the default currently writes an unconditional global standalone-agent setting, which affects unrelated repositories. Mark URL/repository-scoped installation as required production behavior in Phase 2.

**Verify**: `rg -n "full Git LFS compatibility|drop-in replacement" crab/docs packages/web/content/docs` returns no unconditional compatibility claim.

### Step 2: Add protocol transcript fixtures

Add table-driven tests that feed JSON-lines sequences to the transfer agent for init/upload/download/terminate. Cover malformed OID, declared-size mismatch, missing path, unknown event, duplicate OID, progress ordering, transfer error, and clean termination. Assert stderr/stdout separation and exactly one terminal event per requested object.

**Verify**: the CLI LFS test command passes and test names identify each protocol property.

### Step 3: Add a deterministic local compatibility harness

Create a script that accepts executable path, Git LFS executable, remote URL, object count, commit count, current path count, min/max size, seed, and evidence directory. It must generate deterministic content without retaining two copies, use skip-smudge clone, verify ref equality, run checkout and both fsck implementations, record wall time/peak RSS/object counts/bytes, and redact credentials. Default to a small local profile; require explicit flags for expensive profiles.

**Verify**: run the harness against a local/in-memory or caller-provided RustFS endpoint with 3 commits, 10 paths, and 1–2 MiB objects; evidence JSON records the seed and every check as passed.

### Step 4: Correct source maps and test-level language

Replace stale `crab/src/lfs/object_store.rs` references with `crates/crab-lfs/src/object_store.rs`. Label unit-only behavior as Level 1, real object-store integration as Level 3, and production qualification as Level 5.

**Verify**: `rg -n "crab/src/lfs/object_store.rs" crab/docs packages/web/content/docs` returns no matches; docs link check exits 0.

## Test plan

- Transcript tests cover protocol success and error paths without cloud credentials.
- Harness smoke proves a real Git action produces a real object-store side effect and a byte-identical checkout.
- Preserve existing 22 `crab-lfs` tests and the complete filtered CLI LFS suite.

## Acceptance criteria

- [ ] The four compatibility profiles are documented with no ambiguous “full” claim.
- [ ] Protocol transcript tests include at least the eight named cases and all pass.
- [ ] The deterministic harness emits redacted machine-readable evidence.
- [ ] Small live smoke verifies push, clone, selective fetch/checkout, ref equality, and both fsck commands.
- [ ] No credentials or endpoint secrets appear in tracked files or test output.

## STOP conditions

- The installed Git config no longer uses standalone transfer mode.
- Supporting docs reveal a shipped HTTP endpoint not represented in this plan.
- The harness would need to modify an existing external qualification checkout.
- Any test requires hardcoded live credentials.

## Maintenance notes

The support matrix is a release contract. Every later phase must update it only when its qualification profile passes; command-name similarity alone is not compatibility proof.
