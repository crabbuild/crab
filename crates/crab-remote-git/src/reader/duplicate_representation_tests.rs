use std::io::Write as _;

use crab_metadata::git_object_locator::{
    GitLocatorCoverage, GitObjectLocatorEntry, GitObjectLocatorWriter, GitPackLocatorRecord,
};
use object_store::{ObjectStoreExt, memory::InMemory};

use super::*;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Representation {
    LocalOffset,
    LocalReference,
    ExternalReference,
}

impl Representation {
    pub(crate) const ALL: [Self; 3] = [
        Self::LocalOffset,
        Self::LocalReference,
        Self::ExternalReference,
    ];
}

pub(crate) async fn fixture(
    representation: Representation,
    include_anchor: bool,
) -> (
    crate::RemoteGitRepository,
    GitObjectLocatorSession,
    [gix_hash::ObjectId; 2],
) {
    let encode = |header: Header, body: &[u8]| {
        let mut bytes = Vec::new();
        header.write_to(body.len() as u64, &mut bytes).unwrap();
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body).unwrap();
        bytes.extend(encoder.finish().unwrap());
        bytes
    };
    let oid = |bytes: &[u8]| {
        gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Blob, bytes).unwrap()
    };
    let a = oid(b"hello world");
    let b = oid(b"hello world!");
    let full = encode(Header::Blob, b"hello world");
    let forward = encode(
        if matches!(representation, Representation::LocalOffset) {
            Header::OfsDelta {
                base_distance: full.len() as u64,
            }
        } else {
            Header::RefDelta { base_id: a }
        },
        &[11, 12, 0x90, 11, 1, b'!'],
    );
    let reverse = encode(Header::RefDelta { base_id: b }, &[12, 11, 0x90, 11]);
    let store: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
    let runtime = Arc::new(RemoteGitRuntime::default());
    let identity = RepositoryIdentity::new("provider", "repository", 1).unwrap();
    let index_hash = MerkleHash::from_hex(&"22".repeat(32)).unwrap();
    let mut inventory = Vec::new();
    let mut records = Vec::new();
    let mut locations = Vec::new();
    let packs = if matches!(representation, Representation::ExternalReference) {
        vec![vec![(a, full)], vec![(b, forward)], vec![(a, reverse)]]
    } else {
        vec![vec![(a, full), (b, forward)], vec![(a, reverse)]]
    };
    let last_pack = packs.len() - 1;
    for (pack_position, entries) in packs.into_iter().enumerate() {
        let count = entries.len() as u32;
        let mut bytes = b"PACK\0\0\0\x02".to_vec();
        bytes.extend_from_slice(&count.to_be_bytes());
        let mut rows = Vec::new();
        for (oid, entry) in entries {
            rows.push((
                oid,
                bytes.len() as u64,
                entry.len() as u64,
                gix_features::hash::crc32(&entry),
            ));
            bytes.extend(entry);
        }
        let checksum: [u8; 20] = Sha1::digest(&bytes).into();
        let data_end = bytes.len() as u64;
        bytes.extend(checksum);
        let pack_id = MerkleHash::from_hex(blake3::hash(&bytes).to_hex().as_str()).unwrap();
        let pack_size = bytes.len() as u64;
        Store::new(Arc::clone(&store))
            .put(&repo_pack_path("repository", &pack_id), Bytes::from(bytes))
            .await
            .unwrap();
        inventory.push(GitPackInventoryEntry {
            pack_id,
            object_count: u64::from(count),
            pack_size,
        });
        records.push(GitPackLocatorRecord {
            pack_id,
            committed_generation: 1,
            pack_index_hash: index_hash,
            object_count: u64::from(count),
            pack_size,
        });
        locations.push(rows.clone());
        rows.sort_unstable_by_key(|row| row.0);
        let mut offset_order = (0..count).collect::<Vec<_>>();
        offset_order.sort_unstable_by_key(|index| rows[*index as usize].1);
        runtime
            .insert_pack_index(
                crate::runtime::PackIndexCacheKey::new(&identity, pack_id),
                Arc::new(PackIndex {
                    object_ids: rows.iter().map(|row| row.0).collect(),
                    pack_offsets: rows.iter().map(|row| row.1).collect(),
                    crc32: rows.iter().map(|row| row.3).collect(),
                    offset_order,
                    pack_data_end: data_end,
                    pack_checksum: checksum,
                    external_delta_bases: if pack_position == last_pack {
                        HashMap::from([(a, b)])
                    } else if matches!(representation, Representation::ExternalReference)
                        && pack_position == 1
                    {
                        HashMap::from([(b, a)])
                    } else {
                        HashMap::new()
                    },
                    source_bytes: 1,
                }),
            )
            .await;
    }
    let mut writer = GitObjectLocatorWriter::open(Arc::clone(&store), "repository")
        .await
        .unwrap();
    let bindings = writer.bind_packs(&records).await.unwrap();
    // Global lookup chooses the newer REF_DELTA for A. Its original complete
    // representation still anchors B, locally or through an external REF edge.
    let (b_pack, b_entry) = if matches!(representation, Representation::ExternalReference) {
        (1, 0)
    } else {
        (0, 1)
    };
    for (binding, row) in [
        (bindings[b_pack], locations[b_pack][b_entry]),
        (
            bindings[last_pack],
            locations[last_pack][locations[last_pack].len() - 1],
        ),
    ] {
        writer
            .write_locations(
                binding,
                &[GitObjectLocatorEntry {
                    oid: row.0.as_bytes().try_into().unwrap(),
                    location: GitObjectLocation {
                        pack_offset: row.1,
                        entry_len: row.2,
                        crc32: row.3,
                    },
                    metadata: Default::default(),
                }],
            )
            .await
            .unwrap();
    }
    writer.flush_objects().await.unwrap();
    writer
        .set_coverage(GitLocatorCoverage {
            generation: 1,
            pack_index_hash: index_hash,
        })
        .await
        .unwrap();
    writer.close().await.unwrap();
    let session = GitObjectLocatorSession::open(Arc::clone(&store), "repository")
        .await
        .unwrap();
    if !include_anchor {
        inventory.remove(0);
    }
    let reader = Arc::new(
        RemoteGitReader::from_pinned(
            Store::new(store),
            "repository",
            inventory,
            ReaderLimits::default(),
            runtime,
            identity,
            1,
        )
        .unwrap(),
    );
    let repository = crate::RemoteGitRepository {
        generated_pack_lease_provider: None,
        state: Arc::new(crate::state::RepositoryState {
            store: reader.store.clone(),
            layout: crab_storage::StoreLayout::new(reader.store.clone(), "repository".to_owned()),
            runtime: reader.runtime.clone(),
            identity: reader.identity.clone(),
            options: Default::default(),
            generation: 1,
            pack_index_hash: Arc::from(index_hash.to_string()),
            git_validation_digest: Arc::from("validation"),
            manifest_etag: "etag".to_owned(),
            shard_index_hash: Arc::from("shards"),
            catalog_identity: None,
            lookup_catalog_identity: None,
            inventory: reader.inventory.clone(),
            refs: Default::default(),
            reader: Some(reader),
            commit_graph: None,
            path_state_hash: None,
            path_state: Default::default(),
            shallow_closure: None,
        }),
    };
    (repository, session, [a, b])
}

