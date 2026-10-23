use std::io::Write as _;

use crab_metadata::git_object_locator::{
    GitLocatorCoverage, GitObjectLocatorEntry, GitObjectLocatorWriter, GitPackLocatorRecord,
};
use object_store::memory::InMemory;

use super::*;

pub(crate) async fn fixture(
    ofs: bool,
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
        if ofs {
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
    for entries in [vec![(a, full), (b, forward)], vec![(a, reverse)]] {
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
                    external_delta_bases: if count == 1 {
                        HashMap::from([(a, b)])
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
    // Global lookup chooses the newer REF_DELTA for A. B's OFS_DELTA still
    // names the complete A entry in its own pack, which is not a cycle.
    for (binding, row) in [
        (bindings[0], locations[0][1]),
        (bindings[1], locations[1][0]),
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
    for ofs in [true, false] {
        for position in [0, 1] {
            let (repository, session, oids) = fixture(ofs).await;
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
                "ofs={ofs}, position={position}"
            );
        }
    }
}

#[tokio::test]
async fn metadata_keeps_bases_in_their_pack() {
    for ofs in [true, false] {
        for position in [0, 1] {
            let (repository, session, oids) = fixture(ofs).await;
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
                "ofs={ofs}, position={position}"
            );
        }
    }
}

#[tokio::test]
async fn batch_reads_keep_bases_in_their_pack() {
    for ofs in [true, false] {
        let (repository, session, oids) = fixture(ofs).await;
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
            "ofs={ofs}"
        );
    }
}
