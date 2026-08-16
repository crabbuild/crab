//! Experiment identifier + canonical naming for experiment-scoped refs
//! and remote storage paths.
//!
//! Every experiment is keyed by a [`ExperimentId`] — a UUIDv7 newtype.
//! UUIDv7 has a 48-bit millisecond Unix timestamp in its leading bits,
//! so sorting IDs lexicographically orders them by creation time (R19).
//! Stage cache entries and experiment metadata live under deterministic
//! ref and object-store paths that are derived here, never ad-hoc at
//! the call site.
//!
//! This module is deliberately I/O-free. Every function here is a pure
//! string builder. Downstream tasks (4.2 metadata ref CAS, 4.5 GC live-
//! set walker, 4.8 e2e push/fetch) consume these helpers; they are the
//! single source of truth for the format of:
//!
//! - `refs/crab/exp/<uuid>` — experiment commit refs
//! - `refs/crab/exp-meta/<uuid>` — experiment metadata blob refs
//! - `refs/crab/stages/<hex>` — stage cache entry refs
//! - `workflow/stages/<ab>/<full-hex>.json` — sharded stage entry objects
//! - `workflow/exp/<uuid>/meta.json` — experiment metadata objects
//! - `workflow/exp/<uuid>/stage-refs.json` — experiment stage-ref lists

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use crate::ExperimentId;
use crate::{Result, WorkflowError as CrabError};
use crab_types::workflow::StageHash;

/// Ref-name prefix (including trailing slash) for stage cache entries.
///
/// Kept as a `const` so the GC live-set walker (task 4.5) can match refs
/// by prefix without re-hardcoding the string.
pub const STAGE_REF_PREFIX: &str = "refs/crab/stages/";

/// Ref-name prefix (including trailing slash) for experiment commit refs.
///
/// An experiment commit ref points at a real git commit on a hidden
/// branch, so `git fetch` replicates it naturally (design §"Remote
/// Storage Layout").
pub const EXP_REF_PREFIX: &str = "refs/crab/exp/";

/// Ref-name prefix (including trailing slash) for experiment metadata
/// blob refs.
///
/// Separate namespace from `EXP_REF_PREFIX` so the commit ref and its
/// metadata can evolve independently under CAS (design §"Remote Storage
/// Layout").
pub const EXP_META_REF_PREFIX: &str = "refs/crab/exp-meta/";

/// A compare-and-swap update for one Git ref.
#[derive(Debug, Clone)]
pub struct RefCasOp {
    /// Full ref name, for example `refs/heads/main`.
    pub ref_name: String,
    /// ETag for a conditional update, or `None` when creating the ref.
    pub expected_etag: Option<String>,
    /// New object id or content hash the ref should target.
    pub new_sha: String,
}

/// Ref name for the experiment commit ref: `refs/crab/exp/<uuid>`.
///
/// This ref points at a real git commit on a disposable branch, so
/// `crab fetch` / `git fetch` replicate it alongside other refs
/// (design §"Remote Storage Layout").
pub fn exp_ref(id: &ExperimentId) -> String {
    format!("{EXP_REF_PREFIX}{id}")
}

/// Ref name for the experiment metadata ref: `refs/crab/exp-meta/<uuid>`.
///
/// Stored in its own namespace so the metadata blob can be CAS'd
/// independently of the commit ref.
pub fn exp_meta_ref(id: &ExperimentId) -> String {
    format!("{EXP_META_REF_PREFIX}{id}")
}

/// Ref name for a stage cache entry: `refs/crab/stages/<hex>`.
///
/// `<hex>` is the 64-character lowercase hex form of [`StageHash`]
/// (`hasher::StageHash::as_hex`). These refs live under
/// [`STAGE_REF_PREFIX`] for the GC live-set walker's benefit.
pub fn stage_ref(stage_hash: &StageHash) -> String {
    format!("{STAGE_REF_PREFIX}{}", stage_hash.as_hex())
}

/// Object-store key for a stage cache entry:
/// `workflow/stages/<ab>/<full-hex>.json`.
///
/// The two-character shard (`<ab>`) is the first two hex characters of
/// `stage_hash`. Sharding keeps single-prefix listings bounded for very
/// large repos where ~10^6 entries would otherwise fan out into one
/// directory (design §"Remote Storage Layout"). The value is a small
/// canonical JSON object, not a xorb.
///
/// Returns a relative object-store key with no leading slash — callers
/// prepend the per-repo `{repo_prefix}` when they talk to the
/// [`object_store::ObjectStore`](::object_store::ObjectStore).
pub fn stage_entry_object_path(stage_hash: &StageHash) -> String {
    let hex = stage_hash.as_hex();
    // `StageHash::as_hex` yields exactly 64 hex characters, so the
    // first two exist for every possible value and the slice is safe.
    // Assert via a defensive bound to make the invariant visible to
    // future maintainers without panicking in release builds.
    debug_assert_eq!(hex.len(), 64, "stage hash hex must be 64 chars");
    let shard = &hex[..2];
    format!("workflow/stages/{shard}/{hex}.json")
}