#[tokio::test]
async fn point_reads_keep_bases_in_their_pack() {
    for representation in Representation::ALL {
        for position in [0, 1] {
            let (repository, session, oids) = fixture(representation, true).await;
            let reader = repository.state.reader.as_ref().unwrap();
            let budget =
                OperationBudget::new(crate::OperationLimits::default(), reader.runtime.clone());
            let result = reader
                .read_with_session(&session, oids[position], &budget, &CancellationToken::new())
                .await;
            session.close().await.unwrap();
            reader.runtime.shutdown().await;
            assert_eq!(
                result.unwrap().data,
                if position == 0 {
                    b"hello world".as_slice()
                } else {
                    b"hello world!".as_slice()
                },
                "representation={representation:?}, position={position}"
            );
        }
    }
}

#[tokio::test]
async fn metadata_keeps_bases_in_their_pack() {
    for representation in Representation::ALL {
        for position in [0, 1] {
            let (repository, session, oids) = fixture(representation, true).await;
            let reader = repository.state.reader.as_ref().unwrap();
            let budget =
                OperationBudget::new(crate::OperationLimits::default(), reader.runtime.clone());
            let result = reader
                .read_metadata_with_session(
                    &session,
                    oids[position],
                    &budget,
                    &CancellationToken::new(),
                )
                .await;
            session.close().await.unwrap();
            reader.runtime.shutdown().await;
            assert_eq!(
                result.unwrap().size,
                11 + position as u64,
                "representation={representation:?}, position={position}"
            );
        }
    }
}

