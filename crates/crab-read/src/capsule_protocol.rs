//! Verified loading of one capsule-protocol root and its bounded capsule frontier.

use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleControl, CapsuleGitPackDescriptor, CapsulePointer, CapsuleRun,
    CapsuleRunControl, Checkpoint, CheckpointControl, CheckpointPointer, LayeredCheckpoint,
    PackMemberDescriptor, PackSourceDescriptor, PackSourceKind, PointerCatalog, RootRecord,
    load_root, visibility_object_set_digest,
};
use crab_storage::{Store, StoreLayout};
use futures_util::future::try_join_all;
use futures_util::{StreamExt, TryStreamExt};
use gix_hash::ObjectId;
use object_store::ObjectMeta;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::{ReadError, Result};

const LAYERED_SIDECAR_COALESCE_GAP_BYTES: u64 = 64 * 1024;
const LAYERED_SIDECAR_MAX_EXTRA_BYTES: u64 = 4 * 1024 * 1024;
const LAYERED_SIDECAR_MAX_WINDOW_BYTES: u64 = 16 * 1024 * 1024;
const LAYERED_FULL_MAX_WINDOW_BYTES: u64 = 64 * 1024 * 1024;
const LAYERED_SIDECAR_READ_CONCURRENCY: usize = 8;
const LAYERED_LARGE_RANGE_THRESHOLD_BYTES: u64 = 128 * 1024 * 1024;
const LAYERED_LARGE_RANGE_CHUNK_BYTES: u64 = 128 * 1024 * 1024;
const LAYERED_LARGE_RANGE_READ_CONCURRENCY: usize = 6;
const LAYERED_COLD_CLONE_RANGE_CHUNK_BYTES: u64 = 128 * 1024 * 1024;
const LAYERED_COLD_CLONE_RANGE_READ_CONCURRENCY: usize = 10;

/// Caller-owned memory admission for one capsule-protocol repository view.
#[derive(Debug, Clone, Copy)]
pub struct CapsuleReadLimits {
    /// Largest individual capsule body accepted by this reader.
    pub max_capsule_bytes: u64,
    /// Largest aggregate capsule frontier accepted by this reader.
    pub max_frontier_bytes: u64,
}

/// Bounds for a complete reachable dependency proof of one capsule view.
#[derive(Debug, Clone, Copy)]
pub struct CapsuleDependencyLimits {
    /// Largest aggregate Git pack intake accepted by the temporary verifier.
    pub max_git_bytes: u64,
    /// Bounds for the complete reachable Git pointer scan.
    pub pointer_scan: crab_git::walk::PointerScanLimits,
}

/// Counts returned by a complete reachable dependency proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapsuleDependencyProof {
    /// File recipes authenticated by the complete pointer catalog.
    pub catalog_files: u64,
    /// Shard bodies authenticated and hash-verified by the proof.
    pub catalog_shards: u64,
    /// Xorb bodies authenticated and hash-verified by the proof.
    pub catalog_xorbs: u64,
    /// Reachable Crab pointer blobs bound to catalog file recipes.
    pub reachable_crab_pointers: u64,
    /// Distinct reachable LFS bodies read and hash-verified from origin.
    pub reachable_lfs_objects: u64,
}

/// One authenticated repository root and every post-checkpoint capsule it names.
#[derive(Debug, Clone)]
pub struct CapsuleRepositoryView {
    root: crab_metadata::capsule_protocol::RootSnapshot,
    checkpoint: Option<CheckpointData>,
    capsules: Vec<Capsule>,
    capsule_controls: Vec<CapsuleControl>,
    tip_bound_transitions: CapsuleTipBoundTransitions,
    refs: BTreeMap<String, String>,
    peeled_refs: BTreeMap<String, String>,
    visible_ref_transactions: BTreeMap<String, String>,
    ref_capsule_counts: BTreeMap<String, u32>,
    capsule_run_pointers: Vec<CapsulePointer>,
    capsule_run_sources: Vec<PackSourceDescriptor>,
    capsule_run_member_oids: BTreeMap<String, Vec<Vec<[u8; 20]>>>,
    frontier_object_admission: BTreeMap<[u8; 20], Vec<String>>,
}

/// One complete layered Git pack that can be copied directly to a clone.
///
/// This is admitted only for a cold, unfiltered clone whose authenticated view
/// contains exactly one self-contained member. The wire layer still verifies
/// the streamed bytes before returning the session.
#[derive(Debug, Clone)]
pub struct LayeredColdClonePack {
    /// Immutable source object containing the pack body.
    pub source_path: object_store::path::Path,
    /// Byte range of the complete Git pack in `source_path`.
    pub pack_range: std::ops::Range<u64>,
    /// Authenticated BLAKE3 identity of the pack body.
    pub content_hash: String,
    /// Git SHA-1 trailer committed by the pack descriptor.
    pub git_checksum: String,
    /// Object count committed by the pack descriptor.
    pub object_count: u64,
    /// Digest of the complete visibility object set, when the checkpoint
    /// carries the compact cold-clone proof.
    pub object_set_digest: Option<String>,
    /// Number of objects covered by the compact cold-clone proof.
    pub object_set_count: Option<u64>,
}

/// One authenticated per-ref visibility transition available to an ordinary fetch.
///
/// The object sets are derived from the capsule's signed visibility edit. They
/// are an optimization hint only: callers must still require an exact
/// old-tip-to-new-tip chain and fall back to graph traversal when that chain is
/// unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleVisibilityTransition {
    /// Ref tip before this transition, when the ref already existed.
    pub old_oid: Option<ObjectId>,
    /// Ref tip made visible by this transition.
    pub new_oid: ObjectId,
    /// Objects added to the prior ref closure.
    pub added: Vec<ObjectId>,
    /// Objects removed from the prior ref closure.
    pub removed: Vec<ObjectId>,
}

/// Authenticated transition history grouped by ref name.
pub type CapsuleTipBoundTransitions = BTreeMap<String, Vec<CapsuleVisibilityTransition>>;

#[derive(Debug, Clone)]
enum CheckpointData {
    Complete(Checkpoint),
    Control(CheckpointControl),
    Layered(LayeredCheckpoint),
}

/// One authenticated root and its transaction-consistent mutable ref state.
///
/// This view intentionally excludes checkpoint and capsule payloads. Writers
/// may use it for ref policy and expected-old validation, but consumers of Git
/// objects or pointer catalogs must open a [`CapsuleRepositoryView`].
#[derive(Debug, Clone)]
pub struct CapsuleRefView {
    root: crab_metadata::capsule_protocol::RootSnapshot,
    refs: BTreeMap<String, String>,
    peeled_refs: BTreeMap<String, String>,
    visible_ref_transactions: BTreeMap<String, String>,
}

impl CapsuleRefView {
    /// Return the authoritative repository root captured with this ref state.
    #[must_use]
    pub fn root_snapshot(&self) -> &crab_metadata::capsule_protocol::RootSnapshot {
        &self.root
    }

    /// Return refs materialized from the compacted root and captured heads.
    #[must_use]
    pub fn refs(&self) -> &BTreeMap<String, String> {
        &self.refs
    }

    /// Return peeled refs materialized from the same captured heads.
    #[must_use]
    pub fn peeled_refs(&self) -> &BTreeMap<String, String> {
        &self.peeled_refs
    }

    /// Return the transaction identity visible at each captured ref.
    #[must_use]
    pub fn visible_ref_transactions(&self) -> &BTreeMap<String, String> {
        &self.visible_ref_transactions
    }

    /// Return the symbolic HEAD target owned by the compacted root.
    #[must_use]
    pub fn head(&self) -> &str {
        self.root.record().root().head()
    }
}

impl From<CapsuleRepositoryView> for CapsuleRefView {
    fn from(view: CapsuleRepositoryView) -> Self {
        Self {
            root: view.root,
            refs: view.refs,
            peeled_refs: view.peeled_refs,
            visible_ref_transactions: view.visible_ref_transactions,
        }
    }
}

/// Payload-free fingerprint used by background maintenance polling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleRepositoryActivity {
    state_digest: String,
    capsule_count: u64,
    ref_count: u64,
}

impl CapsuleRepositoryActivity {
    /// Return the digest covering the root and every visible per-ref position.
    #[must_use]
    pub fn state_digest(&self) -> &str {
        &self.state_digest
    }

    /// Return the total immutable capsule count in the visible frontier.
    #[must_use]
    pub const fn capsule_count(&self) -> u64 {
        self.capsule_count
    }

    /// Return the number of refs in the transaction-consistent view.
    #[must_use]
    pub const fn ref_count(&self) -> u64 {
        self.ref_count
    }
}

impl CapsuleRepositoryView {
    /// Return the authoritative repository generation and ref state.
    #[must_use]
    pub fn root(&self) -> &RootRecord {
        self.root.record()
    }

    /// Return the provider CAS token bound to the loaded root.
    #[must_use]
    pub fn root_snapshot(&self) -> &crab_metadata::capsule_protocol::RootSnapshot {
        &self.root
    }

    /// Return the complete base checkpoint, when the root names one.
    #[must_use]
    pub fn checkpoint(&self) -> Option<&Checkpoint> {
        match self.checkpoint.as_ref() {
            Some(CheckpointData::Complete(checkpoint)) => Some(checkpoint),
            Some(CheckpointData::Control(_)) | Some(CheckpointData::Layered(_)) | None => None,
        }
    }

    /// Return the authenticated checkpoint control suffix, when loaded.
    #[must_use]
    pub fn checkpoint_control(&self) -> Option<&CheckpointControl> {
        match self.checkpoint.as_ref() {
            Some(CheckpointData::Control(control)) => Some(control),
            Some(CheckpointData::Complete(_)) | None => None,
            Some(CheckpointData::Layered(_)) => None,
        }
    }

    /// Return the metadata-only layered checkpoint, when this view names one.
    #[must_use]
    pub fn layered_checkpoint(&self) -> Option<&LayeredCheckpoint> {
        match self.checkpoint.as_ref() {
            Some(CheckpointData::Layered(checkpoint)) => Some(checkpoint),
            Some(CheckpointData::Complete(_)) | Some(CheckpointData::Control(_)) | None => None,
        }
    }

    /// Return every verified post-checkpoint capsule in publication order.
    #[must_use]
    pub fn capsules(&self) -> &[Capsule] {
        &self.capsules
    }

    /// Return authenticated control-only capsules loaded for a layered view.
    #[must_use]
    pub fn capsule_controls(&self) -> &[CapsuleControl] {
        &self.capsule_controls
    }

    /// Return the refs materialized from the compacted root and per-ref heads.
    #[must_use]
    pub fn refs(&self) -> &BTreeMap<String, String> {
        &self.refs
    }

    /// Return the peeled refs materialized from the same stable view.
    #[must_use]
    pub fn peeled_refs(&self) -> &BTreeMap<String, String> {
        &self.peeled_refs
    }

    /// Return the symbolic HEAD target owned by the compacted control root.
    #[must_use]
    pub fn head(&self) -> &str {
        self.root.record().root().head()
    }

    /// Return the visible per-ref transaction positions captured by this view.
    #[must_use]
    pub fn visible_ref_transactions(&self) -> &BTreeMap<String, String> {
        &self.visible_ref_transactions
    }

    /// Return the post-checkpoint capsule count for one independently mutable ref.
    #[must_use]
    pub fn ref_capsule_count(&self, ref_name: &str) -> u32 {
        self.ref_capsule_counts
            .get(ref_name)
            .copied()
            .unwrap_or_default()
    }

    /// Return every immutable capsule run reachable from this exact view.
    #[must_use]
    pub fn capsule_run_pointers(&self) -> &[CapsulePointer] {
        &self.capsule_run_pointers
    }

    /// Return the authenticated physical sources for the visible capsule runs.
    #[must_use]
    pub fn capsule_run_sources(&self) -> &[PackSourceDescriptor] {
        &self.capsule_run_sources
    }

    /// Return the verified sorted object IDs for each capsule-run pack member.
    ///
    /// The map is populated only for complete views, where run bodies are
    /// already authenticated in memory. Control-only fetch views rely on the
    /// checkpoint's ordinal admission proof instead.
    #[must_use]
    pub fn capsule_run_member_oids(&self) -> &BTreeMap<String, Vec<Vec<[u8; 20]>>> {
        &self.capsule_run_member_oids
    }

    /// Return exact frontier object-to-source-member candidates derived from
    /// authenticated visibility deltas.
    #[must_use]
    pub fn frontier_object_admission(&self) -> &BTreeMap<[u8; 20], Vec<String>> {
        &self.frontier_object_admission
    }

    /// Return authenticated visibility transitions without loading pack bodies.
    ///
    /// This is used only to avoid re-walking a known ref update. The returned
    /// transitions remain bound to the loaded root and controls; a consumer
    /// must not use a partial chain as an authorization proof.
    #[must_use]
    pub fn tip_bound_transitions(&self) -> &CapsuleTipBoundTransitions {
        &self.tip_bound_transitions
    }

    /// Admit the complete layered cold-clone pack set for this exact view.
    ///
    /// The classic remote-helper fetch contract can install several immutable
    /// packs directly into the local object database. Protocol-v2's wire
    /// response still uses the singular member method below, because its
    /// `packfile` response cannot concatenate multiple complete packs.
    pub fn layered_cold_clone_packs(
        &self,
        layout: &StoreLayout<Store>,
        max_bytes: u64,
    ) -> Result<Option<Vec<LayeredColdClonePack>>> {
        let sources = if let Some(checkpoint) = self.layered_checkpoint() {
            if !self.capsules.is_empty() || !self.capsule_controls.is_empty() {
                return Ok(None);
            }
            checkpoint.sources()
        } else if self.checkpoint.is_none()
            && self.capsules.is_empty()
            && self.capsule_controls.is_empty()
            && self.capsule_run_pointers.len() == 1
            && self.capsule_run_sources.len() == 1
        {
            // Before the first checkpoint the run footer and detached controls
            // still authenticate the complete source directory. Restrict this
            // shortcut to one run so a later frontier cannot omit a source.
            self.capsule_run_sources.as_slice()
        } else {
            return Ok(None);
        };

        let mut total_pack_bytes = 0_u64;
        let mut packs = Vec::new();
        for source in sources {
            let source_path = match source.kind() {
                PackSourceKind::CapsuleRun => layout.capsule_path(source.object_hash()),
                PackSourceKind::PackLayer => layout.capsule_pack_layer_path(source.object_hash()),
            };
            for member in source.members() {
                // A direct install preserves the immutable pack bytes exactly;
                // an external delta base would require response-pack repair or
                // a separately proven local base, so fail closed here.
                if !member.external_delta_bases().is_empty() {
                    return Ok(None);
                }
                let pack_end = member
                    .pack()
                    .offset()
                    .checked_add(member.pack().length())
                    .ok_or_else(|| corrupt_path("layered cold clone", "pack range overflowed"))?;
                if pack_end > source.object_size() {
                    return Err(corrupt_path(
                        "layered cold clone",
                        "pack range exceeds its authenticated source",
                    ));
                }
                total_pack_bytes = total_pack_bytes
                    .checked_add(member.pack().length())
                    .ok_or_else(|| {
                        corrupt_path("layered cold clone", "pack byte count overflowed")
                    })?;
                if max_bytes > 0 && total_pack_bytes > max_bytes {
                    return Ok(None);
                }
                packs.push(LayeredColdClonePack {
                    source_path: source_path.clone(),
                    pack_range: member.pack().offset()..pack_end,
                    content_hash: member.pack().blake3().to_owned(),
                    git_checksum: member.git_checksum().to_owned(),
                    object_count: member.object_count(),
                    object_set_digest: self
                        .layered_checkpoint()
                        .and_then(|checkpoint| checkpoint.cold_clone_object_set_digest())
                        .map(str::to_owned),
                    object_set_count: self
                        .layered_checkpoint()
                        .and_then(LayeredCheckpoint::cold_clone_object_count),
                });
            }
        }
        if packs.is_empty() {
            return Ok(None);
        }
        Ok(Some(packs))
    }

    /// Admit the one-pack cold-clone fast path for protocol-v2's wire format.
    pub fn layered_cold_clone_pack(
        &self,
        layout: &StoreLayout<Store>,
        max_bytes: u64,
    ) -> Result<Option<LayeredColdClonePack>> {
        let Some(mut packs) = self.layered_cold_clone_packs(layout, max_bytes)? else {
            return Ok(None);
        };
        if packs.len() != 1 {
            return Ok(None);
        }
        Ok(packs.pop())
    }

    fn layered_cold_clone_source(&self) -> Option<&PackSourceDescriptor> {
        if let Some(checkpoint) = self.layered_checkpoint() {
            return (checkpoint.source_count() == 1 && checkpoint.pack_count().ok() == Some(1))
                .then(|| checkpoint.sources().first())
                .flatten();
        }
        (self.checkpoint.is_none()
            && self.capsules.is_empty()
            && self.capsule_controls.is_empty()
            && self.capsule_run_pointers.len() == 1
            && self.capsule_run_sources.len() == 1
            && self
                .capsule_run_sources
                .first()
                .is_some_and(|source| source.members().len() == 1))
        .then(|| self.capsule_run_sources.first())
        .flatten()
    }

    /// Return whether the cold-clone pack has a complete authenticated
    /// visibility admission proof.
    ///
    /// A layered checkpoint with one physical source/member can skip a second
    /// full Git reachability walk only when its ordinal visibility snapshot
    /// maps every admitted object to that exact member. Older checkpoints and
    /// multi-member views return `false`, leaving callers on native fsck.
    pub fn layered_cold_clone_has_complete_admission(&self) -> Result<bool> {
        if let Some(checkpoint) = self.layered_checkpoint() {
            return self.layered_checkpoint_has_complete_admission(checkpoint);
        }
        self.layered_uncheckpointed_has_complete_admission()
    }

    /// Check a control-only checkpoint's cold-clone proof without reading its body.
    ///
    /// The footer carries an authenticated digest of the complete visibility
    /// object set. The installer compares that digest with the downloaded pack
    /// index; checkpoints without it fail closed to the native validation path.
    pub async fn layered_cold_clone_has_complete_admission_from_store(
        &self,
        _layout: &StoreLayout<Store>,
        _max_bytes: u64,
    ) -> Result<bool> {
        let Some(checkpoint) = self.layered_checkpoint() else {
            return self.layered_uncheckpointed_has_complete_admission();
        };
        if !checkpoint.is_control_only() {
            return self.layered_checkpoint_has_complete_admission(checkpoint);
        }
        if checkpoint.source_count() != 1
            || checkpoint.pack_count()? != 1
            || checkpoint.cold_clone_object_set_digest().is_none()
        {
            return Ok(false);
        }
        let Some(member) = checkpoint
            .sources()
            .first()
            .and_then(|source| source.members().first())
        else {
            return Ok(false);
        };
        Ok(member.external_delta_bases().is_empty()
            && checkpoint.cold_clone_object_count() == Some(member.object_count()))
    }

    fn layered_checkpoint_has_complete_admission(
        &self,
        checkpoint: &LayeredCheckpoint,
    ) -> Result<bool> {
        if checkpoint.is_control_only()
            || checkpoint.source_count() != 1
            || checkpoint.pack_count()? != 1
        {
            return Ok(false);
        }
        let Some(visibility) = checkpoint.visibility_ordinal_snapshot()? else {
            return Ok(false);
        };
        let Some(admission) = visibility.member_admission() else {
            return Ok(false);
        };
        if admission.len() != visibility.objects().len() {
            return Err(corrupt_path(
                "layered cold clone",
                "visibility admission length does not match its object dictionary",
            ));
        }
        if !admission
            .iter()
            .all(|member| member.source_index() == 0 && member.member_index() == 0)
        {
            return Ok(false);
        }
        let catalog_digest =
            crab_metadata::capsule_protocol::source_catalog_digest(checkpoint.sources())?;
        let index = visibility.to_index(0, &"0".repeat(64), &"0".repeat(64), &catalog_digest)?;
        Ok(self
            .refs
            .iter()
            .all(|(ref_name, oid)| index.contains_hex_in_ref(ref_name, oid)))
    }

    fn layered_uncheckpointed_has_complete_admission(&self) -> Result<bool> {
        if self.checkpoint.is_some()
            || self.capsule_run_pointers.len() != 1
            || self.capsule_run_sources.len() != 1
        {
            return Ok(false);
        }
        let Some(source) = self.capsule_run_sources.first() else {
            return Ok(false);
        };
        if source.members().len() != 1 {
            return Ok(false);
        }
        let member = &source.members()[0];
        if !member.external_delta_bases().is_empty() {
            return Ok(false);
        }
        let pack_id = member.pack().blake3();
        let object_count = usize::try_from(member.object_count()).ok();
        // The frontier map is built only from authenticated visibility
        // additions and resolved through the run's OID-to-member admission.
        // Equality with the pack index count therefore proves the complete
        // visible object universe for this one-member, self-contained root.
        if object_count != Some(self.frontier_object_admission.len()) {
            return Ok(false);
        }
        if self.frontier_object_admission.values().any(|pack_ids| {
            pack_ids.len() != 1
                || pack_ids
                    .first()
                    .is_none_or(|candidate| candidate != pack_id)
        }) {
            return Ok(false);
        }
        let contains_ref_tip = |oid: &str| {
            ObjectId::from_hex(oid.as_bytes())
                .ok()
                .and_then(|oid| <[u8; 20]>::try_from(oid.as_bytes()).ok())
                .is_some_and(|oid| self.frontier_object_admission.contains_key(&oid))
        };
        if self
            .refs
            .values()
            .chain(self.peeled_refs.values())
            .any(|oid| !contains_ref_tip(oid))
        {
            return Ok(false);
        }
        Ok(true)
    }

    /// Return the total immutable capsule count represented by the visible frontier.
    pub fn capsule_count(&self) -> Result<u64> {
        self.capsule_run_pointers
            .iter()
            .try_fold(0_u64, |total, pointer| {
                total
                    .checked_add(u64::from(pointer.capsule_count()))
                    .ok_or_else(|| ReadError::internal("capsule frontier count overflowed"))
            })
    }

    /// Return the number of Git packs authenticated by this exact view.
    #[must_use]
    pub fn git_pack_count(&self) -> usize {
        let checkpoint_count = self.checkpoint_pack_count();
        if self.layered_checkpoint().is_some() {
            checkpoint_count
        } else {
            checkpoint_count
                .saturating_add(
                    self.capsules
                        .iter()
                        .map(|capsule| capsule.git_packs().len())
                        .sum(),
                )
                .saturating_add(
                    self.capsule_controls
                        .iter()
                        .map(|capsule| capsule.git_packs().len())
                        .sum(),
                )
        }
    }

    /// Return the total authenticated Git pack-body bytes in this exact view.
    pub fn git_pack_bytes(&self) -> Result<u64> {
        let checkpoint_bytes = match self.checkpoint.as_ref() {
            Some(CheckpointData::Complete(checkpoint)) => checkpoint
                .git_packs()
                .iter()
                .map(|pack| checkpoint.section_bytes(pack.pack_section()))
                .try_fold(0_u64, |total, bytes| {
                    total.checked_add(bytes?.len() as u64).ok_or_else(|| {
                        ReadError::internal("checkpoint Git pack byte total overflowed")
                    })
                })?,
            Some(CheckpointData::Control(control)) => control
                .git_packs()
                .iter()
                .map(|pack| control.section_location(pack.pack_section()))
                .try_fold(0_u64, |total, location| {
                    total.checked_add(location?.length()).ok_or_else(|| {
                        ReadError::internal("checkpoint Git pack byte total overflowed")
                    })
                })?,
            Some(CheckpointData::Layered(checkpoint)) => checkpoint
                .sources()
                .iter()
                .flat_map(|source| source.members().iter())
                .try_fold(0_u64, |total, member| {
                    total.checked_add(member.pack().length()).ok_or_else(|| {
                        ReadError::internal("layered checkpoint Git pack byte total overflowed")
                    })
                })?,
            None => 0,
        };
        if self.layered_checkpoint().is_some() {
            Ok(checkpoint_bytes)
        } else {
            let total = self
                .capsules
                .iter()
                .flat_map(|capsule| capsule.git_packs().iter().map(move |pack| (capsule, pack)))
                .try_fold(checkpoint_bytes, |total, (capsule, pack)| {
                    total
                        .checked_add(capsule.section_bytes(pack.pack_section())?.len() as u64)
                        .ok_or_else(|| {
                            ReadError::internal("capsule Git pack byte total overflowed")
                        })
                })?;
            self.capsule_controls
                .iter()
                .flat_map(CapsuleControl::git_packs)
                .try_fold(total, |total, pack| {
                    total.checked_add(pack.pack().length()).ok_or_else(|| {
                        ReadError::internal("capsule Git pack byte total overflowed")
                    })
                })
        }
    }

    /// Return the total Git objects declared by the authenticated pack inventory.
    pub fn git_object_count(&self) -> Result<u64> {
        let checkpoint_count =
            match self.checkpoint.as_ref() {
                Some(CheckpointData::Complete(checkpoint)) => checkpoint
                    .git_packs()
                    .iter()
                    .try_fold(0_u64, |total, pack| {
                        total.checked_add(pack.object_count()).ok_or_else(|| {
                            ReadError::internal("capsule Git object count overflowed")
                        })
                    })?,
                Some(CheckpointData::Control(control)) => {
                    control.git_packs().iter().try_fold(0_u64, |total, pack| {
                        total.checked_add(pack.object_count()).ok_or_else(|| {
                            ReadError::internal("capsule Git object count overflowed")
                        })
                    })?
                }
                Some(CheckpointData::Layered(checkpoint)) => checkpoint.object_count()?,
                None => 0,
            };
        if self.layered_checkpoint().is_some() {
            Ok(checkpoint_count)
        } else {
            let total = self.capsules.iter().flat_map(Capsule::git_packs).try_fold(
                checkpoint_count,
                |total, pack| {
                    total
                        .checked_add(pack.object_count())
                        .ok_or_else(|| ReadError::internal("capsule Git object count overflowed"))
                },
            )?;
            self.capsule_controls
                .iter()
                .flat_map(CapsuleControl::git_packs)
                .try_fold(total, |total, pack| {
                    total
                        .checked_add(pack.object_count())
                        .ok_or_else(|| ReadError::internal("capsule Git object count overflowed"))
                })
        }
    }

    /// Return a digest that changes with the root or any visible per-ref position.
    #[must_use]
    pub fn state_digest(&self) -> String {
        state_digest(self.root.record(), &self.visible_ref_transactions)
    }

    /// Materialize the complete generation-pinned external pointer catalog.
    pub fn pointer_catalog(&self) -> Result<PointerCatalog> {
        let mut catalog = match self.checkpoint.as_ref() {
            Some(CheckpointData::Complete(checkpoint)) => checkpoint.pointer_catalog()?,
            Some(CheckpointData::Control(control)) => control.pointer_catalog()?,
            Some(CheckpointData::Layered(checkpoint)) => checkpoint.pointer_catalog()?,
            None => PointerCatalog::new(),
        };
        for capsule in &self.capsules {
            if let Some(delta) = capsule.pointer_catalog_delta()? {
                catalog.apply(&delta)?;
            }
        }
        for capsule in &self.capsule_controls {
            if let Some(delta) = capsule.pointer_catalog_delta() {
                catalog.apply(delta)?;
            }
        }
        Ok(catalog)
    }

