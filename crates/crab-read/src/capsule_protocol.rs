//! Verified loading of one capsule-protocol root and its bounded capsule frontier.

use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleGitPackDescriptor, CapsulePointer, CapsuleRun, Checkpoint, CheckpointPointer,
    PointerCatalog, RootRecord, load_root,
};
use crab_storage::{Store, StoreLayout};
use futures_util::future::try_join_all;
use futures_util::{StreamExt, TryStreamExt};
use object_store::ObjectMeta;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::{ReadError, Result};

/// Caller-owned memory admission for one capsule-protocol repository view.
#[derive(Debug, Clone, Copy)]
pub struct CapsuleReadLimits {
    /// Largest individual capsule body accepted by this reader.
    pub max_capsule_bytes: u64,
    /// Largest aggregate capsule frontier accepted by this reader.
    pub max_frontier_bytes: u64,
}

/// One authenticated repository root and every post-checkpoint capsule it names.
#[derive(Debug, Clone)]
pub struct CapsuleRepositoryView {
    root: crab_metadata::capsule_protocol::RootSnapshot,
    checkpoint: Option<Checkpoint>,
    capsules: Vec<Capsule>,
    refs: BTreeMap<String, String>,
    peeled_refs: BTreeMap<String, String>,
    visible_ref_transactions: BTreeMap<String, String>,
    capsule_run_pointers: Vec<CapsulePointer>,
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
        self.checkpoint.as_ref()
    }

    /// Return every verified post-checkpoint capsule in publication order.
    #[must_use]
    pub fn capsules(&self) -> &[Capsule] {
        &self.capsules
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

    /// Return every immutable capsule run reachable from this exact view.
    #[must_use]
    pub fn capsule_run_pointers(&self) -> &[CapsulePointer] {
        &self.capsule_run_pointers
    }

    /// Return a digest that changes with the root or any visible per-ref position.
    #[must_use]
    pub fn state_digest(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crab capsule repository view v2\0");
        hasher.update(self.root.record().digest().as_bytes());
        for (ref_name, transaction_id) in &self.visible_ref_transactions {
            hasher.update(ref_name.as_bytes());
            hasher.update(&[0]);
            hasher.update(transaction_id.as_bytes());
        }
        hasher.finalize().to_hex().to_string()
    }

    /// Materialize the complete generation-pinned external pointer catalog.
    pub fn pointer_catalog(&self) -> Result<PointerCatalog> {
        let mut catalog = self
            .checkpoint
            .as_ref()
            .map(Checkpoint::pointer_catalog)
            .transpose()?
            .unwrap_or_else(PointerCatalog::new);
        for capsule in &self.capsules {
            if let Some(delta) = capsule.pointer_catalog_delta()? {
                catalog.apply(&delta)?;
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
        let workspace = tempfile::tempdir()?;
        let installed = install_git_packs(self, workspace.path(), max_input_bytes).await?;
        let artifacts = self
            .checkpoint
            .iter()
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

    /// Materialize the complete view-bound Git visibility proof.
    pub fn git_visibility_index(
        &self,
    ) -> Result<crab_metadata::git_visibility::GitVisibilityIndex> {
        let mut refs = self
            .checkpoint
            .as_ref()
            .map(Checkpoint::visibility_snapshot)
            .transpose()?
            .flatten()
            .map(|snapshot| snapshot.refs().clone())
            .unwrap_or_default();

        for capsule in &self.capsules {
            apply_capsule_visibility(capsule, &mut refs)?;
        }

        let packs = self.git_pack_manifest_entries()?;
        let manifest = self.git_manifest(&packs)?;
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
        crab_metadata::git_visibility::GitVisibilityIndex::new(
            manifest.generation,
            manifest.pack_index_hash,
            manifest.git_validation_digest,
            refs,
        )
        .map_err(Into::into)
    }

    fn git_pack_manifest_entries(
        &self,
    ) -> Result<Vec<crab_metadata::manifests::PackManifestEntry>> {
        self.checkpoint
            .iter()
            .cloned()
            .map(GitPackContainer::Checkpoint)
            .chain(self.capsules.iter().cloned().map(GitPackContainer::Capsule))
            .flat_map(|container| {
                let descriptors = container.git_packs().to_vec();
                descriptors.into_iter().map(move |descriptor| {
                    let pack = container.section_bytes(descriptor.pack_section())?;
                    let pack_id = blake3::hash(&pack).to_hex().to_string();
                    Ok(crab_metadata::manifests::PackManifestEntry {
                        pack_id: pack_id.clone(),
                        size: pack.len() as u64,
                        content_hash: pack_id,
                        ref_tips: Vec::new(),
                        object_count: descriptor.object_count(),
                    })
                })
            })
            .collect()
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

/// Install every capsule Git pack into a local Git object database.
///
/// Pack bodies, indexes, reverse indexes, and locator metadata are validated
/// as one descriptor before any new pack becomes visible in the destination.
pub async fn install_git_packs(
    view: &CapsuleRepositoryView,
    git_dir: &Path,
    max_input_bytes: u64,
) -> Result<Vec<PathBuf>> {
    let checkpoint = view.checkpoint.clone();
    let capsules = view.capsules.clone();
    let git_dir = git_dir.to_owned();
    tokio::task::spawn_blocking(move || {
        let pack_dir = git_dir.join("objects").join("pack");
        std::fs::create_dir_all(&pack_dir)?;
        let mut total = 0_u64;
        let mut installed = Vec::new();
        let containers = checkpoint
            .into_iter()
            .map(GitPackContainer::Checkpoint)
            .chain(capsules.into_iter().map(GitPackContainer::Capsule));
        for container in containers {
            for descriptor in container.git_packs() {
                let pack_bytes = container.section_bytes(descriptor.pack_section())?;
                let pack_size = u64::try_from(pack_bytes.len()).map_err(|_| {
                    ReadError::internal("Git pack size cannot be represented as u64")
                })?;
                total = total.checked_add(pack_size).ok_or_else(|| {
                    ReadError::internal("capsule-protocol Git intake size overflowed")
                })?;
                if max_input_bytes > 0 && total > max_input_bytes {
                    return Err(ReadError::CapsuleReadLimit {
                        resource: "Git pack intake",
                        maximum: max_input_bytes,
                    });
                }
                let temporary = tempfile::Builder::new()
                    .prefix(".crab-capsule-pack-")
                    .tempdir_in(&pack_dir)?;
                let pack_path = temporary.path().join("pack.pack");
                let index_path = temporary.path().join("pack.idx");
                let reverse_path = temporary.path().join("pack.rev");
                std::fs::write(&pack_path, &pack_bytes)?;
                std::fs::write(
                    &index_path,
                    container.section_bytes(descriptor.index_section())?,
                )?;
                std::fs::write(
                    &reverse_path,
                    container.section_bytes(descriptor.reverse_index_section())?,
                )?;
                let locator = container.section_bytes(descriptor.locator_section())?;
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
                        "pack descriptor does not match its index",
                    ));
                }
                crab_git::pack_locator::validate_pack_kind_metadata(
                    &locator,
                    locations.pack_checksum(),
                    locations.object_count(),
                )
                .map_err(|error| corrupt_path("capsule Git locator", error.to_string()))?;
                let canonical_name = blake3::hash(&pack_bytes).to_hex().to_string();
                let final_pack = pack_dir.join(format!("pack-{canonical_name}.pack"));
                let final_index = pack_dir.join(format!("pack-{canonical_name}.idx"));
                let final_reverse = pack_dir.join(format!("pack-{canonical_name}.rev"));
                let result =
                    if final_pack.exists() && final_index.exists() && final_reverse.exists() {
                        crab_git::pack::install_pack_file_from_path(
                            &pack_dir,
                            &pack_path,
                            &canonical_name,
                            max_input_bytes,
                            false,
                        )
                    } else {
                        crab_git::pack::install_pack_files_from_paths(
                            &pack_dir,
                            &pack_path,
                            &index_path,
                            &reverse_path,
                            &canonical_name,
                            max_input_bytes,
                            descriptor.object_count(),
                        )
                    }
                    .map_err(|error| corrupt_path("capsule Git pack", error.to_string()))?;
                if result.git_sha1 != descriptor.git_checksum() {
                    return Err(corrupt_path(
                        "capsule Git pack",
                        "installed pack checksum does not match its descriptor",
                    ));
                }
                installed.push(result.pack_path);
            }
        }
        Ok(installed)
    })
    .await
    .map_err(|error| ReadError::Internal(format!("capsule pack install worker failed: {error}")))?
}

enum GitPackContainer {
    Checkpoint(Checkpoint),
    Capsule(Capsule),
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
    assemble_view(router, snapshot, limits, heads, active, None).await
}

/// Load only the independently mutable ref heads needed by an explicit push.
///
/// The returned view is authoritative for `ref_names`. Other refs may reflect
/// the compacted root or an atomic transaction shared with a selected ref, but
/// callers must not use them as current values.
pub async fn open_view_from_root_for_refs(
    router: &StoreLayout<Store>,
    snapshot: crab_metadata::capsule_protocol::RootSnapshot,
    ref_names: &BTreeSet<String>,
    limits: CapsuleReadLimits,
) -> Result<CapsuleRepositoryView> {
    let (heads, active) =
        capture_selected_ref_heads(router, snapshot.record().root(), ref_names).await?;
    assemble_view(router, snapshot, limits, heads, active, Some(ref_names)).await
}

async fn assemble_view(
    router: &StoreLayout<Store>,
    snapshot: crab_metadata::capsule_protocol::RootSnapshot,
    limits: CapsuleReadLimits,
    heads: Vec<crab_metadata::capsule_protocol::CapsuleRefHead>,
    active: BTreeSet<String>,
    selected_refs: Option<&BTreeSet<String>>,
) -> Result<CapsuleRepositoryView> {
    let mut refs = snapshot.record().root().refs().clone();
    let mut peeled_refs = snapshot.record().root().peeled_refs().clone();
    let mut pointers = snapshot.record().root().capsule_frontier().to_vec();
    let mut visible_ref_transactions = BTreeMap::new();
    let mut ref_frontiers = BTreeMap::new();
    let mut expected_refs = refs.clone();
    let mut expected_peeled = peeled_refs.clone();
    for head in &heads {
        let state = head.visible(&active);
        if let Some(transaction_id) = state.transaction_id() {
            visible_ref_transactions.insert(head.ref_name().to_owned(), transaction_id.to_owned());
        }
        if state.transaction_id()
            == snapshot
                .record()
                .root()
                .compacted_ref_transactions()
                .get(head.ref_name())
                .map(String::as_str)
        {
            continue;
        }
        match state.oid() {
            Some(oid) => {
                expected_refs.insert(head.ref_name().to_owned(), oid.to_owned());
                match state.peeled_oid() {
                    Some(peeled) => {
                        expected_peeled.insert(head.ref_name().to_owned(), peeled.to_owned());
                    }
                    None => {
                        expected_peeled.remove(head.ref_name());
                    }
                }
            }
            None => {
                expected_refs.remove(head.ref_name());
                expected_peeled.remove(head.ref_name());
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
        ref_frontiers.insert(head.ref_name().to_owned(), state.frontier().to_vec());
    }
    admit_frontier(&pointers, limits)?;
    let checkpoint = async {
        match snapshot.record().root().checkpoint() {
            Some(pointer) => load_checkpoint(router, pointer, limits).await.map(Some),
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
    let mut required_transactions = BTreeSet::new();
    for (ref_name, frontier) in &ref_frontiers {
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
            Some(compacted) => transaction_ids
                .iter()
                .position(|transaction_id| *transaction_id == compacted)
                .map(|index| index + 1)
                .ok_or_else(|| {
                    corrupt_path(
                        "capsule-protocol ref heads",
                        format!("ref {ref_name} does not extend its compacted transaction"),
                    )
                })?,
            None => 0,
        };
        required_transactions.extend(
            transaction_ids[start..]
                .iter()
                .map(|transaction_id| (*transaction_id).to_owned()),
        );
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
    let mut visibility_refs = checkpoint
        .as_ref()
        .map(Checkpoint::visibility_snapshot)
        .transpose()?
        .flatten()
        .map(|visibility| visibility.refs().clone())
        .unwrap_or_default();
    for capsule in &root_capsules {
        if capsule.visibility_delta()?.is_some() {
            apply_capsule_visibility(capsule, &mut visibility_refs)?;
        }
    }
    let ordered = order_ref_capsules(
        snapshot.record().root().refs(),
        snapshot.record().root().peeled_refs(),
        &visibility_refs,
        journal_capsules,
    )?;
    for capsule in &ordered {
        apply_capsule_refs(capsule, &mut refs, &mut peeled_refs)?;
    }
    let refs_match = selected_refs.map_or_else(
        || refs == expected_refs && peeled_refs == expected_peeled,
        |selected| {
            selected.iter().all(|name| {
                refs.get(name) == expected_refs.get(name)
                    && peeled_refs.get(name) == expected_peeled.get(name)
            })
        },
    );
    if !refs_match {
        return Err(corrupt_path(
            "capsule-protocol ref heads",
            "materialized capsules do not match visible ref-head state",
        ));
    }
    root_capsules.extend(ordered);
    Ok(CapsuleRepositoryView {
        root: snapshot,
        checkpoint,
        capsules: root_capsules,
        refs,
        peeled_refs,
        visible_ref_transactions,
        capsule_run_pointers: pointers,
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
    mut pending: BTreeMap<String, Capsule>,
) -> Result<Vec<Capsule>> {
    let mut refs = base_refs.clone();
    let mut peeled = base_peeled.clone();
    let mut visibility = base_visibility.clone();
    let mut ordered = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let ready = pending
            .iter()
            .find_map(
                |(id, capsule)| match capsule_is_ready(capsule, &refs, &visibility) {
                    Ok(true) => Some(Ok(id.clone())),
                    Ok(false) => None,
                    Err(error) => Some(Err(error)),
                },
            )
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
        ordered.push(capsule);
    }
    Ok(ordered)
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
            refs.remove(edit.ref_name());
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
        let prior = match edit.expected_old() {
            Some(expected_old) => {
                if visibility.old_oid.as_deref() != Some(expected_old) {
                    return Err(corrupt_path(
                        "capsule Git visibility",
                        "visibility evidence does not match the expected old ref",
                    ));
                }
                refs.get(edit.ref_name()).map(Vec::as_slice)
            }
            None if visibility.replaces => None,
            None => visibility.old_oid.as_deref().and_then(|old_oid| {
                refs.values()
                    .find(|objects| {
                        objects
                            .binary_search_by(|oid| oid.as_str().cmp(old_oid))
                            .is_ok()
                    })
                    .map(Vec::as_slice)
            }),
        };
        refs.insert(edit.ref_name().to_owned(), visibility.apply(prior)?);
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
        CapsuleVisibilityDelta, RepositoryRoot,
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
    async fn selected_ref_view_avoids_repository_wide_head_enumeration() {
        let inner = Arc::new(InMemory::new());
        seed_prepared_multi_ref(inner.clone(), true, false).await;
        let observer = Arc::new(RecordingObserver::default());
        let store = Store::new(inner).with_storage_observer(observer.clone());
        let router = StoreLayout::new(store, "repositories/test".to_owned());
        let root = load_root(&router).await.unwrap();

        let view = open_view_from_root_for_refs(
            &router,
            root,
            &BTreeSet::from(["refs/heads/feature".to_owned()]),
            TEST_LIMITS,
        )
        .await
        .unwrap();

        assert_eq!(view.refs().get("refs/heads/feature"), Some(&"3".repeat(40)));
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
}