/// Object-store key for an experiment's metadata JSON blob:
/// `workflow/exp/<uuid>/meta.json`.
pub fn exp_meta_object_path(id: &ExperimentId) -> String {
    format!("workflow/exp/{id}/meta.json")
}

/// Object-store key for an experiment's list of referenced stage
/// hashes: `workflow/exp/<uuid>/stage-refs.json`.
///
/// Kept as a sibling to `meta.json` so the GC live-set walker can
/// enumerate an experiment's reachable stage refs with a single read
/// rather than scanning the metadata blob.
pub fn exp_stage_refs_object_path(id: &ExperimentId) -> String {
    format!("workflow/exp/{id}/stage-refs.json")
}

/// Current schema version for the experiment metadata blob on disk.
///
/// Readers refuse any blob stamped with a higher version
/// ([`CrabError::WorkflowExperimentMetadataSchemaNewer`]); lower
/// versions will be migrated up once an older schema ships. The
/// starting value is `1` — there is no v0.
pub const EXPERIMENT_METADATA_SCHEMA_VERSION: u16 = 3;

/// Highest schema version this binary knows how to read. Tracks
/// [`EXPERIMENT_METADATA_SCHEMA_VERSION`] until a migration ladder
/// lands (task 4.5+), at which point this value bumps to the latest
/// version the ladder can produce.
pub const EXPERIMENT_METADATA_MAX_SUPPORTED_SCHEMA: u16 = EXPERIMENT_METADATA_SCHEMA_VERSION;

fn default_experiment_metadata_status() -> String {
    "unknown".to_owned()
}

/// Full experiment state, persisted as a single canonical-JSON blob
/// under [`exp_meta_object_path`].
///
/// Field layout mirrors the on-disk JSON shape: every map is a
/// [`BTreeMap`] so serde emits sorted keys, producing byte-stable
/// output across builds and platforms. Two equivalent structs must
/// serialize to byte-identical bytes — consumers (GC live-set
/// walker, `exp diff`) rely on that property to cache meta-blob
/// content hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentMetadata {
    /// Forward-compat schema stamp. Bumped whenever the on-disk
    /// shape gains or renames a field.
    pub schema_version: u16,

    /// Stable id of this experiment. Redundant with the ref name
    /// (the ref embeds the id), but carrying it inside the blob
    /// means a downloaded meta JSON is self-describing.
    pub exp_id: ExperimentId,

    /// Commit HEAD was at when the run started. Captured at run
    /// start, NOT at queue submission, because HEAD may have moved
    /// in between.
    pub base_commit: String,

    /// Commit HEAD was at when the experiment was queued, if
    /// known. Recorded for auditability when `crab exp run` is
    /// invoked via a queue. `None` for immediate runs where there
    /// is no distinct queue time.
    pub queue_commit: Option<String>,

    /// Optional human-readable experiment label from `crab exp run
    /// --name` or `crab exp save --name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Optional human-readable experiment message from `crab exp run
    /// --message` or `crab exp save --message`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// Terminal state for list filters and audits. Older metadata
    /// predates this field and reads back as `unknown`.
    #[serde(default = "default_experiment_metadata_status")]
    pub status: String,

    /// Parameter overrides applied to the tmpdir worktree before
    /// the DAG ran. Captured so `exp show` / `exp diff` can render
    /// what actually changed relative to `base_commit` without
    /// re-executing the override logic.
    pub param_overrides: BTreeMap<String, String>,

    /// Per-stage summary: stage name → lowercase-hex stage hash.
    /// Used by the GC live-set walker to enumerate an experiment's
    /// reachable stage cache entries without downloading the stage
    /// blobs themselves.
    pub stages: BTreeMap<String, String>,

    /// Collected metrics snapshots: declared metrics file path →
    /// parsed JSON value. A `serde_json::Value` rather than a
    /// structured type because metrics files are user-authored and
    /// schema-free.
    pub metrics: BTreeMap<String, serde_json::Value>,

    /// The `crab exp run` CLI args that produced this experiment,
    /// captured verbatim for `exp show` reproducibility. Does not
    /// include the `crab` prefix or environment variables.
    pub cli_args: Vec<String>,

    /// Host fingerprint string in the same format
    /// [`crate::cache::StageCacheEntry`] uses — e.g.
    /// `"linux-x86_64-crab-0.8.0"`. Used for cross-host diffing.
    pub host_fingerprint: String,

    /// RFC3339 millisecond timestamp of the run start. Wall-clock,
    /// so it's human-readable; humans don't compare durations off
    /// this field — they use `ended_at - started_at` interpreted by
    /// a date library, not by subtracting strings.
    pub started_at: String,

    /// RFC3339 millisecond timestamp of terminal state (success or
    /// failure). `None` while the experiment is still in-flight.
    pub ended_at: Option<String>,
}

