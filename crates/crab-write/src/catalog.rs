//! Canonical Git locator publication shared by CLI and server owners.
use crate::{Result, WriteError};
use bytes::Bytes;
use crab_metadata::manifests::PackManifestEntry;
use crab_storage::{Store, StoreLayout};
use crab_xet::hash::MerkleHash;
use futures_util::{StreamExt, TryStreamExt};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::debug;

const LOCATOR_EVIDENCE_CONCURRENCY: usize = 16;

/// Validated local pack indexes retained for one publication attempt.
#[derive(Debug)]
pub struct LocatorPackEvidence {
    pack_id: MerkleHash,
    idx_path: PathBuf,
    rev_path: PathBuf,
    git_sha1: String,
    kind_by_oid: Option<GitObjectKindMap>,
    _temp: Option<tempfile::TempDir>,
}

/// Verified logical kinds indexed by SHA-1 object identity.
pub type GitObjectKindMap =
    Arc<HashMap<[u8; 20], crab_metadata::git_object_locator::GitObjectKind>>;

impl LocatorPackEvidence {
    /// Validate local immutable index sidecars before catalog publication.
    pub fn from_local(
        pack: &PackManifestEntry,
        idx_path: &Path,
        rev_path: &Path,
        git_sha1: &str,
        kind_by_oid: Option<GitObjectKindMap>,
    ) -> Result<Self> {
        validate_locator_pack_evidence(
            pack,
            idx_path,
            rev_path,
            git_sha1,
            &idx_path.display().to_string(),
        )?;
        let pack_id =
            MerkleHash::from_hex(&pack.pack_id).map_err(|source| WriteError::PackIdentity {
                source: Box::new(source),
            })?;
        Ok(Self {
            pack_id,
            idx_path: idx_path.to_owned(),
            rev_path: rev_path.to_owned(),
            git_sha1: git_sha1.to_owned(),
            kind_by_oid,
            _temp: None,
        })
    }
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(WriteError::Cancelled);
    }
    Ok(())
}

fn metadata_kind(kind: gix_object::Kind) -> crab_metadata::git_object_locator::GitObjectKind {
    match kind {
        gix_object::Kind::Commit => crab_metadata::git_object_locator::GitObjectKind::Commit,
        gix_object::Kind::Tree => crab_metadata::git_object_locator::GitObjectKind::Tree,
        gix_object::Kind::Blob => crab_metadata::git_object_locator::GitObjectKind::Blob,
        gix_object::Kind::Tag => crab_metadata::git_object_locator::GitObjectKind::Tag,
    }
}

fn validate_locator_pack_evidence(
    pack: &PackManifestEntry,
    idx_path: &Path,
    rev_path: &Path,
    expected_git_sha1: &str,
    error_path: &str,
) -> Result<()> {
    let locations = crab_git::pack_locator::PackLocationIter::open(idx_path, rev_path, pack.size)
        .map_err(crab_git::pack::PackError::from)?;
    if locations.object_count() != pack.object_count {
        return Err(WriteError::CorruptObject {
            path: error_path.to_owned(),
            reason: format!(
                "manifest records {} objects but verified index contains {}",
                pack.object_count,
                locations.object_count()
            ),
        });
    }
    if locations.pack_checksum().to_string() != expected_git_sha1 {
        return Err(WriteError::CorruptObject {
            path: error_path.to_owned(),
            reason: "pack index checksum disagrees with verified pack trailer".to_owned(),
        });
    }
    Ok(())
}