#[tokio::test]
async fn batch_reads_keep_bases_in_their_pack() {
    for representation in Representation::ALL {
        let (repository, session, oids) = fixture(representation, true).await;
        let reader = repository.state.reader.as_ref().unwrap();
        let budget =
            OperationBudget::new(crate::OperationLimits::default(), reader.runtime.clone());
        let result = reader
            .read_many_with_session(&session, &oids, 2, &budget, &CancellationToken::new())
            .await;
        session.close().await.unwrap();
        reader.runtime.shutdown().await;
        let data = result
            .unwrap()
            .into_iter()
            .map(|object| object.data)
            .collect::<Vec<_>>();
        assert_eq!(
            data,
            [
                Bytes::from_static(b"hello world"),
                Bytes::from_static(b"hello world!")
            ],
            "representation={representation:?}"
        );
    }
}

#[derive(Clone, Copy, Debug)]
enum ReadShape {
    Point,
    Metadata,
    Batch,
}

async fn read_shape(
    repository: &crate::RemoteGitRepository,
    session: &GitObjectLocatorSession,
    oid: gix_hash::ObjectId,
    shape: ReadShape,
) -> Result<()> {
    let reader = repository.state.reader.as_ref().unwrap();
    let budget = OperationBudget::new(crate::OperationLimits::default(), reader.runtime.clone());
    let cancel = CancellationToken::new();
    match shape {
        ReadShape::Point => reader
            .read_with_session(session, oid, &budget, &cancel)
            .await
            .map(|_| ()),
        ReadShape::Metadata => reader
            .read_metadata_with_session(session, oid, &budget, &cancel)
            .await
            .map(|_| ()),
        ReadShape::Batch => reader
            .read_many_with_session(session, &[oid], 2, &budget, &cancel)
            .await
            .map(|_| ()),
    }
}

#[tokio::test]
async fn missing_acyclic_representation_fails_without_reading_outside_inventory() {
    for shape in [ReadShape::Point, ReadShape::Metadata, ReadShape::Batch] {
        let (repository, session, oids) = fixture(Representation::ExternalReference, false).await;
        let result = read_shape(&repository, &session, oids[0], shape).await;
        session.close().await.unwrap();
        repository.state.runtime.shutdown().await;
        assert!(
            matches!(
                result,
                Err(Error::Corrupt {
                    stage: CorruptionStage::Delta
                })
            ),
            "{shape:?}: {result:?}"
        );
    }
}

#[tokio::test]
async fn corrupt_physical_alias_fails_without_hiding_crc_errors() {
    for shape in [ReadShape::Point, ReadShape::Metadata, ReadShape::Batch] {
        let (repository, session, oids) = fixture(Representation::ExternalReference, true).await;
        let reader = repository.state.reader.as_ref().unwrap();
        let budget =
            OperationBudget::new(crate::OperationLimits::default(), reader.runtime.clone());
        let mut anchor = None;
        for pack_id in reader.inventory.keys() {
            let index = reader
                .load_pack_index(*pack_id, &budget, &CancellationToken::new())
                .await
                .unwrap();
            if index.object_ids == [oids[0]] && index.external_delta_bases.is_empty() {
                anchor = Some(*pack_id);
            }
        }
        let path = repo_pack_path("repository", &anchor.unwrap());
        let (body, _) = reader.store.get_with_etag(&path).await.unwrap();
        let mut corrupt = body.to_vec();
        corrupt[12] ^= 1;
        reader
            .store
            .inner()
            .put(&path, Bytes::from(corrupt).into())
            .await
            .unwrap();
        let result = read_shape(&repository, &session, oids[0], shape).await;
        session.close().await.unwrap();
        repository.state.runtime.shutdown().await;
        assert!(
            matches!(result, Err(Error::PackedEntryCrcMismatch { .. })),
            "{shape:?}: {result:?}"
        );
    }
}

