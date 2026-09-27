//! Verified loading of one capsule-protocol root and its bounded capsule frontier.

use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleControl, CapsulePointer, CapsuleRun, CapsuleRunControl, CheckpointPointer,
    LayeredCheckpoint, PackMemberDescriptor, PackRange, PackSourceDescriptor, PackSourceKind,
    PointerCatalog, RootRecord, load_root, visibility_object_set_digest,
};
use crab_storage::{Store, StoreLayout};
use futures_util::future::try_join_all;
use futures_util::{StreamExt, TryStreamExt};
use gix_hash::ObjectId;
use object_store::ObjectMeta;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::{ReadError, Result};

const LAYERED_SIDECAR_COALESCE_GAP_BYTES: u64 = 64 * 1024;
const LAYERED_SIDECAR_MAX_EXTRA_BYTES: u64 = 4 * 1024 * 1024;
const LAYERED_SIDECAR_MAX_WINDOW_BYTES: u64 = 16 * 1024 * 1024;
const LAYERED_PACK_MAX_WINDOW_BYTES: u64 = 64 * 1024 * 1024;
const LAYERED_FULL_MAX_WINDOW_BYTES: u64 = 64 * 1024 * 1024;
const LAYERED_SIDECAR_READ_CONCURRENCY: usize = 8;
const LAYERED_LARGE_RANGE_THRESHOLD_BYTES: u64 = 128 * 1024 * 1024;
const LAYERED_LARGE_RANGE_CHUNK_BYTES: u64 = 128 * 1024 * 1024;
const LAYERED_LARGE_RANGE_READ_CONCURRENCY: usize = 6;

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
    /// Largest aggregate source-body or Git-pack intake in each verification phase.
    /// Zero disables the byte ceiling.
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
    /// Logical shard/xorb body reads for catalog verification, excluding recipe reads and retries.
    pub catalog_objects_read: u64,
    /// Reachable Crab pointer blobs whose complete file bytes were verified.
    pub reachable_crab_pointers: u64,
    /// Distinct reachable LFS bodies read and hash-verified from origin.
    pub reachable_lfs_objects: u64,
    /// Total declared bytes of those distinct, verified LFS bodies.
    pub reachable_lfs_bytes: u64,
}

/// One authenticated repository root and every post-checkpoint capsule it names.
#[derive(Debug, Clone)]
pub struct CapsuleRepositoryView {
    root: crab_metadata::capsule_protocol::RootSnapshot,
    browse_indexes: Option<crab_metadata::capsule_protocol::BrowseIndexes>,
    checkpoint: Option<LayeredCheckpoint>,
    capsules: Vec<Capsule>,
    capsule_controls: Vec<CapsuleControl>,
    tip_bound_transitions: CapsuleTipBoundTransitions,
    refs: BTreeMap<String, String>,
    peeled_refs: BTreeMap<String, String>,
    visible_ref_transactions: BTreeMap<String, String>,
    ref_capsule_counts: BTreeMap<String, u32>,
    capsule_run_pointers: Vec<CapsulePointer>,
    capsule_run_sources: Vec<PackSourceDescriptor>,
    capsule_run_indexes: BTreeMap<String, Vec<PackRange>>,
    capsule_run_member_oids: BTreeMap<String, Vec<Vec<[u8; 20]>>>,
    frontier_object_admission: BTreeMap<[u8; 20], Vec<String>>,
}

/// One complete layered Git pack that can be copied directly to a clone.
///
/// This is admitted only for a cold, unfiltered clone whose authenticated view
/// contains only self-contained members. Consumers must verify the streamed
/// bytes; the wire layer additionally requires exactly one member.
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
}

/// Installed pack paths and connectivity evidence verified from their indexes.
#[derive(Debug)]
pub struct InstalledGitPacks {
    /// Canonical pack files now present in the destination object database.
    pub paths: Vec<PathBuf>,
    /// The installed index union exactly matches the authenticated visible closure.
    pub complete_visibility: bool,
}

/// One authenticated per-ref visibility transition available to an ordinary fetch.
///
/// The object sets are derived from the capsule's signed visibility edit. They
/// are an optimization hint only: callers must still require an exact
/// old-tip-to-new-tip chain and fall back to graph traversal when that chain is
/// unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleVisibilityTransition {
    /// Authenticated closure base, possibly borrowed from another ref at creation.
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