impl ExperimentMetadata {
    /// Serialize to canonical JSON bytes.
    ///
    /// Byte-stable for equivalent structs: two calls on the same
    /// logical value produce identical output on every platform. The
    /// recipe is:
    ///
    /// 1. Use [`BTreeMap`] for every map field so serde_json emits
    ///    sorted keys directly.
    /// 2. Route through [`serde_json::Value`] and reorder object
    ///    keys with a canonicalizer, belt-and-braces in case a
    ///    nested user-supplied metrics object arrived with keys
    ///    ordered by insertion rather than sort.
    /// 3. Emit with `serde_json::to_vec` (no pretty-printing), so
    ///    there is no whitespace variation across writers.
    ///
    /// Returns [`CrabError::Internal`] if serde_json somehow fails
    /// to serialize the struct — in practice only a user-supplied
    /// metrics `serde_json::Value` with non-string object keys could
    /// trigger this, and that is already rejected by
    /// `MetricsSchemaMismatch` upstream.
    pub fn canonical_json(&self) -> Result<Vec<u8>> {
        let value = serde_json::to_value(self).map_err(|e| {
            CrabError::Internal(format!(
                "experiment metadata serialization failed for {}: {e}",
                self.exp_id
            ))
        })?;
        let canonical = canonicalize_json(value);
        serde_json::to_vec(&canonical).map_err(|e| {
            CrabError::Internal(format!(
                "experiment metadata canonical serialization failed for {}: {e}",
                self.exp_id
            ))
        })
    }

    /// Content-address hash of the canonical JSON bytes.
    ///
    /// The meta-ref CAS (`refs/crab/exp-meta/<uuid>`) points at
    /// this hash rather than a git OID; the stored object lives at
    /// [`exp_meta_object_path`]. Two writers that produce the same
    /// logical metadata see the same hash, which is what makes
    /// ref-CAS ties resolvable by "read the winner's blob, compare
    /// hashes".
    ///
    /// Blake3 to match the rest of crab's content addressing; the
    /// returned string is 64 lowercase hex characters with no prefix.
    pub fn content_hash(&self) -> Result<String> {
        let bytes = self.canonical_json()?;
        Ok(blake3::hash(&bytes).to_hex().to_string())
    }
}