    /// Return complete verified packs suitable for a checkpoint over this view.
    pub fn checkpoint_git_packs(
        &self,
    ) -> Result<Vec<crab_metadata::capsule_protocol::CapsuleGitPack>> {
        if self.layered_checkpoint().is_some() {
            return Err(ReadError::internal(
                "layered checkpoint packs remain in authenticated source objects",
            ));
        }
        if self.checkpoint_control().is_some() {
            return Err(ReadError::internal(
                "complete checkpoint bytes are required to build a replacement checkpoint",
            ));
        }
        self.checkpoint()
            .into_iter()
            .cloned()
            .map(GitPackContainer::Checkpoint)
            .chain(self.capsules.iter().cloned().map(GitPackContainer::Capsule))
            .flat_map(|container| {
                let descriptors = container.git_packs().to_vec();
                descriptors.into_iter().map(move |descriptor| {
                    crab_metadata::capsule_protocol::CapsuleGitPack::new(
                        container.section_bytes(descriptor.pack_section())?,
                        container.section_bytes(descriptor.index_section())?,
                        container.section_bytes(descriptor.reverse_index_section())?,
                        container.section_bytes(descriptor.locator_section())?,
                        descriptor.git_checksum(),
                        descriptor.object_count(),
                    )
                    .map_err(Into::into)
                })
            })
            .collect()
    }

