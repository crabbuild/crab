# Repository Recovery and Repair

Operator-visible recovery planning and verified local restore for missing or
corrupt Crab content.

## Historical Checkpoint Recovery

Protocol-v2 checkpoint publication preserves each compacted repository state
as an immutable authenticated segment under `<repository>/v2/history/`. The
mutable `<repository>/v2/root` authenticates the newest retained segment, and
each segment authenticates its predecessor, checkpoint, exact refs, symbolic
HEAD, compacted ref positions, and capsule runs. History is kept indefinitely
by default. Repository GC retains every checkpoint and capsule run in the
validated chain; the append-only current pointer catalog keeps retained
shard/xorb identities protected from bucket GC.

Operators can preview an explicit retention boundary and then apply it:

```bash
crab recover history prune --keep-last 20
crab recover history prune --keep-last 20 --apply
crab gc --scope repo --dry-run
crab gc --scope repo
```

`--keep-last` retains that many newest checkpoint segments. Prune apply takes
the repository sweep lease and root GC fence, rebuilds the retained immutable
chain without a pointer to the removed suffix, and atomically replaces only the
root's history frontier. It never directly deletes checkpoint, capsule, shard,
or xorb data. A later GC run independently re-evaluates reachability and grace
periods before reclaiming objects unique to removed recovery points.

Writers append history when checkpoint maintenance compacts one or more
capsules. Ordinary foreground pushes therefore add no history request and do
not create a checkpoint for every commit.

List the available roots, verify a chosen root, preview its ref changes, and
then apply it explicitly:

```bash
crab recover history list
crab recover history prune --keep-last 20
crab recover history prune --keep-last 20 --apply
crab recover history verify 41
crab recover history verify 41 --digest <64-character-blake3>
crab recover history restore 41
crab recover history restore 41 --apply
```

`list`, `verify`, and `restore` also accept `--json`. A generation with more
than one valid checkpoint is ambiguous and requires `--digest`. Verification
is mandatory before restore preview: Crab authenticates the history chain,
checkpoint and capsule-run closure, complete pointer catalog, visibility
snapshot, embedded pack/index/reverse-index/locator agreement, every shard and
xorb dependency, and Git connectivity with strict `git fsck`. The result
reports deterministic dependency object and byte counts.

`restore --apply` publishes the selected checkpoint as a new generation. Crab
first verifies every dependency and Git connectivity, checkpoints the displaced
current state into authenticated history, then rotates the ref authority epoch
while acquiring the maintenance fence. Stale or in-flight old-epoch ref heads
cannot override the restore. A single root CAS installs the restored refs,
symbolic HEAD, visibility, and packs while preserving the current append-only
xorb/shard catalog, so historical large files remain GC-protected. Failure
clears the owned fence without falling back to the v1 manifest-restoration path.

Status: release-manifest large-file and workflow-output inventory, Crab pointer
metadata inventory, staged import journal inventory, hashed workflow journal
inventory, shard-list inventory, xorb-list inventory, fsck JSONL missing
shard/xorb inventory, pack-list inventory, file-index inventory, explicit local
source paths, workflow-cache output bytes, and replica-labeled local sources are
implemented. Recovery candidates are hash-verified before restore, apply takes
an advisory restore-root lock, and apply is safe to retry. Shard-list entries
with verified backup object bodies can be restored to the configured Crab remote
with `recover apply --restore-shards`; candidates are rehash-verified before
upload and are written through the normal configured write store. Xorb inventory
entries with verified xorb object bodies can be restored to the configured Crab
remote with `recover apply --restore-xorbs`; candidates are parsed, checked
against the planned xorb hash, and chunk-verified before upload. Pack-list
entries with verified Git pack bodies can be restored to the configured Crab
remote with `recover apply --restore-packs`; apply verifies the planned Blake3
identity, size, Git pack header, and trailing SHA-1 before uploading the pack
body and metadata sidecar.
`recover apply --repair-remote` stages verified file bytes into the repository
staging area and pushes manifest-selected branch refs through the normal Crab
push pipeline, so xorb uploads, shard/index writes, manifest CAS, ref CAS, and
push audit logging stay on the canonical path. `recover apply
--rebuild-file-index` rebuilds `file_index_db` from durable shard objects and
only reports planned file-index mappings as repaired when the rebuilt database
returns the expected shard hash. Pack-list-only entries still carry
item-specific operator follow-up actions because a pack list alone does not
provide pack bytes. This is separate from the internal inflight-operation
recovery described in [crab recovery](recovery.md).

## Commands

