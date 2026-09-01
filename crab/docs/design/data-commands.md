# Crab data source commands

`crab data` owns source provenance that must survive a migration from DVC.
It is separate from the existing raw object-store `crab import` command and
the CLI self-upgrade `crab upgrade` command.

Every successful `data import` or `data import-url` writes a versioned,
credential-free descriptor under `.crab/workflow/sources/<id>.json`. The
descriptor records the source kind, canonical locator, locked revision or
validator, target path, byte count, and recomputed `b3:` content identity.
Secrets from URL userinfo and query fields are never serialized.

The command boundary resolves the current Git worktree root, so invocations
from nested directories use the same descriptor and target root. `data status`
keeps the workspace result compatible with the existing `source:<state>` text
label and adds explicit JSON dimensions for workspace, descriptor, Git, lock,
cache, source freshness, and remote availability. It does not perform network
I/O implicitly: unchecked source/remote state is reported as `not-checked`,
and source descriptors not promoted to the workflow cache report
`cache=not-managed`.

`data update` resolves the descriptor, streams a new candidate into a sibling
temporary path, verifies it, and only then replaces the target and descriptor.
HTTP(S) and S3/GCS/Azure-compatible object URLs use provider validators when
available; a matching strong validator avoids transferring the body again.

Git revision reads are available for local repositories. `data list --rev
<revision>` reads the committed tree, and `data import <repo> --path <path>
--rev <revision>` materializes a committed file, directory, or repository root,
preserves executable bits, and records the resolved commit id. Symlinks and
submodules are rejected before writing. Remote Git transport is not inferred
from a URL or local path.

Database import has an explicit connector boundary. The bundled SQLite
connector opens databases read-only, materializes deterministic JSONL, and
records the query identity so `data update` can rerun it transactionally.
Other connectors and SSH/SFTP, WebDAV, HDFS, Drive, and OSS URL schemes fail
closed instead of pretending that parser acceptance is a runtime adapter.
Object-store URL credentials come from the configured cloud credential chain,
never from persisted source URLs.
