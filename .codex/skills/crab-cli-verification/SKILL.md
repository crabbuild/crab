---
name: crab-cli-verification
description: End-to-end verification pattern for any Crab CLI command against a local RustFS S3-compatible backend. Use when a user asks to verify a `crab` subcommand works from user action to real object-store side effect, especially with large-file repos, RustFS credentials `crab`/`crab`, bucket `crab`, workspace-volume fixtures, push/clone/hydrate proof, or command-specific E2E validation.
---

# Crab CLI Verification

Use this skill to prove a Crab CLI command works end to end, not only that it
parses or compiles. The default proof uses a local RustFS S3 endpoint, the
`crab` bucket, and disposable large-file repos under the workspace volume.

## Setup

1. Work from the CrabBuild repo root.
2. Read `AGENTS.md`, `.kiro/steering/crab.md`, `.kiro/steering/code-style.md`, and `.kiro/steering/crab-e2e-verification.md`.
3. Check `git status --short`; do not overwrite unrelated changes.
4. Keep build artifacts off a full repo-local disk when needed:
   ```bash
   export CARGO_TARGET_DIR=/Volumes/Workspace/CrabBuild/target
   ```

## RustFS Contract

Use a local RustFS backend unless the user explicitly asks for another store.

- S3 endpoint: `http://127.0.0.1:9000`
- Access key: `crab`
- Secret key: `crab`
- Region: `us-east-1`
- Bucket: `crab`
- Fixture root: `/Volumes/Workspace/CrabCLI`
- Remote prefix: `crab://crab/verify-cli/<run-id>`

Start RustFS in a separate terminal/session if it is not already running:

```bash
mkdir -p /Volumes/Workspace/CrabRustFS
RUSTFS_ACCESS_KEY=crab \
RUSTFS_SECRET_KEY=crab \
RUSTFS_CHECK_UPDATE=false \
rustfs server /Volumes/Workspace/CrabRustFS \
  --address :9000 \
  --console-enable \
  --console-address :9001
```

Then export the Crab/AWS environment in the verification shell:

```bash
export AWS_ACCESS_KEY_ID=crab
export AWS_SECRET_ACCESS_KEY=crab
export AWS_REGION=us-east-1
export AWS_DEFAULT_REGION=us-east-1
export AWS_ENDPOINT_URL=http://127.0.0.1:9000
export AWS_ENDPOINT_URL_S3=http://127.0.0.1:9000
export AWS_ALLOW_HTTP=true
export AWS_EC2_METADATA_DISABLED=true
export AWS_VIRTUAL_HOSTED_STYLE_REQUEST=false
export VIRTUAL_HOSTED_STYLE_REQUEST=false
export GIT_TERMINAL_PROMPT=0
```

## Core Pattern

For any CLI, first create a real large-file Crab repo, then run the command
under test inside that repo or a fresh clone, then verify the command's real
side effect.

1. Define the command contract:
   - What user action is being verified?
   - Which repo state must exist before the command?
   - What object-store, worktree, cache, Git, or output side effect proves success?
   - What must remain byte-identical after clone/hydrate?
2. Build a disposable fixture:
   - RustFS bucket exists.
   - `crab init` configures `crab://crab/verify-cli/<run-id>`.
   - `crab track '*.bin'` declares large-file ownership.
   - Deterministic large files are created under `/Volumes/Workspace/CrabCLI/<run-id>`.
   - `crab add`, `git commit`, and `crab push` write real xorbs/shards/refs.
   - `crab clone` and `crab hydrate --all` reconstruct byte-identical files.
3. Run the specific CLI in the fixture.
4. Verify the command's own side effect, not just exit code.
5. Re-run clone/hydrate or fsck when the command can affect remote content, metadata, refs, xorbs, shards, cache, or working-tree materialization.

## Fixture Helper

Create the baseline RustFS fixture:

```bash
.codex/skills/crab-cli-verification/scripts/create-rustfs-cli-fixture.sh
```

Run a CLI command inside the hydrated clone after the fixture is created:

```bash
.codex/skills/crab-cli-verification/scripts/create-rustfs-cli-fixture.sh \
  --command-cwd clone -- status --json
```

Run a CLI command inside the original pushed repo:

```bash
.codex/skills/crab-cli-verification/scripts/create-rustfs-cli-fixture.sh \
  --command-cwd seed -- fsck --json
```

The helper writes `env.sh`, logs, hashes, seed repo, and clone paths under the
run root. Source `env.sh` for follow-up manual checks in the same fixture.

## Command-Specific Proof

Pick the fixture state that matches the command:

- Read-only inspection commands (`status`, `ls-files`, `du`, `stat`, `doctor`, `env`, `fsck`): run in `seed` and/or `clone`; assert output schema and expected facts about the large tracked files.
- Materialization commands (`hydrate`, `dehydrate`, `checkout`, `clone`): start from a pointer or clone state; assert file sizes and SHA-256 before and after.
- Remote mutation commands (`push`, `gc`, `repack`, `compact`, `tier`, `optimize`, `replica`, `workflow-cache`): run against `crab://crab/verify-cli/<run-id>` only; assert changed object-store keys, refs, reports, and a fresh clone/hydrate still reconstructs bytes.
- Cache commands: set `CRAB_CACHE_DIR` inside the run root; assert cache files or stats change and hydrated bytes remain identical.
- Negative-path commands: make the failure condition explicit, capture stderr/JSON, and assert the documented exit code or error code.

Do not use bucket-wide destructive operations. If testing deletion, scope it to
the run prefix and prove no command touches outside `verify-cli/<run-id>`.

## Closeout

Report:

- Command contract verified.
- Run ID and remote prefix.
- Fixture root and log/report files.
- Exact commands run.
- Concrete side effects observed.
- Byte-identical proof after clone/hydrate.
- Any proof skipped and why.