```bash
crab recover plan --manifest release.json --source /mnt/backup --output recover-plan.json --json
crab recover plan --manifest release.json --cache-root .crab/cache --workflow-journal .crab/workflow/runs/<run-id>/journal.db --json
crab recover plan --manifest release.json --import-journal imported-repo --replica-source /mnt/replica --json
crab fsck --jsonl > fsck.jsonl
crab recover plan --manifest release.json --fsck-jsonl fsck.jsonl --cache-root .crab/cache --replica-source /mnt/replica --json
crab recover plan --manifest release.json --shard-list shards.jsonl --xorb-list xorbs.jsonl --pack-list packs.jsonl --file-index file-index.jsonl --json
crab recover show --plan recover-plan.json --json
crab recover apply --plan recover-plan.json --restore-to restored-files --json
crab recover apply --plan recover-plan.json --restore-to restored-files --restore-shards --json
crab recover apply --plan recover-plan.json --restore-to restored-files --restore-xorbs --json
crab recover apply --plan recover-plan.json --restore-to restored-files --restore-packs --json
crab recover apply --plan recover-plan.json --restore-to restored-files --rebuild-file-index --json
crab recover apply --plan recover-plan.json --restore-to restored-files --restore-shards --restore-xorbs --restore-packs --rebuild-file-index --json
crab recover apply --plan recover-plan.json --restore-to restored-files --repair-remote --json
crab recover apply --plan recover-plan.json --restore-to restored-files --repair-remote --repair-refspec refs/heads/main:refs/heads/main --json
```

## Current Scope

Recovery planning reads a release manifest, builds expected file identities from
its Crab large-file inventory and workflow output inventory, and can extend that
inventory from:

- `--pointer-root`, by scanning Crab pointer files for file hash and size.
- `--import-journal`, by reading staged import entries without opening the
  journal for write.
- `--workflow-journal`, by reading hashed workflow output rows without opening
  the journal for write.
- `--shard-list`, by reading newline or JSON shard-list inventories.
- `--xorb-list`, by reading newline or JSON xorb hash inventories.
- `--fsck-jsonl`, by reading `crab fsck --jsonl` warnings for missing xorbs and
  missing shard-list objects.
- `--pack-list`, by reading JSONL or JSON pack-list inventories.
- `--file-index`, by reading JSON or JSONL file-to-shard mappings.

It searches each explicit `--source` file or directory, each `--replica-source`,
matching plain workflow-cache output bytes under `--cache-root`, import-journal
workspace paths, and workflow-journal repository paths for bytes with the
expected size and Blake3 hash. Matching items are marked `repairable`; missing
or mismatched file-byte items are marked `unrecoverable`. Shard-list and fsck
shard entries are marked `repairable` when a `--source` or `--replica-source`
contains a local object body whose Blake3 hash matches the shard hash; otherwise
they remain `inventory_only`. Xorb-list and fsck xorb entries are marked
`repairable` when `--cache-root`, `--source`, or `--replica-source` contains a
valid xorb object whose parsed Merkle hash matches the planned xorb hash;
otherwise they are marked `unrecoverable`. Pack-list entries are marked
`repairable` when a `--source` or `--replica-source` contains a local pack body
whose size and Blake3 hash match the pack-list entry; otherwise they remain
`inventory_only`. File-index entries are marked `inventory_only` because they
identify metadata references that an operator can inspect during recovery, but
they do not by themselves provide plain file bytes for local restore. The plan
action is specific to the metadata kind: restore or re-push shard objects,
restore xorb objects from cache or replica sources, restore Git packs from a
healthy replica or source remote, or rebuild and verify the file-index database
after referenced shards are present.

Recovery apply re-verifies candidate bytes before writing into `--restore-to`
with atomic tempfile-and-rename writes. Reruns report already-present files when
the restored bytes still match the expected identity. With `--repair-remote`,
apply also stages each verified file-byte candidate under the manifest path and
runs a silent Crab push for branch refs selected by the release manifest. The
ref source must resolve to the manifest commit; when the manifest has no
matching branch ref, pass one or more `--repair-refspec` values explicitly.
Successful remote file repairs are counted as `remote_repaired` and include the
refspecs used.

With `--restore-shards`, apply re-verifies each repairable shard candidate,
uploads it to the configured Crab remote shard path, and reports successful
items as `shard_repaired`. `--remote` can select a non-default Crab remote for
`--restore-shards`, `--restore-xorbs`, `--restore-packs`, and `--repair-remote`.
With `--restore-xorbs`, apply re-parses each repairable xorb candidate, checks
the planned Merkle hash and chunk integrity, uploads the xorb body, and reports
successful items as `xorb_repaired`. With `--restore-packs`, apply re-verifies
each repairable pack candidate, checks the Git pack framing and trailing SHA-1,
uploads the pack body and metadata sidecar, and reports successful items as
`pack_repaired`. When remote object repair and `--rebuild-file-index` are used
together, shard, xorb, and pack objects are restored before the file-index
rebuild verifies planned mappings.

With `--rebuild-file-index`, apply rebuilds `file_index_db` from `.crab/shards/`
using the same metadb rebuild path as `crab metadb rebuild --db file_index`,
then checks each planned file-index mapping and reports exact matches as
`metadata_repaired`. Pack inventory items without verified backup bodies are
still skipped with explanatory messages and do not perform direct pack writes.
Concurrent applies to the same restore root are rejected by an advisory lock.
