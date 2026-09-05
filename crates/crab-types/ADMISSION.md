# crab-types Admission Ledger

`crab-types` is the shared foundation crate for stable, non-secret Crab
contracts. A public item belongs here only when it is more stable than one
owner Module and does not pull runtime policy, command output, provider SDKs,
storage transport, server behavior, or rich domain errors into the foundation
crate.

Every new public item must update this ledger in the same change. The entry
must name the contract kind and the reason the type is shared here instead of
staying with an owner crate such as `crab-auth`, `crab-cache`, `crab-storage`,
`crab-metadata`, `crab-read`, `crab-workflow`, or `crab`.

Allowed contract kinds:

- `persisted`: serialized repository/config/cache/workflow data whose shape is
  intentionally stable across owner Modules.
- `wire`: public protocol or helper payloads shared across process/crate
  boundaries.
- `identity`: small stable IDs, provider identities, hashes, or scope names
  shared by multiple owner Modules.
- `category`: coarse non-secret classification shared by output or protocol
  envelopes; rich owner errors stay outside `crab-types`.
- `helper`: deterministic formatting/parsing helpers for the stable contracts
  above. Helpers must not read process env, inspect config files, perform I/O,
  construct stores, or call provider SDKs.

Rejected by default:

- CLI `CrabError`, command output, progress, exit-code policy, or diagnostics.
- Owner-domain errors such as auth, cache, storage, metadata, read, workflow,
  Git, LFS, Xet, or server errors.
- Secrets, credentials, auth sessions, tokens, provider SDK clients, or token
  refresh behavior.
- Broad config aggregates, runtime options, feature flags, fallback policy, or
  migration orchestration.
- Object-store construction, cache-server routes, auth-server receive/view
  runtime, metadata stores, read-store selection, hydration, Git process
  orchestration, or workflow execution.

Dependency budget:

- Normal dependencies should stay limited to serialization/schema support.
- A new normal dependency requires a ledger note explaining why the stable
  contract cannot live in an owner crate, plus `cargo tree -p crab-types
  --edges normal --depth 1` proof in the migration plan.

## Current Public Surface

| Public surface | Contract kind | Why it belongs here |
|----------------|---------------|---------------------|
| `error::ErrorCategory` | category | Coarse machine-readable error classes can be shared by output/protocol envelopes without importing rich owner-domain errors or CLI `CrabError`. |
| `pointer::{Pointer, PointerParseError, VERSION_LINE, LEGACY_VERSION_LINE, MAX_POINTER_SIZE, is_pointer, is_supported_version_line, hex_encode}` | wire | The Crab pointer file format is consumed by CLI, SDK, LFS/read paths, and server helpers as a stable wire payload. Parsing helpers are deterministic and perform no I/O. |
| `replication::{ReplicationConfig, ReplicationMode, ReplicationProviderKind, ReplicationRpo, ReplicationCoordinatorKind, ReplicationCoordinatorConsistency, ReplicationCoordinatorConfig, WriterConfig, ReplicaConfig, ReplicationParseError}` | persisted | Replication configuration is shared by CLI, SDK projection, read selection, and auth/server paths as a persisted config shape. Runtime coordinator/store construction stays in owner crates. |
| `storage::{StorageProviderKind, BucketIdentity, StorageScope}` | identity/wire | Provider identity, bucket identity, and scoped object-store prefixes are shared across auth, storage, replication, SDK, and server boundaries without carrying credential or store-construction behavior. |
| `time::{now_rfc3339_millis, from_epoch_millis}` | helper | Shared timestamp formatting keeps persisted/wire timestamps stable. The helper is deterministic except for the wall-clock wrapper and performs no config, I/O, or runtime ownership. |
| `workflow::StageHash` | identity | Stage hashes identify resolved workflow stages across queue/status/event payloads. Workflow planning and execution stay in `crab-workflow` and `crab`. |