    /// Open the authenticated embedded Git packs as a filesystem-free repository.
    ///
    /// The returned handle is pinned to this exact root and uses a private
    /// in-memory object store. This lets protocol-v2 upload-pack reuse the
    /// bounded remote Git reader without publishing legacy manifests or pack
    /// sidecars alongside the capsule protocol.
    pub async fn git_repository(
        &self,
        identity: crab_remote_git::RepositoryIdentity,
        runtime: Arc<crab_remote_git::RemoteGitRuntime>,
        options: crab_remote_git::RepositoryOptions,
        max_input_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<crab_remote_git::RemoteGitRepository> {
        if cancellation.is_cancelled() {
            return Err(ReadError::Cancelled);
        }
        if self.checkpoint_control().is_some() {
            return Err(ReadError::internal(
                "complete checkpoint bytes are required to open a Git repository",
            ));
        }
        let workspace = tempfile::tempdir()?;
        let installed = install_git_packs(self, workspace.path(), max_input_bytes).await?;
        let artifacts = self
            .checkpoint()
            .into_iter()
            .cloned()
            .map(GitPackContainer::Checkpoint)
            .chain(self.capsules.iter().cloned().map(GitPackContainer::Capsule))
            .flat_map(|container| {
                let descriptors = container.git_packs().to_vec();
                descriptors.into_iter().map(move |descriptor| {
                    let locator = container.section_bytes(descriptor.locator_section());
                    locator.map(|locator| (descriptor, locator))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if installed.len() != artifacts.len() {
            return Err(ReadError::internal(
                "capsule Git installation changed descriptor cardinality",
            ));
        }

        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = StoreLayout::new(store.clone(), "capsule-snapshot".to_owned());
        let mut packs = Vec::with_capacity(installed.len());
        let mut inline_locators = std::collections::HashMap::new();
        for (pack_path, (descriptor, locator_bytes)) in installed.into_iter().zip(artifacts) {
            if cancellation.is_cancelled() {
                return Err(ReadError::Cancelled);
            }
            let pack = Bytes::from(tokio::fs::read(&pack_path).await?);
            let index = Bytes::from(tokio::fs::read(pack_path.with_extension("idx")).await?);
            let reverse = Bytes::from(tokio::fs::read(pack_path.with_extension("rev")).await?);
            let pack_id = blake3::hash(&pack).to_hex().to_string();
            let locations = crab_git::pack_locator::PackLocationIter::open(
                &pack_path.with_extension("idx"),
                &pack_path.with_extension("rev"),
                pack.len() as u64,
            )
            .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
            let locator_entries =
                crab_git::pack_locator::decode_pack_kind_metadata_with_external_deltas(
                    &locator_bytes,
                    locations,
                )
                .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
            let locator_locations = crab_git::pack_locator::PackLocationIter::open(
                &pack_path.with_extension("idx"),
                &pack_path.with_extension("rev"),
                pack.len() as u64,
            )
            .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
            let locator_pack_id = crab_xet::hash::MerkleHash::from_hex(&pack_id)
                .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
            for (ordinal, ((oid, kind, delta_base_oid), location)) in locator_entries
                .into_iter()
                .zip(locator_locations)
                .enumerate()
            {
                let location = location
                    .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
                let oid_bytes = oid
                    .as_bytes()
                    .try_into()
                    .map_err(|_| ReadError::internal("capsule Git locator is not SHA-1"))?;
                let ordinal = u32::try_from(ordinal)
                    .map_err(|_| ReadError::internal("capsule Git locator ordinal overflowed"))?;
                let kind = match kind {
                    gix_object::Kind::Commit => {
                        crab_metadata::git_object_locator::GitObjectKind::Commit
                    }
                    gix_object::Kind::Tree => {
                        crab_metadata::git_object_locator::GitObjectKind::Tree
                    }
                    gix_object::Kind::Blob => {
                        crab_metadata::git_object_locator::GitObjectKind::Blob
                    }
                    gix_object::Kind::Tag => crab_metadata::git_object_locator::GitObjectKind::Tag,
                };
                inline_locators.insert(
                    oid_bytes,
                    crab_metadata::git_object_locator::GitObjectLocator {
                        ordinal,
                        pack_id: locator_pack_id,
                        location: crab_metadata::git_object_locator::GitObjectLocation {
                            pack_offset: location.pack_offset,
                            entry_len: location.entry_len,
                            crc32: location.crc32,
                        },
                        metadata: crab_metadata::git_object_locator::GitObjectMetadata {
                            kind: Some(kind),
                            logical_size: None,
                            delta_base_oid: delta_base_oid
                                .map(|oid| oid.as_bytes().try_into())
                                .transpose()
                                .map_err(|_| {
                                    ReadError::internal("capsule Git delta base is not SHA-1")
                                })?,
                        },
                    },
                );
            }
            let pack_object = layout.pack_path(&pack_id);
            let index_object = layout.pack_index_path(&pack_id);
            let reverse_object = layout.pack_reverse_index_path(&pack_id);
            tokio::try_join!(
                store.put(&pack_object, pack.clone()),
                store.put(&index_object, index),
                store.put(&reverse_object, reverse),
            )?;
            packs.push(crab_metadata::manifests::PackManifestEntry {
                pack_id: pack_id.clone(),
                size: pack.len() as u64,
                content_hash: pack_id,
                ref_tips: Vec::new(),
                object_count: descriptor.object_count(),
            });
        }

        let manifest = self.git_manifest(&packs)?;
        let root = self.root();
        let snapshot = crab_metadata::manifest_store::RepositorySnapshot {
            layout: crab_metadata::layout_descriptor::LayoutDescriptor::canonical(),
            manifest: manifest.clone(),
            manifest_etag: root.digest().to_owned(),
            journal: crab_metadata::ref_journal::RefJournalSnapshot {
                refs: manifest.refs.clone(),
                peeled_refs: manifest.peeled_refs.clone(),
                head: manifest.head.clone(),
                packs,
                shards: Vec::new(),
                transactions: Vec::new(),
                ordered_edits: Vec::new(),
                visible_heads: std::collections::BTreeMap::new(),
                state_digest: root.digest().to_owned(),
            },
        };
        crab_remote_git::RemoteGitRepository::from_snapshot_with_inline_locators(
            layout,
            &snapshot,
            identity,
            runtime,
            options,
            inline_locators,
            cancellation,
        )
        .await
        .map_err(Into::into)
    }

    /// Open a Git repository using checkpoint ranges and inline capsule packs.
    ///
    /// The control suffix is fetched before this method is called. Checkpoint
    /// pack bodies are read by the remote Git reader only for requested object
    /// ranges; capsule packs remain inline because they are already bounded
    /// frontier payloads.
    pub async fn git_repository_from_store(
        &self,
        layout: crab_storage::StoreLayout<crab_storage::Store>,
        identity: crab_remote_git::RepositoryIdentity,
        runtime: Arc<crab_remote_git::RemoteGitRuntime>,
        options: crab_remote_git::RepositoryOptions,
        max_input_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<crab_remote_git::RemoteGitRepository> {
        if let Some(checkpoint) = self.layered_checkpoint().cloned() {
            return self
                .git_repository_from_layered_store(
                    layout,
                    checkpoint,
                    identity,
                    runtime,
                    options,
                    max_input_bytes,
                    cancellation,
                )
                .await;
        }
        let Some(control) = self.checkpoint_control().cloned() else {
            return self
                .git_repository(identity, runtime, options, max_input_bytes, cancellation)
                .await;
        };
        if cancellation.is_cancelled() {
            return Err(ReadError::Cancelled);
        }
        let workspace = tempfile::tempdir()?;
        let checkpoint_path = layout.capsule_checkpoint_path(control.hash());
        let mut packs = Vec::new();
        let mut pack_sources = std::collections::HashMap::new();
        let mut inline_locators = std::collections::HashMap::new();
        for descriptor in control.git_packs() {
            let pack_location = control.section_location(descriptor.pack_section())?;
            let pack_id = crab_xet::hash::MerkleHash::from_hex(pack_location.blake3())
                .map_err(|error| corrupt_path("capsule Git pack", error.to_string()))?;
            let index = control.section_bytes(descriptor.index_section())?;
            let reverse = control.section_bytes(descriptor.reverse_index_section())?;
            let locator = control.section_bytes(descriptor.locator_section())?;
            let index_path =
                workspace
                    .path()
                    .join(format!("{}-{}.idx", pack_id, descriptor.pack_section()));
            let reverse_path =
                workspace
                    .path()
                    .join(format!("{}-{}.rev", pack_id, descriptor.pack_section()));
            std::fs::write(&index_path, &index)?;
            std::fs::write(&reverse_path, &reverse)?;
            let locations = crab_git::pack_locator::PackLocationIter::open(
                &index_path,
                &reverse_path,
                pack_location.length(),
            )
            .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
            if locations.object_count() != descriptor.object_count()
                || locations.pack_checksum().to_string() != descriptor.git_checksum()
            {
                return Err(corrupt_path(
                    "capsule Git locator",
                    "checkpoint pack descriptor does not match its authenticated index",
                ));
            }
            crab_git::pack_locator::validate_pack_kind_metadata(
                &locator,
                locations.pack_checksum(),
                locations.object_count(),
            )
            .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
            inline_locators.extend(inline_locators_for_pack(
                descriptor.object_count(),
                descriptor.git_checksum(),
                pack_id,
                &index_path,
                &reverse_path,
                &locator,
                pack_location.length(),
            )?);
            pack_sources.insert(
                pack_id,
                crab_remote_git::RemoteGitPackSource::embedded(
                    checkpoint_path.clone(),
                    pack_location.offset(),
                    pack_location.length(),
                    index,
                    reverse,
                    Some(locator),
                )?,
            );
            packs.push(crab_metadata::manifests::PackManifestEntry {
                pack_id: pack_id.to_string(),
                size: pack_location.length(),
                content_hash: pack_id.to_string(),
                ref_tips: Vec::new(),
                object_count: descriptor.object_count(),
            });
        }
        for capsule in &self.capsules {
            for descriptor in capsule.git_packs() {
                let pack = capsule.section_bytes(descriptor.pack_section())?;
                let pack_size = pack.len() as u64;
                let index = capsule.section_bytes(descriptor.index_section())?;
                let reverse = capsule.section_bytes(descriptor.reverse_index_section())?;
                let locator = capsule.section_bytes(descriptor.locator_section())?;
                let pack_id =
                    crab_xet::hash::MerkleHash::from_hex(blake3::hash(&pack).to_hex().as_ref())
                        .map_err(|error| corrupt_path("capsule Git pack", error.to_string()))?;
                let index_path =
                    workspace
                        .path()
                        .join(format!("{}-{}.idx", pack_id, descriptor.pack_section()));
                let reverse_path =
                    workspace
                        .path()
                        .join(format!("{}-{}.rev", pack_id, descriptor.pack_section()));
                std::fs::write(&index_path, &index)?;
                std::fs::write(&reverse_path, &reverse)?;
                let locations = crab_git::pack_locator::PackLocationIter::open(
                    &index_path,
                    &reverse_path,
                    pack.len() as u64,
                )
                .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
                if locations.object_count() != descriptor.object_count()
                    || locations.pack_checksum().to_string() != descriptor.git_checksum()
                {
                    return Err(corrupt_path(
                        "capsule Git locator",
                        "capsule pack descriptor does not match its authenticated index",
                    ));
                }
                crab_git::pack_locator::validate_pack_kind_metadata(
                    &locator,
                    locations.pack_checksum(),
                    locations.object_count(),
                )
                .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
                inline_locators.extend(inline_locators_for_pack(
                    descriptor.object_count(),
                    descriptor.git_checksum(),
                    pack_id,
                    &index_path,
                    &reverse_path,
                    &locator,
                    pack_size,
                )?);
                pack_sources.insert(
                    pack_id,
                    crab_remote_git::RemoteGitPackSource::inline(
                        pack,
                        index,
                        reverse,
                        Some(locator),
                    )?,
                );
                packs.push(crab_metadata::manifests::PackManifestEntry {
                    pack_id: pack_id.to_string(),
                    size: pack_size,
                    content_hash: pack_id.to_string(),
                    ref_tips: Vec::new(),
                    object_count: descriptor.object_count(),
                });
            }
        }
        let manifest = self.git_manifest(&packs)?;
        let root = self.root();
        let snapshot = crab_metadata::manifest_store::RepositorySnapshot {
            layout: crab_metadata::layout_descriptor::LayoutDescriptor::canonical(),
            manifest: manifest.clone(),
            manifest_etag: root.digest().to_owned(),
            journal: crab_metadata::ref_journal::RefJournalSnapshot {
                refs: manifest.refs.clone(),
                peeled_refs: manifest.peeled_refs.clone(),
                head: manifest.head.clone(),
                packs,
                shards: Vec::new(),
                transactions: Vec::new(),
                ordered_edits: Vec::new(),
                visible_heads: std::collections::BTreeMap::new(),
                state_digest: root.digest().to_owned(),
            },
        };
        crab_remote_git::RemoteGitRepository::from_snapshot_with_inline_locators_and_pack_sources(
            layout,
            &snapshot,
            identity,
            runtime,
            options,
            inline_locators,
            pack_sources,
            cancellation,
        )
        .await
        .map_err(Into::into)
    }

    async fn git_repository_from_layered_store(
        &self,
        layout: crab_storage::StoreLayout<crab_storage::Store>,
        checkpoint: LayeredCheckpoint,
        identity: crab_remote_git::RepositoryIdentity,
        runtime: Arc<crab_remote_git::RemoteGitRuntime>,
        options: crab_remote_git::RepositoryOptions,
        max_input_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<crab_remote_git::RemoteGitRepository> {
        if cancellation.is_cancelled() {
            return Err(ReadError::Cancelled);
        }
        let workspace = tempfile::tempdir()?;
        let mut packs = Vec::new();
        let mut pack_sources = std::collections::HashMap::new();
        let mut inline_locators = std::collections::HashMap::new();
        let mut preferred_pack_indexes = Vec::new();
        let mut seen_packs = BTreeMap::new();
        let mut all_members = Vec::new();
        let mut sources = checkpoint.sources().to_vec();
        let mut source_hashes = sources
            .iter()
            .map(|source| source.object_hash().to_owned())
            .collect::<BTreeSet<_>>();
        for source in &self.capsule_run_sources {
            if source_hashes.insert(source.object_hash().to_owned()) {
                sources.push(source.clone());
            }
        }
        // Ordinary fetches deliberately keep the large visibility body cold.
        // The footer still authenticates source descriptors and frontier
        // admission, while the complete ordinal join is only available to
        // maintenance/strict readers that loaded the full checkpoint.
        let preferred_object_admission = if checkpoint.is_control_only() {
            None
        } else {
            checkpoint
                .visibility_ordinal_snapshot()?
                .and_then(|snapshot| {
                    snapshot.member_admission().map(|admission| {
                        snapshot
                            .objects()
                            .iter()
                            .zip(admission)
                            .map(|(oid, member)| (*oid, *member))
                            .collect::<Vec<_>>()
                    })
                })
        };
        let mut preferred_object_admission_by_oid: std::collections::HashMap<
            [u8; 20],
            Vec<crab_xet::hash::MerkleHash>,
        > = std::collections::HashMap::new();
        if let Some(admission) = preferred_object_admission.as_ref() {
            for (oid, member) in admission {
                let source = sources
                    .get(usize::from(member.source_index()))
                    .ok_or_else(|| {
                        corrupt_path("layered visibility", "source admission is out of bounds")
                    })?;
                let pack = source
                    .members()
                    .get(usize::from(member.member_index()))
                    .ok_or_else(|| {
                        corrupt_path("layered visibility", "member admission is out of bounds")
                    })?;
                let pack_id = crab_xet::hash::MerkleHash::from_hex(pack.pack().blake3())
                    .map_err(|error| corrupt_path("layered visibility", error.to_string()))?;
                preferred_object_admission_by_oid
                    .entry(*oid)
                    .or_default()
                    .push(pack_id);
            }
        }
        for (oid, pack_ids) in self.frontier_object_admission() {
            let entry = preferred_object_admission_by_oid.entry(*oid).or_default();
            for pack_id in pack_ids {
                let pack_id = crab_xet::hash::MerkleHash::from_hex(pack_id)
                    .map_err(|error| corrupt_path("frontier admission", error.to_string()))?;
                if !entry.contains(&pack_id) {
                    entry.push(pack_id);
                }
            }
        }
        let admitted_pack_ids = preferred_object_admission_by_oid
            .values()
            .flat_map(|pack_ids| pack_ids.iter().map(ToString::to_string))
            .collect::<BTreeSet<_>>();
        let frontier_pack_ids = self
            .capsule_run_sources
            .iter()
            .flat_map(|source| source.members())
            .map(|member| member.pack().blake3().to_owned())
            .collect::<BTreeSet<_>>();
        for source in &sources {
            let source_path = match source.kind() {
                PackSourceKind::CapsuleRun => layout.capsule_path(source.object_hash()),
                PackSourceKind::PackLayer => layout.capsule_pack_layer_path(source.object_hash()),
            };
            for member in source.members() {
                if cancellation.is_cancelled() {
                    return Err(ReadError::Cancelled);
                }
                let pack_id = crab_xet::hash::MerkleHash::from_hex(member.pack().blake3())
                    .map_err(|error| corrupt_path("capsule Git pack", error.to_string()))?;
                let pack_key = pack_id.to_string();
                if let Some(previous) = seen_packs.insert(pack_key.clone(), member.clone()) {
                    let equivalent = previous.index().blake3() == member.index().blake3()
                        && previous.reverse_index().blake3() == member.reverse_index().blake3()
                        && previous.locator().blake3() == member.locator().blake3()
                        && previous.git_checksum() == member.git_checksum()
                        && previous.object_count() == member.object_count()
                        && previous.external_delta_bases() == member.external_delta_bases();
                    if !equivalent {
                        return Err(corrupt_path(
                            "capsule Git pack",
                            "layered sources disagree about one pack identity",
                        ));
                    }
                    continue;
                }
                let member_read = LayeredMemberRead {
                    source_path: source_path.clone(),
                    source_size: source.object_size(),
                    pack_id,
                    member: member.clone(),
                };
                all_members.push(member_read);
            }
        }
        if !checkpoint.is_control_only() && !all_members.is_empty() {
            // A complete layered checkpoint authenticates a kind-bearing
            // locator range for every member. Load those compact sidecars as
            // one coalesced window per source so filtered planning can use
            // OID/kind locators instead of issuing one object read per OID.
            let payload_members = all_members
                .iter()
                .cloned()
                .map(|member| LayeredPayloadMemberRead {
                    member,
                    complete_local: false,
                })
                .collect::<Vec<_>>();
            let selected_members = (0..payload_members.len()).collect::<BTreeSet<_>>();
            let windows = plan_layered_payload_windows_for_members(
                &payload_members,
                &selected_members,
                LayeredPayloadWindowMode::Sidecars,
            )?;
            let window_bytes = windows.iter().try_fold(0_u64, |total, window| {
                total
                    .checked_add(window.range.end.saturating_sub(window.range.start))
                    .ok_or_else(|| corrupt_path("capsule Git locator", "sidecar bytes overflowed"))
            })?;
            if max_input_bytes > 0 && window_bytes > max_input_bytes {
                return Err(ReadError::CapsuleReadLimit {
                    resource: "layered Git locator sidecars",
                    maximum: max_input_bytes,
                });
            }
            let (fetched_windows, _) =
                fetch_layered_payload_windows(layout.store(), &windows).await?;
            for (member_index, member_read) in all_members.iter().enumerate() {
                let window_index = payload_window_for_member(&windows, member_index)?;
                let body = fetched_windows
                    .get(&window_index)
                    .ok_or_else(|| corrupt_path("capsule Git locator", "sidecar window missing"))?;
                let window = windows
                    .get(window_index)
                    .ok_or_else(|| corrupt_path("capsule Git locator", "sidecar window missing"))?;
                let index =
                    layered_range_bytes(body, window.range.start, member_read.member.index())?;
                let reverse_index = layered_range_bytes(
                    body,
                    window.range.start,
                    member_read.member.reverse_index(),
                )?;
                let locator =
                    layered_range_bytes(body, window.range.start, member_read.member.locator())?;
                let pack_id = member_read.pack_id;
                let index_path = workspace.path().join(format!("{pack_id}-layered.idx"));
                let reverse_path = workspace.path().join(format!("{pack_id}-layered.rev"));
                std::fs::write(&index_path, &index)?;
                std::fs::write(&reverse_path, &reverse_index)?;
                inline_locators.extend(inline_locators_for_pack(
                    member_read.member.object_count(),
                    member_read.member.git_checksum(),
                    pack_id,
                    &index_path,
                    &reverse_path,
                    &locator,
                    member_read.member.pack().length(),
                )?);
            }
        }
        // Keep every layered member source lazy. Incremental fetches first
        // probe the frontier pack indexes; reverse indexes and kind metadata
        // are fetched only by explicit pack installation or repack callers.
        // This removes the eager full-sidecar wave while retaining the
        // descriptor hashes and exact pack-index validation at first use.
        for member_read in all_members {
            if cancellation.is_cancelled() {
                return Err(ReadError::Cancelled);
            }
            let pack_id = member_read.pack_id;
            let member = &member_read.member;
            let pack_key = pack_id.to_string();
            let source = crab_remote_git::RemoteGitPackSource::embedded_lazy_index(
                member_read.source_path.clone(),
                member.pack().offset(),
                member.pack().length(),
                member_read.source_size,
                crab_remote_git::RemoteGitSidecarRange {
                    offset: member.index().offset(),
                    length: member.index().length(),
                    blake3: member.index().blake3().to_owned(),
                },
                crab_remote_git::RemoteGitSidecarRange {
                    offset: member.reverse_index().offset(),
                    length: member.reverse_index().length(),
                    blake3: member.reverse_index().blake3().to_owned(),
                },
            )?;
            let preferred_pack_ids = if admitted_pack_ids.is_empty() {
                &frontier_pack_ids
            } else {
                &admitted_pack_ids
            };
            if preferred_pack_ids.contains(&pack_key) {
                preferred_pack_indexes.push(
                    crab_metadata::git_object_locator::GitPackInventoryEntry {
                        pack_id,
                        object_count: member.object_count(),
                        pack_size: member.pack().length(),
                    },
                );
            }
            pack_sources.insert(pack_id, source);
            packs.push(crab_metadata::manifests::PackManifestEntry {
                pack_id: pack_key,
                size: member.pack().length(),
                content_hash: pack_id.to_string(),
                ref_tips: Vec::new(),
                object_count: member.object_count(),
            });
        }
        for capsule in &self.capsules {
            for descriptor in capsule.git_packs() {
                let pack = capsule.section_bytes(descriptor.pack_section())?;
                let pack_size = pack.len() as u64;
                let pack_id =
                    crab_xet::hash::MerkleHash::from_hex(blake3::hash(&pack).to_hex().as_ref())
                        .map_err(|error| corrupt_path("capsule Git pack", error.to_string()))?;
                if pack_sources.contains_key(&pack_id) {
                    continue;
                }
                let index = capsule.section_bytes(descriptor.index_section())?;
                let reverse = capsule.section_bytes(descriptor.reverse_index_section())?;
                let locator = capsule.section_bytes(descriptor.locator_section())?;
                let index_path =
                    workspace
                        .path()
                        .join(format!("{}-{}.idx", pack_id, descriptor.pack_section()));
                let reverse_path =
                    workspace
                        .path()
                        .join(format!("{}-{}.rev", pack_id, descriptor.pack_section()));
                std::fs::write(&index_path, &index)?;
                std::fs::write(&reverse_path, &reverse)?;
                let locations = crab_git::pack_locator::PackLocationIter::open(
                    &index_path,
                    &reverse_path,
                    pack_size,
                )
                .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
                if locations.object_count() != descriptor.object_count()
                    || locations.pack_checksum().to_string() != descriptor.git_checksum()
                {
                    return Err(corrupt_path(
                        "capsule Git locator",
                        "capsule pack descriptor does not match its authenticated index",
                    ));
                }
                crab_git::pack_locator::validate_pack_kind_metadata(
                    &locator,
                    locations.pack_checksum(),
                    locations.object_count(),
                )
                .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
                inline_locators.extend(inline_locators_for_pack(
                    descriptor.object_count(),
                    descriptor.git_checksum(),
                    pack_id,
                    &index_path,
                    &reverse_path,
                    &locator,
                    pack.len() as u64,
                )?);
                pack_sources.insert(
                    pack_id,
                    crab_remote_git::RemoteGitPackSource::inline(
                        pack,
                        index,
                        reverse,
                        Some(locator),
                    )?,
                );
                packs.push(crab_metadata::manifests::PackManifestEntry {
                    pack_id: pack_id.to_string(),
                    size: pack_size,
                    content_hash: pack_id.to_string(),
                    ref_tips: Vec::new(),
                    object_count: descriptor.object_count(),
                });
            }
        }
        let manifest = self.git_manifest(&packs)?;
        let root = self.root();
        let snapshot = crab_metadata::manifest_store::RepositorySnapshot {
            layout: crab_metadata::layout_descriptor::LayoutDescriptor::canonical(),
            manifest: manifest.clone(),
            manifest_etag: root.digest().to_owned(),
            journal: crab_metadata::ref_journal::RefJournalSnapshot {
                refs: manifest.refs.clone(),
                peeled_refs: manifest.peeled_refs.clone(),
                head: manifest.head.clone(),
                packs,
                shards: Vec::new(),
                transactions: Vec::new(),
                ordered_edits: Vec::new(),
                visible_heads: std::collections::BTreeMap::new(),
                state_digest: root.digest().to_owned(),
            },
        };
        crab_remote_git::RemoteGitRepository::from_snapshot_with_inline_locators_and_pack_sources_and_preferred_pack_indexes_and_admission(
            layout,
            &snapshot,
            identity,
            runtime,
            options,
            inline_locators,
            pack_sources,
            preferred_pack_indexes,
            (!preferred_object_admission_by_oid.is_empty())
                .then_some(preferred_object_admission_by_oid),
            cancellation,
        )
        .await
        .map_err(Into::into)
    }

    /// Materialize the complete view-bound Git visibility proof.
    pub fn git_visibility_index(
        &self,
    ) -> Result<crab_metadata::git_visibility::GitVisibilityIndex> {
        let mut index = match self.checkpoint.as_ref() {
            Some(CheckpointData::Layered(checkpoint)) => {
                if let Some(snapshot) = checkpoint.visibility_ordinal_snapshot()? {
                    let catalog_digest = crab_metadata::capsule_protocol::source_catalog_digest(
                        checkpoint.sources(),
                    )?;
                    snapshot.to_index(0, &"0".repeat(64), &"0".repeat(64), &catalog_digest)?
                } else {
                    checkpoint
                        .visibility_snapshot()?
                        .map(|snapshot| snapshot.to_index(0, &"0".repeat(64), &"0".repeat(64)))
                        .transpose()?
                        .unwrap_or(crab_metadata::git_visibility::GitVisibilityIndex::new(
                            0,
                            "",
                            "0".repeat(64),
                            BTreeMap::new(),
                        )?)
                }
            }
            Some(CheckpointData::Complete(checkpoint)) => checkpoint
                .visibility_snapshot()?
                .map(|snapshot| snapshot.to_index(0, &"0".repeat(64), &"0".repeat(64)))
                .transpose()?
                .unwrap_or(crab_metadata::git_visibility::GitVisibilityIndex::new(
                    0,
                    "",
                    "0".repeat(64),
                    BTreeMap::new(),
                )?),
            Some(CheckpointData::Control(control)) => control
                .visibility_snapshot()?
                .map(|snapshot| snapshot.to_index(0, &"0".repeat(64), &"0".repeat(64)))
                .transpose()?
                .unwrap_or(crab_metadata::git_visibility::GitVisibilityIndex::new(
                    0,
                    "",
                    "0".repeat(64),
                    BTreeMap::new(),
                )?),
            None => crab_metadata::git_visibility::GitVisibilityIndex::new(
                0,
                "",
                "0".repeat(64),
                BTreeMap::new(),
            )?,
        };

        for capsule in &self.capsules {
            apply_capsule_visibility_index(capsule, &mut index)?;
        }
        for capsule in &self.capsule_controls {
            apply_capsule_control_visibility_index(capsule, &mut index)?;
        }

        let packs = self.git_pack_manifest_entries()?;
        let manifest = self.git_manifest(&packs)?;
        let refs = index.ref_closures();
        if refs.keys().ne(manifest.refs.keys())
            || manifest.refs.iter().any(|(name, tip)| {
                refs.get(name)
                    .is_none_or(|objects| objects.binary_search(tip).is_err())
            })
            || manifest.peeled_refs.iter().any(|(name, peeled)| {
                refs.get(name)
                    .is_none_or(|objects| objects.binary_search(peeled).is_err())
            })
        {
            return Err(corrupt_path(
                "capsule Git visibility",
                "materialized visibility does not cover the pinned root",
            ));
        }
        index.bind_identity(
            manifest.generation,
            &manifest.pack_index_hash,
            &manifest.git_validation_digest,
        )?;
        Ok(index)
    }

    /// Materialize visibility after one candidate capsule without publishing it.
    pub fn candidate_git_visibility(
        &self,
        candidate: &Capsule,
    ) -> Result<BTreeMap<String, Vec<String>>> {
        let mut refs = self.git_visibility_index()?.ref_closures();
        if !capsule_is_ready(candidate, &self.refs, &refs)? {
            return Err(corrupt_path(
                "candidate capsule Git visibility",
                "candidate does not extend the pinned repository view",
            ));
        }
        apply_capsule_visibility(candidate, &mut refs)?;
        Ok(refs)
    }

    fn git_pack_manifest_entries(
        &self,
    ) -> Result<Vec<crab_metadata::manifests::PackManifestEntry>> {
        let mut entries = Vec::new();
        match self.checkpoint.as_ref() {
            Some(CheckpointData::Complete(checkpoint)) => {
                for descriptor in checkpoint.git_packs() {
                    let pack = checkpoint.section_bytes(descriptor.pack_section())?;
                    let pack_id = blake3::hash(&pack).to_hex().to_string();
                    entries.push(crab_metadata::manifests::PackManifestEntry {
                        pack_id: pack_id.clone(),
                        size: pack.len() as u64,
                        content_hash: pack_id,
                        ref_tips: Vec::new(),
                        object_count: descriptor.object_count(),
                    });
                }
            }
            Some(CheckpointData::Control(control)) => {
                for descriptor in control.git_packs() {
                    let location = control.section_location(descriptor.pack_section())?;
                    entries.push(crab_metadata::manifests::PackManifestEntry {
                        pack_id: location.blake3().to_owned(),
                        size: location.length(),
                        content_hash: location.blake3().to_owned(),
                        ref_tips: Vec::new(),
                        object_count: descriptor.object_count(),
                    });
                }
            }
            Some(CheckpointData::Layered(checkpoint)) => {
                for source in checkpoint.sources() {
                    for member in source.members() {
                        let pack_id = member.pack().blake3().to_owned();
                        entries.push(crab_metadata::manifests::PackManifestEntry {
                            pack_id: pack_id.clone(),
                            size: member.pack().length(),
                            content_hash: pack_id,
                            ref_tips: Vec::new(),
                            object_count: member.object_count(),
                        });
                    }
                }
            }
            None => {}
        }
        if self.layered_checkpoint().is_none() {
            for capsule in &self.capsules {
                for descriptor in capsule.git_packs() {
                    let pack = capsule.section_bytes(descriptor.pack_section())?;
                    let pack_id = blake3::hash(&pack).to_hex().to_string();
                    entries.push(crab_metadata::manifests::PackManifestEntry {
                        pack_id: pack_id.clone(),
                        size: pack.len() as u64,
                        content_hash: pack_id,
                        ref_tips: Vec::new(),
                        object_count: descriptor.object_count(),
                    });
                }
            }
        }
        Ok(entries)
    }

    fn checkpoint_pack_count(&self) -> usize {
        match self.checkpoint.as_ref() {
            Some(CheckpointData::Complete(checkpoint)) => checkpoint.git_packs().len(),
            Some(CheckpointData::Control(control)) => control.git_packs().len(),
            Some(CheckpointData::Layered(checkpoint)) => checkpoint
                .sources()
                .iter()
                .map(|source| source.members().len())
                .sum(),
            None => 0,
        }
    }

    fn git_manifest(
        &self,
        packs: &[crab_metadata::manifests::PackManifestEntry],
    ) -> Result<crab_metadata::manifests::Manifest> {
        let root = self.root().root();
        let mut manifest = crab_metadata::manifests::Manifest::default_for_repo(self.head());
        manifest.generation = root.generation();
        manifest.refs = self.refs.clone();
        manifest.peeled_refs = self.peeled_refs.clone();
        if !packs.is_empty() {
            manifest.pack_index_hash =
                crab_metadata::manifests::compact_pack_index(manifest.generation, packs)?.0;
        }
        manifest.seal_git_validation();
        Ok(manifest)
    }
}

fn append_tip_bound_transitions(
    output: &mut CapsuleTipBoundTransitions,
    transaction: &crab_metadata::capsule_protocol::CapsuleTransaction,
    delta: &crab_metadata::capsule_protocol::CapsuleVisibilityDelta,
) -> Result<()> {
    for (ref_name, edit) in delta.edits() {
        let transaction_edit = transaction
            .edits()
            .iter()
            .find(|candidate| candidate.ref_name() == ref_name)
            .ok_or_else(|| {
                corrupt_path(
                    "capsule Git visibility",
                    "visibility transition has no matching ref edit",
                )
            })?;
        if transaction_edit.expected_old() != edit.old_oid.as_deref()
            || transaction_edit.new_oid() != Some(edit.new_oid.as_str())
        {
            return Err(corrupt_path(
                "capsule Git visibility",
                "visibility transition does not match its ref edit",
            ));
        }
        let parse_oid = |value: &str| {
            ObjectId::from_hex(value.as_bytes()).map_err(|_| {
                corrupt_path(
                    "capsule Git visibility",
                    "visibility transition contains an invalid object ID",
                )
            })
        };
        let old_oid = edit.old_oid.as_deref().map(parse_oid).transpose()?;
        let new_oid = parse_oid(&edit.new_oid)?;
        let added = edit
            .added
            .iter()
            .map(|oid| parse_oid(oid))
            .collect::<Result<Vec<_>>>()?;
        let removed = edit
            .removed
            .iter()
            .map(|oid| parse_oid(oid))
            .collect::<Result<Vec<_>>>()?;
        output
            .entry(ref_name.clone())
            .or_default()
            .push(CapsuleVisibilityTransition {
                old_oid,
                new_oid,
                added,
                removed,
            });
    }
    Ok(())
}

fn append_checkpoint_tip_bound_transitions(
    output: &mut CapsuleTipBoundTransitions,
    history: &BTreeMap<
        String,
        Vec<crab_metadata::git_visibility::GitVisibilityCheckpointTransition>,
    >,
) -> Result<()> {
    for (ref_name, transitions) in history {
        let output_transitions = output.entry(ref_name.clone()).or_default();
        for transition in transitions {
            let parse_oid = |value: &str| {
                ObjectId::from_hex(value.as_bytes()).map_err(|_| {
                    corrupt_path(
                        "capsule Git visibility",
                        "checkpoint visibility transition contains an invalid object ID",
                    )
                })
            };
            let added = transition
                .objects
                .iter()
                .map(|oid| parse_oid(oid))
                .collect::<Result<Vec<_>>>()?;
            output_transitions.push(CapsuleVisibilityTransition {
                old_oid: Some(parse_oid(&transition.from_oid)?),
                new_oid: parse_oid(&transition.to_oid)?,
                added,
                removed: Vec::new(),
            });
        }
    }
    Ok(())
}

fn build_tip_bound_transitions(
    capsules: &[Capsule],
    controls: &[CapsuleControl],
) -> Result<CapsuleTipBoundTransitions> {
    let mut transitions = BTreeMap::new();
    let mut seen_transactions = BTreeSet::new();
    for capsule in capsules {
        let transaction_id = capsule.transaction_id().to_owned();
        if !seen_transactions.insert(transaction_id) {
            continue;
        }
        let transaction = capsule.transaction()?;
        if let Some(delta) = capsule.visibility_delta()? {
            append_tip_bound_transitions(&mut transitions, &transaction, &delta)?;
        }
    }
    for capsule in controls {
        if !seen_transactions.insert(capsule.transaction_id().to_owned()) {
            continue;
        }
        if let Some(delta) = capsule.visibility_delta() {
            append_tip_bound_transitions(&mut transitions, capsule.transaction(), delta)?;
        }
    }
    Ok(transitions)
}

/// Install every capsule Git pack into a local Git object database.
///
/// Pack bodies, indexes, reverse indexes, and locator metadata are validated
/// as one descriptor before any new pack becomes visible in the destination.
pub async fn install_git_packs(
    view: &CapsuleRepositoryView,
    git_dir: &Path,
    max_input_bytes: u64,
) -> Result<Vec<PathBuf>> {
    install_git_packs_with_candidates(view, &[], git_dir, max_input_bytes).await
}

/// Verify every Git and external content dependency reachable from one view.
///
/// This is a deep administrative proof, not a foreground read-path check. It
/// validates the complete shard/xorb catalog, installs and validates all Git
/// packs in an isolated object database, proves every reachable Crab pointer
/// is represented by that catalog, and hashes every reachable LFS object from
/// the origin store. Cancellation never turns a partial scan into success.
pub async fn verify_reachable_dependencies(
    layout: &StoreLayout<Store>,
    view: &CapsuleRepositoryView,
    limits: CapsuleDependencyLimits,
    cancellation: &CancellationToken,
) -> Result<CapsuleDependencyProof> {
    let catalog = view.pointer_catalog()?;
    tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(ReadError::Cancelled),
        result = crate::verify_capsule_pointer_catalog_objects(layout, &catalog) => {
            result?;
        }
    }

    let workspace = tempfile::tempdir()?;
    tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(ReadError::Cancelled),
        result = install_git_packs_from_store(
            view,
            layout.store(),
            layout,
            workspace.path(),
            limits.max_git_bytes,
        ) => {
            result?;
        }
    }
    let refs = view
        .refs()
        .iter()
        .map(|(name, oid)| (name.clone(), oid.clone()))
        .collect::<Vec<_>>();
    let git_dir = workspace.path().to_owned();
    let scan_cancel = cancellation.child_token();
    let worker_cancel = scan_cancel.clone();
    let scan = tokio::task::spawn_blocking(move || -> Result<_> {
        let scan = crab_git::walk::scan_pointers(&git_dir, &refs, limits.pointer_scan, &|| {
            worker_cancel.is_cancelled()
        })?;
        crab_git::batch::verify_git_dir_blobs(&git_dir, &scan.unchecked_blobs, &|| {
            worker_cancel.is_cancelled()
        })?;
        Ok(scan)
    });
    tokio::pin!(scan);
    let scan = tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            scan_cancel.cancel();
            // The worker borrows the temporary Git database by path. Drain it
            // before that database and any caller-owned admission are released.
            let _ = scan.await;
            return Err(ReadError::Cancelled);
        }
        result = &mut scan => result
            .map_err(|error| ReadError::Internal(format!("Git dependency scan failed: {error}")))??,
    };

    for pointer in &scan.pointers {
        let hash = crab_types::pointer::hex_encode(&pointer.file_hash);
        let Some(entry) = catalog.files().get(&hash) else {
            return Err(ReadError::CorruptObject {
                path: gix_hash::ObjectId::Sha1(pointer.oid).to_string(),
                reason: format!("reachable Crab pointer {hash} is absent from the catalog"),
            });
        };
        if entry.size() != pointer.size {
            return Err(ReadError::CorruptObject {
                path: gix_hash::ObjectId::Sha1(pointer.oid).to_string(),
                reason: format!(
                    "reachable Crab pointer {hash} declares size {}, catalog declares {}",
                    pointer.size,
                    entry.size()
                ),
            });
        }
    }

    let lfs = crab_lfs::LfsObjectStore::new(layout.store().clone(), layout.repo_prefix());
    let mut lfs_objects = BTreeMap::new();
    for blob in scan.lfs_pointers {
        if let Some(previous) = lfs_objects.insert(blob.pointer.oid, blob.pointer.size)
            && previous != blob.pointer.size
        {
            return Err(ReadError::CorruptObject {
                path: gix_hash::ObjectId::Sha1(blob.oid).to_string(),
                reason: "reachable LFS pointers declare conflicting sizes for one object"
                    .to_owned(),
            });
        }
    }
    for (oid, size) in &lfs_objects {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ReadError::Cancelled),
            result = lfs.verify_origin(oid, *size) => result?,
        }
    }

    Ok(CapsuleDependencyProof {
        catalog_files: catalog.files().len() as u64,
        catalog_shards: catalog.shards().len() as u64,
        catalog_xorbs: catalog.xorbs().len() as u64,
        reachable_crab_pointers: scan.pointers.len() as u64,
        reachable_lfs_objects: lfs_objects.len() as u64,
    })
}

/// Install the pinned view plus additional verified candidate capsules.
///
/// The caller must separately bind each candidate transaction to the pinned
/// root. Pack intake and the aggregate byte limit cover both base and candidate
/// data before anything becomes visible in the destination object database.
pub async fn install_git_packs_with_candidates(
    view: &CapsuleRepositoryView,
    candidates: &[Capsule],
    git_dir: &Path,
    max_input_bytes: u64,
) -> Result<Vec<PathBuf>> {
    if view.layered_checkpoint().is_some() {
        return Err(ReadError::internal(
            "layered checkpoint sources require object-store-backed installation",
        ));
    }
    if view.checkpoint_control().is_some() {
        return Err(ReadError::internal(
            "complete checkpoint bytes are required for this installation path",
        ));
    }
    let checkpoint = view.checkpoint().cloned();
    let capsules = view.capsules.clone();
    let candidates = candidates.to_vec();
    let mut payloads = Vec::new();
    if let Some(checkpoint) = checkpoint {
        payloads.extend(payloads_from_container(GitPackContainer::Checkpoint(
            checkpoint,
        ))?);
    }
    for capsule in capsules.into_iter().chain(candidates) {
        payloads.extend(payloads_from_container(GitPackContainer::Capsule(capsule))?);
    }
    install_git_pack_payloads(git_dir, payloads, max_input_bytes).await
}

#[derive(Debug)]
struct GitPackPayload {
    pack: Option<Bytes>,
    index: Bytes,
    reverse_index: Bytes,
    locator: Bytes,
    verified_identity: Option<crab_git::pack::VerifiedPackIdentity>,
    content_hash: String,
    git_checksum: String,
    object_count: u64,
}

fn payloads_from_container(container: GitPackContainer) -> Result<Vec<GitPackPayload>> {
    let descriptors = container.git_packs().to_vec();
    descriptors
        .into_iter()
        .map(|descriptor| {
            let pack = container.section_bytes(descriptor.pack_section())?;
            let index = container.section_bytes(descriptor.index_section())?;
            let reverse_index = container.section_bytes(descriptor.reverse_index_section())?;
            let locator = container.section_bytes(descriptor.locator_section())?;
            let content_hash = blake3::hash(&pack).to_hex().to_string();
            Ok(GitPackPayload {
                pack: Some(pack),
                index,
                reverse_index,
                locator,
                verified_identity: None,
                content_hash,
                git_checksum: descriptor.git_checksum().to_owned(),
                object_count: descriptor.object_count(),
            })
        })
        .collect()
}

async fn install_git_pack_payloads(
    git_dir: impl Into<PathBuf>,
    payloads: Vec<GitPackPayload>,
    max_input_bytes: u64,
) -> Result<Vec<PathBuf>> {
    let git_dir = git_dir.into();
    tokio::task::spawn_blocking(move || {
        let pack_dir = git_dir.join("objects").join("pack");
        std::fs::create_dir_all(&pack_dir)?;
        let mut total = 0_u64;
        let mut installed = Vec::new();
        for GitPackPayload {
            pack,
            index,
            reverse_index,
            locator,
            verified_identity,
            content_hash,
            git_checksum,
            object_count,
        } in payloads
        {
            let has_download = pack.is_some();
            let final_pack = pack_dir.join(format!("pack-{content_hash}.pack"));
            let final_index = pack_dir.join(format!("pack-{content_hash}.idx"));
            let final_reverse = pack_dir.join(format!("pack-{content_hash}.rev"));
            let pack_size = match pack.as_ref() {
                Some(pack) => pack.len() as u64,
                None => std::fs::metadata(&final_pack)?.len(),
            };
            if has_download {
                total = total.checked_add(pack_size).ok_or_else(|| {
                    ReadError::internal("capsule-protocol Git intake size overflowed")
                })?;
                if max_input_bytes > 0 && total > max_input_bytes {
                    return Err(ReadError::CapsuleReadLimit {
                        resource: "Git pack intake",
                        maximum: max_input_bytes,
                    });
                }
            }
            let (_temporary, pack_path, index_path, reverse_path) = if let Some(pack) = pack {
                let temporary = tempfile::Builder::new()
                    .prefix(".crab-capsule-pack-")
                    .tempdir_in(&pack_dir)?;
                let pack_path = temporary.path().join("pack.pack");
                let index_path = temporary.path().join("pack.idx");
                let reverse_path = temporary.path().join("pack.rev");
                std::fs::write(&pack_path, &pack)?;
                std::fs::write(&index_path, &index)?;
                std::fs::write(&reverse_path, &reverse_index)?;
                (
                    Some(temporary),
                    Some(pack_path),
                    Some(index_path),
                    Some(reverse_path),
                )
            } else {
                    if !final_pack.exists() || !final_index.exists() || !final_reverse.exists() {
                        return Err(corrupt_path(
                            "capsule Git pack",
                            "checkpoint pack disappeared before its range was installed",
                        ));
                    }
                    let mut file = std::fs::File::open(&final_pack)?;
                    let mut hasher = blake3::Hasher::new();
                    let mut buffer = [0_u8; 64 * 1024];
                    loop {
                        let read = std::io::Read::read(&mut file, &mut buffer)?;
                        if read == 0 {
                            break;
                        }
                        hasher.update(&buffer[..read]);
                    }
                    if hasher.finalize().to_hex().as_str() != content_hash {
                        return Err(corrupt_path(
                            "capsule Git pack",
                            "local checkpoint pack content hash does not match its authenticated section",
                        ));
                    }
                    if std::fs::read(&final_index)? != index.as_ref()
                        || std::fs::read(&final_reverse)? != reverse_index.as_ref()
                    {
                        return Err(corrupt_path(
                            "capsule Git pack",
                            "local checkpoint sidecar does not match its authenticated section",
                        ));
                    }
                    (None, None, None, None)
                };
            let index_for_validation = index_path.as_deref().unwrap_or(&final_index);
            let reverse_for_validation = reverse_path.as_deref().unwrap_or(&final_reverse);
            let locations = crab_git::pack_locator::PackLocationIter::open(
                index_for_validation,
                reverse_for_validation,
                pack_size,
            )
            .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
            if locations.object_count() != object_count
                || locations.pack_checksum().to_string() != git_checksum
            {
                return Err(corrupt_path(
                    "capsule Git locator",
                    "pack descriptor does not match its index",
                ));
            }
            crab_git::pack_locator::validate_pack_kind_metadata(
                &locator,
                locations.pack_checksum(),
                locations.object_count(),
            )
            .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
            if !has_download {
                // The pack body and all Git sidecars were authenticated above.
                // Avoid calling the installer again: its existing-pack path
                // would perform a second full SHA-1 scan of this immutable
                // content-addressed file on every incremental fetch.
                installed.push(final_pack);
                continue;
            }
            let pack_path = pack_path
                .as_deref()
                .ok_or_else(|| ReadError::internal("downloaded layered pack path disappeared"))?;
            let index_path = index_path.as_deref().ok_or_else(|| {
                ReadError::internal("downloaded layered index path disappeared")
            })?;
            let reverse_path = reverse_path.as_deref().ok_or_else(|| {
                ReadError::internal("downloaded layered reverse-index path disappeared")
            })?;
            let result = {
                if final_pack.exists() || final_index.exists() || final_reverse.exists() {
                    return Err(corrupt_path(
                        "capsule Git pack",
                        "pack installation destination already exists with different contents",
                    ));
                }
                crab_git::pack::install_pack_files_from_paths_with_identity(
                    &pack_dir,
                    &pack_path,
                    &index_path,
                    &reverse_path,
                    &content_hash,
                    max_input_bytes,
                    object_count,
                    verified_identity,
                )
            }
            .map_err(|error| corrupt_path("capsule Git pack", error.to_string()))?;
            if result.git_sha1 != git_checksum {
                return Err(corrupt_path(
                    "capsule Git pack",
                    "installed pack checksum does not match its descriptor",
                ));
            }
            installed.push(result.pack_path);
        }
        Ok(installed)
    })
    .await
    .map_err(|error| ReadError::Internal(format!("capsule pack install worker failed: {error}")))?
}

/// Install packs from a view, range-reading only checkpoint pack bodies that
/// are not already present in the destination Git object database.
pub async fn install_git_packs_from_store(
    view: &CapsuleRepositoryView,
    store: &Store,
    router: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
) -> Result<Vec<PathBuf>> {
    if let Some(installed) =
        install_layered_cold_clone_pack_from_store(view, store, router, git_dir, max_input_bytes)
            .await?
    {
        return Ok(installed);
    }
    if let Some(checkpoint) = view.layered_checkpoint() {
        return install_layered_git_packs_from_store(
            checkpoint,
            &view.capsules,
            store,
            router,
            git_dir,
            max_input_bytes,
        )
        .await;
    }
    let Some(control) = view.checkpoint_control().cloned() else {
        return install_git_packs(view, git_dir, max_input_bytes).await;
    };
    let pack_dir = git_dir.join("objects").join("pack");
    tokio::fs::create_dir_all(&pack_dir).await?;
    let mut payloads = Vec::new();
    for descriptor in control.git_packs() {
        let location = control.section_location(descriptor.pack_section())?;
        let content_hash = location.blake3().to_owned();
        let final_pack = pack_dir.join(format!("pack-{content_hash}.pack"));
        let final_index = pack_dir.join(format!("pack-{content_hash}.idx"));
        let final_reverse = pack_dir.join(format!("pack-{content_hash}.rev"));
        let pack_exists = final_pack.exists();
        let index_exists = final_index.exists();
        let reverse_exists = final_reverse.exists();
        let pack = if pack_exists || index_exists || reverse_exists {
            if !(pack_exists && index_exists && reverse_exists) {
                return Err(corrupt_path(
                    "capsule Git pack",
                    "local checkpoint pack installation is incomplete",
                ));
            }
            None
        } else {
            let end = location
                .offset()
                .checked_add(location.length())
                .ok_or_else(|| corrupt_path("capsule Git pack", "pack range overflowed"))?;
            let path = router.capsule_checkpoint_path(control.hash());
            Some(store.range_get(&path, location.offset()..end).await?)
        };
        payloads.push(GitPackPayload {
            pack,
            index: control.section_bytes(descriptor.index_section())?,
            reverse_index: control.section_bytes(descriptor.reverse_index_section())?,
            locator: control.section_bytes(descriptor.locator_section())?,
            verified_identity: None,
            content_hash,
            git_checksum: descriptor.git_checksum().to_owned(),
            object_count: descriptor.object_count(),
        });
    }
    for capsule in view.capsules.iter().cloned() {
        payloads.extend(payloads_from_container(GitPackContainer::Capsule(capsule))?);
    }
    install_git_pack_payloads(git_dir, payloads, max_input_bytes).await
}

async fn install_layered_cold_clone_pack_from_store(
    view: &CapsuleRepositoryView,
    store: &Store,
    router: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
) -> Result<Option<Vec<PathBuf>>> {
    let started = std::time::Instant::now();
    let Some(admitted) = view.layered_cold_clone_pack(router, max_input_bytes)? else {
        return Ok(None);
    };
    let source = view
        .layered_cold_clone_source()
        .ok_or_else(|| ReadError::internal("layered cold clone has no source"))?;
    let member = source
        .members()
        .first()
        .ok_or_else(|| ReadError::internal("layered cold clone has no member"))?;
    let sidecar_start = member
        .index()
        .offset()
        .min(member.reverse_index().offset())
        .min(member.locator().offset());
    let sidecar_end = member
        .index()
        .offset()
        .checked_add(member.index().length())
        .and_then(|end| {
            member
                .reverse_index()
                .offset()
                .checked_add(member.reverse_index().length())
                .map(|reverse_end| end.max(reverse_end))
        })
        .and_then(|end| {
            member
                .locator()
                .offset()
                .checked_add(member.locator().length())
                .map(|locator_end| end.max(locator_end))
        })
        .ok_or_else(|| corrupt_path("layered cold clone", "sidecar range overflowed"))?;
    let source_path = admitted.source_path.clone();
    let pack_dir = git_dir.join("objects").join("pack");
    tokio::fs::create_dir_all(&pack_dir).await?;
    let temporary = tempfile::Builder::new()
        .prefix(".crab-cold-pack-")
        .tempdir_in(&pack_dir)?;
    let pack_path = temporary.path().join("pack.pack");
    let index_path = temporary.path().join("pack.idx");
    let reverse_path = temporary.path().join("pack.rev");
    let source_download_started = std::time::Instant::now();
    let direct_source = if source.object_size() <= max_input_bytes {
        match store
            .try_download_signed_ranges_to_path(
                &source_path,
                &pack_path,
                source.object_size(),
                source.object_hash(),
                admitted.pack_range.clone(),
                sidecar_start..sidecar_end,
            )
            .await
        {
            Ok(result) => result,
            Err(crab_storage::StorageError::NotSupported { .. }) => None,
            Err(error) => return Err(error.into()),
        }
    } else {
        None
    };
    if let Some((_, _)) = direct_source.as_ref() {
        tracing::debug!(
            elapsed_ms = source_download_started.elapsed().as_millis() as u64,
            source_bytes = source.object_size(),
            pack_bytes = admitted.pack_range.end - admitted.pack_range.start,
            "layered cold clone signed ranges read"
        );
    }
    let source_downloaded = direct_source.is_some();
    let sidecars = if let Some((sidecars, _)) = direct_source.as_ref() {
        sidecars.clone()
    } else {
        read_layered_source_range(store, &source_path, sidecar_start..sidecar_end).await?
    };
    tracing::debug!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        sidecar_bytes = sidecars.len(),
        signed_ranges = source_downloaded,
        "layered cold clone sidecars read"
    );
    let index = layered_range_bytes(&sidecars, sidecar_start, member.index())?;
    let reverse_index = layered_range_bytes(&sidecars, sidecar_start, member.reverse_index())?;
    let locator = layered_range_bytes(&sidecars, sidecar_start, member.locator())?;
    if blake3::hash(&index).to_hex().as_str() != member.index().blake3() {
        return Err(corrupt_path(
            "layered cold clone",
            "pack index hash does not match its authenticated descriptor",
        ));
    }
    if blake3::hash(&reverse_index).to_hex().as_str() != member.reverse_index().blake3() {
        return Err(corrupt_path(
            "layered cold clone",
            "pack reverse-index hash does not match its authenticated descriptor",
        ));
    }
    if blake3::hash(&locator).to_hex().as_str() != member.locator().blake3() {
        return Err(corrupt_path(
            "layered cold clone",
            "pack locator hash does not match its authenticated descriptor",
        ));
    }
    let pack_length = admitted
        .pack_range
        .end
        .checked_sub(admitted.pack_range.start)
        .ok_or_else(|| corrupt_path("layered cold clone", "pack range underflowed"))?;
    let mut fallback_pack_hash = None;
    if direct_source.is_none() {
        let mut pack_file = tokio::fs::File::create(&pack_path).await?;
        let mut pack_hasher = blake3::Hasher::new();
        // The source is immutable and its sidecars were already read from
        // authenticated ranges. Larger pack ranges reduce provider request
        // overhead while retaining bounded in-flight memory.
        let ranges = (0..pack_length)
            .step_by(
                usize::try_from(LAYERED_COLD_CLONE_RANGE_CHUNK_BYTES).map_err(|_| {
                    corrupt_path(
                        "layered cold clone",
                        "pack range chunk size is not representable",
                    )
                })?,
            )
            .map(|offset| {
                let end = offset
                    .saturating_add(LAYERED_COLD_CLONE_RANGE_CHUNK_BYTES)
                    .min(pack_length);
                admitted.pack_range.start + offset..admitted.pack_range.start + end
            })
            .collect::<Vec<_>>();
        let mut chunks = futures_util::stream::iter(ranges.into_iter().map(|range| {
            let store = store.clone();
            let source_path = source_path.clone();
            async move { read_layered_source_range_single(&store, &source_path, range).await }
        }))
        .buffered(LAYERED_COLD_CLONE_RANGE_READ_CONCURRENCY);
        while let Some(chunk) = chunks.try_next().await? {
            pack_hasher.update(&chunk);
            pack_file.write_all(&chunk).await?;
        }
        pack_file.flush().await?;
        fallback_pack_hash = Some(pack_hasher.finalize());
    }
    tracing::debug!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        pack_bytes = pack_length,
        "layered cold clone pack ranges read"
    );
    tokio::fs::write(&index_path, &index).await?;
    tokio::fs::write(&reverse_path, &reverse_index).await?;
    let actual_hash = direct_source
        .as_ref()
        .map(|(_, hash)| hash.to_hex().to_string())
        .or_else(|| fallback_pack_hash.map(|hash| hash.to_hex().to_string()))
        .ok_or_else(|| ReadError::internal("cold clone pack hash was not computed"))?;
    if actual_hash != admitted.content_hash {
        return Err(corrupt_path(
            "layered cold clone",
            "pack content hash does not match its authenticated descriptor",
        ));
    }
    let locations =
        crab_git::pack_locator::PackLocationIter::open(&index_path, &reverse_path, pack_length)
            .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
    if locations.object_count() != admitted.object_count
        || locations.pack_checksum().to_string() != admitted.git_checksum
    {
        return Err(corrupt_path(
            "capsule Git locator",
            "pack descriptor does not match its indexes",
        ));
    }
    if let Some(expected_digest) = admitted.object_set_digest.as_deref()
        && admitted.object_set_count == Some(locations.object_count())
    {
        let object_ids = locations
            .sorted_object_ids()
            .map(|oid| {
                <[u8; 20]>::try_from(oid.as_bytes()).map_err(|_| {
                    corrupt_path("layered cold clone", "pack index object ID is not SHA-1")
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if visibility_object_set_digest(&object_ids) != expected_digest {
            return Err(corrupt_path(
                "layered cold clone",
                "pack index object set does not match the authenticated visibility proof",
            ));
        }
        let ref_tips = view
            .refs()
            .values()
            .map(|oid| {
                ObjectId::from_hex(oid.as_bytes())
                    .map_err(|_| corrupt_path("layered cold clone", "ref tip is not SHA-1"))
                    .and_then(|oid| {
                        <[u8; 20]>::try_from(oid.as_bytes())
                            .map_err(|_| corrupt_path("layered cold clone", "ref tip is not SHA-1"))
                    })
            })
            .collect::<Result<BTreeSet<_>>>()?;
        if ref_tips
            .iter()
            .any(|tip| object_ids.binary_search(tip).is_err())
        {
            return Err(corrupt_path(
                "layered cold clone",
                "authenticated ref tip is absent from the downloaded pack index",
            ));
        }
    }
    crab_git::pack_locator::validate_pack_kind_metadata(
        &locator,
        locations.pack_checksum(),
        locations.object_count(),
    )
    .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
    let verified_identity = crab_git::pack::VerifiedPackIdentity {
        git_sha1: locations
            .pack_checksum()
            .as_bytes()
            .try_into()
            .map_err(|_| corrupt_path("layered cold clone", "pack checksum is not SHA-1"))?,
        content_hash: *blake3::Hash::from_hex(&admitted.content_hash)
            .map_err(|error| corrupt_path("layered cold clone", error.to_string()))?
            .as_bytes(),
    };
    let canonical_name = admitted.content_hash.clone();
    let pack_dir_for_install = pack_dir.clone();
    let pack_path_for_install = pack_path.clone();
    let index_path_for_install = index_path.clone();
    let reverse_path_for_install = reverse_path.clone();
    let installed = tokio::task::spawn_blocking(move || {
        crab_git::pack::install_pack_files_from_paths_with_verified_sidecars(
            &pack_dir_for_install,
            &pack_path_for_install,
            &index_path_for_install,
            &reverse_path_for_install,
            &canonical_name,
            max_input_bytes,
            admitted.object_count,
            verified_identity,
        )
        .map(|installed| installed.pack_path)
        .map_err(|error| corrupt_path("layered cold clone", error.to_string()))
    })
    .await
    .map_err(|error| ReadError::Internal(format!("cold pack install worker failed: {error}")))??;
    Ok(Some(vec![installed]))
}

async fn install_layered_git_packs_from_store(
    checkpoint: &LayeredCheckpoint,
    capsules: &[Capsule],
    store: &Store,
    router: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
) -> Result<Vec<PathBuf>> {
    install_layered_git_packs_from_store_selected(
        checkpoint,
        capsules,
        store,
        router,
        git_dir,
        max_input_bytes,
        None,
    )
    .await
}

/// Install only the authenticated layered pack members named by `selected`.
///
/// This is the maintenance primitive for geometric suffix roll-ups. A `None`
/// selection installs every member, while a set selection skips stable packs
/// without reading their bodies or sidecars.
pub async fn install_layered_git_packs_from_store_selected(
    checkpoint: &LayeredCheckpoint,
    capsules: &[Capsule],
    store: &Store,
    router: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
    selected: Option<&BTreeSet<String>>,
) -> Result<Vec<PathBuf>> {
    install_layered_git_packs_from_store_sources_selected(
        checkpoint,
        &[],
        capsules,
        store,
        router,
        git_dir,
        max_input_bytes,
        selected,
    )
    .await
}

/// Install selected layered members from checkpoint and capsule-run sources.
///
/// The source descriptors are already authenticated by the repository view;
/// callers use this form when a control-only view deliberately omits capsule
/// bodies but still needs to materialize a bounded consolidation suffix.
pub async fn install_layered_git_packs_from_store_sources_selected(
    checkpoint: &LayeredCheckpoint,
    additional_sources: &[PackSourceDescriptor],
    capsules: &[Capsule],
    store: &Store,
    router: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
    selected: Option<&BTreeSet<String>>,
) -> Result<Vec<PathBuf>> {
    install_layered_git_packs_from_store_selected_with_sources(
        checkpoint,
        additional_sources,
        capsules,
        store,
        router,
        git_dir,
        max_input_bytes,
        selected,
        None,
    )
    .await?
    .ok_or_else(|| ReadError::internal("layered pack installation was not eligible"))
}

/// Install the exact self-contained layered members for one incremental fetch.
///
/// This is an optimization only. The authenticated member index must prove
/// that every installed object belongs to the requested delta or to a local
/// have, and the member must not carry an external delta base. Any selection
/// ambiguity returns `Ok(None)` so the caller can use the canonical generated
/// response-pack path without weakening authorization.
pub async fn install_layered_git_packs_for_fetch(
    view: &CapsuleRepositoryView,
    store: &Store,
    router: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
    object_ids: &[ObjectId],
    common_haves: &[ObjectId],
) -> Result<Option<Vec<PathBuf>>> {
    if view.layered_checkpoint().is_none() {
        return Ok(None);
    }
    if object_ids.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let Some(selected) = layered_fetch_pack_selection(view, object_ids)? else {
        return Ok(None);
    };
    install_layered_git_packs_for_fetch_selected(
        view,
        store,
        router,
        git_dir,
        max_input_bytes,
        object_ids,
        common_haves,
        &selected,
    )
    .await
}

/// Install an authenticated selected member set for one incremental fetch.
///
/// The selected IDs may come from the compact frontier admission or from a
/// batched locator join for objects reused from an older stable source. The
/// sidecar admission below remains authoritative and returns `None` when the
/// selected members cannot prove an exact, self-contained response. A
/// successful `Some` result is also a connectivity proof: all planned object
/// IDs are covered, every selected member is self-contained, and no member
/// contains an object outside the authenticated delta/common-have set.
pub async fn install_layered_git_packs_for_fetch_selected(
    view: &CapsuleRepositoryView,
    store: &Store,
    router: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
    object_ids: &[ObjectId],
    common_haves: &[ObjectId],
    selected: &BTreeSet<String>,
) -> Result<Option<Vec<PathBuf>>> {
    let Some(checkpoint) = view.layered_checkpoint() else {
        return Ok(None);
    };
    let mut allowed = BTreeSet::new();
    allowed.extend(object_ids.iter().copied());
    allowed.extend(common_haves.iter().copied());
    let required = object_ids.iter().copied().collect::<BTreeSet<_>>();
    install_layered_git_packs_from_store_selected_with_sources(
        checkpoint,
        view.capsule_run_sources(),
        &[],
        store,
        router,
        git_dir,
        max_input_bytes,
        Some(selected),
        Some((&allowed, &required)),
    )
    .await
}

/// Return the authenticated layered members that may cover an incremental
/// object delta without materializing a response pack.
pub fn layered_fetch_pack_selection(
    view: &CapsuleRepositoryView,
    object_ids: &[ObjectId],
) -> Result<Option<BTreeSet<String>>> {
    let mut candidates = BTreeMap::<[u8; 20], BTreeSet<String>>::new();
    for (oid, pack_ids) in view.frontier_object_admission() {
        candidates
            .entry(*oid)
            .or_default()
            .extend(pack_ids.iter().cloned());
    }

    let Some(checkpoint) = view.layered_checkpoint() else {
        return Ok(None);
    };
    if !checkpoint.is_control_only()
        && let Some(snapshot) = checkpoint.visibility_ordinal_snapshot()?
        && let Some(admission) = snapshot.member_admission()
    {
        if admission.len() != snapshot.objects().len() {
            return Err(corrupt_path(
                "layered visibility",
                "member admission count does not match the object dictionary",
            ));
        }
        for (oid, member) in snapshot.objects().iter().zip(admission) {
            let Some(source) = checkpoint.sources().get(usize::from(member.source_index())) else {
                return Err(corrupt_path(
                    "layered visibility",
                    "member admission source is out of bounds",
                ));
            };
            let Some(pack) = source.members().get(usize::from(member.member_index())) else {
                return Err(corrupt_path(
                    "layered visibility",
                    "member admission member is out of bounds",
                ));
            };
            candidates
                .entry(*oid)
                .or_default()
                .insert(pack.pack().blake3().to_owned());
        }
    }

    for (source_hash, members) in view.capsule_run_member_oids() {
        let Some(source) = view
            .capsule_run_sources()
            .iter()
            .find(|source| source.object_hash() == source_hash)
        else {
            return Err(corrupt_path(
                "layered visibility",
                "capsule-run member admission source is missing",
            ));
        };
        if members.len() != source.members().len() {
            return Err(corrupt_path(
                "layered visibility",
                "capsule-run member admission count is invalid",
            ));
        }
        for (member_index, object_ids) in members.iter().enumerate() {
            let Some(pack) = source.members().get(member_index) else {
                return Err(corrupt_path(
                    "layered visibility",
                    "capsule-run member admission member is missing",
                ));
            };
            let pack_id = pack.pack().blake3().to_owned();
            for oid in object_ids {
                candidates.entry(*oid).or_default().insert(pack_id.clone());
            }
        }
    }

    let mut selected = BTreeSet::new();
    for oid in object_ids {
        let raw: [u8; 20] = match oid.as_bytes().try_into() {
            Ok(raw) => raw,
            Err(_) => return Ok(None),
        };
        let Some(pack_ids) = candidates.get(&raw) else {
            return Ok(None);
        };
        selected.extend(pack_ids.iter().cloned());
    }
    if selected.is_empty() {
        Ok(None)
    } else {
        Ok(Some(selected))
    }
}

async fn install_layered_git_packs_from_store_selected_with_sources(
    checkpoint: &LayeredCheckpoint,
    additional_sources: &[PackSourceDescriptor],
    capsules: &[Capsule],
    store: &Store,
    router: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
    selected: Option<&BTreeSet<String>>,
    admission: Option<(&BTreeSet<ObjectId>, &BTreeSet<ObjectId>)>,
) -> Result<Option<Vec<PathBuf>>> {
    let pack_dir = git_dir.join("objects").join("pack");
    tokio::fs::create_dir_all(&pack_dir).await?;
    let mut seen_packs = BTreeMap::new();
    let mut members = Vec::new();
    let mut sources = checkpoint.sources().to_vec();
    let mut source_hashes = sources
        .iter()
        .map(|source| source.object_hash().to_owned())
        .collect::<BTreeSet<_>>();
    for source in additional_sources {
        if source_hashes.insert(source.object_hash().to_owned()) {
            sources.push(source.clone());
        }
    }
    for source in &sources {
        let path = match source.kind() {
            PackSourceKind::CapsuleRun => router.capsule_path(source.object_hash()),
            PackSourceKind::PackLayer => router.capsule_pack_layer_path(source.object_hash()),
        };
        for member in source.members() {
            let content_hash = member.pack().blake3().to_owned();
            if selected.is_some_and(|selected| !selected.contains(&content_hash)) {
                continue;
            }
            if let Some(previous) = seen_packs.insert(content_hash.clone(), member.clone()) {
                let equivalent = previous.index().blake3() == member.index().blake3()
                    && previous.reverse_index().blake3() == member.reverse_index().blake3()
                    && previous.locator().blake3() == member.locator().blake3()
                    && previous.git_checksum() == member.git_checksum()
                    && previous.object_count() == member.object_count()
                    && previous.external_delta_bases() == member.external_delta_bases();
                if !equivalent {
                    return Err(corrupt_path(
                        "capsule Git pack",
                        "layered sources disagree about one pack identity",
                    ));
                }
                continue;
            }
            let final_pack = pack_dir.join(format!("pack-{content_hash}.pack"));
            let final_index = pack_dir.join(format!("pack-{content_hash}.idx"));
            let final_reverse = pack_dir.join(format!("pack-{content_hash}.rev"));
            let complete_local =
                final_pack.exists() && final_index.exists() && final_reverse.exists();
            if final_pack.exists() || final_index.exists() || final_reverse.exists() {
                if !complete_local {
                    return Err(corrupt_path(
                        "capsule Git pack",
                        "local layered pack installation is incomplete",
                    ));
                }
            }
            members.push(LayeredPayloadMemberRead {
                member: LayeredMemberRead {
                    source_path: path.clone(),
                    source_size: source.object_size(),
                    pack_id: crab_xet::hash::MerkleHash::from_hex(&content_hash)
                        .map_err(|error| corrupt_path("capsule Git pack", error.to_string()))?,
                    member: member.clone(),
                },
                complete_local,
            });
        }
    }
    let sidecar_members = admission.map_or_else(
        || (0..members.len()).collect::<BTreeSet<_>>(),
        |_| {
            members
                .iter()
                .enumerate()
                .filter_map(|(index, member)| (!member.complete_local).then_some(index))
                .collect::<BTreeSet<_>>()
        },
    );
    let sidecar_windows = plan_layered_payload_windows_for_members(
        &members,
        &sidecar_members,
        if admission.is_some() {
            LayeredPayloadWindowMode::Sidecars
        } else {
            LayeredPayloadWindowMode::Full
        },
    )?;
    let (fetched_windows, mut fetched_bytes) =
        fetch_layered_payload_windows(store, &sidecar_windows).await?;
    if max_input_bytes > 0 && fetched_bytes > max_input_bytes {
        return Err(ReadError::CapsuleReadLimit {
            resource: "layered Git payload ranges",
            maximum: max_input_bytes,
        });
    }

    if let Some((allowed, required)) = admission {
        let mut covered = BTreeSet::new();
        for (member_index, member_read) in members.iter().enumerate() {
            let member = &member_read.member.member;
            let (object_ids, pack_checksum) = if member_read.complete_local {
                // An ordinary unfiltered fetch only needs the authenticated
                // object set and Git index identity. Kind metadata is needed
                // for remote object reconstruction, not for an already
                // installed self-contained pack.
                local_layered_member_admission(&pack_dir, member_read)?
            } else {
                let window_index = payload_window_for_member(&sidecar_windows, member_index)?;
                let window = sidecar_windows
                    .get(window_index)
                    .ok_or_else(|| ReadError::internal("layered sidecar window disappeared"))?;
                let body = fetched_windows
                    .get(&window_index)
                    .ok_or_else(|| ReadError::internal("layered sidecar bytes disappeared"))?;
                let index = layered_range_bytes(body, window.range.start, member.index())?;
                let object_ids = crab_git::pack_locator::sorted_object_ids_from_index_bytes(&index)
                    .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
                (object_ids.0, object_ids.1.to_string())
            };
            if object_ids.len() as u64 != member.object_count()
                || pack_checksum != member.git_checksum()
                || !member.external_delta_bases().is_empty()
            {
                return Ok(None);
            }
            if object_ids.iter().any(|oid| !allowed.contains(oid)) {
                return Ok(None);
            }
            covered.extend(object_ids);
        }
        if !required.iter().all(|oid| covered.contains(oid)) {
            return Ok(None);
        }

        let pack_members = members
            .iter()
            .enumerate()
            .filter(|(_, member)| !member.complete_local)
            .map(|(index, _)| index)
            .collect::<BTreeSet<_>>();
        if !pack_members.is_empty() {
            let pack_windows = plan_layered_payload_windows_for_members(
                &members,
                &pack_members,
                LayeredPayloadWindowMode::Packs,
            )?;
            let (pack_bytes, pack_read) =
                fetch_layered_payload_windows(store, &pack_windows).await?;
            fetched_bytes = fetched_bytes
                .checked_add(pack_read)
                .ok_or_else(|| ReadError::internal("layered payload byte count overflowed"))?;
            if max_input_bytes > 0 && fetched_bytes > max_input_bytes {
                return Err(ReadError::CapsuleReadLimit {
                    resource: "layered Git payload ranges",
                    maximum: max_input_bytes,
                });
            }
            return build_layered_payload_install(
                &members,
                &sidecar_windows,
                &fetched_windows,
                Some(&pack_windows),
                Some(&pack_bytes),
                capsules,
                selected,
                git_dir,
                max_input_bytes,
                true,
            )
            .await
            .map(Some);
        }
    }

    build_layered_payload_install(
        &members,
        &sidecar_windows,
        &fetched_windows,
        None,
        None,
        capsules,
        selected,
        git_dir,
        max_input_bytes,
        false,
    )
    .await
    .map(Some)
}

fn local_layered_member_admission(
    pack_dir: &Path,
    member_read: &LayeredPayloadMemberRead,
) -> Result<(Vec<gix_hash::ObjectId>, String)> {
    let descriptor = &member_read.member.member;
    let content_hash = descriptor.pack().blake3();
    let final_pack = pack_dir.join(format!("pack-{content_hash}.pack"));
    let final_index = pack_dir.join(format!("pack-{content_hash}.idx"));
    let final_reverse = pack_dir.join(format!("pack-{content_hash}.rev"));
    let pack_size = std::fs::metadata(&final_pack)?.len();
    if pack_size != descriptor.pack().length() {
        return Err(corrupt_path(
            "capsule Git pack",
            "local layered pack size does not match its authenticated descriptor",
        ));
    }
    let mut pack_file = std::fs::File::open(&final_pack)?;
    let mut pack_hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = std::io::Read::read(&mut pack_file, &mut buffer)?;
        if read == 0 {
            break;
        }
        pack_hasher.update(&buffer[..read]);
    }
    if pack_hasher.finalize().to_hex().as_str() != content_hash {
        return Err(corrupt_path(
            "capsule Git pack",
            "local layered pack content hash does not match its authenticated descriptor",
        ));
    }
    let index = std::fs::read(&final_index)?;
    if blake3::hash(&index).to_hex().as_str() != descriptor.index().blake3() {
        return Err(corrupt_path(
            "capsule Git locator",
            "local layered index hash does not match its authenticated descriptor",
        ));
    }
    let reverse_index = std::fs::read(&final_reverse)?;
    if blake3::hash(&reverse_index).to_hex().as_str() != descriptor.reverse_index().blake3() {
        return Err(corrupt_path(
            "capsule Git locator",
            "local layered reverse-index hash does not match its authenticated descriptor",
        ));
    }
    let locations =
        crab_git::pack_locator::PackLocationIter::open(&final_index, &final_reverse, pack_size)
            .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
    if locations.object_count() != descriptor.object_count()
        || locations.pack_checksum().to_string() != descriptor.git_checksum()
    {
        return Err(corrupt_path(
            "capsule Git locator",
            "local layered pack descriptor does not match its indexes",
        ));
    }
    let object_ids = locations
        .map(|location| {
            location
                .map(|location| location.oid)
                .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((object_ids, descriptor.git_checksum().to_owned()))
}

async fn build_layered_payload_install(
    members: &[LayeredPayloadMemberRead],
    sidecar_windows: &[LayeredPayloadWindow],
    sidecar_bytes: &BTreeMap<usize, Bytes>,
    pack_windows: Option<&[LayeredPayloadWindow]>,
    pack_bytes: Option<&BTreeMap<usize, Bytes>>,
    capsules: &[Capsule],
    selected: Option<&BTreeSet<String>>,
    git_dir: &Path,
    max_input_bytes: u64,
    skip_complete_local: bool,
) -> Result<Vec<PathBuf>> {
    let mut payloads = Vec::with_capacity(members.len());
    for (member_index, member_read) in members.iter().enumerate() {
        if skip_complete_local && member_read.complete_local {
            continue;
        }
        let sidecar_window_index = payload_window_for_member(sidecar_windows, member_index)?;
        let sidecar_window = sidecar_windows
            .get(sidecar_window_index)
            .ok_or_else(|| ReadError::internal("layered sidecar window disappeared"))?;
        let sidecar_body = sidecar_bytes
            .get(&sidecar_window_index)
            .ok_or_else(|| ReadError::internal("layered sidecar bytes disappeared"))?;
        let start = sidecar_window.range.start;
        let member = &member_read.member.member;
        let index = layered_range_bytes(sidecar_body, start, member.index())?;
        let reverse_index = layered_range_bytes(sidecar_body, start, member.reverse_index())?;
        let locator = layered_range_bytes(sidecar_body, start, member.locator())?;
        let pack = if member_read.complete_local {
            None
        } else if let (Some(pack_windows), Some(pack_bytes)) = (pack_windows, pack_bytes) {
            let pack_window_index = payload_window_for_member(pack_windows, member_index)?;
            let pack_window = pack_windows
                .get(pack_window_index)
                .ok_or_else(|| ReadError::internal("layered pack window disappeared"))?;
            let pack_body = pack_bytes
                .get(&pack_window_index)
                .ok_or_else(|| ReadError::internal("layered pack bytes disappeared"))?;
            Some(layered_range_bytes(
                pack_body,
                pack_window.range.start,
                member.pack(),
            )?)
        } else {
            Some(layered_range_bytes(sidecar_body, start, member.pack())?)
        };
        let verified_identity = pack
            .as_ref()
            .map(|_| verified_layered_pack_identity(member))
            .transpose()?;
        payloads.push(GitPackPayload {
            pack,
            index,
            reverse_index,
            locator,
            verified_identity,
            content_hash: member_read.member.pack_id.to_string(),
            git_checksum: member.git_checksum().to_owned(),
            object_count: member.object_count(),
        });
    }
    for capsule in capsules.iter().cloned() {
        for payload in payloads_from_container(GitPackContainer::Capsule(capsule))? {
            if selected.is_some_and(|selected| !selected.contains(&payload.content_hash)) {
                continue;
            }
            if let Some(existing) = payloads
                .iter()
                .find(|existing| existing.content_hash == payload.content_hash)
            {
                if existing.git_checksum != payload.git_checksum
                    || existing.object_count != payload.object_count
                    || existing.index != payload.index
                    || existing.reverse_index != payload.reverse_index
                    || existing.locator != payload.locator
                {
                    return Err(corrupt_path(
                        "capsule Git pack",
                        "layered sources disagree about one pack identity",
                    ));
                }
                continue;
            }
            payloads.push(payload);
        }
    }
    install_git_pack_payloads(git_dir, payloads, max_input_bytes).await
}

fn verified_layered_pack_identity(
    member: &PackMemberDescriptor,
) -> Result<crab_git::pack::VerifiedPackIdentity> {
    let git_sha1 = gix_hash::ObjectId::from_hex(member.git_checksum().as_bytes())
        .map_err(|error| corrupt_path("capsule Git pack", error.to_string()))?
        .as_bytes()
        .try_into()
        .map_err(|_| corrupt_path("capsule Git pack", "Git pack checksum is not SHA-1"))?;
    let content_hash = *blake3::Hash::from_hex(member.pack().blake3())
        .map_err(|error| corrupt_path("capsule Git pack", error.to_string()))?
        .as_bytes();
    Ok(crab_git::pack::VerifiedPackIdentity {
        git_sha1,
        content_hash,
    })
}

async fn fetch_layered_payload_windows(
    store: &Store,
    windows: &[LayeredPayloadWindow],
) -> Result<(BTreeMap<usize, Bytes>, u64)> {
    let fetched = futures_util::stream::iter(
        windows
            .iter()
            .map(|window| (window.path.clone(), window.range.clone()))
            .enumerate()
            .map(|(window_index, (path, range))| {
                let store = store.clone();
                async move {
                    let bytes = read_layered_source_range(&store, &path, range).await?;
                    Ok::<_, ReadError>((window_index, bytes))
                }
            }),
    )
    .buffer_unordered(LAYERED_SIDECAR_READ_CONCURRENCY)
    .try_collect::<Vec<_>>()
    .await?
    .into_iter()
    .collect::<BTreeMap<usize, Bytes>>();
    let bytes = fetched.values().try_fold(0_u64, |total, bytes| {
        total
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| ReadError::internal("layered payload byte count overflowed"))
    })?;
    Ok((fetched, bytes))
}

async fn read_layered_source_range(
    store: &Store,
    path: &object_store::path::Path,
    range: std::ops::Range<u64>,
) -> Result<Bytes> {
    let expected = range
        .end
        .checked_sub(range.start)
        .ok_or_else(|| corrupt_path("capsule Git pack", "source range underflowed"))?;
    if expected <= LAYERED_LARGE_RANGE_THRESHOLD_BYTES {
        return read_layered_source_range_single(store, path, range).await;
    }

    let mut ranges = Vec::new();
    let mut start = range.start;
    while start < range.end {
        let end = start
            .saturating_add(LAYERED_LARGE_RANGE_CHUNK_BYTES)
            .min(range.end);
        ranges.push(start..end);
        start = end;
    }
    let mut fetched =
        futures_util::stream::iter(ranges.into_iter().enumerate().map(|(index, subrange)| {
            let store = store.clone();
            let path = path.clone();
            async move {
                let bytes = read_layered_source_range_single(&store, &path, subrange).await?;
                Ok::<_, ReadError>((index, bytes))
            }
        }))
        .buffer_unordered(LAYERED_LARGE_RANGE_READ_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
    fetched.sort_unstable_by_key(|(index, _)| *index);
    let capacity = usize::try_from(expected)
        .map_err(|_| corrupt_path("capsule Git pack", "layered source range is too large"))?;
    let mut assembled = Vec::with_capacity(capacity);
    for (_, bytes) in fetched {
        assembled.extend_from_slice(&bytes);
    }
    if assembled.len() as u64 != expected {
        return Err(corrupt_path(
            "capsule Git pack",
            "layered source range reassembly returned an unexpected length",
        ));
    }
    Ok(Bytes::from(assembled))
}

async fn read_layered_source_range_single(
    store: &Store,
    path: &object_store::path::Path,
    range: std::ops::Range<u64>,
) -> Result<Bytes> {
    let expected = range
        .end
        .checked_sub(range.start)
        .ok_or_else(|| corrupt_path("capsule Git pack", "source range underflowed"))?;
    let bytes = store.range_get(path, range).await?;
    if bytes.len() as u64 != expected {
        return Err(corrupt_path(
            "capsule Git pack",
            "layered source range returned an unexpected length",
        ));
    }
    Ok(bytes)
}

#[derive(Clone)]
struct LayeredMemberRead {
    source_path: object_store::path::Path,
    source_size: u64,
    pack_id: crab_xet::hash::MerkleHash,
    member: PackMemberDescriptor,
}

struct LayeredPayloadMemberRead {
    member: LayeredMemberRead,
    complete_local: bool,
}

struct LayeredPayloadWindow {
    path: object_store::path::Path,
    range: std::ops::Range<u64>,
    member_indices: Vec<usize>,
    useful_bytes: u64,
}

#[derive(Clone, Copy)]
enum LayeredPayloadWindowMode {
    Full,
    Sidecars,
    Packs,
}

#[cfg(test)]
fn plan_layered_payload_windows(
    members: &[LayeredPayloadMemberRead],
    mode: LayeredPayloadWindowMode,
) -> Result<Vec<LayeredPayloadWindow>> {
    let selected = (0..members.len()).collect::<BTreeSet<_>>();
    plan_layered_payload_windows_for_members(members, &selected, mode)
}

fn plan_layered_payload_windows_for_members(
    members: &[LayeredPayloadMemberRead],
    selected_members: &BTreeSet<usize>,
    mode: LayeredPayloadWindowMode,
) -> Result<Vec<LayeredPayloadWindow>> {
    let (max_gap, max_extra, max_window) = match mode {
        LayeredPayloadWindowMode::Full => (u64::MAX, u64::MAX, LAYERED_FULL_MAX_WINDOW_BYTES),
        LayeredPayloadWindowMode::Sidecars | LayeredPayloadWindowMode::Packs => (
            LAYERED_SIDECAR_COALESCE_GAP_BYTES,
            LAYERED_SIDECAR_MAX_EXTRA_BYTES,
            LAYERED_SIDECAR_MAX_WINDOW_BYTES,
        ),
    };
    let mut ordered = members
        .iter()
        .enumerate()
        .filter(|(index, _)| selected_members.contains(index))
        .filter(|(_, member)| {
            !matches!(mode, LayeredPayloadWindowMode::Packs) || !member.complete_local
        })
        .map(|(index, member)| {
            let descriptor = &member.member.member;
            let sidecar_start = descriptor
                .index()
                .offset()
                .min(descriptor.reverse_index().offset())
                .min(descriptor.locator().offset());
            let sidecar_end = descriptor
                .index()
                .offset()
                .checked_add(descriptor.index().length())
                .and_then(|end| {
                    descriptor
                        .reverse_index()
                        .offset()
                        .checked_add(descriptor.reverse_index().length())
                        .map(|reverse_end| end.max(reverse_end))
                })
                .and_then(|end| {
                    descriptor
                        .locator()
                        .offset()
                        .checked_add(descriptor.locator().length())
                        .map(|locator_end| end.max(locator_end))
                })
                .ok_or_else(|| {
                    corrupt_path("capsule Git pack", "layered payload range overflowed")
                })?;
            let (start, end) = match mode {
                LayeredPayloadWindowMode::Sidecars => (sidecar_start, sidecar_end),
                LayeredPayloadWindowMode::Packs => {
                    let end = descriptor
                        .pack()
                        .offset()
                        .checked_add(descriptor.pack().length())
                        .ok_or_else(|| {
                            corrupt_path("capsule Git pack", "layered pack range overflowed")
                        })?;
                    (descriptor.pack().offset(), end)
                }
                LayeredPayloadWindowMode::Full => {
                    let (start, end) = if member.complete_local {
                        (sidecar_start, sidecar_end)
                    } else {
                        let pack_end = descriptor
                            .pack()
                            .offset()
                            .checked_add(descriptor.pack().length())
                            .ok_or_else(|| {
                                corrupt_path("capsule Git pack", "layered pack range overflowed")
                            })?;
                        (
                            descriptor.pack().offset().min(sidecar_start),
                            pack_end.max(sidecar_end),
                        )
                    };
                    (start, end)
                }
            };
            if end <= start || end > member.member.source_size {
                return Err(corrupt_path(
                    "capsule Git pack",
                    "layered payload range is empty or outside its source",
                ));
            }
            Ok((index, start, end))
        })
        .collect::<Result<Vec<_>>>()?;
    ordered.sort_by(|left, right| {
        members[left.0]
            .member
            .source_path
            .cmp(&members[right.0].member.source_path)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
    });

    let mut windows = Vec::new();
    for (member_index, start, end) in ordered {
        let useful_bytes = end.saturating_sub(start);
        let can_extend = windows.last().is_some_and(|window: &LayeredPayloadWindow| {
            if window.path != members[member_index].member.source_path || start < window.range.start
            {
                return false;
            }
            let gap = start.saturating_sub(window.range.end);
            let candidate_end = window.range.end.max(end);
            let candidate_size = candidate_end.saturating_sub(window.range.start);
            let candidate_useful = window.useful_bytes.saturating_add(useful_bytes);
            let extra = candidate_size.saturating_sub(candidate_useful);
            gap <= max_gap && extra <= max_extra && candidate_size <= max_window
        });
        if can_extend {
            let window = windows
                .last_mut()
                .ok_or_else(|| ReadError::internal("layered payload window disappeared"))?;
            window.range.end = window.range.end.max(end);
            window.member_indices.push(member_index);
            window.useful_bytes = window.useful_bytes.saturating_add(useful_bytes);
        } else {
            windows.push(LayeredPayloadWindow {
                path: members[member_index].member.source_path.clone(),
                range: start..end,
                member_indices: vec![member_index],
                useful_bytes,
            });
        }
    }
    Ok(windows)
}

fn payload_window_for_member(
    windows: &[LayeredPayloadWindow],
    member_index: usize,
) -> Result<usize> {
    windows
        .iter()
        .position(|window| window.member_indices.contains(&member_index))
        .ok_or_else(|| ReadError::internal("layered member has no payload window"))
}

fn layered_range_bytes(
    body: &Bytes,
    body_offset: u64,
    range: &crab_metadata::capsule_protocol::PackRange,
) -> Result<Bytes> {
    let relative = range
        .offset()
        .checked_sub(body_offset)
        .ok_or_else(|| corrupt_path("capsule Git pack", "layered source range is before window"))?;
    let end = relative
        .checked_add(range.length())
        .ok_or_else(|| corrupt_path("capsule Git pack", "layered source range overflowed"))?;
    let start_usize = usize::try_from(relative)
        .map_err(|_| corrupt_path("capsule Git pack", "layered source range offset overflowed"))?;
    let end_usize = usize::try_from(end)
        .map_err(|_| corrupt_path("capsule Git pack", "layered source range end overflowed"))?;
    let bytes = body
        .get(start_usize..end_usize)
        .ok_or_else(|| corrupt_path("capsule Git pack", "layered source range is out of bounds"))?;
    if blake3::hash(bytes).to_hex().as_str() != range.blake3() {
        return Err(corrupt_path(
            "capsule Git pack",
            "layered source range hash does not match its descriptor",
        ));
    }
    // Keep the authenticated window alive through installation instead of
    // copying every pack and sidecar range into a second allocation. The
    // installer still writes immutable files and validates every sidecar.
    Ok(body.slice(start_usize..end_usize))
}

enum GitPackContainer {
    Checkpoint(Checkpoint),
    Capsule(Capsule),
}

fn inline_locators_for_pack(
    object_count: u64,
    git_checksum: &str,
    pack_id: crab_xet::hash::MerkleHash,
    index_path: &Path,
    reverse_path: &Path,
    locator_bytes: &Bytes,
    pack_size: u64,
) -> Result<std::collections::HashMap<[u8; 20], crab_metadata::git_object_locator::GitObjectLocator>>
{
    let locations =
        crab_git::pack_locator::PackLocationIter::open(index_path, reverse_path, pack_size)
            .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
    if locations.pack_checksum().to_string() != git_checksum {
        return Err(corrupt_path(
            "capsule Git locator",
            "pack index checksum does not match its descriptor",
        ));
    }
    let locator_entries = crab_git::pack_locator::decode_pack_kind_metadata_with_external_deltas(
        locator_bytes,
        locations,
    )
    .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
    let locator_locations =
        crab_git::pack_locator::PackLocationIter::open(index_path, reverse_path, pack_size)
            .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
    let mut inline = std::collections::HashMap::new();
    for (ordinal, ((oid, kind, delta_base_oid), location)) in locator_entries
        .into_iter()
        .zip(locator_locations)
        .enumerate()
    {
        let location =
            location.map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
        let oid_bytes = oid
            .as_bytes()
            .try_into()
            .map_err(|_| ReadError::internal("capsule Git locator is not SHA-1"))?;
        let ordinal = u32::try_from(ordinal)
            .map_err(|_| ReadError::internal("capsule Git locator ordinal overflowed"))?;
        let kind = match kind {
            gix_object::Kind::Commit => crab_metadata::git_object_locator::GitObjectKind::Commit,
            gix_object::Kind::Tree => crab_metadata::git_object_locator::GitObjectKind::Tree,
            gix_object::Kind::Blob => crab_metadata::git_object_locator::GitObjectKind::Blob,
            gix_object::Kind::Tag => crab_metadata::git_object_locator::GitObjectKind::Tag,
        };
        inline.insert(
            oid_bytes,
            crab_metadata::git_object_locator::GitObjectLocator {
                ordinal,
                pack_id,
                location: crab_metadata::git_object_locator::GitObjectLocation {
                    pack_offset: location.pack_offset,
                    entry_len: location.entry_len,
                    crc32: location.crc32,
                },
                metadata: crab_metadata::git_object_locator::GitObjectMetadata {
                    kind: Some(kind),
                    logical_size: None,
                    delta_base_oid: delta_base_oid
                        .map(|oid| oid.as_bytes().try_into())
                        .transpose()
                        .map_err(|_| ReadError::internal("capsule Git delta base is not SHA-1"))?,
                },
            },
        );
    }
    if inline.len() as u64 != object_count {
        return Err(corrupt_path(
            "capsule Git locator",
            "locator metadata count does not match its pack descriptor",
        ));
    }
    Ok(inline)
}

impl GitPackContainer {
    fn git_packs(&self) -> &[CapsuleGitPackDescriptor] {
        match self {
            Self::Checkpoint(checkpoint) => checkpoint.git_packs(),
            Self::Capsule(capsule) => capsule.git_packs(),
        }
    }

    fn section_bytes(&self, section: u32) -> Result<Bytes> {
        match self {
            Self::Checkpoint(checkpoint) => Ok(checkpoint.section_bytes(section)?),
            Self::Capsule(capsule) => Ok(capsule.section_bytes(section)?),
        }
    }
}

/// Load a root and its bounded capsule frontier with one request per object.
///
/// Capsule bodies are fetched concurrently, then checked against the exact
/// size, content identity, transaction, and base-root bindings in the root.
pub async fn open_view(
    router: &StoreLayout<Store>,
    limits: CapsuleReadLimits,
) -> Result<CapsuleRepositoryView> {
    let snapshot = load_root(router).await?;
    open_view_from_root(router, snapshot, limits).await
}

/// Read the complete transaction-consistent ref map without fetching capsule payloads.
///
/// Ref-head bodies and any transaction records that make prepared multi-ref updates
/// visible are still verified. Callers that consume Git objects must open a full view
/// before trusting the immutable payloads named by those refs.
pub async fn read_visible_refs(router: &StoreLayout<Store>) -> Result<BTreeMap<String, String>> {
    let snapshot = load_root(router).await?;
    read_visible_refs_from_root(router, &snapshot).await
}

/// Read current refs from an already verified root without fetching capsule payloads.
pub async fn read_visible_refs_from_root(
    router: &StoreLayout<Store>,
    snapshot: &crab_metadata::capsule_protocol::RootSnapshot,
) -> Result<BTreeMap<String, String>> {
    Ok(open_ref_view_from_root(router, snapshot.clone())
        .await?
        .refs)
}

/// Capture every current ref without fetching checkpoint or capsule payloads.
pub async fn open_ref_view_from_root(
    router: &StoreLayout<Store>,
    snapshot: crab_metadata::capsule_protocol::RootSnapshot,
) -> Result<CapsuleRefView> {
    let (heads, active) = capture_ref_heads(router, snapshot.record().root()).await?;
    let visible = materialize_visible_ref_heads(snapshot.record().root(), &heads, &active)?;
    Ok(CapsuleRefView {
        root: snapshot,
        refs: visible.refs,
        peeled_refs: visible.peeled_refs,
        visible_ref_transactions: visible.transactions,
    })
}

/// Inspect one root and every visible per-ref position without loading payloads.
///
/// Maintenance polling uses this to detect quiescence without repeatedly
/// downloading stable capsule or checkpoint bodies.
pub async fn read_activity_from_root(
    router: &StoreLayout<Store>,
    snapshot: &crab_metadata::capsule_protocol::RootSnapshot,
) -> Result<CapsuleRepositoryActivity> {
    let (heads, active) = capture_ref_heads(router, snapshot.record().root()).await?;
    let visible = materialize_visible_ref_heads(snapshot.record().root(), &heads, &active)?;
    let capsule_count = visible.pointers.iter().try_fold(0_u64, |total, pointer| {
        total
            .checked_add(u64::from(pointer.capsule_count()))
            .ok_or_else(|| ReadError::internal("capsule frontier count overflowed"))
    })?;
    Ok(CapsuleRepositoryActivity {
        state_digest: state_digest(snapshot.record(), &visible.transactions),
        capsule_count,
        ref_count: u64::try_from(visible.refs.len())
            .map_err(|_| ReadError::internal("capsule ref count overflowed"))?,
    })
}

/// Read only requested current refs from an already verified root.
///
/// Missing and deleted refs are omitted. The result is authoritative only for
/// `ref_names` and never includes compacted values for unchecked sibling refs.
pub async fn read_visible_refs_from_root_for_refs(
    router: &StoreLayout<Store>,
    snapshot: &crab_metadata::capsule_protocol::RootSnapshot,
    ref_names: &BTreeSet<String>,
) -> Result<BTreeMap<String, String>> {
    Ok(
        open_ref_view_from_root_for_refs(router, snapshot.clone(), ref_names)
            .await?
            .refs,
    )
}

/// Capture only requested current refs without fetching immutable payloads.
///
/// The result is authoritative only for `ref_names`; absent entries represent
/// missing or deleted selected refs rather than knowledge of sibling refs.
pub async fn open_ref_view_from_root_for_refs(
    router: &StoreLayout<Store>,
    snapshot: crab_metadata::capsule_protocol::RootSnapshot,
    ref_names: &BTreeSet<String>,
) -> Result<CapsuleRefView> {
    let (heads, active) =
        capture_selected_ref_heads(router, snapshot.record().root(), ref_names).await?;
    let visible = materialize_visible_ref_heads(snapshot.record().root(), &heads, &active)?;
    Ok(CapsuleRefView {
        root: snapshot,
        refs: visible
            .refs
            .into_iter()
            .filter(|(ref_name, _)| ref_names.contains(ref_name))
            .collect(),
        peeled_refs: visible
            .peeled_refs
            .into_iter()
            .filter(|(ref_name, _)| ref_names.contains(ref_name))
            .collect(),
        visible_ref_transactions: visible
            .transactions
            .into_iter()
            .filter(|(ref_name, _)| ref_names.contains(ref_name))
            .collect(),
    })
}

/// Capture selected refs once for a push admission snapshot.
///
/// The writer immediately rechecks each selected head's ETag and expected old
/// value before publishing, so this avoids a redundant reader-side stability
/// probe without weakening the publication CAS or conflict checks.
pub async fn open_ref_view_from_root_for_push(
    router: &StoreLayout<Store>,
    snapshot: crab_metadata::capsule_protocol::RootSnapshot,
    ref_names: &BTreeSet<String>,
) -> Result<CapsuleRefView> {
    let loaded = load_selected_ref_heads(router, ref_names).await?;
    let heads = loaded
        .iter()
        .filter_map(|entry| entry.as_ref().map(|(head, _)| head.clone()))
        .filter(|head| head.ref_epoch() == snapshot.record().root().ref_epoch())
        .collect::<Vec<_>>();
    let active = resolve_referenced_activations(router, &heads).await?;
    for head in &heads {
        let visible = head.visible(&active);
        let base_oid = snapshot
            .record()
            .root()
            .refs()
            .get(head.ref_name())
            .map(String::as_str);
        if visible.transaction_id().is_none() && visible.oid() != base_oid {
            return Err(corrupt_path(
                "capsule-protocol ref heads",
                "capsule ref head without a transaction differs from the compacted root",
            ));
        }
    }
    let visible = materialize_visible_ref_heads(snapshot.record().root(), &heads, &active)?;
    Ok(CapsuleRefView {
        root: snapshot,
        refs: visible
            .refs
            .into_iter()
            .filter(|(ref_name, _)| ref_names.contains(ref_name))
            .collect(),
        peeled_refs: visible
            .peeled_refs
            .into_iter()
            .filter(|(ref_name, _)| ref_names.contains(ref_name))
            .collect(),
        visible_ref_transactions: visible
            .transactions
            .into_iter()
            .filter(|(ref_name, _)| ref_names.contains(ref_name))
            .collect(),
    })
}

/// Load the immutable objects named by one already authenticated root.
///
/// Remote-helper sessions use this entry point to bind advertisement and
/// transfer to one root while avoiding a redundant mutable-root request.
pub async fn open_view_from_root(
    router: &StoreLayout<Store>,
    snapshot: crab_metadata::capsule_protocol::RootSnapshot,
    limits: CapsuleReadLimits,
) -> Result<CapsuleRepositoryView> {
    let (heads, active) = capture_ref_heads(router, snapshot.record().root()).await?;
    assemble_view(
        router,
        snapshot,
        limits,
        heads,
        active,
        CheckpointLoad::Complete,
    )
    .await
}

/// Load a repository view with only the range-addressable checkpoint control suffix.
///
/// This path is used by ordinary fetch. It authenticates checkpoint metadata and
/// leaves pack bodies available for selective range reads, so an existing local
/// clone does not download its checkpoint again.
pub async fn open_view_from_root_with_control(
    router: &StoreLayout<Store>,
    snapshot: crab_metadata::capsule_protocol::RootSnapshot,
    limits: CapsuleReadLimits,
) -> Result<CapsuleRepositoryView> {
    let (heads, active) = capture_ref_heads(router, snapshot.record().root()).await?;
    assemble_view(
        router,
        snapshot,
        limits,
        heads,
        active,
        CheckpointLoad::Control,
    )
    .await
}

/// Load a layered repository view using only its authenticated checkpoint
/// footer and run controls.
///
/// This path is for ordinary advertised-ref fetches. It deliberately leaves
/// the large visibility body cold; callers that need tags, filters, shallow
/// history, maintenance, or strict fsck must use the complete view path.
pub async fn open_view_from_root_with_layered_control(
    router: &StoreLayout<Store>,
    snapshot: crab_metadata::capsule_protocol::RootSnapshot,
    limits: CapsuleReadLimits,
) -> Result<CapsuleRepositoryView> {
    let (heads, active) = capture_ref_heads(router, snapshot.record().root()).await?;
    assemble_view(
        router,
        snapshot,
        limits,
        heads,
        active,
        CheckpointLoad::LayeredControl,
    )
    .await
}

#[derive(Debug, Clone, Copy)]
enum CheckpointLoad {
    Complete,
    Control,
    LayeredControl,
}

async fn assemble_view(
    router: &StoreLayout<Store>,
    snapshot: crab_metadata::capsule_protocol::RootSnapshot,
    limits: CapsuleReadLimits,
    heads: Vec<crab_metadata::capsule_protocol::CapsuleRefHead>,
    active: BTreeSet<String>,
    checkpoint_load: CheckpointLoad,
) -> Result<CapsuleRepositoryView> {
    let checkpoint_is_layered = snapshot
        .record()
        .root()
        .checkpoint()
        .is_some_and(|pointer| pointer.format() == 5);
    let uncheckpointed_layered_control = matches!(checkpoint_load, CheckpointLoad::LayeredControl)
        && snapshot.record().root().checkpoint().is_none();
    if matches!(
        checkpoint_load,
        CheckpointLoad::Control | CheckpointLoad::LayeredControl
    ) && (checkpoint_is_layered || uncheckpointed_layered_control)
    {
        return assemble_layered_control_view(
            router,
            snapshot,
            limits,
            heads,
            active,
            matches!(checkpoint_load, CheckpointLoad::LayeredControl),
        )
        .await;
    }
    let mut refs = snapshot.record().root().refs().clone();
    let mut peeled_refs = snapshot.record().root().peeled_refs().clone();
    let visible = materialize_visible_ref_heads(snapshot.record().root(), &heads, &active)?;
    let expected_refs = visible.refs;
    let expected_peeled = visible.peeled_refs;
    let visible_ref_transactions = visible.transactions;
    let ref_frontiers = visible.frontiers;
    let pointers = visible.pointers;
    admit_frontier(&pointers, limits)?;
    let checkpoint = async {
        match snapshot.record().root().checkpoint() {
            Some(pointer) => match checkpoint_load {
                CheckpointLoad::Complete => {
                    if pointer.format() == 5 {
                        load_layered_checkpoint(router, pointer, limits)
                            .await
                            .map(CheckpointData::Layered)
                    } else {
                        load_checkpoint(router, pointer, limits)
                            .await
                            .map(CheckpointData::Complete)
                    }
                }
                CheckpointLoad::Control => {
                    if pointer.format() == 5 {
                        load_layered_checkpoint(router, pointer, limits)
                            .await
                            .map(CheckpointData::Layered)
                    } else {
                        load_checkpoint_control(router, pointer, limits)
                            .await
                            .map(CheckpointData::Control)
                    }
                }
                CheckpointLoad::LayeredControl => {
                    if pointer.format() == 5 {
                        load_layered_checkpoint_control(router, pointer, limits)
                            .await
                            .map(CheckpointData::Layered)
                    } else {
                        load_checkpoint_control(router, pointer, limits)
                            .await
                            .map(CheckpointData::Control)
                    }
                }
            }
            .map(Some),
            None => Ok(None),
        }
    };
    let runs = try_join_all(pointers.iter().map(|pointer| load_run(router, pointer)));
    let (checkpoint, runs) = tokio::try_join!(checkpoint, runs)?;
    let root_transactions = snapshot
        .record()
        .root()
        .capsule_frontier()
        .iter()
        .flat_map(|pointer| pointer.transaction_ids().iter().cloned())
        .collect::<BTreeSet<_>>();
    let runs = runs
        .into_iter()
        .map(|run| (run.hash().to_owned(), run))
        .collect::<BTreeMap<_, _>>();
    let mut capsule_run_sources = Vec::new();
    let mut capsule_run_member_oids = BTreeMap::new();
    let mut frontier_object_admission = BTreeMap::new();
    let mut source_hashes = BTreeSet::new();
    for pointer in &pointers {
        if !source_hashes.insert(pointer.hash().to_owned()) {
            continue;
        }
        let run = runs
            .get(pointer.hash())
            .ok_or_else(|| ReadError::internal("loaded capsule run disappeared"))?;
        if run
            .capsules()
            .iter()
            .any(|capsule| !capsule.git_packs().is_empty())
        {
            let source = PackSourceDescriptor::from_capsule_run(run)?;
            for capsule in run.capsules() {
                if let Some(visibility) = capsule.visibility_delta()? {
                    extend_frontier_object_admission(
                        &mut frontier_object_admission,
                        &visibility,
                        &source,
                        run.admission(),
                    );
                }
            }
            capsule_run_sources.push(source);
            if let Some(member_oids) = member_oids_for_capsule_run(run) {
                capsule_run_member_oids.insert(pointer.hash().to_owned(), member_oids);
            }
        }
    }
    let mut required_transactions = BTreeSet::new();
    let mut transaction_predecessors = BTreeMap::<String, BTreeSet<String>>::new();
    let mut ref_capsule_counts = BTreeMap::new();
    for (ref_name, (checkpoint_transaction_id, frontier)) in &ref_frontiers {
        let transaction_ids = frontier
            .iter()
            .map(|pointer| {
                runs.get(pointer.hash())
                    .ok_or_else(|| ReadError::internal("loaded capsule run disappeared"))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flat_map(|run| run.capsules().iter().map(Capsule::transaction_id))
            .collect::<Vec<_>>();
        let start = match snapshot
            .record()
            .root()
            .compacted_ref_transactions()
            .get(ref_name)
        {
            Some(compacted) => match transaction_ids
                .iter()
                .position(|transaction_id| *transaction_id == compacted)
            {
                Some(index) => index + 1,
                None if checkpoint_transaction_id.as_deref() == Some(compacted.as_str()) => 0,
                None => {
                    return Err(corrupt_path(
                        "capsule-protocol ref heads",
                        format!("ref {ref_name} does not extend its compacted transaction"),
                    ));
                }
            },
            None if checkpoint_transaction_id.is_none() => 0,
            None => {
                return Err(corrupt_path(
                    "capsule-protocol ref heads",
                    format!("ref {ref_name} names a checkpoint absent from the repository root"),
                ));
            }
        };
        let required = &transaction_ids[start..];
        ref_capsule_counts.insert(
            ref_name.clone(),
            u32::try_from(required.len())
                .map_err(|_| ReadError::internal("ref capsule count overflowed"))?,
        );
        required_transactions.extend(
            required
                .iter()
                .map(|transaction_id| (*transaction_id).to_owned()),
        );
        // Expected-old OIDs cannot order A -> B -> A histories. Preserve the
        // authenticated per-ref frontier order so a later A successor waits.
        for pair in required.windows(2) {
            transaction_predecessors
                .entry(pair[1].to_owned())
                .or_default()
                .insert(pair[0].to_owned());
        }
    }
    let mut root_capsules = Vec::new();
    let mut root_capsule_identities = BTreeMap::new();
    for pointer in snapshot.record().root().capsule_frontier() {
        let run = runs
            .get(pointer.hash())
            .ok_or_else(|| ReadError::internal("loaded root capsule run disappeared"))?;
        for capsule in run.capsules() {
            match root_capsule_identities.get(capsule.transaction_id()) {
                Some(existing) if existing != capsule => {
                    return Err(corrupt_path(
                        "capsule-protocol root frontier",
                        "transaction identity names conflicting capsules",
                    ));
                }
                Some(_) => {}
                None => {
                    root_capsule_identities
                        .insert(capsule.transaction_id().to_owned(), capsule.clone());
                    root_capsules.push(capsule.clone());
                }
            }
        }
    }
    let mut journal_capsules = BTreeMap::new();
    for capsule in runs.values().flat_map(|run| run.capsules().iter().cloned()) {
        if root_transactions.contains(capsule.transaction_id()) {
            continue;
        }
        if !required_transactions.contains(capsule.transaction_id()) {
            continue;
        }
        match journal_capsules.get(capsule.transaction_id()) {
            Some(existing) if existing != &capsule => {
                return Err(corrupt_path(
                    "capsule-protocol ref heads",
                    "transaction identity names conflicting capsules",
                ));
            }
            Some(_) => {}
            None => {
                journal_capsules.insert(capsule.transaction_id().to_owned(), capsule);
            }
        }
    }
    let mut visibility_refs = match checkpoint.as_ref() {
        Some(CheckpointData::Complete(checkpoint)) => checkpoint
            .visibility_snapshot()?
            .map(|visibility| visibility.refs().clone())
            .unwrap_or_default(),
        Some(CheckpointData::Control(control)) => control
            .visibility_snapshot()?
            .map(|visibility| visibility.refs().clone())
            .unwrap_or_default(),
        Some(CheckpointData::Layered(checkpoint)) => {
            if let Some(visibility) = checkpoint.visibility_ordinal_snapshot()? {
                let catalog_digest =
                    crab_metadata::capsule_protocol::source_catalog_digest(checkpoint.sources())?;
                visibility
                    .to_index(0, &"0".repeat(64), &"0".repeat(64), &catalog_digest)?
                    .ref_closures()
            } else {
                checkpoint
                    .visibility_snapshot()?
                    .map(|visibility| visibility.refs().clone())
                    .unwrap_or_default()
            }
        }
        None => BTreeMap::new(),
    };
    for capsule in &root_capsules {
        if capsule.visibility_delta()?.is_some() {
            apply_capsule_visibility(capsule, &mut visibility_refs)?;
        }
    }
    let ordered = order_ref_capsules(
        snapshot.record().root().refs(),
        snapshot.record().root().peeled_refs(),
        &visibility_refs,
        &transaction_predecessors,
        journal_capsules,
    )?;
    for capsule in &ordered {
        apply_capsule_refs(capsule, &mut refs, &mut peeled_refs)?;
    }
    if refs != expected_refs || peeled_refs != expected_peeled {
        return Err(corrupt_path(
            "capsule-protocol ref heads",
            "materialized capsules do not match visible ref-head state",
        ));
    }
    root_capsules.extend(ordered);
    let tip_bound_transitions = build_tip_bound_transitions(&root_capsules, &[])?;
    Ok(CapsuleRepositoryView {
        root: snapshot,
        checkpoint,
        capsules: root_capsules,
        capsule_controls: Vec::new(),
        tip_bound_transitions,
        refs,
        peeled_refs,
        visible_ref_transactions,
        ref_capsule_counts,
        capsule_run_pointers: pointers,
        capsule_run_sources,
        capsule_run_member_oids,
        frontier_object_admission,
    })
}

async fn assemble_layered_control_view(
    router: &StoreLayout<Store>,
    snapshot: crab_metadata::capsule_protocol::RootSnapshot,
    limits: CapsuleReadLimits,
    heads: Vec<crab_metadata::capsule_protocol::CapsuleRefHead>,
    active: BTreeSet<String>,
    footer_only: bool,
) -> Result<CapsuleRepositoryView> {
    let mut refs = snapshot.record().root().refs().clone();
    let mut peeled_refs = snapshot.record().root().peeled_refs().clone();
    let visible = materialize_visible_ref_heads(snapshot.record().root(), &heads, &active)?;
    let expected_refs = visible.refs;
    let expected_peeled = visible.peeled_refs;
    let visible_ref_transactions = visible.transactions;
    let ref_frontiers = visible.frontiers;
    let pointers = visible.pointers;
    admit_frontier(&pointers, limits)?;
    let checkpoint = match snapshot.record().root().checkpoint() {
        Some(pointer) => {
            let checkpoint = if footer_only {
                load_layered_checkpoint_control(router, pointer, limits).await?
            } else {
                load_layered_checkpoint(router, pointer, limits).await?
            };
            Some(CheckpointData::Layered(checkpoint))
        }
        None => None,
    };
    let loaded = try_join_all(
        pointers
            .iter()
            .map(|pointer| load_run_control(router, pointer)),
    )
    .await?;
    let runs = loaded
        .into_iter()
        .map(|(control, capsules)| {
            let hash = control.hash().to_owned();
            // Ref-only runs have no pack source; validate the source boundary
            // only when this run actually contributes Git members.
            if !control.git_packs().is_empty() {
                control.source_descriptor()?;
            }
            Ok((hash, control, capsules))
        })
        .collect::<Result<Vec<_>>>()?;
    let runs = runs
        .into_iter()
        .map(|(hash, control, capsules)| (hash, (control, capsules)))
        .collect::<BTreeMap<_, _>>();
    let all_capsule_controls = runs
        .values()
        .flat_map(|(_, capsules)| capsules.iter().cloned())
        .collect::<Vec<_>>();
    let mut tip_bound_transitions = BTreeMap::new();
    if let Some(CheckpointData::Layered(checkpoint)) = checkpoint.as_ref() {
        append_checkpoint_tip_bound_transitions(
            &mut tip_bound_transitions,
            checkpoint.visibility_transitions(),
        )?;
    }
    for (ref_name, transitions) in build_tip_bound_transitions(&[], &all_capsule_controls)? {
        tip_bound_transitions
            .entry(ref_name)
            .or_default()
            .extend(transitions);
    }
    let mut capsule_run_sources = Vec::new();
    let mut capsule_run_member_oids = BTreeMap::new();
    let mut frontier_object_admission = BTreeMap::new();
    let mut source_hashes = BTreeSet::new();
    for pointer in &pointers {
        if !source_hashes.insert(pointer.hash().to_owned()) {
            continue;
        }
        let (run, capsules) = runs
            .get(pointer.hash())
            .ok_or_else(|| ReadError::internal("loaded capsule run control disappeared"))?;
        if !run.git_packs().is_empty() {
            let source = run.source_descriptor()?;
            if let Some(admission) = run.admission() {
                let mut member_oids = vec![Vec::new(); run.git_packs().len()];
                for (oid, members) in admission.entries() {
                    for member in members {
                        let member_index = usize::try_from(*member).map_err(|_| {
                            corrupt_path(
                                "capsule run admission",
                                "member ordinal cannot be represented",
                            )
                        })?;
                        let Some(object_ids) = member_oids.get_mut(member_index) else {
                            return Err(corrupt_path(
                                "capsule run admission",
                                "member ordinal is out of bounds",
                            ));
                        };
                        object_ids.push(*oid);
                    }
                }
                capsule_run_member_oids.insert(pointer.hash().to_owned(), member_oids);
            }
            for capsule in capsules {
                if let Some(visibility) = capsule.visibility_delta() {
                    extend_frontier_object_admission(
                        &mut frontier_object_admission,
                        visibility,
                        &source,
                        run.admission(),
                    );
                }
            }
            capsule_run_sources.push(source);
        }
    }
    let root_transactions = snapshot
        .record()
        .root()
        .capsule_frontier()
        .iter()
        .flat_map(|pointer| pointer.transaction_ids().iter().cloned())
        .collect::<BTreeSet<_>>();
    let mut required_transactions = BTreeSet::new();
    let mut transaction_predecessors = BTreeMap::<String, BTreeSet<String>>::new();
    let mut ref_capsule_counts = BTreeMap::new();
    for (ref_name, (checkpoint_transaction_id, frontier)) in &ref_frontiers {
        let transaction_ids = frontier
            .iter()
            .map(|pointer| {
                runs.get(pointer.hash())
                    .ok_or_else(|| ReadError::internal("loaded capsule run control disappeared"))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flat_map(|(_, capsules)| capsules.iter().map(|capsule| capsule.transaction_id()))
            .collect::<Vec<_>>();
        let start = match snapshot
            .record()
            .root()
            .compacted_ref_transactions()
            .get(ref_name)
        {
            Some(compacted) => match transaction_ids
                .iter()
                .position(|transaction_id| *transaction_id == compacted)
            {
                Some(index) => index + 1,
                None if checkpoint_transaction_id.as_deref() == Some(compacted.as_str()) => 0,
                None => {
                    return Err(corrupt_path(
                        "capsule-protocol ref heads",
                        format!("ref {ref_name} does not extend its compacted transaction"),
                    ));
                }
            },
            None if checkpoint_transaction_id.is_none() => 0,
            None => {
                return Err(corrupt_path(
                    "capsule-protocol ref heads",
                    format!("ref {ref_name} names a checkpoint absent from the repository root"),
                ));
            }
        };
        let required = &transaction_ids[start..];
        ref_capsule_counts.insert(
            ref_name.clone(),
            u32::try_from(required.len())
                .map_err(|_| ReadError::internal("ref capsule count overflowed"))?,
        );
        required_transactions.extend(
            required
                .iter()
                .map(|transaction_id| (*transaction_id).to_owned()),
        );
        for pair in required.windows(2) {
            transaction_predecessors
                .entry(pair[1].to_owned())
                .or_default()
                .insert(pair[0].to_owned());
        }
    }
    let mut root_capsules = Vec::new();
    let mut root_capsule_identities = BTreeMap::new();
    for pointer in snapshot.record().root().capsule_frontier() {
        let (_, capsules) = runs
            .get(pointer.hash())
            .ok_or_else(|| ReadError::internal("loaded root capsule run control disappeared"))?;
        for capsule in capsules {
            match root_capsule_identities.get(capsule.transaction_id()) {
                Some(existing) if existing != capsule => {
                    return Err(corrupt_path(
                        "capsule-protocol root frontier",
                        "transaction identity names conflicting capsules",
                    ));
                }
                Some(_) => {}
                None => {
                    root_capsule_identities
                        .insert(capsule.transaction_id().to_owned(), capsule.clone());
                    root_capsules.push(capsule.clone());
                }
            }
        }
    }
    if footer_only {
        return Ok(CapsuleRepositoryView {
            root: snapshot,
            checkpoint,
            capsules: Vec::new(),
            capsule_controls: Vec::new(),
            tip_bound_transitions,
            refs: expected_refs,
            peeled_refs: expected_peeled,
            visible_ref_transactions,
            ref_capsule_counts,
            capsule_run_pointers: pointers,
            capsule_run_sources,
            capsule_run_member_oids,
            frontier_object_admission,
        });
    }
    let mut journal_capsules = BTreeMap::new();
    for (_, capsules) in runs.values() {
        for capsule in capsules {
            if root_transactions.contains(capsule.transaction_id())
                || !required_transactions.contains(capsule.transaction_id())
            {
                continue;
            }
            match journal_capsules.get(capsule.transaction_id()) {
                Some(existing) if existing != capsule => {
                    return Err(corrupt_path(
                        "capsule-protocol ref heads",
                        "transaction identity names conflicting capsules",
                    ));
                }
                Some(_) => {}
                None => {
                    journal_capsules.insert(capsule.transaction_id().to_owned(), capsule.clone());
                }
            }
        }
    }
    let mut visibility_refs = match checkpoint.as_ref() {
        Some(CheckpointData::Layered(checkpoint)) => {
            if let Some(visibility) = checkpoint.visibility_ordinal_snapshot()? {
                let catalog_digest =
                    crab_metadata::capsule_protocol::source_catalog_digest(checkpoint.sources())?;
                visibility
                    .to_index(0, &"0".repeat(64), &"0".repeat(64), &catalog_digest)?
                    .ref_closures()
            } else {
                checkpoint
                    .visibility_snapshot()?
                    .map(|visibility| visibility.refs().clone())
                    .unwrap_or_default()
            }
        }
        _ => BTreeMap::new(),
    };
    for capsule in &root_capsules {
        if capsule.visibility_delta().is_some() {
            apply_capsule_control_visibility(capsule, &mut visibility_refs)?;
        }
    }
    let ordered = order_ref_controls(
        snapshot.record().root().refs(),
        snapshot.record().root().peeled_refs(),
        &visibility_refs,
        &transaction_predecessors,
        journal_capsules,
    )?;
    for capsule in &ordered {
        apply_control_refs(capsule, &mut refs, &mut peeled_refs)?;
    }
    if refs != expected_refs || peeled_refs != expected_peeled {
        return Err(corrupt_path(
            "capsule-protocol ref heads",
            "materialized capsules do not match visible ref-head state",
        ));
    }
    root_capsules.extend(ordered);
    Ok(CapsuleRepositoryView {
        root: snapshot,
        checkpoint,
        capsules: Vec::new(),
        capsule_controls: root_capsules,
        tip_bound_transitions,
        refs,
        peeled_refs,
        visible_ref_transactions,
        ref_capsule_counts,
        capsule_run_pointers: pointers,
        capsule_run_sources,
        capsule_run_member_oids,
        frontier_object_admission,
    })
}

fn member_oids_for_capsule_run(run: &CapsuleRun) -> Option<Vec<Vec<[u8; 20]>>> {
    let mut members = Vec::new();
    for capsule in run.capsules() {
        for descriptor in capsule.git_packs() {
            let index = capsule.section_bytes(descriptor.index_section()).ok()?;
            let (object_ids, pack_checksum) =
                crab_git::pack_locator::sorted_object_ids_from_index_bytes(&index).ok()?;
            if object_ids.len() as u64 != descriptor.object_count()
                || pack_checksum.to_string() != descriptor.git_checksum()
            {
                return None;
            }
            members.push(
                object_ids
                    .into_iter()
                    .map(|object_id| object_id.as_bytes().try_into().ok())
                    .collect::<Option<Vec<_>>>()?,
            );
        }
    }
    Some(members)
}

fn extend_frontier_object_admission(
    admission: &mut BTreeMap<[u8; 20], Vec<String>>,
    visibility: &crab_metadata::capsule_protocol::CapsuleVisibilityDelta,
    source: &PackSourceDescriptor,
    run_admission: Option<&crab_metadata::capsule_protocol::CapsuleRunAdmission>,
) {
    let all_pack_ids = source
        .members()
        .iter()
        .map(|member| member.pack().blake3().to_owned())
        .collect::<Vec<_>>();
    let mut all_pack_ids = all_pack_ids;
    all_pack_ids.sort_unstable();
    all_pack_ids.dedup();
    if all_pack_ids.is_empty() {
        return;
    }
    for edit in visibility.edits().values() {
        for oid in &edit.added {
            let Ok(oid) = gix_hash::ObjectId::from_hex(oid.as_bytes()) else {
                continue;
            };
            let Ok(oid) = oid.as_bytes().try_into() else {
                continue;
            };
            let entry = admission.entry(oid).or_default();
            let pack_ids = run_admission
                .and_then(|run_admission| run_admission.object_members(&oid))
                .map(|members| {
                    members
                        .iter()
                        .filter_map(|member| {
                            source
                                .members()
                                .get(usize::try_from(*member).ok()?)
                                .map(|member| member.pack().blake3().to_owned())
                        })
                        .collect::<Vec<_>>()
                })
                .filter(|pack_ids| !pack_ids.is_empty())
                .unwrap_or_else(|| all_pack_ids.clone());
            for pack_id in &pack_ids {
                if !entry.iter().any(|candidate| candidate == pack_id) {
                    entry.push(pack_id.clone());
                }
            }
        }
    }
}

struct VisibleRefHeads {
    refs: BTreeMap<String, String>,
    peeled_refs: BTreeMap<String, String>,
    transactions: BTreeMap<String, String>,
    frontiers: BTreeMap<String, (Option<String>, Vec<CapsulePointer>)>,
    pointers: Vec<CapsulePointer>,
}

fn state_digest(root: &RootRecord, transactions: &BTreeMap<String, String>) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab capsule repository view v2\0");
    hasher.update(root.digest().as_bytes());
    for (ref_name, transaction_id) in transactions {
        hasher.update(ref_name.as_bytes());
        hasher.update(&[0]);
        hasher.update(transaction_id.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn materialize_visible_ref_heads(
    root: &crab_metadata::capsule_protocol::RepositoryRoot,
    heads: &[crab_metadata::capsule_protocol::CapsuleRefHead],
    active: &BTreeSet<String>,
) -> Result<VisibleRefHeads> {
    let mut refs = root.refs().clone();
    let mut peeled_refs = root.peeled_refs().clone();
    let mut transactions = BTreeMap::new();
    let mut frontiers = BTreeMap::new();
    let mut pointers = root.capsule_frontier().to_vec();
    for head in heads {
        let state = head.visible(active);
        if let Some(transaction_id) = state.transaction_id() {
            transactions.insert(head.ref_name().to_owned(), transaction_id.to_owned());
        }
        if state.transaction_id()
            == root
                .compacted_ref_transactions()
                .get(head.ref_name())
                .map(String::as_str)
        {
            continue;
        }
        match state.oid() {
            Some(oid) => {
                refs.insert(head.ref_name().to_owned(), oid.to_owned());
                match state.peeled_oid() {
                    Some(peeled) => {
                        peeled_refs.insert(head.ref_name().to_owned(), peeled.to_owned());
                    }
                    None => {
                        peeled_refs.remove(head.ref_name());
                    }
                }
            }
            None => {
                refs.remove(head.ref_name());
                peeled_refs.remove(head.ref_name());
            }
        }
        for pointer in state.frontier() {
            match pointers
                .iter()
                .find(|candidate| candidate.hash() == pointer.hash())
            {
                Some(candidate) if candidate != pointer => {
                    return Err(corrupt_path(
                        "capsule-protocol ref heads",
                        "capsule run identity has conflicting authenticated metadata",
                    ));
                }
                Some(_) => {}
                None => pointers.push(pointer.clone()),
            }
        }
        frontiers.insert(
            head.ref_name().to_owned(),
            (
                state.checkpoint_transaction_id().map(str::to_owned),
                state.frontier().to_vec(),
            ),
        );
    }
    Ok(VisibleRefHeads {
        refs,
        peeled_refs,
        transactions,
        frontiers,
        pointers,
    })
}

async fn load_ref_heads(
    router: &StoreLayout<Store>,
    objects: &[ObjectMeta],
) -> Result<Option<Vec<crab_metadata::capsule_protocol::CapsuleRefHead>>> {
    let loaded = futures_util::stream::iter(objects.iter().cloned().map(|object| async move {
        let (body, etag) = router.store().get_with_etag(&object.location).await?;
        let head = crab_metadata::capsule_protocol::CapsuleRefHead::decode(&body)?;
        let expected = router.capsule_ref_head_path(
            &crab_metadata::capsule_protocol::capsule_ref_name_key(head.ref_name()),
        );
        if expected != object.location {
            return Err(corrupt(
                &object.location,
                "capsule ref-head key does not match its ref name",
            ));
        }
        Ok::<_, ReadError>((head, listed_version_matches(&object, &etag)))
    }))
    .buffer_unordered(32)
    .try_collect::<Vec<_>>()
    .await?;
    if loaded.iter().any(|(_, matched)| !matched) {
        return Ok(None);
    }
    let mut heads = loaded.into_iter().map(|(head, _)| head).collect::<Vec<_>>();
    heads.sort_unstable_by(|left, right| left.ref_name().cmp(right.ref_name()));
    Ok(Some(heads))
}

async fn capture_ref_heads(
    router: &StoreLayout<Store>,
    root: &crab_metadata::capsule_protocol::RepositoryRoot,
) -> Result<(
    Vec<crab_metadata::capsule_protocol::CapsuleRefHead>,
    BTreeSet<String>,
)> {
    for _ in 0..8 {
        let before = list_ref_head_objects(router).await?;
        let Some(heads) = load_ref_heads(router, &before).await? else {
            continue;
        };
        let heads = heads
            .into_iter()
            .filter(|head| head.ref_epoch() == root.ref_epoch())
            .collect::<Vec<_>>();
        let active = resolve_referenced_activations(router, &heads).await?;
        let after = list_ref_head_objects(router).await?;
        if before != after {
            continue;
        }
        for head in &heads {
            let visible = head.visible(&active);
            let base_oid = root.refs().get(head.ref_name()).map(String::as_str);
            if visible.transaction_id().is_none() && visible.oid() != base_oid {
                return Err(corrupt_path(
                    "capsule-protocol ref heads",
                    "capsule ref head without a transaction differs from the compacted root",
                ));
            }
        }
        return Ok((heads, active));
    }
    Err(ReadError::internal(
        "capsule ref snapshot changed during every bounded capture attempt",
    ))
}

async fn capture_selected_ref_heads(
    router: &StoreLayout<Store>,
    root: &crab_metadata::capsule_protocol::RepositoryRoot,
    ref_names: &BTreeSet<String>,
) -> Result<(
    Vec<crab_metadata::capsule_protocol::CapsuleRefHead>,
    BTreeSet<String>,
)> {
    for _ in 0..8 {
        let before = load_selected_ref_heads(router, ref_names).await?;
        let heads = before
            .iter()
            .filter_map(|entry| entry.as_ref().map(|(head, _)| head.clone()))
            .filter(|head| head.ref_epoch() == root.ref_epoch())
            .collect::<Vec<_>>();
        let active = resolve_referenced_activations(router, &heads).await?;
        let after = load_selected_ref_heads(router, ref_names).await?;
        if before != after {
            continue;
        }
        for head in &heads {
            let visible = head.visible(&active);
            let base_oid = root.refs().get(head.ref_name()).map(String::as_str);
            if visible.transaction_id().is_none() && visible.oid() != base_oid {
                return Err(corrupt_path(
                    "capsule-protocol ref heads",
                    "capsule ref head without a transaction differs from the compacted root",
                ));
            }
        }
        return Ok((heads, active));
    }
    Err(ReadError::internal(
        "selected capsule ref heads changed during every bounded capture attempt",
    ))
}

async fn load_selected_ref_heads(
    router: &StoreLayout<Store>,
    ref_names: &BTreeSet<String>,
) -> Result<
    Vec<
        Option<(
            crab_metadata::capsule_protocol::CapsuleRefHead,
            crab_storage::ETag,
        )>,
    >,
> {
    futures_util::stream::iter(ref_names.iter().map(|ref_name| async move {
        let path = router.capsule_ref_head_path(
            &crab_metadata::capsule_protocol::capsule_ref_name_key(ref_name),
        );
        let (body, etag) = match router.store().get_with_etag(&path).await {
            Ok(value) => value,
            Err(crab_storage::StorageError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(ReadError::from(error)),
        };
        let head = crab_metadata::capsule_protocol::CapsuleRefHead::decode(&body)?;
        if head.ref_name() != ref_name {
            return Err(corrupt(
                &path,
                "capsule ref-head key does not match its ref name",
            ));
        }
        Ok(Some((head, etag)))
    }))
    .buffered(32)
    .try_collect()
    .await
}

async fn list_ref_head_objects(router: &StoreLayout<Store>) -> Result<Vec<ObjectMeta>> {
    let mut objects = router
        .store()
        .list_prefix_bounded(
            &router.capsule_ref_heads_prefix(),
            crab_metadata::capsule_protocol::MAX_CAPSULE_REF_HEADS,
        )
        .await?
        .ok_or_else(|| ReadError::internal("capsule ref-head limit exceeded"))?;
    objects.sort_unstable_by(|left, right| left.location.cmp(&right.location));
    Ok(objects)
}

fn listed_version_matches(object: &ObjectMeta, etag: &crab_storage::ETag) -> bool {
    object
        .e_tag
        .as_ref()
        .is_none_or(|listed| etag.e_tag.as_ref() == Some(listed))
        && object
            .version
            .as_ref()
            .is_none_or(|listed| etag.version.as_ref() == Some(listed))
}

async fn resolve_referenced_activations(
    router: &StoreLayout<Store>,
    heads: &[crab_metadata::capsule_protocol::CapsuleRefHead],
) -> Result<BTreeSet<String>> {
    let referenced = heads
        .iter()
        .filter_map(|head| head.prepared_activation_id())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    futures_util::stream::iter(referenced.into_iter().map(|activation_id| async move {
        let path = router.capsule_transaction_path(&activation_id);
        let (body, _) = router
            .store()
            .get_with_etag_bounded(
                &path,
                crab_metadata::capsule_protocol::MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
            )
            .await?;
        let record = crab_metadata::capsule_protocol::CapsuleTransactionRecord::decode(&body)?;
        if record.activation_id() != activation_id {
            return Err(corrupt(
                &path,
                "capsule transaction record does not match its activation key",
            ));
        }
        if heads.iter().any(|head| {
            head.prepared_activation_id() == Some(activation_id.as_str())
                && head
                    .visible(&BTreeSet::from([activation_id.clone()]))
                    .transaction_id()
                    != Some(record.transaction_id())
        }) {
            return Err(corrupt(
                &path,
                "capsule transaction record does not match its prepared ref heads",
            ));
        }
        Ok::<_, ReadError>((
            activation_id,
            record.status() == crab_metadata::capsule_protocol::CapsuleTransactionStatus::Committed,
        ))
    }))
    .buffer_unordered(32)
    .try_filter_map(
        |(activation_id, committed)| async move { Ok(committed.then_some(activation_id)) },
    )
    .try_collect()
    .await
}

fn order_ref_capsules(
    base_refs: &BTreeMap<String, String>,
    base_peeled: &BTreeMap<String, String>,
    base_visibility: &BTreeMap<String, Vec<String>>,
    predecessors: &BTreeMap<String, BTreeSet<String>>,
    mut pending: BTreeMap<String, Capsule>,
) -> Result<Vec<Capsule>> {
    let mut refs = base_refs.clone();
    let mut peeled = base_peeled.clone();
    let mut visibility = base_visibility.clone();
    let tracked = pending
        .values()
        .map(|capsule| capsule.transaction_id().to_owned())
        .collect::<BTreeSet<_>>();
    let mut applied = BTreeSet::new();
    let mut ordered = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let ready = pending
            .iter()
            .find_map(|(id, capsule)| {
                if predecessors
                    .get(capsule.transaction_id())
                    .is_some_and(|required| {
                        required.iter().any(|transaction_id| {
                            tracked.contains(transaction_id) && !applied.contains(transaction_id)
                        })
                    })
                {
                    return None;
                }
                match capsule_is_ready(capsule, &refs, &visibility) {
                    Ok(true) => Some(Ok(id.clone())),
                    Ok(false) => None,
                    Err(error) => Some(Err(error)),
                }
            })
            .transpose()?;
        let Some(ready) = ready else {
            return Err(corrupt_path(
                "capsule-protocol ref heads",
                "capsule ref history is cyclic or does not extend the compacted root",
            ));
        };
        let capsule = pending
            .remove(&ready)
            .ok_or_else(|| ReadError::internal("ready capsule disappeared"))?;
        apply_capsule_refs(&capsule, &mut refs, &mut peeled)?;
        if capsule.visibility_delta()?.is_some() {
            apply_capsule_visibility(&capsule, &mut visibility)?;
        }
        applied.insert(capsule.transaction_id().to_owned());
        ordered.push(capsule);
    }
    Ok(ordered)
}

fn order_ref_controls(
    base_refs: &BTreeMap<String, String>,
    base_peeled: &BTreeMap<String, String>,
    base_visibility: &BTreeMap<String, Vec<String>>,
    predecessors: &BTreeMap<String, BTreeSet<String>>,
    mut pending: BTreeMap<String, CapsuleControl>,
) -> Result<Vec<CapsuleControl>> {
    let mut refs = base_refs.clone();
    let mut peeled = base_peeled.clone();
    let mut visibility = base_visibility.clone();
    let tracked = pending.keys().cloned().collect::<BTreeSet<_>>();
    let mut applied = BTreeSet::new();
    let mut ordered = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let ready = pending
            .iter()
            .find_map(|(id, capsule)| {
                if predecessors
                    .get(capsule.transaction_id())
                    .is_some_and(|required| {
                        required.iter().any(|transaction_id| {
                            tracked.contains(transaction_id) && !applied.contains(transaction_id)
                        })
                    })
                {
                    return None;
                }
                match control_is_ready(capsule, &refs, &visibility) {
                    Ok(true) => Some(Ok(id.clone())),
                    Ok(false) => None,
                    Err(error) => Some(Err(error)),
                }
            })
            .transpose()?;
        let Some(ready) = ready else {
            return Err(corrupt_path(
                "capsule-protocol ref heads",
                "capsule ref history is cyclic or does not extend the compacted root",
            ));
        };
        let capsule = pending
            .remove(&ready)
            .ok_or_else(|| ReadError::internal("ready capsule control disappeared"))?;
        apply_control_refs(&capsule, &mut refs, &mut peeled)?;
        if capsule.visibility_delta().is_some() {
            apply_capsule_control_visibility(&capsule, &mut visibility)?;
        }
        applied.insert(capsule.transaction_id().to_owned());
        ordered.push(capsule);
    }
    Ok(ordered)
}

fn control_is_ready(
    capsule: &CapsuleControl,
    refs: &BTreeMap<String, String>,
    visibility_refs: &BTreeMap<String, Vec<String>>,
) -> Result<bool> {
    let transaction = capsule.transaction();
    if transaction
        .edits()
        .iter()
        .any(|edit| refs.get(edit.ref_name()).map(String::as_str) != edit.expected_old())
    {
        return Ok(false);
    }
    let Some(delta) = capsule.visibility_delta() else {
        return Ok(true);
    };
    let mut evidence = delta.edits().clone();
    for edit in transaction.edits() {
        let Some(new_oid) = edit.new_oid() else {
            if evidence.remove(edit.ref_name()).is_some() {
                return Err(corrupt_path(
                    "capsule Git visibility",
                    "deleted ref has visibility evidence",
                ));
            }
            continue;
        };
        let Some(visibility) = evidence.remove(edit.ref_name()) else {
            return Err(corrupt_path(
                "capsule Git visibility",
                "live ref edit has no visibility evidence",
            ));
        };
        visibility.validate()?;
        if visibility.new_oid != new_oid {
            return Err(corrupt_path(
                "capsule Git visibility",
                "visibility evidence does not match its ref edit",
            ));
        }
        match edit.expected_old() {
            Some(expected_old) => {
                if visibility.old_oid.as_deref() != Some(expected_old) {
                    return Err(corrupt_path(
                        "capsule Git visibility",
                        "visibility evidence does not match the expected old ref",
                    ));
                }
                if !visibility_refs.contains_key(edit.ref_name()) {
                    return Ok(false);
                }
            }
            None if visibility.replaces => {}
            None => {
                let Some(old_oid) = visibility.old_oid.as_deref() else {
                    continue;
                };
                if !visibility_refs.values().any(|objects| {
                    objects
                        .binary_search_by(|oid| oid.as_str().cmp(old_oid))
                        .is_ok()
                }) {
                    return Ok(false);
                }
            }
        }
    }
    if !evidence.is_empty() {
        return Err(corrupt_path(
            "capsule Git visibility",
            "visibility evidence contains an uncommitted ref",
        ));
    }
    Ok(true)
}

fn apply_control_refs(
    capsule: &CapsuleControl,
    refs: &mut BTreeMap<String, String>,
    peeled_refs: &mut BTreeMap<String, String>,
) -> Result<()> {
    for edit in capsule.transaction().edits() {
        if refs.get(edit.ref_name()).map(String::as_str) != edit.expected_old() {
            return Err(corrupt_path(
                "capsule-protocol ref heads",
                "capsule expected-old ref does not match its parent state",
            ));
        }
        match edit.new_oid() {
            Some(oid) => {
                refs.insert(edit.ref_name().to_owned(), oid.to_owned());
                match edit.peeled_oid() {
                    Some(peeled) => {
                        peeled_refs.insert(edit.ref_name().to_owned(), peeled.to_owned());
                    }
                    None => {
                        peeled_refs.remove(edit.ref_name());
                    }
                }
            }
            None => {
                refs.remove(edit.ref_name());
                peeled_refs.remove(edit.ref_name());
            }
        }
    }
    Ok(())
}

fn capsule_is_ready(
    capsule: &Capsule,
    refs: &BTreeMap<String, String>,
    visibility_refs: &BTreeMap<String, Vec<String>>,
) -> Result<bool> {
    let transaction = capsule.transaction()?;
    if transaction
        .edits()
        .iter()
        .any(|edit| refs.get(edit.ref_name()).map(String::as_str) != edit.expected_old())
    {
        return Ok(false);
    }
    let Some(delta) = capsule.visibility_delta()? else {
        // Ref ordering is authenticated by expected-old links. Visibility is a
        // separate read-admission proof and remains fail-closed when requested.
        return Ok(true);
    };
    let mut evidence = delta.edits().clone();
    for edit in transaction.edits() {
        let Some(new_oid) = edit.new_oid() else {
            if evidence.remove(edit.ref_name()).is_some() {
                return Err(corrupt_path(
                    "capsule Git visibility",
                    "deleted ref has visibility evidence",
                ));
            }
            continue;
        };
        let Some(visibility) = evidence.remove(edit.ref_name()) else {
            return Err(corrupt_path(
                "capsule Git visibility",
                "live ref edit has no visibility evidence",
            ));
        };
        visibility.validate()?;
        if visibility.new_oid != new_oid {
            return Err(corrupt_path(
                "capsule Git visibility",
                "visibility evidence does not match its ref edit",
            ));
        }
        match edit.expected_old() {
            Some(expected_old) => {
                if visibility.old_oid.as_deref() != Some(expected_old) {
                    return Err(corrupt_path(
                        "capsule Git visibility",
                        "visibility evidence does not match the expected old ref",
                    ));
                }
                if !visibility_refs.contains_key(edit.ref_name()) {
                    return Ok(false);
                }
            }
            None if visibility.replaces => {}
            None => {
                let Some(old_oid) = visibility.old_oid.as_deref() else {
                    continue;
                };
                if !visibility_refs.values().any(|objects| {
                    objects
                        .binary_search_by(|oid| oid.as_str().cmp(old_oid))
                        .is_ok()
                }) {
                    return Ok(false);
                }
            }
        }
    }
    if !evidence.is_empty() {
        return Err(corrupt_path(
            "capsule Git visibility",
            "visibility evidence contains an uncommitted ref",
        ));
    }
    Ok(true)
}

fn apply_capsule_visibility(
    capsule: &Capsule,
    refs: &mut BTreeMap<String, Vec<String>>,
) -> Result<()> {
    let pack_index_hash = if refs.is_empty() {
        String::new()
    } else {
        "0".repeat(64)
    };
    let mut index = crab_metadata::git_visibility::GitVisibilityIndex::new(
        0,
        pack_index_hash,
        "0".repeat(64),
        std::mem::take(refs),
    )?;
    apply_capsule_visibility_index(capsule, &mut index)?;
    *refs = index.ref_closures();
    Ok(())
}

fn apply_capsule_control_visibility(
    capsule: &CapsuleControl,
    refs: &mut BTreeMap<String, Vec<String>>,
) -> Result<()> {
    let pack_index_hash = if refs.is_empty() {
        String::new()
    } else {
        "0".repeat(64)
    };
    let mut index = crab_metadata::git_visibility::GitVisibilityIndex::new(
        0,
        pack_index_hash,
        "0".repeat(64),
        std::mem::take(refs),
    )?;
    let transaction = capsule.transaction();
    let delta = capsule.visibility_delta();
    let mut evidence = delta.map(|delta| delta.edits().clone()).unwrap_or_default();
    for edit in transaction.edits() {
        let Some(new_oid) = edit.new_oid() else {
            if evidence.remove(edit.ref_name()).is_some() {
                return Err(corrupt_path(
                    "capsule Git visibility",
                    "deleted ref has visibility evidence",
                ));
            }
            index.remove_ref(edit.ref_name());
            continue;
        };
        let visibility = evidence.remove(edit.ref_name()).ok_or_else(|| {
            corrupt_path(
                "capsule Git visibility",
                "live ref edit has no visibility evidence",
            )
        })?;
        if visibility.new_oid != new_oid {
            return Err(corrupt_path(
                "capsule Git visibility",
                "visibility evidence does not match its ref edit",
            ));
        }
        if let Some(expected_old) = edit.expected_old()
            && visibility.old_oid.as_deref() != Some(expected_old)
        {
            return Err(corrupt_path(
                "capsule Git visibility",
                "visibility evidence does not match the expected old ref",
            ));
        }
        index.apply_ref_edit(edit.ref_name().to_owned(), &visibility)?;
    }
    if !evidence.is_empty() {
        return Err(corrupt_path(
            "capsule Git visibility",
            "visibility evidence contains an uncommitted ref",
        ));
    }
    *refs = index.ref_closures();
    Ok(())
}

fn apply_capsule_control_visibility_index(
    capsule: &CapsuleControl,
    index: &mut crab_metadata::git_visibility::GitVisibilityIndex,
) -> Result<()> {
    let transaction = capsule.transaction();
    let delta = capsule.visibility_delta();
    let mut evidence = delta.map(|delta| delta.edits().clone()).unwrap_or_default();
    for edit in transaction.edits() {
        let Some(new_oid) = edit.new_oid() else {
            if evidence.remove(edit.ref_name()).is_some() {
                return Err(corrupt_path(
                    "capsule Git visibility",
                    "deleted ref has visibility evidence",
                ));
            }
            index.remove_ref(edit.ref_name());
            continue;
        };
        let visibility = evidence.remove(edit.ref_name()).ok_or_else(|| {
            corrupt_path(
                "capsule Git visibility",
                "live ref edit has no visibility evidence",
            )
        })?;
        if visibility.new_oid != new_oid {
            return Err(corrupt_path(
                "capsule Git visibility",
                "visibility evidence does not match its ref edit",
            ));
        }
        if let Some(expected_old) = edit.expected_old()
            && visibility.old_oid.as_deref() != Some(expected_old)
        {
            return Err(corrupt_path(
                "capsule Git visibility",
                "visibility evidence does not match the expected old ref",
            ));
        }
        index.apply_ref_edit(edit.ref_name().to_owned(), &visibility)?;
    }
    if !evidence.is_empty() {
        return Err(corrupt_path(
            "capsule Git visibility",
            "visibility evidence contains an uncommitted ref",
        ));
    }
    Ok(())
}

fn apply_capsule_visibility_index(
    capsule: &Capsule,
    index: &mut crab_metadata::git_visibility::GitVisibilityIndex,
) -> Result<()> {
    let transaction = capsule.transaction()?;
    let delta = capsule.visibility_delta()?;
    let mut evidence = delta.map(|delta| delta.edits().clone()).unwrap_or_default();
    for edit in transaction.edits() {
        let Some(new_oid) = edit.new_oid() else {
            if evidence.remove(edit.ref_name()).is_some() {
                return Err(corrupt_path(
                    "capsule Git visibility",
                    "deleted ref has visibility evidence",
                ));
            }
            index.remove_ref(edit.ref_name());
            continue;
        };
        let visibility = evidence.remove(edit.ref_name()).ok_or_else(|| {
            corrupt_path(
                "capsule Git visibility",
                "live ref edit has no visibility evidence",
            )
        })?;
        if visibility.new_oid != new_oid {
            return Err(corrupt_path(
                "capsule Git visibility",
                "visibility evidence does not match its ref edit",
            ));
        }
        if let Some(expected_old) = edit.expected_old()
            && visibility.old_oid.as_deref() != Some(expected_old)
        {
            return Err(corrupt_path(
                "capsule Git visibility",
                "visibility evidence does not match the expected old ref",
            ));
        }
        index.apply_ref_edit(edit.ref_name().to_owned(), &visibility)?;
    }
    if !evidence.is_empty() {
        return Err(corrupt_path(
            "capsule Git visibility",
            "visibility evidence contains an uncommitted ref",
        ));
    }
    Ok(())
}

fn apply_capsule_refs(
    capsule: &Capsule,
    refs: &mut BTreeMap<String, String>,
    peeled_refs: &mut BTreeMap<String, String>,
) -> Result<()> {
    for edit in capsule.transaction()?.edits() {
        if refs.get(edit.ref_name()).map(String::as_str) != edit.expected_old() {
            return Err(corrupt_path(
                "capsule-protocol ref heads",
                "capsule expected-old ref does not match its parent state",
            ));
        }
        match edit.new_oid() {
            Some(oid) => {
                refs.insert(edit.ref_name().to_owned(), oid.to_owned());
                match edit.peeled_oid() {
                    Some(peeled) => {
                        peeled_refs.insert(edit.ref_name().to_owned(), peeled.to_owned());
                    }
                    None => {
                        peeled_refs.remove(edit.ref_name());
                    }
                }
            }
            None => {
                refs.remove(edit.ref_name());
                peeled_refs.remove(edit.ref_name());
            }
        }
    }
    Ok(())
}

async fn load_checkpoint(
    router: &StoreLayout<Store>,
    pointer: &CheckpointPointer,
    limits: CapsuleReadLimits,
) -> Result<Checkpoint> {
    if pointer.size() > limits.max_capsule_bytes {
        return Err(ReadError::CapsuleReadLimit {
            resource: "checkpoint bytes",
            maximum: limits.max_capsule_bytes,
        });
    }
    let path = router.capsule_checkpoint_path(pointer.hash());
    let (bytes, _) = router
        .store()
        .get_with_etag_bounded(&path, pointer.size())
        .await?;
    let checkpoint = Checkpoint::decode(bytes)?;
    let object_count = checkpoint
        .git_packs()
        .iter()
        .try_fold(0_u64, |total, pack| total.checked_add(pack.object_count()))
        .ok_or_else(|| ReadError::internal("checkpoint object count overflowed"))?;
    if checkpoint.hash() != pointer.hash()
        || checkpoint.bytes().len() as u64 != pointer.size()
        || checkpoint.control_offset() != pointer.control_offset()
        || checkpoint.control_size() != pointer.control_size()
        || checkpoint.footer_hash() != pointer.footer_hash()
        || checkpoint.covered_generation() != pointer.covered_generation()
        || checkpoint.covered_root_digest() != pointer.covered_root_digest()
        || checkpoint.git_packs().len() as u32 != pointer.pack_count()
        || object_count != pointer.object_count()
    {
        return Err(corrupt(
            &path,
            "checkpoint does not match its authenticated root pointer",
        ));
    }
    Ok(checkpoint)
}

async fn load_layered_checkpoint(
    router: &StoreLayout<Store>,
    pointer: &CheckpointPointer,
    limits: CapsuleReadLimits,
) -> Result<LayeredCheckpoint> {
    if pointer.size() > limits.max_capsule_bytes {
        return Err(ReadError::CapsuleReadLimit {
            resource: "layered checkpoint bytes",
            maximum: limits.max_capsule_bytes,
        });
    }
    crab_metadata::capsule_protocol::load_layered_checkpoint(router, pointer)
        .await
        .map_err(|error| corrupt_path("capsule-protocol layered checkpoint", error.to_string()))
}

async fn load_layered_checkpoint_control(
    router: &StoreLayout<Store>,
    pointer: &CheckpointPointer,
    limits: CapsuleReadLimits,
) -> Result<LayeredCheckpoint> {
    if pointer.control_size() > limits.max_capsule_bytes {
        return Err(ReadError::CapsuleReadLimit {
            resource: "layered checkpoint control bytes",
            maximum: limits.max_capsule_bytes,
        });
    }
    crab_metadata::capsule_protocol::load_layered_checkpoint_control(router, pointer)
        .await
        .map_err(|error| {
            corrupt_path(
                "capsule-protocol layered checkpoint control",
                error.to_string(),
            )
        })
}

/// Load and verify only a checkpoint's authenticated control suffix.
pub async fn load_checkpoint_control(
    router: &StoreLayout<Store>,
    pointer: &CheckpointPointer,
    limits: CapsuleReadLimits,
) -> Result<CheckpointControl> {
    if pointer.control_size() > limits.max_capsule_bytes {
        return Err(ReadError::CapsuleReadLimit {
            resource: "checkpoint control bytes",
            maximum: limits.max_capsule_bytes,
        });
    }
    let path = router.capsule_checkpoint_path(pointer.hash());
    let bytes = router
        .store()
        .range_get(&path, pointer.control_offset()..pointer.size())
        .await?;
    let control = Checkpoint::decode_control(
        bytes,
        pointer.size(),
        pointer.hash(),
        pointer.control_offset(),
        pointer.control_size(),
        pointer.footer_hash(),
    )?;
    let object_count = control
        .git_packs()
        .iter()
        .try_fold(0_u64, |total, pack| total.checked_add(pack.object_count()))
        .ok_or_else(|| corrupt(&path, "checkpoint object count overflowed"))?;
    if control.covered_generation() != pointer.covered_generation()
        || control.covered_root_digest() != pointer.covered_root_digest()
        || control.git_packs().len() as u32 != pointer.pack_count()
        || object_count != pointer.object_count()
    {
        return Err(corrupt(
            &path,
            "checkpoint control does not match its authenticated root pointer",
        ));
    }
    Ok(control)
}

fn admit_frontier(pointers: &[CapsulePointer], limits: CapsuleReadLimits) -> Result<()> {
    let mut total = 0u64;
    for pointer in pointers {
        if pointer.size() > limits.max_capsule_bytes {
            return Err(ReadError::CapsuleReadLimit {
                resource: "individual capsule bytes",
                maximum: limits.max_capsule_bytes,
            });
        }
        total = total
            .checked_add(pointer.size())
            .ok_or(ReadError::CapsuleReadLimit {
                resource: "frontier bytes",
                maximum: limits.max_frontier_bytes,
            })?;
        if total > limits.max_frontier_bytes {
            return Err(ReadError::CapsuleReadLimit {
                resource: "frontier bytes",
                maximum: limits.max_frontier_bytes,
            });
        }
    }
    Ok(())
}

async fn load_run(router: &StoreLayout<Store>, pointer: &CapsulePointer) -> Result<CapsuleRun> {
    let path = router.capsule_path(pointer.hash());
    let (bytes, _) = router
        .store()
        .get_with_etag_bounded(&path, pointer.size())
        .await?;
    let actual_size = u64::try_from(bytes.len())
        .map_err(|_| ReadError::internal("capsule size cannot be represented as u64"))?;
    if actual_size != pointer.size() {
        return Err(corrupt(
            &path,
            format!(
                "capsule size is {actual_size} bytes; root declares {}",
                pointer.size()
            ),
        ));
    }
    let run = CapsuleRun::decode(bytes)?;
    if run.hash() != pointer.hash()
        || run.level() != pointer.level()
        || run.transaction_ids() != pointer.transaction_ids()
        || run.newest_base_root_digest() != pointer.newest_base_root_digest()
    {
        return Err(corrupt(
            &path,
            "capsule run does not match its authenticated root pointer",
        ));
    }
    Ok(run)
}

async fn load_run_control(
    router: &StoreLayout<Store>,
    pointer: &CapsulePointer,
) -> Result<(CapsuleRunControl, Vec<CapsuleControl>)> {
    let path = router.capsule_path(pointer.hash());
    let (control, capsules) =
        crab_metadata::capsule_protocol::load_capsule_run_control(router, pointer)
            .await
            .map_err(|error| corrupt_path(path.to_string(), error.to_string()))?;
    Ok((control, capsules))
}

fn corrupt(path: &object_store::path::Path, reason: impl Into<String>) -> ReadError {
    ReadError::CorruptObject {
        path: path.to_string(),
        reason: reason.into(),
    }
}

fn corrupt_path(path: impl Into<String>, reason: impl Into<String>) -> ReadError {
    ReadError::CorruptObject {
        path: path.into(),
        reason: reason.into(),
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use crab_metadata::capsule_protocol::{
        CapsuleGitPack, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
        CapsuleVisibilityDelta, Checkpoint, CheckpointPointer, HistorySegment, HistorySegmentState,
        PackRange, RepositoryRoot,
    };
    use crab_metadata::git_visibility::GitVisibilityEdit;
    use crab_storage::{StorageObservation, StorageObserver, StorageOperation, StorageOutcome};
    use object_store::memory::InMemory;

    use super::*;

    const TEST_LIMITS: CapsuleReadLimits = CapsuleReadLimits {
        max_capsule_bytes: 1024 * 1024,
        max_frontier_bytes: 8 * 1024 * 1024,
    };

    #[derive(Default)]
    struct RecordingObserver {
        observations: Mutex<Vec<StorageObservation>>,
    }

    impl StorageObserver for RecordingObserver {
        fn started(&self, _operation: StorageOperation) {}

        fn finished(&self, observation: StorageObservation) {
            self.observations.lock().unwrap().push(observation);
        }
    }

    async fn seed_one_capsule(inner: Arc<InMemory>, pointer_transaction_id: Option<String>) {
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "repositories/test".to_owned());
        let initial = RootRecord::encode(
            RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        let transaction = CapsuleTransaction::new(
            initial.digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some("2".repeat(40)),
                None,
            )],
        )
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            vec![
                CapsuleGitPack::new(
                    Bytes::from_static(b"PACK capsule-protocol read test"),
                    Bytes::from_static(b"index"),
                    Bytes::from_static(b"reverse"),
                    Bytes::from_static(b"locator"),
                    "4".repeat(40),
                    1,
                )
                .unwrap(),
            ],
            Vec::new(),
        )
        .unwrap();
        let run = CapsuleRun::leaf(capsule).unwrap();
        store
            .put(&router.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let mut transaction_ids = run.transaction_ids();
        if let Some(pointer_transaction_id) = pointer_transaction_id {
            transaction_ids[0] = pointer_transaction_id;
        }
        let pointer_transaction_id = transaction_ids[0].clone();
        let pointer = CapsulePointer::new(
            run.hash(),
            run.bytes().len() as u64,
            run.level(),
            transaction_ids,
            run.newest_base_root_digest(),
        )
        .unwrap();
        let next = initial
            .root()
            .advance(
                initial.digest(),
                std::collections::BTreeMap::from([("refs/heads/main".to_owned(), "2".repeat(40))]),
                std::collections::BTreeMap::new(),
                vec![pointer],
                &pointer_transaction_id,
            )
            .unwrap();
        let root = RootRecord::encode(next).unwrap();
        store
            .create_strict(&router.capsule_root_path(), root.bytes().clone())
            .await
            .unwrap();
    }

    async fn seed_prepared_multi_ref(
        inner: Arc<InMemory>,
        commit_record: bool,
        publish_marker: bool,
    ) {
        let store = Store::new(inner);
        let router = StoreLayout::new(store.clone(), "repositories/test".to_owned());
        let initial = RootRecord::encode(
            RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        store
            .create_strict(&router.capsule_root_path(), initial.bytes().clone())
            .await
            .unwrap();
        let transaction = CapsuleTransaction::new(
            initial.digest(),
            vec![
                CapsuleRefEdit::new("refs/heads/main", None, Some("2".repeat(40)), None),
                CapsuleRefEdit::new("refs/heads/feature", None, Some("3".repeat(40)), None),
            ],
        )
        .unwrap();
        let transaction_id = transaction.id().unwrap();
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([
            (
                "refs/heads/main".to_owned(),
                GitVisibilityEdit::from_delta_objects(
                    None,
                    "2".repeat(40),
                    vec!["2".repeat(40)],
                    Vec::new(),
                ),
            ),
            (
                "refs/heads/feature".to_owned(),
                GitVisibilityEdit::from_delta_objects(
                    None,
                    "3".repeat(40),
                    vec!["3".repeat(40)],
                    Vec::new(),
                ),
            ),
        ]))
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            vec![
                CapsuleGitPack::new(
                    Bytes::from_static(b"PACK capsule-protocol multi-ref read test"),
                    Bytes::from_static(b"index"),
                    Bytes::from_static(b"reverse"),
                    Bytes::from_static(b"locator"),
                    "4".repeat(40),
                    1,
                )
                .unwrap(),
            ],
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                visibility.encode().unwrap(),
            )],
        )
        .unwrap();
        let run = CapsuleRun::leaf(capsule).unwrap();
        store
            .put(&router.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let pointer = CapsulePointer::new(
            run.hash(),
            run.bytes().len() as u64,
            run.level(),
            run.transaction_ids(),
            run.newest_base_root_digest(),
        )
        .unwrap();
        let activation_id = "5".repeat(64);
        for edit in transaction.edits() {
            let head = crab_metadata::capsule_protocol::CapsuleRefHead::from_root(
                edit.ref_name(),
                initial.root().ref_epoch().to_owned(),
                None,
                None,
            )
            .unwrap();
            let state = head
                .successor_state(
                    &BTreeSet::new(),
                    edit.new_oid().map(str::to_owned),
                    edit.peeled_oid().map(str::to_owned),
                    transaction_id.clone(),
                    vec![pointer.clone()],
                )
                .unwrap();
            let prepared = head
                .prepare(
                    head.visible(&BTreeSet::new()).clone(),
                    activation_id.clone(),
                    state,
                )
                .unwrap();
            store
                .create_strict(
                    &router.capsule_ref_head_path(
                        &crab_metadata::capsule_protocol::capsule_ref_name_key(edit.ref_name()),
                    ),
                    prepared.encode().unwrap(),
                )
                .await
                .unwrap();
        }
        let preparing = crab_metadata::capsule_protocol::CapsuleTransactionRecord::preparing(
            activation_id.clone(),
            transaction_id,
        )
        .unwrap();
        let record = if commit_record {
            preparing.commit().unwrap()
        } else {
            preparing
        };
        store
            .create_strict(
                &router.capsule_transaction_path(&activation_id),
                record.encode().unwrap(),
            )
            .await
            .unwrap();
        if publish_marker {
            store
                .create_strict(
                    &router.capsule_committed_transaction_path(&activation_id),
                    record.encode().unwrap(),
                )
                .await
                .unwrap();
        }
    }

    fn layered_member_read(path: &str, start: u64, seed: char) -> LayeredMemberRead {
        let bytes = |length: usize| vec![seed as u8; length];
        let pack = PackRange::new(start, &bytes(10)).unwrap();
        let index = PackRange::new(start + 10, &bytes(10)).unwrap();
        let reverse = PackRange::new(start + 20, &bytes(10)).unwrap();
        let locator = PackRange::new(start + 30, &bytes(10)).unwrap();
        let member =
            PackMemberDescriptor::new(pack, index, reverse, locator, "0".repeat(40), 1, Vec::new())
                .unwrap();
        LayeredMemberRead {
            source_path: object_store::path::Path::from(path),
            source_size: start + 40,
            pack_id: crab_xet::hash::MerkleHash::from_hex(&seed.to_string().repeat(64)).unwrap(),
            member,
        }
    }

    #[test]
    fn layered_payload_windows_coalesce_selected_members() {
        let members = vec![
            LayeredPayloadMemberRead {
                member: layered_member_read("runs/a", 0, 'a'),
                complete_local: false,
            },
            LayeredPayloadMemberRead {
                member: layered_member_read("runs/a", 50, 'b'),
                complete_local: false,
            },
        ];
        let windows =
            plan_layered_payload_windows(&members, LayeredPayloadWindowMode::Full).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].range, 0..90);
        assert_eq!(windows[0].member_indices, [0, 1]);
        assert_eq!(windows[0].useful_bytes, 80);
    }

    #[test]
    fn layered_payload_windows_skip_local_pack_bodies() {
        let members = vec![LayeredPayloadMemberRead {
            member: layered_member_read("runs/a", 0, 'a'),
            complete_local: true,
        }];
        let windows =
            plan_layered_payload_windows(&members, LayeredPayloadWindowMode::Full).unwrap();
        assert_eq!(windows[0].range, 10..40);
    }

    #[test]
    fn layered_pack_identity_uses_authenticated_descriptor_hashes() {
        let member = layered_member_read("runs/a", 0, 'a').member;
        let identity = verified_layered_pack_identity(&member).unwrap();

        assert_eq!(identity.git_sha1, [0; 20]);
        assert_eq!(
            identity.content_hash,
            *blake3::hash(&vec![b'a'; 10]).as_bytes()
        );
    }

    #[test]
    fn layered_payload_windows_split_direct_fetch_ranges() {
        let members = vec![LayeredPayloadMemberRead {
            member: layered_member_read("runs/a", 0, 'a'),
            complete_local: false,
        }];
        let sidecars =
            plan_layered_payload_windows(&members, LayeredPayloadWindowMode::Sidecars).unwrap();
        let packs =
            plan_layered_payload_windows(&members, LayeredPayloadWindowMode::Packs).unwrap();
        assert_eq!(sidecars[0].range, 10..40);
        assert_eq!(packs[0].range, 0..10);
    }

    #[test]
    fn full_layered_windows_coalesce_metadata_gaps_but_bound_reads() {
        let members = vec![
            LayeredPayloadMemberRead {
                member: layered_member_read("runs/a", 0, 'a'),
                complete_local: false,
            },
            LayeredPayloadMemberRead {
                member: layered_member_read("runs/a", 8 * 1024 * 1024, 'b'),
                complete_local: false,
            },
        ];
        let full = plan_layered_payload_windows(&members, LayeredPayloadWindowMode::Full).unwrap();
        let sidecars =
            plan_layered_payload_windows(&members, LayeredPayloadWindowMode::Sidecars).unwrap();
        assert_eq!(full.len(), 1);
        assert_eq!(full[0].range, 0..(8 * 1024 * 1024 + 40));
        assert_eq!(sidecars.len(), 2);
    }

    #[tokio::test]
    async fn stale_ref_epoch_is_invisible_to_ref_reads() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "repositories/test".to_owned());
        let root = RootRecord::encode(
            RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        store
            .create_strict(&router.capsule_root_path(), root.bytes().clone())
            .await
            .unwrap();
        let transaction_id = "2".repeat(64);
        let pointer = CapsulePointer::new(
            "3".repeat(64),
            1,
            0,
            vec![transaction_id.clone()],
            root.digest(),
        )
        .unwrap();
        let old = crab_metadata::capsule_protocol::CapsuleRefHead::from_root(
            "refs/heads/main",
            "9".repeat(64),
            None,
            None,
        )
        .unwrap();
        let state = old
            .successor_state(
                &BTreeSet::new(),
                Some("4".repeat(40)),
                None,
                transaction_id,
                vec![pointer],
            )
            .unwrap();
        let stale = old.commit(state).unwrap();
        store
            .create_strict(
                &router.capsule_ref_head_path(
                    &crab_metadata::capsule_protocol::capsule_ref_name_key("refs/heads/main"),
                ),
                stale.encode().unwrap(),
            )
            .await
            .unwrap();
        let snapshot = load_root(&router).await.unwrap();

        let refs = read_visible_refs_from_root(&router, &snapshot)
            .await
            .unwrap();

        assert!(refs.is_empty());
    }

    #[tokio::test]
    async fn one_compacted_capsule_view_captures_stable_transaction_and_ref_indexes() {
        let inner = Arc::new(InMemory::new());
        seed_one_capsule(inner.clone(), None).await;
        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner).with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());

        let view = open_view(&router, TEST_LIMITS).await.unwrap();

        assert_eq!(view.root().root().generation(), 1);
        assert_eq!(view.capsules().len(), 1);
        let operations = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .map(|observation| observation.operation)
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            vec![
                StorageOperation::Get,
                StorageOperation::List,
                StorageOperation::List,
                StorageOperation::Get,
            ]
        );
    }

    #[tokio::test]
    async fn control_view_range_reads_checkpoint_suffix_without_pack_body() {
        let inner = Arc::new(InMemory::new());
        seed_one_capsule(inner.clone(), None).await;
        let seed_store = Store::new(inner.clone());
        let seed_router = StoreLayout::new(seed_store.clone(), "repositories/test".to_owned());
        let current = load_root(&seed_router).await.unwrap();
        let checkpoint = Checkpoint::build_with_pointer_catalog(
            current.record().root().generation(),
            current.record().digest(),
            vec![
                CapsuleGitPack::new(
                    Bytes::from_static(b"PACK checkpoint control test body"),
                    Bytes::from_static(b"index"),
                    Bytes::from_static(b"reverse"),
                    Bytes::from_static(b"locator"),
                    "4".repeat(40),
                    1,
                )
                .unwrap(),
            ],
            crab_metadata::capsule_protocol::PointerCatalog::new(),
        )
        .unwrap();
        let checkpoint_pointer = CheckpointPointer::new(
            checkpoint.hash(),
            checkpoint.bytes().len() as u64,
            checkpoint.control_offset(),
            checkpoint.control_size(),
            checkpoint.footer_hash(),
            checkpoint.covered_generation(),
            checkpoint.covered_root_digest(),
            checkpoint.git_packs().len() as u32,
            checkpoint
                .git_packs()
                .iter()
                .map(|pack| pack.object_count())
                .sum(),
        )
        .unwrap();
        let history = HistorySegment::build(
            checkpoint_pointer.clone(),
            None,
            HistorySegmentState::new(
                current.record().root().refs().clone(),
                current.record().root().peeled_refs().clone(),
                current.record().root().head().to_owned(),
                current.record().root().compacted_ref_transactions().clone(),
                current.record().root().capsule_frontier().to_vec(),
            ),
        )
        .unwrap();
        let root = current
            .record()
            .root()
            .install_checkpoint(
                current.record().digest(),
                checkpoint_pointer,
                Some(history.pointer().unwrap()),
            )
            .unwrap();
        let root = RootRecord::encode(root).unwrap();
        seed_store
            .put(
                &seed_router.capsule_checkpoint_path(checkpoint.hash()),
                checkpoint.bytes().clone(),
            )
            .await
            .unwrap();
        seed_store
            .update(
                &seed_router.capsule_root_path(),
                root.bytes().clone(),
                current.etag().clone(),
            )
            .await
            .unwrap();

        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner).with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        let view = open_view_from_root_with_control(
            &router,
            load_root(&router).await.unwrap(),
            TEST_LIMITS,
        )
        .await
        .unwrap();

        assert!(view.checkpoint().is_none());
        assert!(view.checkpoint_control().is_some());
        let operations = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .map(|observation| observation.operation)
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            vec![
                StorageOperation::Get,
                StorageOperation::List,
                StorageOperation::List,
                StorageOperation::Range
            ]
        );
    }

    #[tokio::test]
    async fn run_control_loads_large_detached_visibility_section() {
        let inner = Arc::new(InMemory::new());
        let seed_store = Store::new(inner.clone());
        let seed_router = StoreLayout::new(seed_store.clone(), "repositories/test".to_owned());
        let transaction = CapsuleTransaction::new(
            &"1".repeat(64),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some("2".repeat(40)),
                None,
            )],
        )
        .unwrap();
        let tip = format!("{:040x}", 19_999);
        let objects = (0..20_000)
            .map(|index| format!("{index:040x}"))
            .collect::<Vec<_>>();
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            "refs/heads/main".to_owned(),
            GitVisibilityEdit::from_replacement_objects(None, tip, objects),
        )]))
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            Vec::new(),
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                visibility.encode().unwrap(),
            )],
        )
        .unwrap();
        let run = CapsuleRun::leaf(capsule).unwrap();
        let pointer = CapsulePointer::new_with_control(
            run.hash(),
            run.bytes().len() as u64,
            run.control_offset(),
            run.control_size(),
            run.footer_hash(),
            run.level(),
            run.transaction_ids(),
            run.newest_base_root_digest(),
        )
        .unwrap();
        seed_store
            .put(&seed_router.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();

        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner).with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        let (control, capsules) =
            crab_metadata::capsule_protocol::load_capsule_run_control(&router, &pointer)
                .await
                .unwrap();

        assert!(
            control
                .capsule_locations()
                .first()
                .and_then(|location| location.visibility())
                .is_some()
        );
        assert_eq!(capsules.len(), 1);
        assert_eq!(capsules[0].visibility_delta().unwrap().edits().len(), 1);
        let operations = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .map(|observation| observation.operation)
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            vec![StorageOperation::Range, StorageOperation::Range]
        );
    }

    #[tokio::test]
    async fn ref_view_does_not_fetch_capsule_payloads() {
        let inner = Arc::new(InMemory::new());
        seed_one_capsule(inner.clone(), None).await;
        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner).with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());

        let root = load_root(&router).await.unwrap();
        let view = open_ref_view_from_root(&router, root).await.unwrap();

        assert_eq!(view.refs().get("refs/heads/main"), Some(&"2".repeat(40)));
        let operations = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .map(|observation| observation.operation)
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            vec![
                StorageOperation::Get,
                StorageOperation::List,
                StorageOperation::List,
            ]
        );
    }

    #[tokio::test]
    async fn activity_poll_matches_full_view_without_fetching_capsules() {
        let inner = Arc::new(InMemory::new());
        seed_one_capsule(inner.clone(), None).await;
        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner).with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());

        let root = load_root(&router).await.unwrap();
        let activity = read_activity_from_root(&router, &root).await.unwrap();
        let poll_operations = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .map(|observation| observation.operation)
            .collect::<Vec<_>>();
        assert_eq!(
            poll_operations,
            vec![
                StorageOperation::Get,
                StorageOperation::List,
                StorageOperation::List,
            ]
        );

        let view = open_view_from_root(&router, root, TEST_LIMITS)
            .await
            .unwrap();
        assert_eq!(activity.state_digest(), view.state_digest());
        assert_eq!(activity.capsule_count(), view.capsule_count().unwrap());
        assert_eq!(activity.ref_count(), view.refs().len() as u64);
        assert_eq!(view.git_object_count().unwrap(), 1);
    }

    #[tokio::test]
    async fn capsule_must_match_every_root_pointer_identity() {
        let inner = Arc::new(InMemory::new());
        seed_one_capsule(inner.clone(), Some("3".repeat(64))).await;
        let store = Store::new(inner);
        let router = StoreLayout::new(store, "repositories/test".to_owned());

        let error = open_view(&router, TEST_LIMITS)
            .await
            .expect_err("mismatched transaction identity must fail");

        assert!(matches!(error, ReadError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn multi_ref_prepared_heads_are_visible_from_their_committed_record() {
        let preparing = Arc::new(InMemory::new());
        seed_prepared_multi_ref(preparing.clone(), false, false).await;
        let router = StoreLayout::new(Store::new(preparing), "repositories/test".to_owned());
        let old = open_view(&router, TEST_LIMITS).await.unwrap();
        assert!(old.refs().is_empty());
        assert!(old.capsules().is_empty());

        let without_marker = Arc::new(InMemory::new());
        seed_prepared_multi_ref(without_marker.clone(), true, false).await;
        let router = StoreLayout::new(Store::new(without_marker), "repositories/test".to_owned());
        let recovered = open_view(&router, TEST_LIMITS).await.unwrap();
        assert_eq!(
            recovered.refs().get("refs/heads/main"),
            Some(&"2".repeat(40))
        );
        assert_eq!(
            recovered.refs().get("refs/heads/feature"),
            Some(&"3".repeat(40))
        );
        assert_eq!(recovered.capsules().len(), 1);

        let with_marker = Arc::new(InMemory::new());
        seed_prepared_multi_ref(with_marker.clone(), true, true).await;
        let router = StoreLayout::new(Store::new(with_marker), "repositories/test".to_owned());
        let new = open_view(&router, TEST_LIMITS).await.unwrap();
        assert_eq!(new.refs().get("refs/heads/main"), Some(&"2".repeat(40)));
        assert_eq!(new.refs().get("refs/heads/feature"), Some(&"3".repeat(40)));
        assert_eq!(new.capsules().len(), 1);
    }

    #[tokio::test]
    async fn visible_ref_read_resolves_committed_multi_ref_updates() {
        let inner = Arc::new(InMemory::new());
        seed_prepared_multi_ref(inner.clone(), true, false).await;
        let router = StoreLayout::new(Store::new(inner), "repositories/test".to_owned());

        let refs = read_visible_refs(&router).await.unwrap();

        assert_eq!(refs.get("refs/heads/main"), Some(&"2".repeat(40)));
        assert_eq!(refs.get("refs/heads/feature"), Some(&"3".repeat(40)));
    }

    #[tokio::test]
    async fn selected_visible_ref_read_avoids_repository_wide_objects() {
        let inner = Arc::new(InMemory::new());
        seed_prepared_multi_ref(inner.clone(), true, false).await;
        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner).with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        let root = load_root(&router).await.unwrap();

        let refs = read_visible_refs_from_root_for_refs(
            &router,
            &root,
            &BTreeSet::from(["refs/heads/feature".to_owned()]),
        )
        .await
        .unwrap();

        assert_eq!(
            refs,
            BTreeMap::from([("refs/heads/feature".to_owned(), "3".repeat(40))])
        );
        let operations = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .map(|observation| observation.operation)
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            vec![
                StorageOperation::Get,
                StorageOperation::Get,
                StorageOperation::Get,
                StorageOperation::Get,
            ]
        );
    }

    #[tokio::test]
    async fn frontier_admission_fails_before_capsule_gets() {
        let inner = Arc::new(InMemory::new());
        seed_one_capsule(inner.clone(), None).await;
        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner).with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());

        let error = open_view(
            &router,
            CapsuleReadLimits {
                max_capsule_bytes: 1,
                max_frontier_bytes: 1,
            },
        )
        .await
        .expect_err("oversized frontier must fail admission");

        assert!(matches!(error, ReadError::CapsuleReadLimit { .. }));
        let operations = observer
            .observations
            .lock()
            .unwrap()
            .iter()
            .filter(|observation| observation.outcome == StorageOutcome::Success)
            .map(|observation| observation.operation)
            .collect::<Vec<_>>();
        assert_eq!(
            operations,
            vec![
                StorageOperation::Get,
                StorageOperation::List,
                StorageOperation::List,
            ]
        );
    }

    #[tokio::test]
    async fn candidate_git_packs_share_the_base_intake_limit() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        let initial = RootRecord::encode(
            RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        crab_metadata::capsule_protocol::create_root(&router, initial.clone())
            .await
            .unwrap();
        let view = open_view(&router, TEST_LIMITS).await.unwrap();
        let transaction = CapsuleTransaction::new(
            initial.digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some("2".repeat(40)),
                None,
            )],
        )
        .unwrap();
        let candidate = Capsule::build(
            &transaction,
            vec![
                CapsuleGitPack::new(
                    Bytes::from_static(b"PACK candidate"),
                    Bytes::from_static(b"index"),
                    Bytes::from_static(b"reverse"),
                    Bytes::from_static(b"locator"),
                    "4".repeat(40),
                    1,
                )
                .unwrap(),
            ],
            Vec::new(),
        )
        .unwrap();
        let git_dir = tempfile::tempdir().unwrap();

        let error = install_git_packs_with_candidates(&view, &[candidate], git_dir.path(), 1)
            .await
            .expect_err("candidate pack must count against the shared intake limit");

        assert!(matches!(error, ReadError::CapsuleReadLimit { .. }));
    }

    #[tokio::test]
    async fn candidate_visibility_materializes_without_publication() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        let initial = RootRecord::encode(
            RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        crab_metadata::capsule_protocol::create_root(&router, initial.clone())
            .await
            .unwrap();
        let view = open_view(&router, TEST_LIMITS).await.unwrap();
        let tip = "2".repeat(40);
        let transaction = CapsuleTransaction::new(
            initial.digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some(tip.clone()),
                None,
            )],
        )
        .unwrap();
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            "refs/heads/main".to_owned(),
            GitVisibilityEdit::from_replacement_objects(None, tip.clone(), vec![tip.clone()]),
        )]))
        .unwrap();
        let candidate = Capsule::build(
            &transaction,
            Vec::new(),
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                visibility.encode().unwrap(),
            )],
        )
        .unwrap();

        let actual = view.candidate_git_visibility(&candidate).unwrap();

        assert_eq!(
            actual,
            BTreeMap::from([("refs/heads/main".to_owned(), vec![tip])])
        );
        assert!(view.refs().is_empty());
    }

    #[test]
    fn capsule_visibility_retains_incremental_fetch_transition() {
        let old_tip = "a".repeat(40);
        let new_tip = "b".repeat(40);
        let added = "c".repeat(40);
        let transaction = CapsuleTransaction::new(
            &"1".repeat(64),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                Some(old_tip.clone()),
                Some(new_tip.clone()),
                None,
            )],
        )
        .unwrap();
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            "refs/heads/main".to_owned(),
            GitVisibilityEdit::from_delta_objects(
                Some(old_tip),
                new_tip.clone(),
                vec![added, new_tip],
                Vec::new(),
            ),
        )]))
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            Vec::new(),
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                visibility.encode().unwrap(),
            )],
        )
        .unwrap();
        let mut index = crab_metadata::git_visibility::GitVisibilityIndex::new(
            0,
            "0".repeat(64),
            "0".repeat(64),
            BTreeMap::from([("refs/heads/main".to_owned(), vec!["a".repeat(40)])]),
        )
        .unwrap();

        apply_capsule_visibility_index(&capsule, &mut index).unwrap();

        assert_eq!(
            index.incremental_objects("refs/heads/main", &[0xbb; 20], &[[0xaa; 20]]),
            Some(vec![[0xbb; 20], [0xcc; 20]])
        );
    }

    #[test]
    fn ref_capsules_wait_for_cross_ref_visibility_dependencies() {
        let old_tip = "1".repeat(40);
        let new_tip = "2".repeat(40);
        let source_transaction = CapsuleTransaction::new(
            &"a".repeat(64),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                Some(old_tip.clone()),
                Some(new_tip.clone()),
                None,
            )],
        )
        .unwrap();
        let source_visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            "refs/heads/main".to_owned(),
            GitVisibilityEdit::from_delta_objects(
                Some(old_tip.clone()),
                new_tip.clone(),
                vec![new_tip.clone()],
                Vec::new(),
            ),
        )]))
        .unwrap();
        let source = Capsule::build(
            &source_transaction,
            Vec::new(),
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                source_visibility.encode().unwrap(),
            )],
        )
        .unwrap();

        let branch_transaction = CapsuleTransaction::new(
            &"a".repeat(64),
            vec![CapsuleRefEdit::new(
                "refs/heads/feature",
                None,
                Some(new_tip.clone()),
                None,
            )],
        )
        .unwrap();
        let branch_visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            "refs/heads/feature".to_owned(),
            GitVisibilityEdit::from_delta_objects(
                Some(new_tip.clone()),
                new_tip.clone(),
                Vec::new(),
                Vec::new(),
            ),
        )]))
        .unwrap();
        let branch = Capsule::build(
            &branch_transaction,
            Vec::new(),
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                branch_visibility.encode().unwrap(),
            )],
        )
        .unwrap();
        let pending = BTreeMap::from([
            ("a-branch".to_owned(), branch.clone()),
            ("z-source".to_owned(), source.clone()),
        ]);

        let ordered = order_ref_capsules(
            &BTreeMap::from([("refs/heads/main".to_owned(), old_tip.clone())]),
            &BTreeMap::new(),
            &BTreeMap::from([("refs/heads/main".to_owned(), vec![old_tip])]),
            &BTreeMap::new(),
            pending,
        )
        .unwrap();

        assert_eq!(
            ordered
                .iter()
                .map(Capsule::transaction_id)
                .collect::<Vec<_>>(),
            vec![source.transaction_id(), branch.transaction_id()]
        );
    }

    #[test]
    fn ref_capsules_follow_authenticated_predecessors_when_oid_repeats() {
        let oid_a = "1".repeat(40);
        let oid_b = "2".repeat(40);
        let oid_c = "3".repeat(40);
        let capsule = |expected_old: Option<String>, new_oid: String| {
            let transaction = CapsuleTransaction::new(
                &"a".repeat(64),
                vec![CapsuleRefEdit::new(
                    "refs/heads/main",
                    expected_old,
                    Some(new_oid),
                    None,
                )],
            )
            .unwrap();
            Capsule::build(&transaction, Vec::new(), Vec::new()).unwrap()
        };
        let first = capsule(None, oid_a.clone());
        let second = capsule(Some(oid_a.clone()), oid_b.clone());
        let third = capsule(Some(oid_b), oid_a.clone());
        let fourth = capsule(Some(oid_a), oid_c.clone());
        let predecessors = BTreeMap::from([
            (
                second.transaction_id().to_owned(),
                BTreeSet::from([first.transaction_id().to_owned()]),
            ),
            (
                third.transaction_id().to_owned(),
                BTreeSet::from([second.transaction_id().to_owned()]),
            ),
            (
                fourth.transaction_id().to_owned(),
                BTreeSet::from([third.transaction_id().to_owned()]),
            ),
        ]);
        let pending = BTreeMap::from([
            ("a-first".to_owned(), first.clone()),
            ("b-fourth".to_owned(), fourth.clone()),
            ("c-second".to_owned(), second.clone()),
            ("d-third".to_owned(), third.clone()),
        ]);

        let ordered = order_ref_capsules(
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            &predecessors,
            pending,
        )
        .unwrap();

        assert_eq!(
            ordered
                .iter()
                .map(Capsule::transaction_id)
                .collect::<Vec<_>>(),
            vec![
                first.transaction_id(),
                second.transaction_id(),
                third.transaction_id(),
                fourth.transaction_id(),
            ]
        );
        let mut refs = BTreeMap::new();
        let mut peeled = BTreeMap::new();
        for capsule in &ordered {
            apply_capsule_refs(capsule, &mut refs, &mut peeled).unwrap();
        }
        assert_eq!(refs["refs/heads/main"], oid_c);
    }
}
