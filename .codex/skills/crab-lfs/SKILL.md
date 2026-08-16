---
name: crab-lfs
description: Implement and operate Crab's Git LFS compatibility layer, including native and transfer-agent modes, LFS hooks, pointers, locks, fetch/push/pull, conversion, deduplication, pruning, and protocol diagnostics. Use whenever a request mentions `crab lfs`, Git LFS interoperability, `.gitattributes` LFS filters, the LFS transfer agent, or `optimize lfs`.
compatibility: Crab CLI with Git LFS-compatible repositories and the `crab-lfs` crate.
---

# Crab Git LFS compatibility

Keep Git LFS compatibility separate from Crab-native pointer workflows. The
two systems may share storage or conversion tools, but their pointer formats,
hooks, transfer protocols, and verification rules differ.

## Command scope

All `crab lfs` subcommands, hidden `crab lfs-transfer-agent`, and
`crab optimize lfs` (`dedup`, `convert`, `prune`).

## Operating modes

1. Read `crab/docs/architecture/lfs-compatibility.md` and determine whether
   the repository uses native mode, transfer-agent mode, or a conversion path.
2. Inspect `.gitattributes`, Git config, LFS config, pointer format, object
   location, and the command's hook/protocol entry point.
3. For fetch/push/checkout/prune, prove remote object identity and local
   checkout behavior. A Git LFS pointer being present is not proof that its
   content is available.
4. For conversion, require a dry-run and retain the documented rollback or
   transaction boundary. Never silently convert a repository's pointer format.
5. For deduplication and prune, verify the destination or Crab cache before
   deleting any LFS object. Respect remote verification flags and recent-object
   protection.

## Protocol discipline

- Treat the transfer-agent stdin/stdout protocol as machine-readable; do not
  write human logs into the protocol stream.
- Preserve LFS lock ownership and force semantics.
- Keep credentials and signed URLs out of logs and final reports.
- Use the crate tests and a real remote fixture for protocol changes. Do not
  substitute a native Crab hydration test for LFS interoperability proof.

## Read first

- `crab/docs/guides/lfs.md`
- `crab/docs/architecture/lfs-compatibility.md`
- `crab/docs/design/lfs.md`
- `crab/src/lfs/`
- `crab/src/cmd/lfs/`
- `crates/crab-lfs/`
- `.codex/skills/crab-cli-core/references/contracts.md`
