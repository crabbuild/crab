# DVC migration contract

`crab migrate from-dvc` is a state-preserving migration protocol, not a file
rename. It may write Crab pointers and workflow files, but it never deletes
DVC state. A migration is cutover-safe only after every discovered source has a
verified Crab representation and a clean-clone restore check.

## Source precedence

For each tracked DVC output, Crab resolves bytes in this order:

1. the materialized worktree output, when its declared DVC size and checksum
   verify;
2. a local DVC cache object, including `.dir` manifests;
3. a supported live remote selected by explicit configuration.

DVC MD5 values, etags, and remote object keys are source identities and
verification inputs. Crab recomputes its own content identity through the
canonical add/staging path and never stores a DVC MD5 as a Crab hash.

## Trust boundaries and redaction

`.dvc/config` is read before `.dvc/config.local`, matching DVC's project-local
precedence. Credentials, URL userinfo, access keys, tokens, database DSNs, and
OAuth state are never written to the journal or report. Reports contain only a
remote name, scheme, credential-source category, and resolution status. A
remote without an explicit Crab destination mapping is an unsafe finding.

## Journal and resume

Migration state is kept under Crab's per-worktree state as a versioned,
canonical JSON journal. Each item has a stable inventory key, source locator,
source fingerprint, byte count, Crab hash (once transferred), and transfer and
verification states. Journal updates are atomic. Resume refuses to reuse an
entry when the source or effective DVC configuration fingerprint changed.

The journal is retained after failures. Published Crab objects are immutable;
rollback means leaving them available for a later resume, not deleting them.

## Cutover criteria

`safe_to_remove_dvc` is true only when all of the following hold:

- every `.dvc` pointer, directory manifest, workflow lock entry, run-cache
  record, and relevant remote is inventoried exactly once;
- every required byte is present, checksum-verified, and represented by Crab;
- import provenance is retained in a versioned source descriptor;
- no checkpoint, unknown schema, unsupported provider, corrupt object, dirty
  output, or unresolved remote remains;
- a fresh worktree can hydrate and reproduce the recorded bytes and tree shape.

The command never offers a delete flag. A false result reports stable reason
codes and the recovery action instead of suggesting removal.

## Compatibility profile

The current local implementation targets DVC 2.x-compatible `.dvc` files,
ordinary file and directory (`.dir`) cache objects, and local caches. Remote
descriptors can be inventoried and explicitly mapped, but live remote fetch,
push, clone, and hydrate evidence is not wired into this command yet, so the
remote clean-clone gate always keeps `safe_to_remove_dvc` false. Legacy
checkpoint records and unknown cache/run-cache schemas remain fatal findings
until Crab has true lineage semantics. The pinned fixture version and fixture
digest must be recorded by release qualification; the source `.dvc/` tree
remains unchanged in every fixture.
