//! Verified loading of one capsule-protocol root and its bounded capsule frontier.

use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleGitPackDescriptor, CapsulePointer, CapsuleRun, Checkpoint, CheckpointPointer,
    PointerCatalog, RootRecord, load_root,
};
use crab_storage::{Store, StoreLayout};
use futures_util::future::try_join_all;
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

    /// Materialize the complete generation-bound Git visibility proof.
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
        let mut manifest = crab_metadata::manifests::Manifest::default_for_repo(root.head());
        manifest.generation = root.generation();
        manifest.refs = root.refs().clone();
        manifest.peeled_refs = root.peeled_refs().clone();
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
    admit_frontier(snapshot.record().root().capsule_frontier(), limits)?;
    let checkpoint = async {
        match snapshot.record().root().checkpoint() {
            Some(pointer) => load_checkpoint(router, pointer, limits).await.map(Some),
            None => Ok(None),
        }
    };
    let runs = try_join_all(
        snapshot
            .record()
            .root()
            .capsule_frontier()
            .iter()
            .map(|pointer| load_run(router, pointer)),
    );
    let (checkpoint, runs) = tokio::try_join!(checkpoint, runs)?;
    let capsules = runs
        .into_iter()
        .flat_map(|run| run.capsules().to_vec())
        .collect();
    Ok(CapsuleRepositoryView {
        root: snapshot,
        checkpoint,
        capsules,
    })
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
        CapsuleGitPack, CapsuleRefEdit, CapsuleTransaction, RepositoryRoot,
    };
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

    #[tokio::test]
    async fn one_capsule_view_uses_exactly_two_gets() {
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
            vec![StorageOperation::Get, StorageOperation::Get]
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
        assert_eq!(operations, vec![StorageOperation::Get]);
    }
}