#[tokio::test]
async fn displaced_external_bases_search_captured_pages_without_pack_sidecars() {
    use crab_metadata::git_object_locator::{PagedGitObjectIndex, PagedGitObjectIndexWrite};
    use crab_metadata::manifests::{Manifest, PackManifestEntry, compact_pack_index};
    use crab_storage::StoreLayout;

    let (repository, session, oids) = fixture(Representation::ExternalReference, true).await;
    let source = repository.state.reader.as_ref().unwrap();
    let budget = OperationBudget::new(crate::OperationLimits::default(), source.runtime.clone());
    let cancel = CancellationToken::new();
    let root = source
        .locate(&session, oids[0], &budget, &cancel)
        .await
        .unwrap();
    let inventory = source.inventory.values().copied().collect::<Vec<_>>();
    let mut entries = Vec::new();
    for pack in &inventory {
        let index = source
            .load_pack_index(pack.pack_id, &budget, &cancel)
            .await
            .unwrap();
        for (position, oid) in index.object_ids.iter().enumerate() {
            entries.push((
                oid.as_bytes().try_into().unwrap(),
                GitObjectLocator {
                    ordinal: 0,
                    pack_id: pack.pack_id,
                    location: index.location_at(position).unwrap(),
                    metadata: Default::default(),
                },
            ));
        }
    }
    let mut manifest = Manifest::default_for_repo("refs/heads/main");
    manifest.generation = 1;
    manifest.git_validation_digest = "b".repeat(64);
    let mut packs = inventory
        .iter()
        .map(|pack| PackManifestEntry {
            pack_id: pack.pack_id.to_string(),
            size: pack.pack_size,
            content_hash: pack.pack_id.to_string(),
            ref_tips: Vec::new(),
            object_count: pack.object_count,
        })
        .collect::<Vec<_>>();
    packs.sort_unstable_by(|left, right| left.pack_id.cmp(&right.pack_id));
    manifest.pack_index_hash = compact_pack_index(1, &packs).unwrap().0;
    let store = source.store.clone();
    let layout = StoreLayout::new(store.clone(), "repository".to_owned());
    let write =
        PagedGitObjectIndexWrite::build(&"c".repeat(64), &manifest, &inventory, &entries).unwrap();
    let hash = write.upload(&store, &layout).await.unwrap();
    let index = PagedGitObjectIndex::load(
        &store,
        &layout,
        &hash,
        &"c".repeat(64),
        &manifest,
        128 * 1024,
    )
    .await
    .unwrap();
    index
        .verify_complete(&store, &manifest, 128 * 1024)
        .await
        .unwrap();
    session.close().await.unwrap();
    source.runtime.shutdown().await;
    // A fresh runtime has no cached indexes, and the fixture stores no IDX/REV.
    let runtime = Arc::new(RemoteGitRuntime::default());
    let reader = Arc::new(
        RemoteGitReader::from_pinned_with_preferred_pack_indexes(
            store,
            "repository",
            inventory,
            SnapshotLookupSources::default().with_object_index(index),
            ReaderLimits::default(),
            runtime.clone(),
            source.identity.clone(),
            1,
        )
        .unwrap(),
    );
    let budget = OperationBudget::new(crate::OperationLimits::default(), runtime.clone());
    let session = GitObjectLocatorSession::without_catalog();
    let plan = reader
        .search_delta_aliases(
            &session,
            (oids[0], root),
            8,
            HashMap::new(),
            &budget,
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(
        plan.iter().map(|step| step.oid).collect::<Vec<_>>(),
        [oids[0], oids[1], oids[0]]
    );
    let object = reader
        .read_resolved_locator(&session, oids[0], root, 1024, &budget, &cancel)
        .await
        .unwrap();
    assert_eq!(object.data, b"hello world".as_slice());
    runtime.shutdown().await;
}
