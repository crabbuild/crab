# Crab data source commands

`crab data` owns source provenance that must survive a migration from DVC.
It is separate from the existing raw object-store `crab import` command and
the CLI self-update `crab update` command.

Every successful `data import` or `data import-url` writes a versioned,
credential-free descriptor under `.crab/workflow/sources/<id>.json`. The
descriptor records the source kind, canonical locator, locked revision or
validator, target path, byte count, and recomputed `b3:` content identity.
Secrets from URL userinfo and query fields are never serialized.

`data update` resolves the descriptor, streams a new candidate into a sibling
temporary path, verifies it, and only then replaces the target and descriptor.
HTTP(S) and S3/GCS/Azure-compatible object URLs use provider validators when
available; a matching strong validator avoids transferring the body again.

Git revision reads are available for local repositories. `data list --rev
<revision>` reads the committed tree, and `data import <repo> --path <file>
--rev <revision>` streams one committed blob, preserves its executable bit,
and records the resolved commit id. A directory at a revision is rejected
until its tree materialization contract is implemented. Remote Git transport
is not inferred from a URL or local path.

Database import has an explicit connector boundary. The bundled SQLite
connector opens databases read-only, materializes deterministic JSONL, and
records the query identity so `data update` can rerun it transactionally.
Other connectors and SSH/SFTP, WebDAV, HDFS, Drive, and OSS URL schemes fail
closed instead of pretending that parser acceptance is a runtime adapter.
Object-store URL credentials come from the configured cloud credential chain,
never from persisted source URLs.