fn visibility_index_for_checkpoint(
    checkpoint: Option<&LayeredCheckpoint>,
) -> Result<crab_metadata::git_visibility::GitVisibilityIndex> {
    let empty = || {
        crab_metadata::git_visibility::GitVisibilityIndex::new(
            0,
            "",
            "0".repeat(64),
            BTreeMap::new(),
        )
    };
    match checkpoint {
        Some(checkpoint) => Ok(checkpoint
            .visibility_index(0, &"0".repeat(64), &"0".repeat(64))?
            .unwrap_or(empty()?)),
        None => Ok(empty()?),
    }
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
    push_ref_head_bases: BTreeMap<String, Option<(Bytes, crab_storage::ETag)>>,
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

    /// Return captured ref-head bodies and CAS versions from a push admission view.
    #[must_use]
    pub fn push_ref_head_bases(&self) -> &BTreeMap<String, Option<(Bytes, crab_storage::ETag)>> {
        &self.push_ref_head_bases
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
            push_ref_head_bases: BTreeMap::new(),
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

    /// Return the metadata-only layered checkpoint, when this view names one.
    #[must_use]
    pub fn layered_checkpoint(&self) -> Option<&LayeredCheckpoint> {
        self.checkpoint.as_ref()
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
        let Some(sources) = self.layered_cold_clone_sources() else {
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

    fn layered_cold_clone_sources(&self) -> Option<&[PackSourceDescriptor]> {
        if let Some(checkpoint) = self.layered_checkpoint() {
            // Footer-only views omit capsule controls, but their ref positions
            // still expose newer transactions. Installing only checkpoint
            // sources must not discard that newer frontier.
            return (self.capsules.is_empty()
                && self.capsule_controls.is_empty()
                && self.root.record().root().capsule_frontier().is_empty()
                && self.ref_capsule_counts.values().all(|count| *count == 0))
            .then(|| checkpoint.sources());
        }
        (self.checkpoint.is_none()
            && self.capsules.is_empty()
            && self.capsule_controls.is_empty()
            && self.capsule_run_pointers.len() == 1
            && self.capsule_run_sources.len() == 1)
            .then_some(self.capsule_run_sources.as_slice())
    }

    fn verify_cold_clone_visibility(&self, object_ids: &[[u8; 20]]) -> Result<bool> {
        for tip in self.refs.values().chain(self.peeled_refs.values()) {
            let oid = ObjectId::from_hex(tip.as_bytes())
                .map_err(|_| corrupt_path("layered cold clone", "ref tip is not SHA-1"))?;
            let oid: [u8; 20] = oid
                .as_bytes()
                .try_into()
                .map_err(|_| corrupt_path("layered cold clone", "ref tip is not SHA-1"))?;
            if object_ids.binary_search(&oid).is_err() {
                return Err(corrupt_path(
                    "layered cold clone",
                    "authenticated ref tip is absent from the downloaded pack indexes",
                ));
            }
        }
        if let Some(checkpoint) = self.layered_checkpoint() {
            let Some(expected) = checkpoint.cold_clone_object_set_digest() else {
                return Ok(false);
            };
            // Physical packs may retain unreachable objects. That is not an
            // exact-closure proof; the caller must run native connectivity.
            if checkpoint.cold_clone_object_count() != u64::try_from(object_ids.len()).ok() {
                return Ok(false);
            }
            if visibility_object_set_digest(object_ids) != expected {
                return Err(corrupt_path(
                    "layered cold clone",
                    "pack index union does not match the authenticated visibility proof",
                ));
            }
            return Ok(true);
        }
        if object_ids.len() != self.frontier_object_admission.len() {
            return Ok(false);
        }
        if !object_ids.iter().eq(self.frontier_object_admission.keys()) {
            return Err(corrupt_path(
                "layered cold clone",
                "pack index union does not match authenticated run admission",
            ));
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
        if let Some(checkpoint) = self.layered_checkpoint() {
            return self.layered_pack_members(checkpoint).count();
        }
        self.capsules
            .iter()
            .map(|capsule| capsule.git_packs().len())
            .sum::<usize>()
            .saturating_add(
                self.capsule_controls
                    .iter()
                    .map(|capsule| capsule.git_packs().len())
                    .sum(),
            )
    }

    /// Return the total authenticated Git pack-body bytes in this exact view.
    pub fn git_pack_bytes(&self) -> Result<u64> {
        if let Some(checkpoint) = self.layered_checkpoint() {
            return self
                .layered_pack_members(checkpoint)
                .try_fold(0_u64, |total, member| {
                    total.checked_add(member.pack().length()).ok_or_else(|| {
                        ReadError::internal("layered checkpoint Git pack byte total overflowed")
                    })
                });
        }
        let total = self
            .capsules
            .iter()
            .flat_map(|capsule| capsule.git_packs().iter().map(move |pack| (capsule, pack)))
            .try_fold(0_u64, |total, (capsule, pack)| {
                total
                    .checked_add(capsule.section_bytes(pack.pack_section())?.len() as u64)
                    .ok_or_else(|| ReadError::internal("capsule Git pack byte total overflowed"))
            })?;
        self.capsule_controls
            .iter()
            .flat_map(CapsuleControl::git_packs)
            .try_fold(total, |total, pack| {
                total
                    .checked_add(pack.pack().length())
                    .ok_or_else(|| ReadError::internal("capsule Git pack byte total overflowed"))
            })
    }

    /// Return the total Git objects declared by the authenticated pack inventory.
    pub fn git_object_count(&self) -> Result<u64> {
        if let Some(checkpoint) = self.layered_checkpoint() {
            return self
                .layered_pack_members(checkpoint)
                .try_fold(0_u64, |total, member| {
                    total
                        .checked_add(member.object_count())
                        .ok_or_else(|| ReadError::internal("layered Git object count overflowed"))
                });
        }
        let total =
            self.capsules
                .iter()
                .flat_map(Capsule::git_packs)
                .try_fold(0_u64, |total, pack| {
                    total
                        .checked_add(pack.object_count())
                        .ok_or_else(|| ReadError::internal("capsule Git object count overflowed"))
                })?;
        self.capsule_controls
            .iter()
            .flat_map(CapsuleControl::git_packs)
            .try_fold(total, |total, pack| {
                total
                    .checked_add(pack.object_count())
                    .ok_or_else(|| ReadError::internal("capsule Git object count overflowed"))
            })
    }

    /// Return a digest that changes with the root or any visible per-ref position.
    #[must_use]
    pub fn state_digest(&self) -> String {
        state_digest(self.root.record(), &self.visible_ref_transactions)
    }

    /// Attach derived indexes only when they name this exact captured state.
    ///
    /// Stale records are ignored. Open the result with `git_repository_from_store`;
    /// the explicit private-store reader cannot load origin index objects.
    #[must_use]
    pub fn with_browse_indexes(
        mut self,
        indexes: Option<crab_metadata::capsule_protocol::BrowseIndexes>,
    ) -> Self {
        self.browse_indexes = indexes.filter(|value| value.state_digest() == self.state_digest());
        self
    }

    /// Capture the exact Git state for derived indexes without reading or writing v1 metadata.
    ///
    /// The synthetic ETag binds visible per-ref positions, not only the root.
    /// It is a snapshot identity, never an object-store CAS token.
    pub fn git_snapshot(&self) -> Result<crab_metadata::manifest_store::RepositorySnapshot> {
        let packs = self.git_pack_manifest_entries()?;
        let manifest = self.git_manifest(&packs)?;
        let state_digest = self.state_digest();
        Ok(crab_metadata::manifest_store::RepositorySnapshot {
            layout: crab_metadata::layout_descriptor::LayoutDescriptor::canonical(),
            manifest: manifest.clone(),
            manifest_etag: state_digest.clone(),
            journal: crab_metadata::ref_journal::RefJournalSnapshot {
                refs: manifest.refs,
                peeled_refs: manifest.peeled_refs,
                head: manifest.head,
                packs,
                shards: Vec::new(),
                transactions: Vec::new(),
                ordered_edits: Vec::new(),
                visible_heads: BTreeMap::new(),
                state_digest,
            },
        })
    }

    /// Materialize the complete generation-pinned external pointer catalog.
    pub fn pointer_catalog(&self) -> Result<PointerCatalog> {
        let mut catalog = match self.checkpoint.as_ref() {
            Some(checkpoint) => checkpoint.pointer_catalog()?,
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
        if self.browse_indexes.is_some() {
            return Err(ReadError::internal(
                "browse indexes require the origin-backed Git reader",
            ));
        }
        let workspace = tempfile::tempdir()?;
        let installed = install_git_packs(self, workspace.path(), max_input_bytes).await?;
        let artifacts = self
            .capsules
            .iter()
            .cloned()
            .flat_map(|capsule| {
                let descriptors = capsule.git_packs().to_vec();
                descriptors.into_iter().map(move |descriptor| {
                    capsule
                        .section_bytes(descriptor.locator_section())
                        .map_err(ReadError::from)
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
        let mut inline_locators = std::collections::HashMap::new();
        for (pack_path, locator_bytes) in installed.into_iter().zip(artifacts) {
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
        }

        let snapshot = self.git_snapshot()?;
        let lookup_sources =
            crab_remote_git::SnapshotLookupSources::default().with_inline_locators(inline_locators);
        crab_remote_git::RemoteGitRepository::from_snapshot_with_lookup_sources(
            layout,
            &snapshot,
            identity,
            runtime,
            options,
            lookup_sources,
            cancellation,
        )
        .await
        .map_err(Into::into)
    }

    /// Open authenticated layered sources or an uncheckpointed capsule frontier.
    pub async fn git_repository_from_store(
        &self,
        layout: crab_storage::StoreLayout<crab_storage::Store>,
        identity: crab_remote_git::RepositoryIdentity,
        runtime: Arc<crab_remote_git::RemoteGitRuntime>,
        options: crab_remote_git::RepositoryOptions,
        max_input_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<crab_remote_git::RemoteGitRepository> {
        if cancellation.is_cancelled() {
            return Err(ReadError::Cancelled);
        }
        let checkpoint = self.layered_checkpoint();
        if checkpoint.is_none() && max_input_bytes > 0 && self.git_pack_bytes()? > max_input_bytes {
            return Err(ReadError::CapsuleReadLimit {
                resource: "Git pack intake",
                maximum: max_input_bytes,
            });
        }
        let workspace = tempfile::tempdir()?;
        let mut pack_sources = std::collections::HashMap::new();
        let mut inline_locators = std::collections::HashMap::new();
        let mut preferred_pack_indexes = Vec::new();
        let mut seen_packs = BTreeMap::new();
        let mut all_members = Vec::new();
        let mut lookup_indexes = std::collections::HashMap::new();
        // Keep the supplied origin even before the first checkpoint. Derived
        // indexes and placement checks must not switch to a private store.
        let mut sources = checkpoint.map_or_else(Vec::new, |value| value.sources().to_vec());
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
        let preferred_object_admission = if let Some(checkpoint) = checkpoint
            && !checkpoint.is_control_only()
        {
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
        } else {
            None
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
        // Delta dependencies need physical placement even when absent from
        // visibility additions. These hints neither authorize wants nor prove
        // client ownership of thin-pack bases.
        for (pack_id, object_ids) in capsule_run_member_oids_by_pack(self)? {
            let pack_id = crab_xet::hash::MerkleHash::from_hex(&pack_id)
                .map_err(|error| corrupt_path("capsule run admission", error.to_string()))?;
            for oid in object_ids {
                let entry = preferred_object_admission_by_oid.entry(oid).or_default();
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
            for (member_index, member) in source.members().iter().enumerate() {
                if cancellation.is_cancelled() {
                    return Err(ReadError::Cancelled);
                }
                let pack_id = crab_xet::hash::MerkleHash::from_hex(member.pack().blake3())
                    .map_err(|error| corrupt_path("capsule Git pack", error.to_string()))?;
                let pack_key = pack_id.to_string();
                if let Some(previous) = seen_packs.insert(pack_key.clone(), member.clone()) {
                    if !previous.has_same_content(member) {
                        return Err(corrupt_path(
                            "capsule Git pack",
                            "layered sources disagree about one pack identity",
                        ));
                    }
                    continue;
                }
                if let Some(indexes) = self.capsule_run_indexes.get(source.object_hash()) {
                    let index = indexes.get(member_index).ok_or_else(|| {
                        corrupt_path("capsule Git index", "run index directory is incomplete")
                    })?;
                    if index.length() != member.index().length()
                        || index.blake3() != member.index().blake3()
                    {
                        return Err(corrupt_path(
                            "capsule Git index",
                            "pooled index disagrees with its member",
                        ));
                    }
                    lookup_indexes.insert(pack_id, index.clone());
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
        if checkpoint.is_some_and(|value| !value.is_control_only()) && !all_members.is_empty() {
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
        // Captured compacted runs name pooled index copies. Checkpoint sources
        // keep their canonical index ranges; neither path relocates pack bodies
        // or changes the sidecars used by whole-member installation.
        for member_read in all_members {
            if cancellation.is_cancelled() {
                return Err(ReadError::Cancelled);
            }
            let pack_id = member_read.pack_id;
            let member = &member_read.member;
            let pack_key = pack_id.to_string();
            let index = lookup_indexes
                .get(&pack_id)
                .unwrap_or_else(|| member.index());
            let source = crab_remote_git::RemoteGitPackSource::embedded_lazy_index(
                member_read.source_path.clone(),
                member.pack().offset(),
                member.pack().length(),
                member_read.source_size,
                crab_remote_git::RemoteGitSidecarRange {
                    offset: index.offset(),
                    length: index.length(),
                    blake3: index.blake3().to_owned(),
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
        }
        let mut materialized_packs = BTreeSet::new();
        for capsule in &self.capsules {
            for descriptor in capsule.git_packs() {
                let pack = capsule.section_bytes(descriptor.pack_section())?;
                let pack_size = pack.len() as u64;
                let pack_id =
                    crab_xet::hash::MerkleHash::from_hex(blake3::hash(&pack).to_hex().as_ref())
                        .map_err(|error| corrupt_path("capsule Git pack", error.to_string()))?;
                if !materialized_packs.insert(pack_id) {
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
                // A complete capsule already authenticates these bytes. Reuse
                // them instead of downloading the same source through the
                // origin retained for metadata and non-materialized packs.
                pack_sources.insert(
                    pack_id,
                    crab_remote_git::RemoteGitPackSource::inline(
                        pack,
                        index,
                        reverse,
                        Some(locator),
                    )?,
                );
            }
        }
        let snapshot = self.git_snapshot()?;
        let mut lookup_sources = crab_remote_git::SnapshotLookupSources::default()
            .with_inline_locators(inline_locators)
            .with_pack_sources(pack_sources)
            .with_preferred_pack_indexes(preferred_pack_indexes);
        if !preferred_object_admission_by_oid.is_empty() {
            lookup_sources =
                lookup_sources.with_preferred_object_admission(preferred_object_admission_by_oid);
        }
        crab_remote_git::RemoteGitRepository::from_snapshot_with_lookup_sources(
            layout,
            &snapshot,
            identity,
            runtime,
            options,
            lookup_sources,
            cancellation,
        )
        .await
        .map_err(Into::into)
    }

    /// Materialize the complete view-bound Git visibility proof.
    pub fn git_visibility_index(
        &self,
    ) -> Result<crab_metadata::git_visibility::GitVisibilityIndex> {
        let mut index = visibility_index_for_checkpoint(self.checkpoint.as_ref())?;

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
        let mut index = self.git_visibility_index()?;
        if !capsule_is_ready(candidate, &self.refs, &index)? {
            return Err(corrupt_path(
                "candidate capsule Git visibility",
                "candidate does not extend the pinned repository view",
            ));
        }
        apply_capsule_visibility_index(candidate, &mut index)?;
        Ok(index.ref_closures())
    }

    fn git_pack_manifest_entries(
        &self,
    ) -> Result<Vec<crab_metadata::manifests::PackManifestEntry>> {
        let mut entries = Vec::new();
        if let Some(checkpoint) = self.layered_checkpoint() {
            for member in self.layered_pack_members(checkpoint) {
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
            for member in self
                .capsule_controls
                .iter()
                .flat_map(CapsuleControl::git_packs)
            {
                entries.push(crab_metadata::manifests::PackManifestEntry {
                    pack_id: member.pack().blake3().to_owned(),
                    size: member.pack().length(),
                    content_hash: member.pack().blake3().to_owned(),
                    ref_tips: Vec::new(),
                    object_count: member.object_count(),
                });
            }
        }
        // A pack can occur in multiple runs. Snapshot and reader identities
        // must bind the same physical inventory regardless of source order.
        entries.sort_unstable_by(|left, right| left.pack_id.cmp(&right.pack_id));
        if entries
            .windows(2)
            .any(|pair| pair[0].pack_id == pair[1].pack_id && pair[0] != pair[1])
        {
            return Err(corrupt_path(
                "capsule Git packs",
                "sources disagree about one pack identity",
            ));
        }
        entries.dedup();
        Ok(entries)
    }

    fn layered_pack_members<'a>(
        &'a self,
        checkpoint: &'a LayeredCheckpoint,
    ) -> impl Iterator<Item = &'a PackMemberDescriptor> {
        // A frontier run can also belong to the checkpoint. Count each physical
        // source once, but include new runs in both metrics and visibility identity.
        let mut sources = BTreeSet::new();
        checkpoint
            .sources()
            .iter()
            .chain(&self.capsule_run_sources)
            .filter(move |source| sources.insert(source.object_hash()))
            .flat_map(PackSourceDescriptor::members)
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
        if let Some(indexes) = &self.browse_indexes {
            manifest.commit_graph_hash = Some(indexes.commit_graph_hash().to_owned());
            manifest.path_state_hash = Some(indexes.path_state_hash().to_owned());
        }
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
        // Creation may borrow another committed ref's closure. Ordering and
        // visibility application already authenticate that base; only an
        // existing ref requires its own expected-old tip here.
        if transaction_edit
            .expected_old()
            .is_some_and(|old| Some(old) != edit.old_oid.as_deref())
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
/// authenticates complete immutable sources, validates the shard/xorb catalog,
/// installs all Git packs in an isolated database, and hashes every reachable
/// Crab file and LFS object at origin. Source verification and pack installation
/// have separate intake phases bounded by `max_git_bytes`.
/// Callers must signal token cancellation and await completion before shutting
/// down the runtime; native installation and Git scan workers drain before return.
pub async fn verify_reachable_dependencies(
    layout: &StoreLayout<Store>,
    view: &CapsuleRepositoryView,
    limits: CapsuleDependencyLimits,
    cancellation: &CancellationToken,
) -> Result<CapsuleDependencyProof> {
    if cancellation.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    let catalog = view.pointer_catalog()?;
    let mut sources = BTreeMap::new();
    for source in view
        .layered_checkpoint()
        .into_iter()
        .flat_map(LayeredCheckpoint::sources)
        .chain(view.capsule_run_sources())
    {
        let path = match source.kind() {
            PackSourceKind::CapsuleRun => layout.capsule_path(source.object_hash()),
            PackSourceKind::PackLayer => layout.capsule_pack_layer_path(source.object_hash()),
        };
        if let Some(previous) = sources.insert(path.clone(), source)
            && previous != source
        {
            return Err(corrupt_path(
                path.to_string(),
                "immutable source has conflicting descriptors",
            ));
        }
    }
    let source_bytes = sources.values().try_fold(0_u64, |total, source| {
        total
            .checked_add(source.object_size())
            .ok_or_else(|| ReadError::internal("layered source byte count overflowed"))
    })?;
    if limits.max_git_bytes > 0 && source_bytes > limits.max_git_bytes {
        return Err(ReadError::CapsuleReadLimit {
            resource: "layered source bodies",
            maximum: limits.max_git_bytes,
        });
    }
    let pointers = view
        .capsule_run_pointers()
        .iter()
        .map(|pointer| (pointer.hash(), pointer))
        .collect::<BTreeMap<_, _>>();
    // Range installation proves Git members, not unused source framing. Deep
    // administrative proof authenticates each complete source once, separately.
    for source in sources.into_values() {
        let retained_run = match source.kind() {
            PackSourceKind::CapsuleRun => pointers.get(source.object_hash()).copied(),
            PackSourceKind::PackLayer => None,
        };
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ReadError::Cancelled),
            result = verify_layered_source(layout, source, retained_run, source.object_size()) => {
                result?;
            }
        }
    }
    if cancellation.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    let workspace = tempfile::tempdir()?;
    crab_git::initialize_bare_git_dir(workspace.path()).map_err(std::io::Error::other)?;
    let installation = install_git_packs_from_store(
        view,
        layout,
        workspace.path(),
        limits.max_git_bytes,
        None,
        cancellation,
    );
    tokio::pin!(installation);
    tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            // A started blocking installer cannot be aborted. Keep its database
            // alive until it finishes instead of detaching writes into a removed path.
            let _ = installation.await;
            return Err(ReadError::Cancelled);
        }
        result = &mut installation => {
            result?;
        }
    }
    verify_installed_dependencies(
        layout,
        workspace.path(),
        view.refs(),
        &catalog,
        limits.pointer_scan,
        cancellation,
    )
    .await
}

/// Verify catalog bodies and reachable Crab/LFS pointers in an installed Git database.
///
/// Callers must install authenticated packs, pin the supplied refs and catalog,
/// and keep the database alive until this future completes. Token cancellation
/// drains its Git scan before returning; catalog and whole-file bytes are checked at origin.
pub async fn verify_installed_dependencies(
    layout: &StoreLayout<Store>,
    git_dir: &Path,
    refs: &BTreeMap<String, String>,
    catalog: &PointerCatalog,
    limits: crab_git::walk::PointerScanLimits,
    cancellation: &CancellationToken,
) -> Result<CapsuleDependencyProof> {
    let catalog_stats = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(ReadError::Cancelled),
        result = crate::verify_capsule_pointer_catalog_objects(layout, catalog) => {
            result?
        }
    };
    let refs = refs
        .iter()
        .map(|(name, oid)| (name.clone(), oid.clone()))
        .collect::<Vec<_>>();
    let git_dir = git_dir.to_owned();
    let scan_cancel = cancellation.child_token();
    let worker_cancel = scan_cancel.clone();
    let scan = tokio::task::spawn_blocking(move || -> Result<_> {
        let scan = crab_git::walk::scan_pointers(&git_dir, &refs, limits, &|| {
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
            .map_err(|error| ReadError::Io(std::io::Error::other(error)))??,
    };

    let mut verified_files = BTreeSet::new();
    for pointer in &scan.pointers {
        let hash = crab_xet::hash::MerkleHash::from(pointer.file_hash).hex();
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
        // Different Git pointer blobs can name the same content. Validate every
        // declared size above, but reconstruct each file version only once.
        if verified_files.insert(pointer.file_hash) {
            let pointer = crab_types::pointer::Pointer {
                file_hash: pointer.file_hash,
                size: pointer.size,
                shard_hint: None,
            };
            crate::verify_catalog_file_recipe(layout, &pointer, entry, cancellation).await?;
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
        catalog_objects_read: catalog_stats.object_read_count,
        reachable_crab_pointers: scan.pointers.len() as u64,
        reachable_lfs_objects: lfs_objects.len() as u64,
        reachable_lfs_bytes: lfs_objects.values().try_fold(0_u64, |total, size| {
            total
                .checked_add(*size)
                .ok_or_else(|| ReadError::internal("LFS dependency byte count overflowed"))
        })?,
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
    let capsules = view.capsules.clone();
    let candidates = candidates.to_vec();
    let mut payloads = Vec::new();
    for capsule in capsules.into_iter().chain(candidates) {
        payloads.extend(payloads_from_capsule(capsule)?);
    }
    install_git_pack_payloads(git_dir, payloads, max_input_bytes, true).await
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
    external_delta_bases: Vec<String>,
}

fn payloads_from_capsule(capsule: Capsule) -> Result<Vec<GitPackPayload>> {
    let descriptors = capsule.git_packs().to_vec();
    descriptors
        .into_iter()
        .map(|descriptor| {
            let pack = capsule.section_bytes(descriptor.pack_section())?;
            let index = capsule.section_bytes(descriptor.index_section())?;
            let reverse_index = capsule.section_bytes(descriptor.reverse_index_section())?;
            let locator = capsule.section_bytes(descriptor.locator_section())?;
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
                external_delta_bases: descriptor.external_delta_bases().to_vec(),
            })
        })
        .collect()
}

async fn install_git_pack_payloads(
    git_dir: impl Into<PathBuf>,
    payloads: Vec<GitPackPayload>,
    max_input_bytes: u64,
    native_repository: bool,
) -> Result<Vec<PathBuf>> {
    let git_dir = git_dir.into();
    tokio::task::spawn_blocking(move || {
        let pack_dir = git_dir.join("objects").join("pack");
        std::fs::create_dir_all(&pack_dir)?;
        let payloads = if native_repository {
            order_native_pack_payloads(payloads)?
        } else {
            payloads
        };
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
            external_delta_bases,
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
            // Complete capsule views may carry bodies already installed by a
            // previous read. Reuse only after the existing hash/sidecar checks;
            // receipt of the same payload is not a destination conflict.
            let pack = if final_pack.exists() && final_index.exists() && final_reverse.exists() {
                None
            } else {
                pack
            };
            let needs_install = pack.is_some();
            let (_temporary, pack_path, index_path, reverse_path) = if let Some(pack) = pack {
                if native_repository && !external_delta_bases.is_empty() {
                    crab_git::pack::verify_pack_sha1(&pack)?;
                    let expected = ObjectId::from_hex(git_checksum.as_bytes())
                        .map_err(|error| corrupt_path("capsule Git pack", error.to_string()))?;
                    if !pack.ends_with(expected.as_bytes()) {
                        return Err(corrupt_path("capsule Git pack", "thin pack checksum does not match its descriptor"));
                    }
                }
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
            let metadata = crab_git::pack_locator::decode_pack_kind_metadata_records(
                &locator,
                locations.pack_checksum(),
                locations.object_count(),
            )
            .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
            let declared_bases = external_delta_bases.iter().cloned().collect::<BTreeSet<_>>();
            let actual_bases = metadata.into_iter().filter_map(|(_, base)| base.map(|oid| oid.to_string())).collect::<BTreeSet<_>>();
            if actual_bases != declared_bases {
                return Err(corrupt_path("capsule Git locator", "external bases disagree with the authenticated descriptor"));
            }
            if native_repository && !external_delta_bases.is_empty() {
                let source_path = pack_path.as_deref().ok_or_else(|| corrupt_path(
                    "capsule Git pack", "a thin source pack cannot be reused as a native Git pack",
                ))?;
                let mut expected = locations.map(|location| location.map(|entry| entry.oid))
                    .collect::<std::result::Result<BTreeSet<_>, _>>()
                    .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
                for base in external_delta_bases {
                    expected.insert(ObjectId::from_hex(base.as_bytes()).map_err(|error| corrupt_path("capsule Git delta base", error.to_string()))?);
                }
                let repaired = crab_git::pack::install_thin_pack_with_content_identity(
                    &pack_dir, source_path, max_input_bytes, &expected,
                ).map_err(ReadError::GitPack)?;
                installed.push(repaired.pack_path);
                continue;
            }
            if !needs_install {
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
                    pack_path,
                    index_path,
                    reverse_path,
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

fn order_native_pack_payloads(payloads: Vec<GitPackPayload>) -> Result<Vec<GitPackPayload>> {
    if payloads
        .iter()
        .all(|pack| pack.external_delta_bases.is_empty())
    {
        return Ok(payloads);
    }
    // Source order is publication order, not a base dependency order. Prove the
    // complete installation can resolve every base before exposing any pack.
    let mut pending = payloads
        .into_iter()
        .map(|payload| {
            let (oids, _) =
                crab_git::pack_locator::sorted_object_ids_from_index_bytes(&payload.index)
                    .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
            let bases = payload
                .external_delta_bases
                .iter()
                .map(|oid| {
                    ObjectId::from_hex(oid.as_bytes())
                        .map_err(|error| corrupt_path("capsule Git delta base", error.to_string()))
                })
                .collect::<Result<BTreeSet<_>>>()?;
            Ok((payload, oids, bases))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut available = BTreeSet::new();
    let mut ordered = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let position = pending
            .iter()
            .position(|(_, _, bases)| bases.is_subset(&available))
            .ok_or_else(|| {
                corrupt_path(
                    "capsule Git pack",
                    "external delta bases are missing or cyclic",
                )
            })?;
        let (payload, oids, _) = pending.remove(position);
        available.extend(oids);
        ordered.push(payload);
    }
    Ok(ordered)
}

/// Install packs from a view, range-reading only checkpoint pack bodies that
/// are not already present in the destination Git object database.
///
/// The layout owns the selected origin; a cache never changes read authority.
/// Signal cancellation and await completion so file and blocking installers drain
/// before private staging is removed. Already verified local packs may remain.
pub async fn install_git_packs_from_store(
    view: &CapsuleRepositoryView,
    router: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
    cache: Option<&crab_cache_store::CachingStore>,
    cancel: &CancellationToken,
) -> Result<InstalledGitPacks> {
    if cancel.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    if let Some(installed) = install_layered_cold_clone_packs_from_store(
        view,
        router,
        git_dir,
        max_input_bytes,
        cache,
        cancel,
    )
    .await?
    {
        return Ok(installed);
    }
    if let Some(checkpoint) = view.layered_checkpoint() {
        let paths = install_layered_git_packs_from_store_selected_with_sources(
            checkpoint.sources(),
            &view.capsule_run_sources,
            &view.capsules,
            router,
            git_dir,
            max_input_bytes,
            LayeredInstallSelection::Native,
            cancel,
        )
        .await?
        .ok_or_else(|| ReadError::internal("native layered pack installation was not eligible"))?;
        if cancel.is_cancelled() {
            return Err(ReadError::Cancelled);
        }
        return Ok(InstalledGitPacks {
            paths,
            complete_visibility: false,
        });
    }
    let paths = install_git_packs(view, git_dir, max_input_bytes).await?;
    if cancel.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    Ok(InstalledGitPacks {
        paths,
        complete_visibility: false,
    })
}

struct StagedColdClonePack {
    pack_path: PathBuf,
    index_path: PathBuf,
    reverse_path: PathBuf,
    canonical_name: String,
    object_count: u64,
    identity: crab_git::pack::VerifiedPackIdentity,
}

fn cold_clone_sidecar_range(member: &PackMemberDescriptor) -> Result<std::ops::Range<u64>> {
    let start = member
        .index()
        .offset()
        .min(member.reverse_index().offset())
        .min(member.locator().offset());
    [member.index(), member.reverse_index(), member.locator()]
        .into_iter()
        .try_fold(start..start, |range, sidecar| {
            let end = sidecar
                .offset()
                .checked_add(sidecar.length())
                .ok_or_else(|| corrupt_path("layered cold clone", "sidecar range overflowed"))?;
            Ok(range.start..range.end.max(end))
        })
}

async fn install_layered_cold_clone_packs_from_store(
    view: &CapsuleRepositoryView,
    router: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
    cache: Option<&crab_cache_store::CachingStore>,
    cancel: &CancellationToken,
) -> Result<Option<InstalledGitPacks>> {
    let started = std::time::Instant::now();
    let Some(admitted) = view.layered_cold_clone_packs(router, max_input_bytes)? else {
        return Ok(None);
    };
    let sources = view
        .layered_cold_clone_sources()
        .ok_or_else(|| ReadError::internal("layered cold clone has no sources"))?;
    let read_bytes = sources
        .iter()
        .flat_map(|source| source.members())
        .try_fold(0_u64, |total, member| {
            let sidecars = cold_clone_sidecar_range(member)?;
            total
                .checked_add(member.pack().length())
                .and_then(|bytes| bytes.checked_add(sidecars.end - sidecars.start))
                .ok_or_else(|| corrupt_path("layered cold clone", "read byte count overflowed"))
        })?;
    if max_input_bytes > 0 && read_bytes > max_input_bytes {
        return Err(ReadError::CapsuleReadLimit {
            resource: "cold clone pack and sidecar bytes",
            maximum: max_input_bytes,
        });
    }
    // Finish the borrowed iterator before I/O: retaining its nested closure
    // across suspension prevents server-spawned installers from being Send.
    let source_members = sources
        .iter()
        .flat_map(|source| source.members().iter().map(move |member| (source, member)))
        .collect::<Vec<_>>();
    let pack_dir = git_dir.join("objects").join("pack");
    tokio::fs::create_dir_all(&pack_dir).await?;
    let temporary = tempfile::Builder::new()
        .prefix(".crab-cold-pack-")
        .tempdir_in(&pack_dir)?;
    let mut staged = Vec::with_capacity(admitted.len());
    let mut object_ids = Vec::new();
    for (ordinal, (admitted, (source, member))) in
        admitted.into_iter().zip(source_members).enumerate()
    {
        let sidecar_range = cold_clone_sidecar_range(member)?;
        let sidecar_start = sidecar_range.start;
        let source_path = admitted.source_path.clone();
        let pack_path = temporary.path().join(format!("{ordinal}.pack"));
        let index_path = temporary.path().join(format!("{ordinal}.idx"));
        let reverse_path = temporary.path().join(format!("{ordinal}.rev"));
        let pack_length = admitted
            .pack_range
            .end
            .checked_sub(admitted.pack_range.start)
            .ok_or_else(|| corrupt_path("layered cold clone", "pack range underflowed"))?;
        let pack_hash = blake3::Hash::from_hex(&admitted.content_hash)
            .map_err(|error| corrupt_path("layered cold clone", error.to_string()))?;
        let download = crab_cache_store::git_pack::GitPackSource {
            path: &source_path,
            size: source.object_size(),
            hash: source.object_hash(),
            pack: admitted.pack_range.clone(),
            sidecars: sidecar_range,
            pack_hash,
        };
        let sidecars = crab_cache_store::git_pack::read_pack_ranges(
            router.store(),
            cache,
            &download,
            &pack_path,
            cancel,
        )
        .await
        .map_err(|error| match error {
            crab_cache_store::CacheStoreError::Storage(crab_storage::StorageError::Cancelled) => {
                ReadError::Cancelled
            }
            crab_cache_store::CacheStoreError::Storage(
                crab_storage::StorageError::CorruptObject { path, reason },
            ) => ReadError::CorruptObject { path, reason },
            error => error.into(),
        })?;
        // The cache proves only pack bytes. Every read still authenticates the
        // selected sidecars and complete visible union before publishing files.
        let index = layered_range_bytes(&sidecars, sidecar_start, member.index())?;
        let reverse_index = layered_range_bytes(&sidecars, sidecar_start, member.reverse_index())?;
        let locator = layered_range_bytes(&sidecars, sidecar_start, member.locator())?;
        tracing::debug!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            pack_bytes = pack_length,
            "layered cold clone pack ranges read"
        );
        tokio::fs::write(&index_path, &index).await?;
        tokio::fs::write(&reverse_path, &reverse_index).await?;
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
        for oid in locations.sorted_object_ids() {
            object_ids.push(<[u8; 20]>::try_from(oid.as_bytes()).map_err(|_| {
                corrupt_path("layered cold clone", "pack index object ID is not SHA-1")
            })?);
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
        staged.push(StagedColdClonePack {
            pack_path,
            index_path,
            reverse_path,
            canonical_name: admitted.content_hash,
            object_count: admitted.object_count,
            identity: verified_identity,
        });
    }
    // Validate the complete downloaded union before any pack becomes visible.
    // Repeated objects across stable layers are valid and counted only once.
    object_ids.sort_unstable();
    object_ids.dedup();
    let complete_visibility = view.verify_cold_clone_visibility(&object_ids)?;
    let mut paths = Vec::with_capacity(staged.len());
    let mut installed_hashes = BTreeSet::new();
    for pack in staged {
        if cancel.is_cancelled() {
            return Err(ReadError::Cancelled);
        }
        // Different immutable sources may carry the same pack. Verify all
        // source commitments above, but publish its canonical files only once.
        if !installed_hashes.insert(pack.canonical_name.clone()) {
            continue;
        }
        let pack_dir_for_install = pack_dir.clone();
        let installed = tokio::task::spawn_blocking(move || {
            crab_git::pack::install_pack_files_from_paths_with_verified_sidecars(
                &pack_dir_for_install,
                &pack.pack_path,
                &pack.index_path,
                &pack.reverse_path,
                &pack.canonical_name,
                max_input_bytes,
                pack.object_count,
                pack.identity,
            )
            .map(|installed| installed.pack_path)
            .map_err(|error| corrupt_path("layered cold clone", error.to_string()))
        })
        .await
        .map_err(|error| {
            ReadError::Internal(format!("cold pack install worker failed: {error}"))
        })??;
        paths.push(installed);
    }
    if cancel.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    Ok(Some(InstalledGitPacks {
        paths,
        complete_visibility,
    }))
}

/// Install an authenticated historical checkpoint into a native Git object database.
///
/// All source members are verified; thin members are repaired against their
/// authenticated dependencies before local publication. The caller must pin
/// the checkpoint and protect its sources from concurrent collection.
pub async fn install_layered_checkpoint(
    checkpoint: &LayeredCheckpoint,
    router: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
) -> Result<()> {
    install_layered_git_packs_from_store_selected_with_sources(
        checkpoint.sources(),
        &[],
        &[],
        router,
        git_dir,
        max_input_bytes,
        LayeredInstallSelection::Native,
        &CancellationToken::new(),
    )
    .await?
    .ok_or_else(|| ReadError::internal("native layered pack installation was not eligible"))?;
    Ok(())
}

/// Verify one complete immutable source against its checkpoint and optional retained run.
///
/// Strict integrity and recovery callers use full-body decoding, not the
/// selected-range proof used by ordinary fetch. The read is bounded by
/// `maximum_bytes`; a retained run also authenticates transactions and its base.
pub async fn verify_layered_source(
    router: &StoreLayout<Store>,
    source: &PackSourceDescriptor,
    retained_run: Option<&CapsulePointer>,
    maximum_bytes: u64,
) -> Result<()> {
    let path = match source.kind() {
        PackSourceKind::CapsuleRun => router.capsule_path(source.object_hash()),
        PackSourceKind::PackLayer => router.capsule_pack_layer_path(source.object_hash()),
    };
    if source.object_size() > maximum_bytes {
        return Err(ReadError::CapsuleReadLimit {
            resource: "layered source body",
            maximum: maximum_bytes,
        });
    }
    if let Some(pointer) = retained_run {
        if source.kind() != PackSourceKind::CapsuleRun
            || pointer.hash() != source.object_hash()
            || pointer.size() != source.object_size()
        {
            return Err(corrupt_path(
                path.to_string(),
                "retained run conflicts with its source descriptor",
            ));
        }
        let run = crab_metadata::capsule_protocol::load_capsule_run(router, pointer).await?;
        if &PackSourceDescriptor::from_capsule_run(&run)? != source {
            return Err(corrupt_path(
                path.to_string(),
                "retained run does not match its source descriptor",
            ));
        }
        return Ok(());
    }
    let (bytes, _) = router
        .store()
        .get_with_etag_bounded(&path, source.object_size())
        .await?;
    if bytes.len() as u64 != source.object_size() {
        return Err(corrupt_path(
            path.to_string(),
            "layered source size does not match its descriptor",
        ));
    }
    let actual = match source.kind() {
        PackSourceKind::CapsuleRun => {
            PackSourceDescriptor::from_capsule_run(&CapsuleRun::decode(bytes.clone())?)?
        }
        PackSourceKind::PackLayer => {
            let layer = crab_metadata::capsule_protocol::PackLayer::decode(bytes.clone())?;
            for member in source.members() {
                for range in [
                    member.pack(),
                    member.index(),
                    member.reverse_index(),
                    member.locator(),
                ] {
                    layered_range_bytes(&bytes, 0, range)?;
                }
            }
            layer.source_descriptor()?
        }
    };
    if &actual != source {
        return Err(corrupt_path(
            path.to_string(),
            "layered source does not match its checkpoint descriptor",
        ));
    }
    Ok(())
}

/// Install only the authenticated layered pack members named by `selected`.
///
/// This is the maintenance primitive for geometric suffix roll-ups. A `None`
/// selection installs every member, while a set selection skips stable packs
/// without reading their bodies or sidecars.
pub async fn install_layered_git_packs_from_store_selected(
    checkpoint: &LayeredCheckpoint,
    capsules: &[Capsule],
    router: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
    selected: Option<&BTreeSet<String>>,
    cancel: &CancellationToken,
) -> Result<Vec<PathBuf>> {
    install_layered_git_packs_from_store_sources_selected(
        checkpoint.sources(),
        &[],
        capsules,
        router,
        git_dir,
        max_input_bytes,
        selected,
        cancel,
    )
    .await
}

/// Install selected layered members from the supplied authenticated sources.
///
/// The source descriptors are already authenticated by the repository view;
/// callers use this form when a control-only view deliberately omits capsule
/// bodies but still needs to materialize a bounded consolidation suffix. Sources
/// outside these slices are never searched for duplicate pack bodies.
pub async fn install_layered_git_packs_from_store_sources_selected(
    sources: &[PackSourceDescriptor],
    additional_sources: &[PackSourceDescriptor],
    capsules: &[Capsule],
    router: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
    selected: Option<&BTreeSet<String>>,
    cancel: &CancellationToken,
) -> Result<Vec<PathBuf>> {
    let selection = LayeredInstallSelection::Maintenance(selected);
    install_layered_git_packs_from_store_selected_with_sources(
        sources,
        additional_sources,
        capsules,
        router,
        git_dir,
        max_input_bytes,
        selection,
        cancel,
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
    router: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
    object_ids: &[ObjectId],
    common_haves: &[ObjectId],
    cancel: &CancellationToken,
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
        router,
        git_dir,
        max_input_bytes,
        object_ids,
        common_haves,
        &selected,
        cancel,
    )
    .await
}

fn capsule_run_member_oids_by_pack(
    view: &CapsuleRepositoryView,
) -> Result<BTreeMap<String, Vec<[u8; 20]>>> {
    let mut object_ids_by_pack = BTreeMap::new();
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
        for (member, object_ids) in source.members().iter().zip(members) {
            let mut object_ids = object_ids.clone();
            object_ids.sort_unstable();
            object_ids.dedup();
            if object_ids.len() as u64 != member.object_count() {
                return Err(corrupt_path(
                    "capsule run admission",
                    "member object count does not match its authenticated descriptor",
                ));
            }
            match object_ids_by_pack.entry(member.pack().blake3().to_owned()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(object_ids);
                }
                std::collections::btree_map::Entry::Occupied(entry)
                    if entry.get() != &object_ids =>
                {
                    return Err(corrupt_path(
                        "capsule run admission",
                        "duplicate pack identities have conflicting object sets",
                    ));
                }
                std::collections::btree_map::Entry::Occupied(_) => {}
            }
        }
    }
    Ok(object_ids_by_pack)
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
    router: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
    object_ids: &[ObjectId],
    common_haves: &[ObjectId],
    selected: &BTreeSet<String>,
    cancel: &CancellationToken,
) -> Result<Option<Vec<PathBuf>>> {
    let Some(checkpoint) = view.layered_checkpoint() else {
        return Ok(None);
    };
    let mut allowed = BTreeSet::new();
    allowed.extend(object_ids.iter().copied());
    allowed.extend(common_haves.iter().copied());
    let required = object_ids.iter().copied().collect::<BTreeSet<_>>();
    let member_oids_by_pack = capsule_run_member_oids_by_pack(view)?;
    let selection = LayeredInstallSelection::Fetch {
        packs: selected,
        member_oids: &member_oids_by_pack,
        allowed: &allowed,
        required: &required,
    };
    install_layered_git_packs_from_store_selected_with_sources(
        checkpoint.sources(),
        view.capsule_run_sources(),
        &[],
        router,
        git_dir,
        max_input_bytes,
        selection,
        cancel,
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

#[derive(Clone, Copy)]
enum LayeredInstallSelection<'a> {
    Native,
    Maintenance(Option<&'a BTreeSet<String>>),
    Fetch {
        packs: &'a BTreeSet<String>,
        member_oids: &'a BTreeMap<String, Vec<[u8; 20]>>,
        allowed: &'a BTreeSet<ObjectId>,
        required: &'a BTreeSet<ObjectId>,
    },
}

async fn install_layered_git_packs_from_store_selected_with_sources(
    source_descriptors: &[PackSourceDescriptor],
    additional_sources: &[PackSourceDescriptor],
    capsules: &[Capsule],
    router: &StoreLayout<Store>,
    git_dir: &Path,
    max_input_bytes: u64,
    selection: LayeredInstallSelection<'_>,
    cancel: &CancellationToken,
) -> Result<Option<Vec<PathBuf>>> {
    if cancel.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    let (selected, authenticated_member_oids, admission) = match selection {
        LayeredInstallSelection::Native => (None, None, None),
        LayeredInstallSelection::Maintenance(packs) => (packs, None, None),
        LayeredInstallSelection::Fetch {
            packs,
            member_oids,
            allowed,
            required,
        } => (Some(packs), Some(member_oids), Some((allowed, required))),
    };
    let pack_dir = git_dir.join("objects").join("pack");
    tokio::fs::create_dir_all(&pack_dir).await?;
    let mut seen_packs = BTreeMap::new();
    let mut members = Vec::new();
    let mut sources = source_descriptors.to_vec();
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
                if !previous.has_same_content(member) {
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
            if (final_pack.exists() || final_index.exists() || final_reverse.exists())
                && !complete_local
            {
                return Err(corrupt_path(
                    "capsule Git pack",
                    "local layered pack installation is incomplete",
                ));
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
    let payload_plan = if admission.is_some() {
        let pre_admitted_members = match pre_admit_layered_members(
            &members,
            &pack_dir,
            authenticated_member_oids,
            admission,
        )? {
            LayeredMemberPreAdmission::Proven => sidecar_members.clone(),
            LayeredMemberPreAdmission::NeedsSidecars => BTreeSet::new(),
            LayeredMemberPreAdmission::Rejected => return Ok(None),
        };
        plan_layered_admission_payload_windows(
            &members,
            &sidecar_members,
            &pre_admitted_members,
            max_input_bytes,
        )?
    } else {
        LayeredPayloadWindowPlan::Combined(plan_layered_payload_windows_for_members(
            &members,
            &sidecar_members,
            LayeredPayloadWindowMode::Full,
        )?)
    };
    let sidecar_windows = match &payload_plan {
        LayeredPayloadWindowPlan::Combined(windows) => windows,
        LayeredPayloadWindowPlan::Separate { sidecars, .. } => sidecars,
    };
    let planned_sidecar_bytes = layered_payload_window_bytes(sidecar_windows)?;
    if max_input_bytes > 0 && planned_sidecar_bytes > max_input_bytes {
        return Err(ReadError::CapsuleReadLimit {
            resource: "layered Git payload ranges",
            maximum: max_input_bytes,
        });
    }
    let (fetched_windows, mut fetched_bytes) = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(ReadError::Cancelled),
        result = fetch_layered_payload_windows(router.store(), sidecar_windows) => result?,
    };
    if max_input_bytes > 0 && fetched_bytes > max_input_bytes {
        return Err(ReadError::CapsuleReadLimit {
            resource: "layered Git payload ranges",
            maximum: max_input_bytes,
        });
    }
    let sidecar_read = LayeredPayloadRead {
        windows: sidecar_windows,
        bytes: &fetched_windows,
    };

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
                let (start, body) = sidecar_read.member_window(member_index)?;
                let index = layered_range_bytes(body, start, member.index())?;
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
            if let Some(expected) = authenticated_member_oids
                .and_then(|object_ids| object_ids.get(member.pack().blake3()))
            {
                let Some(actual) = object_ids_as_sha1(&object_ids.iter().copied().collect()) else {
                    return Err(corrupt_path(
                        "capsule run admission",
                        "member index contains a non-SHA-1 object ID",
                    ));
                };
                if actual != expected.iter().copied().collect() {
                    return Err(corrupt_path(
                        "capsule run admission",
                        "member index does not match its authenticated OID set",
                    ));
                }
            }
            if object_ids.iter().any(|oid| !allowed.contains(oid)) {
                return Ok(None);
            }
            covered.extend(object_ids);
        }
        if !required.iter().all(|oid| covered.contains(oid)) {
            return Ok(None);
        }

        // Local members already passed content and admission checks above.
        // Even an all-local retry must skip their absent download windows.
        return match &payload_plan {
            LayeredPayloadWindowPlan::Combined(_) => build_layered_payload_install(
                &members,
                sidecar_read,
                None,
                capsules,
                selected,
                git_dir,
                max_input_bytes,
                selection,
            )
            .await
            .map(Some),
            LayeredPayloadWindowPlan::Separate { packs, .. } => {
                let planned_pack_bytes = layered_payload_window_bytes(packs)?;
                let planned_total_bytes = fetched_bytes
                    .checked_add(planned_pack_bytes)
                    .ok_or_else(|| ReadError::internal("layered payload byte count overflowed"))?;
                if max_input_bytes > 0 && planned_total_bytes > max_input_bytes {
                    return Err(ReadError::CapsuleReadLimit {
                        resource: "layered Git payload ranges",
                        maximum: max_input_bytes,
                    });
                }
                let (pack_bytes, pack_read) = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(ReadError::Cancelled),
                    result = fetch_layered_payload_windows(router.store(), packs) => result?,
                };
                fetched_bytes = fetched_bytes
                    .checked_add(pack_read)
                    .ok_or_else(|| ReadError::internal("layered payload byte count overflowed"))?;
                if max_input_bytes > 0 && fetched_bytes > max_input_bytes {
                    return Err(ReadError::CapsuleReadLimit {
                        resource: "layered Git payload ranges",
                        maximum: max_input_bytes,
                    });
                }
                let pack_read = LayeredPayloadRead {
                    windows: packs,
                    bytes: &pack_bytes,
                };
                build_layered_payload_install(
                    &members,
                    sidecar_read,
                    Some(pack_read),
                    capsules,
                    selected,
                    git_dir,
                    max_input_bytes,
                    selection,
                )
                .await
                .map(Some)
            }
        };
    }

    build_layered_payload_install(
        &members,
        sidecar_read,
        None,
        capsules,
        selected,
        git_dir,
        max_input_bytes,
        selection,
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
    sidecars: LayeredPayloadRead<'_>,
    packs: Option<LayeredPayloadRead<'_>>,
    capsules: &[Capsule],
    selected: Option<&BTreeSet<String>>,
    git_dir: &Path,
    max_input_bytes: u64,
    selection: LayeredInstallSelection<'_>,
) -> Result<Vec<PathBuf>> {
    let mut payloads = Vec::with_capacity(members.len());
    for (member_index, member_read) in members.iter().enumerate() {
        if matches!(selection, LayeredInstallSelection::Fetch { .. }) && member_read.complete_local
        {
            continue;
        }
        let member = &member_read.member.member;
        let (start, sidecar_body) = sidecars.member_window(member_index)?;
        let index = layered_range_bytes(sidecar_body, start, member.index())?;
        let reverse_index = layered_range_bytes(sidecar_body, start, member.reverse_index())?;
        let locator = layered_range_bytes(sidecar_body, start, member.locator())?;
        let pack = if member_read.complete_local {
            None
        } else {
            let (pack_start, pack_body) = match &packs {
                Some(packs) => packs.member_window(member_index)?,
                None => (start, sidecar_body),
            };
            Some(layered_range_bytes(pack_body, pack_start, member.pack())?)
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
            external_delta_bases: member.external_delta_bases().to_vec(),
        });
    }
    for capsule in capsules.iter().cloned() {
        for payload in payloads_from_capsule(capsule)? {
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
    let native_repository = !matches!(selection, LayeredInstallSelection::Maintenance(_));
    install_git_pack_payloads(git_dir, payloads, max_input_bytes, native_repository).await
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
    // Own range descriptors before suspension so server-spawned readers do not
    // retain a lifetime-dependent iterator closure in their Send future.
    let requests = windows
        .iter()
        .map(|window| (window.path.clone(), window.range.clone()))
        .collect::<Vec<_>>();
    let fetched = futures_util::stream::iter(requests.into_iter().enumerate().map(
        |(window_index, (path, range))| {
            let store = store.clone();
            async move {
                let bytes = read_layered_source_range(&store, &path, range).await?;
                Ok::<_, ReadError>((window_index, bytes))
            }
        },
    ))
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

struct LayeredPayloadRead<'a> {
    windows: &'a [LayeredPayloadWindow],
    bytes: &'a BTreeMap<usize, Bytes>,
}

impl LayeredPayloadRead<'_> {
    fn member_window(&self, member_index: usize) -> Result<(u64, &Bytes)> {
        let window_index = payload_window_for_member(self.windows, member_index)?;
        let window = self
            .windows
            .get(window_index)
            .ok_or_else(|| ReadError::internal("layered payload window disappeared"))?;
        let body = self
            .bytes
            .get(&window_index)
            .ok_or_else(|| ReadError::internal("layered payload bytes disappeared"))?;
        Ok((window.range.start, body))
    }
}

enum LayeredPayloadWindowPlan {
    Combined(Vec<LayeredPayloadWindow>),
    Separate {
        sidecars: Vec<LayeredPayloadWindow>,
        packs: Vec<LayeredPayloadWindow>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayeredMemberPreAdmission {
    Proven,
    NeedsSidecars,
    Rejected,
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
        LayeredPayloadWindowMode::Sidecars => (
            LAYERED_SIDECAR_COALESCE_GAP_BYTES,
            LAYERED_SIDECAR_MAX_EXTRA_BYTES,
            LAYERED_SIDECAR_MAX_WINDOW_BYTES,
        ),
        LayeredPayloadWindowMode::Packs => (
            LAYERED_SIDECAR_COALESCE_GAP_BYTES,
            LAYERED_SIDECAR_MAX_EXTRA_BYTES,
            LAYERED_PACK_MAX_WINDOW_BYTES,
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

fn plan_layered_admission_payload_windows(
    members: &[LayeredPayloadMemberRead],
    selected_members: &BTreeSet<usize>,
    pre_admitted_members: &BTreeSet<usize>,
    max_input_bytes: u64,
) -> Result<LayeredPayloadWindowPlan> {
    let sidecars = plan_layered_payload_windows_for_members(
        members,
        selected_members,
        LayeredPayloadWindowMode::Sidecars,
    )?;
    let packs = plan_layered_payload_windows_for_members(
        members,
        selected_members,
        LayeredPayloadWindowMode::Packs,
    )?;
    let combined = plan_layered_payload_windows_for_members(
        members,
        selected_members,
        LayeredPayloadWindowMode::Full,
    )?;
    let split_window_count = sidecars
        .len()
        .checked_add(packs.len())
        .ok_or_else(|| ReadError::internal("layered payload window count overflowed"))?;
    let split_bytes = layered_payload_window_bytes(&sidecars)?
        .checked_add(layered_payload_window_bytes(&packs)?)
        .ok_or_else(|| ReadError::internal("layered payload byte count overflowed"))?;
    let combined_bytes = layered_payload_window_bytes(&combined)?;
    let self_contained = selected_members.iter().all(|index| {
        members
            .get(*index)
            .is_some_and(|member| member.member.member.external_delta_bases().is_empty())
    });
    // Admission can reject after checking indexes; do not overfetch beyond the
    // split successful path or the caller's read budget before that decision.
    let every_member_admitted = selected_members
        .iter()
        .all(|index| pre_admitted_members.contains(index));
    if self_contained
        && every_member_admitted
        && combined.len() < split_window_count
        && combined_bytes <= split_bytes
        && (max_input_bytes == 0 || combined_bytes <= max_input_bytes)
    {
        Ok(LayeredPayloadWindowPlan::Combined(combined))
    } else {
        Ok(LayeredPayloadWindowPlan::Separate { sidecars, packs })
    }
}

fn pre_admit_layered_members(
    members: &[LayeredPayloadMemberRead],
    pack_dir: &Path,
    authenticated_member_oids: Option<&BTreeMap<String, Vec<[u8; 20]>>>,
    admission: Option<(&BTreeSet<ObjectId>, &BTreeSet<ObjectId>)>,
) -> Result<LayeredMemberPreAdmission> {
    let Some((allowed, required)) = admission else {
        return Ok(LayeredMemberPreAdmission::NeedsSidecars);
    };
    let Some(allowed) = object_ids_as_sha1(allowed) else {
        return Ok(LayeredMemberPreAdmission::NeedsSidecars);
    };
    let Some(required) = object_ids_as_sha1(required) else {
        return Ok(LayeredMemberPreAdmission::NeedsSidecars);
    };
    let mut all_members_admitted = true;
    let mut covered = BTreeSet::new();
    for member in members {
        if !member.member.member.external_delta_bases().is_empty() {
            return Ok(LayeredMemberPreAdmission::Rejected);
        }
        let object_ids = if member.complete_local {
            let (object_ids, checksum) = local_layered_member_admission(pack_dir, member)?;
            if object_ids.len() as u64 != member.member.member.object_count()
                || checksum != member.member.member.git_checksum()
            {
                return Ok(LayeredMemberPreAdmission::Rejected);
            }
            let Some(object_ids) = object_ids_as_sha1(&object_ids.into_iter().collect()) else {
                return Ok(LayeredMemberPreAdmission::NeedsSidecars);
            };
            object_ids
        } else {
            let Some(object_ids) = authenticated_member_oids
                .and_then(|object_ids| object_ids.get(member.member.member.pack().blake3()))
            else {
                all_members_admitted = false;
                continue;
            };
            if object_ids.len() as u64 != member.member.member.object_count() {
                return Ok(LayeredMemberPreAdmission::NeedsSidecars);
            }
            object_ids.iter().copied().collect::<BTreeSet<_>>()
        };
        if object_ids.iter().any(|oid| !allowed.contains(oid)) {
            return Ok(LayeredMemberPreAdmission::Rejected);
        }
        covered.extend(object_ids);
    }
    if !all_members_admitted {
        return Ok(LayeredMemberPreAdmission::NeedsSidecars);
    }
    if required.is_subset(&covered) {
        Ok(LayeredMemberPreAdmission::Proven)
    } else {
        Ok(LayeredMemberPreAdmission::Rejected)
    }
}

fn object_ids_as_sha1(object_ids: &BTreeSet<ObjectId>) -> Option<BTreeSet<[u8; 20]>> {
    object_ids
        .iter()
        .map(|object_id| object_id.as_bytes().try_into().ok())
        .collect()
}

fn layered_payload_window_bytes(windows: &[LayeredPayloadWindow]) -> Result<u64> {
    windows.iter().try_fold(0_u64, |total, window| {
        let window_bytes = window
            .range
            .end
            .checked_sub(window.range.start)
            .ok_or_else(|| ReadError::internal("layered payload window underflowed"))?;
        total
            .checked_add(window_bytes)
            .ok_or_else(|| ReadError::internal("layered payload byte count overflowed"))
    })
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
        push_ref_head_bases: BTreeMap::new(),
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
        push_ref_head_bases: BTreeMap::new(),
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
        .filter_map(|entry| entry.as_ref().map(|(head, _, _)| head.clone()))
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
    let push_ref_head_bases = ref_names
        .iter()
        .cloned()
        .zip(loaded.iter().map(|entry| {
            entry
                .as_ref()
                .map(|(_, etag, body)| (body.clone(), etag.clone()))
        }))
        .collect();
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
        push_ref_head_bases,
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

/// Load complete checkpoint catalog/visibility metadata and frontier controls.
///
/// Pack bodies remain in their immutable sources. Before the first checkpoint,
/// this view retains complete capsules for callers using the inline Git reader.
/// Ordinary advertised-ref fetches use the footer-only entry point instead.
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

/// Load only the root-owned layered checkpoint, excluding newer per-ref heads.
///
/// Physical maintenance uses this view to preserve the checkpoint's transaction
/// positions while concurrent pushes remain in their independent ref heads.
/// The root must name a layered checkpoint with no root-owned capsule frontier;
/// this is not a current-ref advertisement or ordinary repository read.
pub async fn open_compacted_view_from_root(
    router: &StoreLayout<Store>,
    snapshot: crab_metadata::capsule_protocol::RootSnapshot,
    limits: CapsuleReadLimits,
) -> Result<CapsuleRepositoryView> {
    let root = snapshot.record().root();
    if root.checkpoint().is_none() || !root.capsule_frontier().is_empty() {
        return Err(ReadError::internal(
            "compacted view requires a layered checkpoint without a root frontier",
        ));
    }
    let pointer = root
        .checkpoint()
        .ok_or_else(|| ReadError::internal("compacted view requires a layered checkpoint"))?;
    let checkpoint = load_layered_checkpoint(router, pointer, limits).await?;
    compacted_view_from_checkpoint(snapshot, checkpoint, limits)
}

/// Bind an already loaded complete checkpoint to its exact committed root.
///
/// This performs the same pointer and visibility validation as a stored read,
/// without I/O. It excludes newer ref heads and rejects footer-only metadata.
pub fn compacted_view_from_checkpoint(
    snapshot: crab_metadata::capsule_protocol::RootSnapshot,
    checkpoint: LayeredCheckpoint,
    limits: CapsuleReadLimits,
) -> Result<CapsuleRepositoryView> {
    let root = snapshot.record().root();
    let pointer = root
        .checkpoint()
        .ok_or_else(|| ReadError::internal("compacted view requires a layered checkpoint"))?;
    if !root.capsule_frontier().is_empty()
        || checkpoint.is_control_only()
        || !checkpoint.matches_pointer(pointer)?
    {
        return Err(corrupt_path(
            "capsule-protocol layered checkpoint",
            "compacted view requires the complete checkpoint authenticated by its root",
        ));
    }
    if pointer.size() > limits.max_capsule_bytes {
        return Err(ReadError::CapsuleReadLimit {
            resource: "layered checkpoint bytes",
            maximum: limits.max_capsule_bytes,
        });
    }
    visibility_index_for_checkpoint(Some(&checkpoint))?;
    let mut tip_bound_transitions = BTreeMap::new();
    append_checkpoint_tip_bound_transitions(
        &mut tip_bound_transitions,
        checkpoint.visibility_transitions(),
    )?;
    Ok(CapsuleRepositoryView {
        refs: root.refs().clone(),
        peeled_refs: root.peeled_refs().clone(),
        root: snapshot,
        browse_indexes: None,
        checkpoint: Some(checkpoint),
        capsules: Vec::new(),
        capsule_controls: Vec::new(),
        tip_bound_transitions,
        visible_ref_transactions: BTreeMap::new(),
        ref_capsule_counts: BTreeMap::new(),
        capsule_run_pointers: Vec::new(),
        capsule_run_sources: Vec::new(),
        capsule_run_indexes: BTreeMap::new(),
        capsule_run_member_oids: BTreeMap::new(),
        frontier_object_admission: BTreeMap::new(),
    })
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

/// Load complete visibility and catalog controls for logical checkpoint publication.
///
/// Unlike ordinary footer-only fetch admission, this orders every visible
/// transaction and materializes its metadata, including before the first
/// checkpoint. Layered Git pack bodies remain cold.
pub async fn open_view_from_root_for_checkpoint(
    router: &StoreLayout<Store>,
    snapshot: crab_metadata::capsule_protocol::RootSnapshot,
    limits: CapsuleReadLimits,
) -> Result<CapsuleRepositoryView> {
    let (heads, active) = capture_ref_heads(router, snapshot.record().root()).await?;
    assemble_layered_control_view(router, snapshot, limits, heads, active, false).await
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
    if matches!(checkpoint_load, CheckpointLoad::LayeredControl)
        || (matches!(checkpoint_load, CheckpointLoad::Control)
            && snapshot.record().root().checkpoint().is_some())
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
            Some(pointer) => load_layered_checkpoint(router, pointer, limits)
                .await
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
    let mut capsule_run_indexes = BTreeMap::new();
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
            capsule_run_indexes.insert(pointer.hash().to_owned(), run.git_index_ranges()?);
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
    let mut visibility_index = visibility_index_for_checkpoint(checkpoint.as_ref())?;
    for capsule in &root_capsules {
        if capsule.visibility_delta()?.is_some() {
            apply_capsule_visibility_index(capsule, &mut visibility_index)?;
        }
    }
    let ordered = order_ref_capsules(
        snapshot.record().root().refs(),
        snapshot.record().root().peeled_refs(),
        visibility_index,
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
        browse_indexes: None,
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
        capsule_run_indexes,
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
            Some(checkpoint)
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
    if let Some(checkpoint) = checkpoint.as_ref() {
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
    let mut capsule_run_indexes = BTreeMap::new();
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
            capsule_run_indexes.insert(pointer.hash().to_owned(), run.git_index_ranges()?);
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
            browse_indexes: None,
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
            capsule_run_indexes,
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
    let mut visibility_index = visibility_index_for_checkpoint(checkpoint.as_ref())?;
    for capsule in &root_capsules {
        if capsule.visibility_delta().is_some() {
            apply_capsule_control_visibility_index(capsule, &mut visibility_index)?;
        }
    }
    let ordered = order_ref_controls(
        snapshot.record().root().refs(),
        snapshot.record().root().peeled_refs(),
        visibility_index,
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
        browse_indexes: None,
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
        capsule_run_indexes,
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
            .filter_map(|entry| entry.as_ref().map(|(head, _, _)| head.clone()))
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
            Bytes,
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
        Ok(Some((head, etag, body)))
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
    mut visibility: crab_metadata::git_visibility::GitVisibilityIndex,
    predecessors: &BTreeMap<String, BTreeSet<String>>,
    mut pending: BTreeMap<String, Capsule>,
) -> Result<Vec<Capsule>> {
    let mut refs = base_refs.clone();
    let mut peeled = base_peeled.clone();
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
            apply_capsule_visibility_index(&capsule, &mut visibility)?;
        }
        applied.insert(capsule.transaction_id().to_owned());
        ordered.push(capsule);
    }
    Ok(ordered)
}

fn order_ref_controls(
    base_refs: &BTreeMap<String, String>,
    base_peeled: &BTreeMap<String, String>,
    mut visibility: crab_metadata::git_visibility::GitVisibilityIndex,
    predecessors: &BTreeMap<String, BTreeSet<String>>,
    mut pending: BTreeMap<String, CapsuleControl>,
) -> Result<Vec<CapsuleControl>> {
    let mut refs = base_refs.clone();
    let mut peeled = base_peeled.clone();
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
            apply_capsule_control_visibility_index(&capsule, &mut visibility)?;
        }
        applied.insert(capsule.transaction_id().to_owned());
        ordered.push(capsule);
    }
    Ok(ordered)
}

fn control_is_ready(
    capsule: &CapsuleControl,
    refs: &BTreeMap<String, String>,
    visibility_index: &crab_metadata::git_visibility::GitVisibilityIndex,
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
                if !visibility_index.contains_ref(edit.ref_name()) {
                    return Ok(false);
                }
            }
            None if visibility.replaces => {}
            None => {
                let Some(old_oid) = visibility.old_oid.as_deref() else {
                    continue;
                };
                if !visibility_index.contains_hex_in_any_ref(old_oid) {
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
    visibility_index: &crab_metadata::git_visibility::GitVisibilityIndex,
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
                if !visibility_index.contains_ref(edit.ref_name()) {
                    return Ok(false);
                }
            }
            None if visibility.replaces => {}
            None => {
                let Some(old_oid) = visibility.old_oid.as_deref() else {
                    continue;
                };
                if !visibility_index.contains_hex_in_any_ref(old_oid) {
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
#[path = "capsule_protocol/dependency_tests.rs"]
mod dependency_tests;

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use crab_metadata::capsule_protocol::{
        CapsuleGitPack, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
        CapsuleVisibilityDelta, CapsuleVisibilitySnapshot, CheckpointPointer, HistorySegment,
        HistorySegmentState, PackLayer, PackRange, RepositoryRoot,
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

    fn visibility_index(
        refs: BTreeMap<String, Vec<String>>,
    ) -> crab_metadata::git_visibility::GitVisibilityIndex {
        let pack_index_hash = if refs.is_empty() {
            String::new()
        } else {
            "0".repeat(64)
        };
        crab_metadata::git_visibility::GitVisibilityIndex::new(
            0,
            pack_index_hash,
            "0".repeat(64),
            refs,
        )
        .unwrap()
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
            run.control_offset(),
            run.control_size(),
            run.footer_hash(),
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
            run.control_offset(),
            run.control_size(),
            run.footer_hash(),
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
        layered_member_read_with_pack_size(path, start, seed, 10)
    }

    fn layered_member_read_with_pack_size(
        path: &str,
        start: u64,
        seed: char,
        pack_size: usize,
    ) -> LayeredMemberRead {
        let bytes = |length: usize| vec![seed as u8; length];
        let sidecar_start = start + pack_size as u64;
        let pack = PackRange::new(start, &bytes(pack_size)).unwrap();
        let index = PackRange::new(sidecar_start, &bytes(10)).unwrap();
        let reverse = PackRange::new(sidecar_start + 10, &bytes(10)).unwrap();
        let locator = PackRange::new(sidecar_start + 20, &bytes(10)).unwrap();
        let member =
            PackMemberDescriptor::new(pack, index, reverse, locator, "0".repeat(40), 1, Vec::new())
                .unwrap();
        LayeredMemberRead {
            source_path: object_store::path::Path::from(path),
            source_size: sidecar_start + 30,
            pack_id: crab_xet::hash::MerkleHash::from_hex(&seed.to_string().repeat(64)).unwrap(),
            member,
        }
    }

    fn dense_layered_payload_members(
        count: usize,
        pack_size: usize,
    ) -> (Vec<LayeredPayloadMemberRead>, u64) {
        let stride = u64::try_from(pack_size).unwrap() + 30;
        let source_size = stride * u64::try_from(count).unwrap();
        let members = (0..count)
            .map(|index| {
                let start = u64::try_from(index).unwrap() * stride;
                let seed = char::from(b"0123456789abcdef"[index % 16]);
                let mut member =
                    layered_member_read_with_pack_size("runs/a", start, seed, pack_size);
                member.source_size = source_size;
                LayeredPayloadMemberRead {
                    member,
                    complete_local: false,
                }
            })
            .collect();
        (members, source_size)
    }

    #[test]
    fn dense_layered_admission_uses_one_combined_window() {
        let (members, source_size) = dense_layered_payload_members(16, 1024 * 1024);
        let selected = (0..members.len()).collect::<BTreeSet<_>>();
        let plan = plan_layered_admission_payload_windows(&members, &selected, &BTreeSet::new(), 0)
            .unwrap();
        assert!(matches!(plan, LayeredPayloadWindowPlan::Separate { .. }));
        let plan =
            plan_layered_admission_payload_windows(&members, &selected, &selected, 0).unwrap();
        let LayeredPayloadWindowPlan::Combined(windows) = plan else {
            panic!("authenticated dense members should share one combined source window");
        };

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].range, 0..source_size);
    }

    #[test]
    fn authenticated_run_admission_allows_combined_payload_before_index_reads() {
        let (members, _) = dense_layered_payload_members(1, 1024);
        let member = &members[0].member.member;
        let oid = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
        let admitted = BTreeMap::from([(
            member.pack().blake3().to_owned(),
            vec![oid.as_bytes().try_into().unwrap()],
        )]);
        let allowed = BTreeSet::from([oid]);
        let required = allowed.clone();

        let result = pre_admit_layered_members(
            &members,
            Path::new("."),
            Some(&admitted),
            Some((&allowed, &required)),
        )
        .unwrap();

        assert_eq!(result, LayeredMemberPreAdmission::Proven);
    }

    #[test]
    fn unauthorized_run_member_is_rejected_before_payload_planning() {
        let (members, _) = dense_layered_payload_members(1, 1024);
        let member = &members[0].member.member;
        let oid = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
        let admitted = BTreeMap::from([(
            member.pack().blake3().to_owned(),
            vec![oid.as_bytes().try_into().unwrap()],
        )]);
        let unauthorized = ObjectId::from_hex(b"1123456789012345678901234567890123456789").unwrap();
        let allowed = BTreeSet::from([unauthorized]);
        let required = BTreeSet::from([unauthorized]);

        let result = pre_admit_layered_members(
            &members,
            Path::new("."),
            Some(&admitted),
            Some((&allowed, &required)),
        )
        .unwrap();

        assert_eq!(result, LayeredMemberPreAdmission::Rejected);
    }

    #[test]
    fn missing_run_admission_requires_sidecar_first_fallback() {
        let (members, _) = dense_layered_payload_members(1, 1024);
        let oid = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
        let allowed = BTreeSet::from([oid]);
        let required = allowed.clone();

        let result =
            pre_admit_layered_members(&members, Path::new("."), None, Some((&allowed, &required)))
                .unwrap();

        assert_eq!(result, LayeredMemberPreAdmission::NeedsSidecars);
    }

    #[tokio::test]
    async fn layered_payload_reads_can_run_in_spawned_tasks() {
        let store = Store::new(Arc::new(InMemory::new()));
        let path = object_store::path::Path::from("runs/spawned");
        let body = Bytes::from_static(b"first-second");
        store.put(&path, body.clone()).await.unwrap();
        let windows = vec![LayeredPayloadWindow {
            path,
            range: 0..body.len() as u64,
            member_indices: vec![0],
            useful_bytes: body.len() as u64,
        }];

        let (fetched, _) =
            tokio::spawn(async move { fetch_layered_payload_windows(&store, &windows).await })
                .await
                .unwrap()
                .unwrap();

        assert_eq!(fetched, BTreeMap::from([(0, body)]));
    }

    #[tokio::test]
    async fn dense_layered_admission_reads_one_range_per_source() {
        let (members, source_size) = dense_layered_payload_members(16, 64 * 1024);
        let selected = (0..members.len()).collect::<BTreeSet<_>>();
        let pre_admitted = selected.clone();
        let LayeredPayloadWindowPlan::Combined(windows) =
            plan_layered_admission_payload_windows(&members, &selected, &pre_admitted, 0).unwrap()
        else {
            panic!("authenticated dense members should share one combined source window");
        };
        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(Arc::new(InMemory::new())).with_storage_observer(observer.clone());
        let path = members[0].member.source_path.clone();
        store
            .put(&path, Bytes::from(vec![0; source_size as usize]))
            .await
            .unwrap();
        observer.observations.lock().unwrap().clear();

        let (_, bytes_read) = fetch_layered_payload_windows(&store, &windows)
            .await
            .unwrap();

        let observations = observer.observations.lock().unwrap();
        assert_eq!(bytes_read, source_size);
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].operation, StorageOperation::Range);
        assert_eq!(observations[0].bytes_read, source_size);
    }

    #[test]
    fn sparse_layered_admission_keeps_split_ranges_when_combining_overfetches() {
        let (members, _) = dense_layered_payload_members(16, 1024 * 1024);
        let selected = BTreeSet::from([0, 15]);
        let plan =
            plan_layered_admission_payload_windows(&members, &selected, &selected, 0).unwrap();
        let LayeredPayloadWindowPlan::Separate { sidecars, packs } = plan else {
            panic!("sparse selected members should keep bounded split ranges");
        };

        assert_eq!(sidecars.len(), 2);
        assert_eq!(packs.len(), 2);
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
    fn layered_pack_windows_coalesce_selected_members_up_to_sixty_four_mib() {
        let pack_size = 4 * 1024 * 1024;
        let members = (0..5)
            .map(|index| LayeredPayloadMemberRead {
                member: layered_member_read_with_pack_size(
                    "runs/a",
                    index * (pack_size as u64 + 30),
                    char::from(b'a' + u8::try_from(index).unwrap()),
                    pack_size,
                ),
                complete_local: false,
            })
            .collect::<Vec<_>>();

        let windows =
            plan_layered_payload_windows(&members, LayeredPayloadWindowMode::Packs).unwrap();

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].range.end, 5 * pack_size as u64 + 120);
        assert_eq!(windows[0].useful_bytes, 5 * pack_size as u64);
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
        assert_eq!(identity.content_hash, *blake3::hash(&[b'a'; 10]).as_bytes());
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
            2,
            1,
            1,
            "5".repeat(64),
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
    async fn layered_control_view_range_reads_checkpoint_suffix_without_pack_body() {
        for visibility in [
            None,
            Some(
                CapsuleVisibilitySnapshot::from_index(&visibility_index(BTreeMap::new())).unwrap(),
            ),
        ] {
            let inner = Arc::new(InMemory::new());
            seed_one_capsule(inner.clone(), None).await;
            let seed_store = Store::new(inner.clone());
            let seed_router = StoreLayout::new(seed_store.clone(), "repositories/test".to_owned());
            let current = load_root(&seed_router).await.unwrap();
            let layer = PackLayer::build(
                &CapsuleGitPack::new(
                    Bytes::from_static(b"PACK checkpoint control test body"),
                    Bytes::from_static(b"index"),
                    Bytes::from_static(b"reverse"),
                    Bytes::from_static(b"locator"),
                    "4".repeat(40),
                    1,
                )
                .unwrap(),
            )
            .unwrap();
            seed_store
                .put(
                    &seed_router.capsule_pack_layer_path(layer.hash()),
                    layer.bytes().clone(),
                )
                .await
                .unwrap();
            let checkpoint = LayeredCheckpoint::build(
                current.record().root().generation(),
                current.record().digest(),
                vec![layer.source_descriptor().unwrap()],
                crab_metadata::capsule_protocol::PointerCatalog::new(),
                visibility,
            )
            .unwrap();
            let checkpoint_pointer = CheckpointPointer::new_layered(
                checkpoint.hash(),
                checkpoint.bytes().len() as u64,
                checkpoint.control_offset(),
                checkpoint.control_size(),
                checkpoint.footer_hash(),
                checkpoint.covered_generation(),
                checkpoint.covered_root_digest(),
                checkpoint.pack_count().unwrap(),
                checkpoint.object_count().unwrap(),
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
            let view = open_view_from_root_with_layered_control(
                &router,
                load_root(&router).await.unwrap(),
                TEST_LIMITS,
            )
            .await
            .unwrap();

            assert_eq!(
                view.layered_checkpoint().unwrap().sources(),
                &[layer.source_descriptor().unwrap()]
            );
            assert!(view.layered_checkpoint().unwrap().is_control_only());
            let observations = observer.observations.lock().unwrap();
            assert_eq!(
                observations.iter().map(|read| read.bytes_read).sum::<u64>(),
                root.bytes().len() as u64 + checkpoint.bytes().len() as u64
                    - checkpoint.control_offset()
            );
            let operations = observations
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
        let pointer = CapsulePointer::new(
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
    async fn dependency_verification_checks_large_blobs_in_a_bare_workspace() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let source = tempfile::tempdir().unwrap();
        let source_git = source.path().join("source.git");
        let initialized = Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&source_git)
            .output()
            .unwrap();
        assert!(initialized.status.success());
        let run_git = |arguments: &[&str], input: &[u8]| {
            let mut child = Command::new("git")
                .arg("--git-dir")
                .arg(&source_git)
                .args(arguments)
                .env("GIT_AUTHOR_NAME", "Crab Test")
                .env("GIT_AUTHOR_EMAIL", "crab@example.invalid")
                .env("GIT_COMMITTER_NAME", "Crab Test")
                .env("GIT_COMMITTER_EMAIL", "crab@example.invalid")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(input).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "git {arguments:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        let blob_oid = run_git(&["hash-object", "-w", "--stdin"], &[0xa5; 4096]);
        let tree_input = format!("100644 blob {blob_oid}\tlarge.bin\n");
        let tree_oid = run_git(&["mktree"], tree_input.as_bytes());
        let commit_oid = run_git(&["commit-tree", &tree_oid, "-m", "large blob"], b"");
        run_git(&["update-ref", "refs/heads/main", &commit_oid], b"");
        run_git(&["repack", "-a", "-d"], b"");

        let pack_path = std::fs::read_dir(source_git.join("objects/pack"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "pack")
            })
            .unwrap();
        let pack_bytes = std::fs::read(&pack_path).unwrap();
        let indexed_dir = tempfile::tempdir().unwrap();
        let indexed = crab_git::pack::install_pack_file_from_path(
            indexed_dir.path(),
            &pack_path,
            blake3::hash(&pack_bytes).to_hex().as_ref(),
            0,
            true,
        )
        .unwrap();
        let mut locations = crab_git::pack_locator::PackLocationIter::open(
            &indexed.idx_path,
            &indexed.rev_path,
            pack_bytes.len() as u64,
        )
        .unwrap();
        let object_count = locations.object_count();
        let object_ids = locations
            .by_ref()
            .map(|location| location.unwrap().oid)
            .collect::<Vec<_>>();
        let kinds = crab_git::object_kinds_from_git_dir(&source_git, &object_ids).unwrap();
        let ordered_kinds = object_ids
            .iter()
            .map(|oid| *kinds.get(oid).unwrap())
            .collect::<Vec<_>>();
        let checksum = gix_hash::ObjectId::from_hex(indexed.git_sha1.as_bytes()).unwrap();
        let locator =
            crab_git::pack_locator::encode_pack_kind_metadata(checksum, &ordered_kinds).unwrap();
        let pack = crab_metadata::capsule_protocol::CapsuleGitPack::new(
            Bytes::from(pack_bytes),
            Bytes::from(std::fs::read(&indexed.idx_path).unwrap()),
            Bytes::from(std::fs::read(&indexed.rev_path).unwrap()),
            Bytes::from(locator),
            indexed.git_sha1,
            object_count,
        )
        .unwrap();

        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "repositories/large-blob".to_owned());
        let initial = RootRecord::encode(
            RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        let transaction = CapsuleTransaction::new(
            initial.digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some(commit_oid.clone()),
                None,
            )],
        )
        .unwrap();
        let transaction_id = transaction.id().unwrap();
        let capsule = Capsule::build(&transaction, vec![pack], Vec::new()).unwrap();
        let run = CapsuleRun::leaf(capsule).unwrap();
        store
            .put(&router.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let pointer = CapsulePointer::new(
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
        let root = initial
            .root()
            .advance(
                initial.digest(),
                BTreeMap::from([("refs/heads/main".to_owned(), commit_oid)]),
                BTreeMap::new(),
                vec![pointer],
                &transaction_id,
            )
            .unwrap();
        let root = RootRecord::encode(root).unwrap();
        store
            .create_strict(&router.capsule_root_path(), root.bytes().clone())
            .await
            .unwrap();
        let view = open_view(&router, TEST_LIMITS).await.unwrap();
        let proof = verify_reachable_dependencies(
            &router,
            &view,
            CapsuleDependencyLimits {
                max_git_bytes: 8 * 1024 * 1024,
                pointer_scan: crab_git::walk::PointerScanLimits {
                    objects: 16,
                    lookups: 64,
                    allocation_bytes: 1024 * 1024,
                },
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(proof.reachable_crab_pointers, 0);
        assert_eq!(proof.reachable_lfs_objects, 0);
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
            visibility_index(BTreeMap::from([(
                "refs/heads/main".to_owned(),
                vec![old_tip],
            )])),
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
        // Fetch hints must accept the same authenticated borrowed base as the
        // ref-ordering reader, rather than rejecting this valid new branch.
        let full = build_tip_bound_transitions(&ordered, &[]).unwrap();
        assert_eq!(
            full["refs/heads/feature"][0].old_oid.unwrap().to_string(),
            new_tip
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
            visibility_index(BTreeMap::new()),
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