/// Canonicalize a `serde_json::Value` by recursively reordering
/// object keys alphabetically. Array order is preserved.
///
/// Kept private: callers only see the byte output via
/// [`ExperimentMetadata::canonical_json`]. This is the same pattern
/// `workflow::cache` uses — duplicated here rather than shared
/// because a cross-module helper would tie two otherwise
/// independent modules together for a three-line function.
fn canonicalize_json(value: serde_json::Value) -> serde_json::Value {
    use serde_json::{Map, Value};
    match value {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<String, Value> = map.into_iter().collect();
            let mut out = Map::with_capacity(sorted.len());
            for (k, v) in sorted {
                out.insert(k, canonicalize_json(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonicalize_json).collect()),
        other => other,
    }
}

/// In-memory handle to an experiment.
///
/// Wraps the durable [`ExperimentMetadata`] plus the git commit OID
/// that the experiment's commit ref
/// (`refs/crab/exp/<uuid>`) resolves to. The commit OID lives
/// outside the metadata blob because the blob is content-addressed —
/// the commit value pinning the ref can change over the experiment's
/// lifetime as the DAG progresses, while the metadata captures a
/// snapshot of the final (or current) state.
#[derive(Debug, Clone)]
pub struct Experiment {
    /// Durable metadata snapshot. The value written under
    /// [`exp_meta_object_path`].
    pub metadata: ExperimentMetadata,

    /// Git commit OID this experiment's commit ref points at, once
    /// it has been CAS'd. `None` before the initial commit-ref CAS
    /// has published a value.
    pub commit_oid: Option<String>,
}

impl Experiment {
    /// Build a fresh experiment with no published commit yet.
    pub fn new(metadata: ExperimentMetadata) -> Self {
        Self {
            metadata,
            commit_oid: None,
        }
    }

    /// Convenience borrow of the underlying id.
    pub fn id(&self) -> &ExperimentId {
        &self.metadata.exp_id
    }
}

/// Build a [`RefCasOp`] that CAS-writes the experiment's commit ref
/// (`refs/crab/exp/<uuid>`) to `commit_oid`.
///
/// For a brand-new experiment (no prior commit ref), pass
/// `expected_etag = None`; the existing CAS layer treats `None` as
/// "create only if absent" and fails loudly on collision.
///
/// For an update (retry after a contention, or a progressing DAG
/// advancing the commit), pass the etag observed on the most recent
/// read so the CAS is a conditional write.
pub fn build_exp_ref_cas(
    id: &ExperimentId,
    commit_oid: &str,
    expected_etag: Option<String>,
) -> RefCasOp {
    RefCasOp {
        ref_name: exp_ref(id),
        expected_etag,
        new_sha: commit_oid.to_owned(),
    }
}

/// Build a [`RefCasOp`] that CAS-writes the experiment's metadata
/// ref (`refs/crab/exp-meta/<uuid>`) to `meta_blob_hash`.
///
/// `meta_blob_hash` is the content-addressed hash of the metadata
/// blob's canonical bytes (see
/// [`ExperimentMetadata::content_hash`]), not a git OID — the
/// metadata blob is a plain JSON object, not a git commit.
///
/// Etag semantics match [`build_exp_ref_cas`].
pub fn build_exp_meta_ref_cas(
    id: &ExperimentId,
    meta_blob_hash: &str,
    expected_etag: Option<String>,
) -> RefCasOp {
    RefCasOp {
        ref_name: exp_meta_ref(id),
        expected_etag,
        new_sha: meta_blob_hash.to_owned(),
    }
}

/// Minimal read surface for fetching experiment metadata.
///
/// Abstracted as a trait so tests can exercise
/// [`read_experiment_metadata`] with an in-memory double, mirroring
/// how [`crate::coordination::pipelined_commit::CasStore`] is used
/// for the write side. Production callers back this with the crate's
/// [`crate::storage::Store`] via a thin adapter.
///
/// Implementations must be infallible for "not found" — the
/// absence of a ref or object is a normal state, not an error. All
/// other failures (auth, network, corruption) surface as
/// [`CrabError`].
pub trait ExperimentMetaRead: Send + Sync {
    /// Resolve a ref name to its current target.
    ///
    /// Returns `Ok(None)` if the ref does not exist, `Ok(Some(..))`
    /// with the ref's current value on hit. The value for a meta
    /// ref is the content hash of the meta blob (see
    /// [`build_exp_meta_ref_cas`]).
    fn read_ref(
        &self,
        ref_name: &str,
    ) -> impl std::future::Future<Output = Result<Option<String>>> + Send;

    /// Read the object stored at `object_path` and return its raw
    /// bytes.
    ///
    /// Returns `Ok(None)` if the object does not exist; `Err(..)`
    /// for auth or transport failures. Byte-identical semantics:
    /// the caller parses JSON from these bytes, so no implicit
    /// transcoding.
    fn read_object(
        &self,
        object_path: &str,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>>> + Send;
}

/// Read and deserialize the experiment metadata for `id`.
///
/// Two-step lookup:
///
/// 1. Resolve the meta ref `refs/crab/exp-meta/<uuid>` to find the
///    current content hash of the blob.
/// 2. Fetch the blob at [`exp_meta_object_path`] and deserialize.
///
/// Returns `Ok(None)` when either step misses — the experiment
/// simply doesn't exist for this reader. Schema mismatches surface
/// as [`CrabError::WorkflowExperimentMetadataSchemaNewer`] (blob
/// from a newer binary) or
/// [`CrabError::MetricsSchemaMismatch`]-style corruption errors
/// wrapped through `CrabError::Internal` for malformed JSON.
///
/// The `_content_hash` returned by the ref is currently only used
/// for logging; once a verification pass is added (follow-up spec),
/// it'll gate whether the deserialized bytes are trusted.
pub async fn read_experiment_metadata<S>(
    store: &S,
    id: &ExperimentId,
) -> Result<Option<ExperimentMetadata>>
where
    S: ExperimentMetaRead + ?Sized,
{
    let ref_name = exp_meta_ref(id);

    let Some(_content_hash) = store.read_ref(&ref_name).await? else {
        return Ok(None);
    };

    let object_path = exp_meta_object_path(id);
    let Some(bytes) = store.read_object(&object_path).await? else {
        // Ref exists but object is missing. Could mean a concurrent
        // GC just deleted the blob, or the ref was created without
        // a backing blob. Neither is recoverable here — but it's
        // also not a "metadata unreadable" error the caller must
        // surface with a distinct code. Treat as not-found; the GC
        // walker and `exp show` both handle that case cleanly.
        tracing::warn!(
            %id,
            %ref_name,
            %object_path,
            "experiment meta ref present but object missing"
        );
        return Ok(None);
    };

    // Parse just the schema_version first so we can reject
    // newer-than-supported blobs before interpreting any other
    // fields (whose shape may have drifted).
    let probe: SchemaProbe = serde_json::from_slice(&bytes).map_err(|e| {
        CrabError::Internal(format!("experiment metadata malformed JSON for {id}: {e}"))
    })?;

    if probe.schema_version > EXPERIMENT_METADATA_MAX_SUPPORTED_SCHEMA {
        return Err(CrabError::WorkflowExperimentMetadataSchemaNewer {
            id: id.to_string(),
            found: probe.schema_version,
            supported: EXPERIMENT_METADATA_MAX_SUPPORTED_SCHEMA,
        });
    }

    let metadata: ExperimentMetadata = serde_json::from_slice(&bytes).map_err(|e| {
        CrabError::Internal(format!("experiment metadata shape mismatch for {id}: {e}"))
    })?;

    Ok(Some(metadata))
}

/// Minimal prelude parse used to surface schema-newer errors with
/// actionable diagnostics before interpreting unknown fields.
#[derive(Deserialize)]
struct SchemaProbe {
    schema_version: u16,
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use std::collections::HashSet;
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::WorkflowError;

    /// Fabricate a deterministic [`StageHash`] whose first byte is the
    /// given prefix. Keeps the sharding tests straightforward to read.
    fn stage_hash_with_first_byte(first: u8) -> StageHash {
        let mut bytes = [0u8; 32];
        bytes[0] = first;
        // Fill the rest with a recognizable pattern so the full hex is
        // easy to eyeball in failures.
        for (i, slot) in bytes.iter_mut().enumerate().skip(1) {
            *slot = (i as u8).wrapping_add(0x10);
        }
        StageHash(bytes)
    }

    #[test]
    fn new_v7_produces_version_seven() {
        let id = ExperimentId::new_v7();
        assert_eq!(
            id.as_uuid().get_version_num(),
            7,
            "ExperimentId::new_v7 must always produce a UUIDv7"
        );
    }

    #[test]
    fn new_v7_ids_sort_in_chronological_order_as_strings() {
        // UUIDv7's leading 48 bits are a millisecond unix timestamp, so
        // ten IDs generated over a few milliseconds must sort
        // chronologically when compared as their canonical strings.
        // The sleep is 2ms to ensure the timestamp actually advances
        // between consecutive IDs on high-resolution clocks.
        let mut ids = Vec::with_capacity(10);
        for _ in 0..10 {
            ids.push(ExperimentId::new_v7());
            thread::sleep(Duration::from_millis(2));
        }

        let as_strings: Vec<String> = ids.iter().map(ToString::to_string).collect();
        let mut sorted = as_strings.clone();
        sorted.sort();

        assert_eq!(
            as_strings, sorted,
            "UUIDv7 string form must sort chronologically"
        );

        // Duplicate IDs would break the sort-equality check silently
        // (both halves would match); assert distinctness explicitly.
        let distinct: HashSet<&String> = as_strings.iter().collect();
        assert_eq!(distinct.len(), 10, "ten sequential v7 IDs must be distinct");
    }

    #[test]
    fn display_and_from_str_round_trip() {
        let id = ExperimentId::new_v7();
        let s = id.to_string();
        let parsed: ExperimentId = s.parse().expect("canonical form must round-trip");
        assert_eq!(id, parsed);
        assert_eq!(s, parsed.to_string());
    }

    #[test]
    fn from_str_rejects_too_short() {
        let err = "abc".parse::<ExperimentId>().unwrap_err();
        assert!(
            matches!(err, WorkflowError::ExperimentIdInvalid { .. }),
            "wrong variant: {err}"
        );
    }

    #[test]
    fn from_str_rejects_non_hex() {
        let err = "01931b9e-4b3c-7b2a-b9f0-zzzzzzzzzzzz"
            .parse::<ExperimentId>()
            .unwrap_err();
        assert!(matches!(err, WorkflowError::ExperimentIdInvalid { .. }));
    }

    #[test]
    fn from_str_rejects_uppercase() {
        // Canonical form is lowercase; uppercase hex round-trips
        // through `Uuid::parse_str` but would cause byte-level drift
        // vs. `to_string()`.
        let id = ExperimentId::new_v7();
        let upper = id.to_string().to_ascii_uppercase();
        let err = upper.parse::<ExperimentId>().unwrap_err();
        assert!(matches!(err, WorkflowError::ExperimentIdInvalid { .. }));
    }

    #[test]
    fn from_str_rejects_non_v7_uuid() {
        // A well-known v4 UUID — version nibble is 4, not 7. FromStr
        // must reject it even though the hex itself is valid.
        let v4 = "f47ac10b-58cc-4372-a567-0e02b2c3d479";
        let err = v4.parse::<ExperimentId>().unwrap_err();
        match err {
            WorkflowError::ExperimentIdInvalid { raw, reason } => {
                assert_eq!(raw, v4);
                assert!(reason.contains("UUIDv7"), "reason was: {reason}");
            }
            other => panic!("wrong variant: {other}"),
        }
    }

    #[test]
    fn exp_ref_builder_uses_canonical_prefix() {
        // Pin a known v7 UUID (valid version nibble, lowercase hex) so
        // this test locks down the wire format of the ref name.
        let raw = "01931b9e-4b3c-7b2a-b9f0-0123456789ab";
        let id: ExperimentId = raw.parse().expect("v7 uuid must parse");
        assert_eq!(exp_ref(&id), format!("refs/crab/exp/{raw}"));
    }

    #[test]
    fn exp_meta_ref_builder_uses_canonical_prefix() {
        let raw = "01931b9e-4b3c-7b2a-b9f0-0123456789ab";
        let id: ExperimentId = raw.parse().expect("v7 uuid must parse");
        assert_eq!(exp_meta_ref(&id), format!("refs/crab/exp-meta/{raw}"));
    }

    #[test]
    fn stage_ref_builder_uses_canonical_prefix_and_hex() {
        let hash = stage_hash_with_first_byte(0xab);
        let r = stage_ref(&hash);
        assert!(r.starts_with("refs/crab/stages/"), "ref: {r}");
        assert!(r.ends_with(&hash.as_hex()), "ref: {r}");
    }

    #[test]
    fn stage_entry_object_path_shards_on_first_two_hex_chars() {
        // Hash beginning with byte 0xab → hex "ab..." → shard "ab".
        let hash = stage_hash_with_first_byte(0xab);
        let path = stage_entry_object_path(&hash);
        let full_hex = hash.as_hex();

        assert_eq!(
            path,
            format!("workflow/stages/ab/{full_hex}.json"),
            "unexpected object path: {path}"
        );
        assert!(!path.starts_with('/'), "object keys must not be absolute");
    }

    #[test]
    fn exp_meta_object_path_is_repo_scoped_and_relative() {
        let raw = "01931b9e-4b3c-7b2a-b9f0-0123456789ab";
        let id: ExperimentId = raw.parse().expect("v7 uuid must parse");
        let path = exp_meta_object_path(&id);
        assert_eq!(path, format!("workflow/exp/{raw}/meta.json"));
        assert!(!path.starts_with('/'));
    }

    #[test]
    fn exp_stage_refs_object_path_is_repo_scoped_and_relative() {
        let raw = "01931b9e-4b3c-7b2a-b9f0-0123456789ab";
        let id: ExperimentId = raw.parse().expect("v7 uuid must parse");
        let path = exp_stage_refs_object_path(&id);
        assert_eq!(path, format!("workflow/exp/{raw}/stage-refs.json"));
        assert!(!path.starts_with('/'));
    }

    #[test]
    fn serde_round_trips_through_string_form() {
        let id = ExperimentId::new_v7();
        let json = serde_json::to_string(&id).expect("serialize must succeed");

        // The on-wire form is the quoted canonical string, not a
        // JSON-array of 16 bytes. This pins the human-readable
        // contract downstream consumers rely on.
        assert_eq!(json, format!("\"{id}\""));

        let back: ExperimentId = serde_json::from_str(&json).expect("deserialize must succeed");
        assert_eq!(id, back);
    }

    #[test]
    fn serde_rejects_non_v7_json_payload() {
        // Confirm the Deserialize path routes through the same
        // validator — a v4 UUID must fail deserialization loudly
        // rather than sneak through as a valid ExperimentId.
        let v4_json = "\"f47ac10b-58cc-4372-a567-0e02b2c3d479\"";
        let res: std::result::Result<ExperimentId, _> = serde_json::from_str(v4_json);
        assert!(res.is_err(), "v4 UUID must fail deserialization");
    }

    #[test]
    fn ref_prefix_constants_match_builder_output() {
        // The constants and the builders must stay in lock-step; the
        // GC walker uses the constants to match refs the builders
        // produced. Regression-proof it.
        let raw = "01931b9e-4b3c-7b2a-b9f0-0123456789ab";
        let id: ExperimentId = raw.parse().expect("v7 uuid must parse");
        assert!(exp_ref(&id).starts_with(EXP_REF_PREFIX));
        assert!(exp_meta_ref(&id).starts_with(EXP_META_REF_PREFIX));

        let hash = stage_hash_with_first_byte(0x00);
        assert!(stage_ref(&hash).starts_with(STAGE_REF_PREFIX));
    }

    // ───────────── ExperimentMetadata + CAS helpers ─────────────

    fn sample_metadata(id: ExperimentId) -> ExperimentMetadata {
        // Exercise every field — a struct with an always-empty
        // optional map would let reorder bugs hide.
        let mut param_overrides = BTreeMap::new();
        param_overrides.insert("model.lr".to_owned(), "0.001".to_owned());
        param_overrides.insert("data.window".to_owned(), "30".to_owned());

        let mut stages = BTreeMap::new();
        stages.insert("train".to_owned(), "aa".repeat(32));
        stages.insert("evaluate".to_owned(), "bb".repeat(32));

        let mut metrics = BTreeMap::new();
        metrics.insert(
            "metrics/train.json".to_owned(),
            serde_json::json!({ "loss": 0.12, "acc": 0.93 }),
        );

        ExperimentMetadata {
            schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
            exp_id: id,
            base_commit: "abcdef0123456789abcdef0123456789abcdef01".to_owned(),
            queue_commit: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
            name: Some("baseline-tune".to_owned()),
            message: Some("tune baseline learning rate".to_owned()),
            status: "success".to_owned(),
            param_overrides,
            stages,
            metrics,
            cli_args: vec!["exp".to_owned(), "run".to_owned(), "--set".to_owned()],
            host_fingerprint: "linux-x86_64-crab-0.8.0".to_owned(),
            started_at: "2026-04-27T14:23:11.083Z".to_owned(),
            ended_at: Some("2026-04-27T14:31:02.441Z".to_owned()),
        }
    }

    fn pinned_id() -> ExperimentId {
        // Fixed v7 UUID — avoids flaky tests that depend on
        // wall-clock when comparing byte outputs across CI runs.
        "01931b9e-4b3c-7b2a-b9f0-0123456789ab"
            .parse()
            .expect("v7 uuid must parse")
    }

    #[test]
    fn metadata_canonical_json_is_byte_stable() {
        // Two back-to-back serializations of the same logical value
        // must produce identical bytes. This is what
        // `ExperimentMetadata::content_hash` depends on.
        let meta = sample_metadata(pinned_id());
        let a = meta.canonical_json().expect("serialize ok");
        let b = meta.canonical_json().expect("serialize ok");
        assert_eq!(a, b, "canonical JSON must be byte-identical across calls");
    }

    #[test]
    fn metadata_canonical_json_is_order_independent_for_maps() {
        // `BTreeMap` guarantees sorted iteration, but verify that
        // inserting keys in different orders into two separate maps
        // produces byte-identical canonical JSON — the property the
        // GC walker and `exp diff` rely on.
        let id = pinned_id();
        let mut a = sample_metadata(id);
        let mut b = sample_metadata(id);

        // Rebuild maps with opposite insertion order. `BTreeMap`
        // iteration is still sorted, so outputs must match.
        a.param_overrides = BTreeMap::new();
        a.param_overrides.insert("a".to_owned(), "1".to_owned());
        a.param_overrides.insert("z".to_owned(), "26".to_owned());

        b.param_overrides = BTreeMap::new();
        b.param_overrides.insert("z".to_owned(), "26".to_owned());
        b.param_overrides.insert("a".to_owned(), "1".to_owned());

        assert_eq!(
            a.canonical_json().expect("a ok"),
            b.canonical_json().expect("b ok"),
            "BTreeMap insertion order must not affect canonical JSON"
        );
    }

    #[test]
    fn metadata_round_trips_through_canonical_json() {
        let meta = sample_metadata(pinned_id());
        let bytes = meta.canonical_json().expect("serialize ok");
        let back: ExperimentMetadata = serde_json::from_slice(&bytes).expect("deserialize ok");
        assert_eq!(meta, back);
    }

    #[test]
    fn metadata_v2_without_status_reads_as_unknown() {
        let mut value = serde_json::to_value(sample_metadata(pinned_id())).expect("serialize ok");
        let object = value.as_object_mut().expect("metadata object");
        object.insert("schema_version".to_owned(), serde_json::json!(2));
        object.remove("status");

        let back: ExperimentMetadata = serde_json::from_value(value).expect("deserialize ok");
        assert_eq!(back.schema_version, 2);
        assert_eq!(back.status, "unknown");
    }

    #[test]
    fn metadata_content_hash_is_64_hex_chars() {
        let meta = sample_metadata(pinned_id());
        let hash = meta.content_hash().expect("hash ok");
        assert_eq!(hash.len(), 64, "blake3 hex must be 64 chars");
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "hash must be lowercase hex: {hash}"
        );
    }

    #[test]
    fn build_exp_ref_cas_targets_commit_ref() {
        let id = pinned_id();
        let oid = "deadbeefcafebabedeadbeefcafebabedeadbeef";
        let etag = Some("etag-123".to_owned());

        let op = build_exp_ref_cas(&id, oid, etag.clone());
        assert_eq!(op.ref_name, format!("refs/crab/exp/{id}"));
        assert_eq!(op.new_sha, oid);
        assert_eq!(op.expected_etag, etag);
    }

    #[test]
    fn build_exp_ref_cas_accepts_none_etag() {
        let id = pinned_id();
        let op = build_exp_ref_cas(&id, "deadbeef", None);
        assert!(
            op.expected_etag.is_none(),
            "None etag must pass through for first-write semantics"
        );
    }

    #[test]
    fn build_exp_meta_ref_cas_targets_meta_ref() {
        let id = pinned_id();
        let hash = "ff".repeat(32);
        let op = build_exp_meta_ref_cas(&id, &hash, None);
        assert_eq!(op.ref_name, format!("refs/crab/exp-meta/{id}"));
        assert_eq!(op.new_sha, hash);
    }

    /// In-memory mock `ExperimentMetaRead` used to exercise the read
    /// helper. Stores refs and objects in plain maps — a minimal
    /// parallel to the `SuccessStore` pattern in
    /// `pipelined_commit::tests`.
    #[derive(Default)]
    struct MockMetaStore {
        refs: std::collections::HashMap<String, String>,
        objects: std::collections::HashMap<String, Vec<u8>>,
    }

    impl ExperimentMetaRead for MockMetaStore {
        async fn read_ref(&self, ref_name: &str) -> Result<Option<String>> {
            Ok(self.refs.get(ref_name).cloned())
        }

        async fn read_object(&self, object_path: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.objects.get(object_path).cloned())
        }
    }

    #[tokio::test]
    async fn read_experiment_metadata_hit_returns_metadata() {
        let id = pinned_id();
        let meta = sample_metadata(id);
        let bytes = meta.canonical_json().expect("serialize ok");
        let hash = meta.content_hash().expect("hash ok");

        let mut store = MockMetaStore::default();
        store.refs.insert(exp_meta_ref(&id), hash);
        store.objects.insert(exp_meta_object_path(&id), bytes);

        let got = read_experiment_metadata(&store, &id)
            .await
            .expect("read ok");
        assert_eq!(got, Some(meta));
    }

    #[tokio::test]
    async fn read_experiment_metadata_missing_ref_returns_none() {
        let id = pinned_id();
        let store = MockMetaStore::default();
        let got = read_experiment_metadata(&store, &id)
            .await
            .expect("read ok");
        assert!(got.is_none(), "missing ref must surface as Ok(None)");
    }

    #[tokio::test]
    async fn read_experiment_metadata_ref_without_object_returns_none() {
        // Ref points somewhere but the blob itself is gone (e.g.
        // concurrent GC raced a reader). The helper logs and
        // returns None so downstream callers can skip this exp.
        let id = pinned_id();
        let mut store = MockMetaStore::default();
        store.refs.insert(exp_meta_ref(&id), "ff".repeat(32));
        // Intentionally no matching object.

        let got = read_experiment_metadata(&store, &id)
            .await
            .expect("read ok");
        assert!(got.is_none(), "missing object must surface as Ok(None)");
    }

    #[tokio::test]
    async fn read_experiment_metadata_rejects_newer_schema() {
        let id = pinned_id();
        // Hand-craft a blob stamped with a schema version this
        // binary doesn't know about.
        let future_blob = serde_json::json!({
            "schema_version": 999u16,
            "exp_id": id,
            "base_commit": "x",
            "queue_commit": null,
            "param_overrides": {},
            "stages": {},
            "metrics": {},
            "cli_args": [],
            "host_fingerprint": "test",
            "started_at": "2026-01-01T00:00:00.000Z",
            "ended_at": null,
        });
        let bytes = serde_json::to_vec(&future_blob).expect("serialize ok");

        let mut store = MockMetaStore::default();
        store.refs.insert(exp_meta_ref(&id), "dummy".to_owned());
        store.objects.insert(exp_meta_object_path(&id), bytes);

        let err = read_experiment_metadata(&store, &id)
            .await
            .expect_err("newer schema must error");
        match err {
            CrabError::WorkflowExperimentMetadataSchemaNewer {
                id: got_id,
                found,
                supported,
            } => {
                assert_eq!(got_id, id.to_string());
                assert_eq!(found, 999);
                assert_eq!(supported, EXPERIMENT_METADATA_MAX_SUPPORTED_SCHEMA);
            }
            other => panic!("wrong variant: {other}"),
        }
    }

    #[tokio::test]
    async fn read_experiment_metadata_surfaces_malformed_json_as_internal() {
        // Missing `schema_version` key — not "newer than supported",
        // just broken JSON. The helper surfaces this as
        // `CrabError::Internal` so the caller can distinguish
        // "future binary" from "garbage on disk".
        let id = pinned_id();
        let mut store = MockMetaStore::default();
        store.refs.insert(exp_meta_ref(&id), "dummy".to_owned());
        store
            .objects
            .insert(exp_meta_object_path(&id), b"not even json".to_vec());

        let err = read_experiment_metadata(&store, &id)
            .await
            .expect_err("malformed blob must error");
        assert!(matches!(err, CrabError::Internal(_)), "got: {err}");
    }

    #[test]
    fn experiment_wrapper_defaults_to_no_commit_oid() {
        let exp = Experiment::new(sample_metadata(pinned_id()));
        assert!(exp.commit_oid.is_none());
        assert_eq!(exp.id(), &pinned_id());
    }
}
