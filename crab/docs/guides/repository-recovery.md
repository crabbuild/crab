# Repository Recovery and Repair

Operator-visible recovery planning and verified local restore for missing or
corrupt Crab content.

## Historical Manifest Recovery

Every successful manifest replacement now preserves the displaced committed
manifest as an immutable object under
`<repository>/manifests/history/<generation>-<blake3>.json`. The current
`<repository>/manifest` remains the only visible repository state. History is
kept indefinitely by default, and repository, bucket, and managed-service GC
retain every pack, pack index, reverse index, metadata segment, shard, and xorb
reachable from every validated historical root.

Operators can preview an explicit retention boundary and then apply it:

```bash
crab recover history prune --keep-last 20
crab recover history prune --keep-last 20 --apply
crab gc --scope repo --dry-run
crab gc --scope repo
```

`--keep-last` counts distinct generations and retains every root in each kept
generation. Prune never removes the current manifest or dependent data; a
later GC run re-evaluates reachability and grace periods before reclaiming
objects that were unique to removed roots. Prune apply, restore apply, and
destructive repository GC share a renewable maintenance lease, so recovery
cannot race object deletion. Destructive bucket GC acquires the same lease for
every registered repository before deleting shared objects.

Writers create history only when they replace a manifest. Repositories pushed
only by older Crab versions therefore have no retroactive history for those
earlier generations. After all writers are upgraded, each later successful
push archives the state it displaced.

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
than one valid root is ambiguous and requires `--digest`. Verification is
mandatory before both preview and apply: Crab validates the historical
manifest digest, segmented metadata, pack bodies and canonical indexes, Git
object connectivity with strict `git fsck`, shard structure, and every
referenced xorb payload and chunk. Stored reverse indexes are validated; when a
direct push has no remote reverse-index sidecar, Crab regenerates that
derivable acceleration data from the verified canonical pack index. The result
reports deterministic remote dependency object and byte counts.

Restore never rewrites an old generation in place. It acquires leases for the
union of current and historical refs, renews them while working, confirms the
current manifest still matches the state used for the preview, and publishes
the historical contents as `current generation + 1` through manifest CAS. A
concurrent push or held ref lease aborts the restore without moving the
manifest. The displaced bad state is itself archived, so the recovery can be
reversed. Generation-pinned Git locator metadata is rebuilt after publication;
if that optional acceleration rebuild needs repair, the restored manifest is
still authoritative and the command reports `acceleration_rebuilt=false`.

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
