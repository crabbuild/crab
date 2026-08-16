---
name: crab-cli-core
description: Shared implementation and routing guidance for the Crab CLI. Use whenever a task changes Crab command dispatch, configuration, structured output, error handling, CLI architecture, or when a Crab CLI request needs to be routed to a focused skill. Read the command map before choosing an owner.
compatibility: Crab monorepo with Rust 2024, Cargo, and repository-local `.codex/skills`.
---

# Crab CLI core

Use this skill for cross-cutting Crab CLI work and as the routing layer for
specialized skills. Keep the command surface boring: one canonical path,
narrow APIs, typed errors, and evidence for externally visible behavior.

## Route first

Read `references/command-map.md`, then choose the narrowest skill. Use the
specialized skill for the user-facing workflow and keep this skill active for
cross-cutting contracts. Existing `crab-cli-verification` and
`crab-release-publish` take precedence for their respective tasks.

## Implementation loop

1. Read `AGENTS.md`, the relevant command definition in
   `crab/src/main.rs`, the complete command module, its callers and callees,
   sibling implementations, tests, and the relevant guide.
2. Identify the owner boundary and the public contract before editing. Search
   all consumers when changing a type, schema, config key, storage layout,
   pointer format, or error.
3. Make the smallest bounded refactor that leaves one canonical path. Delete
   stale branches and wrappers when they are not a shipped compatibility
   contract.
4. Preserve typed errors, cancellation, lock release, SlateDB closure, and
   structured output behavior. Do not add `unwrap`, `expect`, `panic`,
   `todo!`, or an untracked fallback path.
5. Update the adjacent guide when behavior or a public option changes.
6. Verify with focused tests first, then the appropriate broad gate. Use the
   RustFS E2E skill when a real object-store side effect is part of the claim.

## Shared references

- `references/command-map.md` — complete command-to-skill ownership and
  overlap rules.
- `references/contracts.md` — output, storage, safety, and verification
  contracts that every Crab skill must honor.

## Common validation

```bash
cd crab
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo check --locked
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test --locked
```

Use a dedicated target directory for another checkout or worktree. Report
exactly which gate ran and which proof remains unavailable.