async fn download_locator_pack_evidence(
    store: &Store,
    router: &StoreLayout<Store>,
    pack: &PackManifestEntry,
    populate_kind_metadata: bool,
    cancel: &CancellationToken,
) -> Result<LocatorPackEvidence> {
    check_cancelled(cancel)?;
    if pack.size < 20 {
        return Err(WriteError::CorruptObject {
            path: router.pack_path(&pack.pack_id).as_ref().to_owned(),
            reason: "canonical Git pack is too short for its trailer".to_owned(),
        });
    }
    let trailer = store
        .range_get(&router.pack_path(&pack.pack_id), pack.size - 20..pack.size)
        .await?;
    let expected_git_sha1 =
        gix_hash::ObjectId::from(<[u8; 20]>::try_from(trailer.as_ref()).map_err(|_| {
            WriteError::CorruptObject {
                path: router.pack_path(&pack.pack_id).as_ref().to_owned(),
                reason: "canonical Git pack trailer is not 20 bytes".to_owned(),
            }
        })?)
        .to_string();
    let temp = tempfile::tempdir().map_err(WriteError::Io)?;
    let idx_path = temp.path().join("pack.idx");
    let rev_path = temp.path().join("pack.rev");
    let index_maximum =
        crab_git::pack_locator::max_pack_index_size(pack.object_count).ok_or_else(|| {
            WriteError::CorruptObject {
                path: router.pack_index_path(&pack.pack_id).as_ref().to_owned(),
                reason: "Git pack index size overflows its bound".to_owned(),
            }
        })?;
    let reverse_maximum = crab_git::pack_locator::pack_reverse_index_size(pack.object_count)
        .ok_or_else(|| WriteError::CorruptObject {
            path: router
                .pack_reverse_index_path(&pack.pack_id)
                .as_ref()
                .to_owned(),
            reason: "Git reverse index size overflows its bound".to_owned(),
        })?;
    store
        .download_to_path_bounded(
            &router.pack_index_path(&pack.pack_id),
            &idx_path,
            index_maximum,
        )
        .await?;
    check_cancelled(cancel)?;
    store
        .download_to_path_bounded(
            &router.pack_reverse_index_path(&pack.pack_id),
            &rev_path,
            reverse_maximum,
        )
        .await?;
    check_cancelled(cancel)?;
    validate_locator_pack_evidence(
        pack,
        &idx_path,
        &rev_path,
        &expected_git_sha1,
        router.pack_index_path(&pack.pack_id).as_ref(),
    )?;
    let kind_by_oid = if let Some(kinds) =
        load_pack_kind_metadata(store, router, pack, &idx_path, &rev_path).await?
    {
        Some(kinds)
    } else if populate_kind_metadata {
        crab_git::initialize_bare_git_dir(temp.path())?;
        let source = temp.path().join("source.pack");
        let downloaded = store
            .download_to_path_bounded(&router.pack_path(&pack.pack_id), &source, pack.size)
            .await?;
        check_cancelled(cancel)?;
        if downloaded != pack.size {
            return Err(WriteError::CorruptObject {
                path: source.display().to_string(),
                reason: format!(
                    "committed pack has size {downloaded}, expected {}",
                    pack.size
                ),
            });
        }
        let git_dir = temp.path().to_owned();
        let pack_dir = git_dir.join("objects/pack");
        let canonical_name = pack.pack_id.clone();
        let index_path = idx_path.clone();
        let reverse_index_path = rev_path.clone();
        let object_count = pack.object_count;
        let pack_size = pack.size;
        let (kinds, kind_metadata) = tokio::task::spawn_blocking(move || -> Result<_> {
            std::fs::create_dir_all(&pack_dir)?;
            crab_git::pack::install_pack_file_from_path(
                &pack_dir,
                &source,
                &canonical_name,
                0,
                false,
            )?;
            let mut locations = crab_git::pack_locator::PackLocationIter::open(
                &index_path,
                &reverse_index_path,
                pack_size,
            )
            .map_err(crab_git::pack::PackError::from)?;
            let object_ids = locations
                .by_ref()
                .map(|location| {
                    location
                        .map(|location| location.oid)
                        .map_err(crab_git::pack::PackError::from)
                })
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(WriteError::from)?;
            if object_ids.len() != object_count as usize {
                return Err(WriteError::CorruptObject {
                    path: index_path.display().to_string(),
                    reason: format!(
                        "pack index contains {} objects, expected {object_count}",
                        object_ids.len()
                    ),
                });
            }
            let kinds = crab_git::object_kinds_from_git_dir(&git_dir, &object_ids)
                .map_err(WriteError::from)?;
            if kinds.len() != object_ids.len() {
                return Err(WriteError::Internal(
                    "Git object-kind catalog returned an incomplete pack result".to_owned(),
                ));
            }
            let ordered_kinds = object_ids
                .iter()
                .map(|oid| {
                    kinds.get(oid).copied().ok_or_else(|| {
                        WriteError::Internal(
                            "Git object-kind catalog omitted a pack object".to_owned(),
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let kind_metadata = crab_git::pack_locator::encode_pack_kind_metadata(
                locations.pack_checksum(),
                &ordered_kinds,
            )
            .map_err(crab_git::pack::PackError::from)
            .map_err(WriteError::from)?;
            let kinds = kinds
                .into_iter()
                .map(|(oid, kind)| {
                    let oid: [u8; 20] = oid.as_bytes().try_into().map_err(|_| {
                        WriteError::Internal(
                            "Git object-kind catalog returned a non-SHA1 object".to_owned(),
                        )
                    })?;
                    Ok((oid, metadata_kind(kind)))
                })
                .collect::<Result<HashMap<_, _>>>()?;
            Ok((kinds, kind_metadata))
        })
        .await
        .map_err(WriteError::Worker)??;
        store
            .put(
                &router.pack_kind_metadata_path(&pack.pack_id),
                Bytes::from(kind_metadata),
            )
            .await?;
        Some(Arc::new(kinds))
    } else {
        None
    };
    check_cancelled(cancel)?;
    let pack_id =
        MerkleHash::from_hex(&pack.pack_id).map_err(|source| WriteError::PackIdentity {
            source: Box::new(source),
        })?;
    Ok(LocatorPackEvidence {
        pack_id,
        idx_path,
        rev_path,
        git_sha1: expected_git_sha1,
        kind_by_oid,
        _temp: Some(temp),
    })
}

async fn load_pack_kind_metadata(
    store: &Store,
    router: &StoreLayout<Store>,
    pack: &PackManifestEntry,
    idx_path: &Path,
    rev_path: &Path,
) -> Result<Option<GitObjectKindMap>> {
    let path = router.pack_kind_metadata_path(&pack.pack_id);
    let maximum =
        crab_git::pack_locator::pack_kind_metadata_size(pack.object_count).ok_or_else(|| {
            WriteError::CorruptObject {
                path: path.as_ref().to_owned(),
                reason: "Git kind metadata size overflows its bound".to_owned(),
            }
        })?;
    let bytes = match store.get_with_etag_bounded(&path, maximum).await {
        Ok((bytes, _)) => bytes,
        Err(crab_storage::StorageError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let idx_path = idx_path.to_owned();
    let rev_path = rev_path.to_owned();
    let pack_size = pack.size;
    let object_count =
        usize::try_from(pack.object_count).map_err(|_| WriteError::CorruptObject {
            path: "pack kind metadata".to_owned(),
            reason: "kind metadata object count does not fit in memory".to_owned(),
        })?;
    let map = tokio::task::spawn_blocking(move || -> Result<GitObjectKindMap> {
        let locations =
            crab_git::pack_locator::PackLocationIter::open(&idx_path, &rev_path, pack_size)
                .map_err(crab_git::pack::PackError::from)?;
        let entries = crab_git::pack_locator::decode_pack_kind_metadata_iter(&bytes, locations)
            .map_err(crab_git::pack::PackError::from)?;
        let mut kinds = HashMap::with_capacity(entries.len());
        for entry in entries {
            let (oid, kind) = entry.map_err(crab_git::pack::PackError::from)?;
            let oid: [u8; 20] = oid.as_bytes().try_into().map_err(|_| {
                WriteError::Internal("Git kind metadata contains a non-SHA1 object".to_owned())
            })?;
            if kinds.insert(oid, metadata_kind(kind)).is_some() {
                return Err(WriteError::CorruptObject {
                    path: "pack kind metadata".to_owned(),
                    reason: "kind metadata contains a duplicate object".to_owned(),
                });
            }
        }
        if kinds.len() != object_count {
            return Err(WriteError::CorruptObject {
                path: "pack kind metadata".to_owned(),
                reason: format!(
                    "kind metadata contains {} objects, expected {}",
                    kinds.len(),
                    object_count
                ),
            });
        }
        Ok(Arc::new(kinds))
    })
    .await
    .map_err(WriteError::Worker)??;
    Ok(Some(map))
}

async fn collect_locator_pack_evidence(
    store: &Store,
    router: &StoreLayout<Store>,
    local_evidence: &mut HashMap<MerkleHash, LocatorPackEvidence>,
    packs: &[PackManifestEntry],
    skip_packs: &HashSet<MerkleHash>,
    populate_kind_metadata: bool,
    cancel: &CancellationToken,
) -> Result<Vec<LocatorPackEvidence>> {
    let mut evidence = Vec::new();
    let mut remote_packs = Vec::new();
    for pack in packs {
        check_cancelled(cancel)?;
        let pack_id =
            MerkleHash::from_hex(&pack.pack_id).map_err(|source| WriteError::PackIdentity {
                source: Box::new(source),
            })?;
        if skip_packs.contains(&pack_id) {
            continue;
        }
        if let Some(local) = local_evidence.remove(&pack_id) {
            if local.pack_id != pack_id {
                return Err(WriteError::CorruptObject {
                    path: local.idx_path.display().to_string(),
                    reason: "local index evidence belongs to a different pack".to_owned(),
                });
            }
            validate_locator_pack_evidence(
                pack,
                &local.idx_path,
                &local.rev_path,
                &local.git_sha1,
                &local.idx_path.display().to_string(),
            )?;
            evidence.push(local);
        } else {
            remote_packs.push(pack.clone());
        }
    }
    let remote_pack_count = remote_packs.len();
    let started = Instant::now();
    let remote_evidence =
        futures_util::stream::iter(remote_packs.into_iter().map(|pack| async move {
            check_cancelled(cancel)?;
            download_locator_pack_evidence(store, router, &pack, populate_kind_metadata, cancel)
                .await
        }))
        .buffer_unordered(LOCATOR_EVIDENCE_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
    tracing::debug!(
        locator_remote_pack_count = remote_pack_count,
        locator_evidence_download_ms = started.elapsed().as_millis() as u64,
        "downloaded Git locator pack evidence"
    );
    evidence.extend(remote_evidence);
    Ok(evidence)
}

async fn write_locator_pack_evidence(
    writer: &mut crab_metadata::git_object_locator::GitObjectLocatorWriter,
    bindings: &HashMap<MerkleHash, crab_metadata::git_object_locator::GitPackLocatorBinding>,
    evidence: &[LocatorPackEvidence],
) -> Result<()> {
    for pack_evidence in evidence {
        let binding = *bindings.get(&pack_evidence.pack_id).ok_or_else(|| {
            WriteError::Internal("locator evidence has no current manifest pack binding".to_owned())
        })?;
        let mut locations = crab_git::pack_locator::PackLocationIter::open(
            &pack_evidence.idx_path,
            &pack_evidence.rev_path,
            binding.record.pack_size,
        )
        .map_err(crab_git::pack::PackError::from)?;
        if locations.pack_checksum().to_string() != pack_evidence.git_sha1 {
            return Err(WriteError::CorruptObject {
                path: pack_evidence.idx_path.display().to_string(),
                reason: "pack index checksum changed during locator publication".to_owned(),
            });
        }
        let mut entries = Vec::with_capacity(25_000);
        for location in &mut locations {
            let location = location.map_err(crab_git::pack::PackError::from)?;
            let oid = location.oid.as_bytes().try_into().map_err(|_| {
                WriteError::Internal("generated pack index contained non-SHA1 object".to_owned())
            })?;
            entries.push(crab_metadata::git_object_locator::GitObjectLocatorEntry {
                oid,
                location: crab_metadata::git_object_locator::GitObjectLocation {
                    pack_offset: location.pack_offset,
                    entry_len: location.entry_len,
                    crc32: location.crc32,
                },
                metadata: crab_metadata::git_object_locator::GitObjectMetadata {
                    kind: pack_evidence
                        .kind_by_oid
                        .as_ref()
                        .and_then(|kinds| kinds.get(&oid).copied()),
                    ..Default::default()
                },
            });
            if entries.len() == 25_000 {
                writer.write_locations(binding, &entries).await?;
                entries.clear();
            }
        }
        if !entries.is_empty() {
            writer.write_locations(binding, &entries).await?;
        }
    }
    Ok(())
}

/// Publish exact inventory coverage into an exclusively owned locator writer.
///
/// The caller owns the locator lease, writer close, GC fences and cancellation
/// lifecycle. `current_packs` must be the complete inventory read from the
/// supplied anchor; local sidecars must remain immutable throughout the call.
/// Publish new rows before sweeping obsolete slots; a changed object
/// universe requires replay into a rebuilt dense catalog. Coverage is withheld
/// if the manifest base changes. Index evidence is bounded by pack cardinality.
/// This updates derived metadata, not refs, and does not close the supplied writer.
pub async fn publish_inventory(
    writer: &mut crab_metadata::git_object_locator::GitObjectLocatorWriter,
    store: &Store,
    router: &StoreLayout<Store>,
    local_evidence: &mut HashMap<MerkleHash, LocatorPackEvidence>,
    anchor: crab_metadata::git_object_locator::GitLocatorCoverage,
    current_packs: &[PackManifestEntry],
    allow_catalog_rebuild: bool,
    cancel: &CancellationToken,
) -> Result<(bool, crab_metadata::git_object_locator::LocatorSweepStats)> {
    check_cancelled(cancel)?;
    let mut pack_records = Vec::with_capacity(current_packs.len());
    for pack in current_packs {
        let pack_id =
            MerkleHash::from_hex(&pack.pack_id).map_err(|source| WriteError::PackIdentity {
                source: Box::new(source),
            })?;
        pack_records.push(crab_metadata::git_object_locator::GitPackLocatorRecord {
            pack_id,
            committed_generation: anchor.generation,
            pack_index_hash: anchor.pack_index_hash,
            object_count: pack.object_count,
            pack_size: pack.size,
        });
    }
    let bindings = writer.bind_packs(&pack_records).await?;
    let retained_slots = bindings
        .iter()
        .map(|binding| binding.pack_slot)
        .collect::<HashSet<_>>();
    // Publish current-pack rows before sweeping stale slots. A repack can move
    // every OID to a new slot without changing the object universe; sweeping
    // first would mistake that valid rewrite for an object-set change and
    // trigger a full dense-ordinal catalog rebuild.
    let covered = bindings
        .iter()
        .filter(|binding| writer.binding_has_covered_objects(**binding))
        .map(|binding| binding.record.pack_id)
        .collect::<HashSet<_>>();
    // Pack bytes are immutable. Sweeping an obsolete slot removes only rows
    // owned by that slot; covered retained packs remain valid and do not need
    // another full index scan.
    // Rebinding preserves known object kinds. Missing kind sidecars use the
    // canonical reader's metadata path; publication does not scan full packs.
    let mut evidence = collect_locator_pack_evidence(
        store,
        router,
        local_evidence,
        current_packs,
        &covered,
        false,
        cancel,
    )
    .await?;
    let bindings = bindings
        .into_iter()
        .map(|binding| (binding.record.pack_id, binding))
        .collect::<HashMap<_, _>>();
    write_locator_pack_evidence(&mut *writer, &bindings, &evidence).await?;

    let sweep = writer.sweep_unreferenced(&retained_slots).await?;
    if sweep.object_rows_deleted != 0 {
        if !allow_catalog_rebuild {
            // A normal push may leave stale rows after a repack, but rebuilding
            // the dense catalog is owner work and must not extend the ack path.
            debug!(
                object_rows_deleted = sweep.object_rows_deleted,
                "deferred stale Git locator catalog rebuild to generation owner"
            );
            return Ok((false, sweep));
        }
        // Only deleting an object proves that the dense ordinal universe
        // changed. Rebuild then replay every current pack, including packs
        // that were covered before the sweep.
        writer.replace_object_catalog(&retained_slots).await?;
        let already_loaded = evidence
            .iter()
            .map(|item| item.pack_id)
            .collect::<HashSet<_>>();
        let mut replay = collect_locator_pack_evidence(
            store,
            router,
            local_evidence,
            current_packs,
            &already_loaded,
            false,
            cancel,
        )
        .await?;
        evidence.append(&mut replay);
        write_locator_pack_evidence(&mut *writer, &bindings, &evidence).await?;
        writer.complete_object_catalog_rebuild().await?;
    }
    debug!(
        object_rows_deleted = sweep.object_rows_deleted,
        pack_rows_deleted = sweep.pack_rows_deleted,
        catalog_rebuilt = sweep.object_rows_deleted != 0,
        "swept stale Git locator rows"
    );
    check_cancelled(cancel)?;
    let (after, _) = crab_metadata::manifest_store::read_manifest(store, router).await?;
    if after.generation != anchor.generation
        || after.pack_index_hash != anchor.pack_index_hash.hex()
    {
        return Ok((false, sweep));
    }
    check_cancelled(cancel)?;
    writer
        .set_coverage(crab_metadata::git_object_locator::GitLocatorCoverage {
            generation: anchor.generation,
            pack_index_hash: anchor.pack_index_hash,
        })
        .await?;
    Ok((true, sweep))
}
