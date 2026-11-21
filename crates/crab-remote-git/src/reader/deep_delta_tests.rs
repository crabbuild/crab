use std::io::Write as _;

use object_store::memory::InMemory;

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn deep_delta_reads_fit_the_default_worker_stack_and_preserve_depth_limits() {
    const DEPTH: u8 = 64;
    let mut pack = b"PACK\0\0\0\x02".to_vec();
    pack.extend(u32::from(DEPTH + 1).to_be_bytes());
    let mut rows: Vec<(gix_hash::ObjectId, GitObjectLocation)> = Vec::new();
    for depth in 0..=DEPTH {
        let body = vec![b'x'; usize::from(depth) + 1];
        let oid =
            gix_object::compute_hash(gix_hash::Kind::Sha1, gix_object::Kind::Blob, &body).unwrap();
        let mut entry = Vec::new();
        let data = if depth == 0 {
            Header::Blob.write_to(1, &mut entry).unwrap();
            body
        } else {
            Header::RefDelta {
                base_id: rows.last().unwrap().0,
            }
            .write_to(6, &mut entry)
            .unwrap();
            vec![depth, depth + 1, 0x90, depth, 1, b'x']
        };
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&data).unwrap();
        entry.extend(encoder.finish().unwrap());
        rows.push((
            oid,
            GitObjectLocation {
                pack_offset: pack.len() as u64,
                entry_len: entry.len() as u64,
                crc32: gix_features::hash::crc32(&entry),
            },
        ));
        pack.extend(entry);
    }
    let checksum: [u8; 20] = Sha1::digest(&pack).into();
    let pack_data_end = pack.len() as u64;
    pack.extend_from_slice(&checksum);
    let pack_id = MerkleHash::from_hex(blake3::hash(&pack).to_hex().as_str()).unwrap();
    let pack_size = pack.len() as u64;
    let store = Store::new(Arc::new(InMemory::new()));
    store
        .put(&repo_pack_path("repository", &pack_id), Bytes::from(pack))
        .await
        .unwrap();
    let target = rows.last().unwrap().0;
    let identity = RepositoryIdentity::new("memory", "repository", 1).unwrap();
    let mut indexed = rows.clone();
    indexed.sort_unstable_by_key(|(oid, _)| *oid);
    let mut offset_order = (0..indexed.len() as u32).collect::<Vec<_>>();
    offset_order.sort_unstable_by_key(|position| indexed[*position as usize].1.pack_offset);
    let index = Arc::new(PackIndex {
        _reservation: None,
        object_ids: indexed.iter().map(|row| row.0).collect(),
        pack_offsets: indexed.iter().map(|row| row.1.pack_offset).collect(),
        crc32: indexed.iter().map(|row| row.1.crc32).collect(),
        offset_order,
        pack_data_end,
        pack_checksum: checksum,
        external_delta_bases: HashMap::new(),
        source_bytes: 1072 + indexed.len() as u64 * 28,
    });
    for (maximum, batch) in [
        (usize::from(DEPTH), false),
        (usize::from(DEPTH), true),
        (usize::from(DEPTH) - 1, false),
        (usize::from(DEPTH) - 1, true),
    ] {
        let runtime = Arc::new(RemoteGitRuntime::default());
        runtime
            .insert_pack_index(
                crate::runtime::PackIndexCacheKey::new(&identity, pack_id),
                index.clone(),
            )
            .await;
        let mut reader = RemoteGitReader::from_pinned(
            store.clone(),
            "repository",
            [GitPackInventoryEntry {
                pack_id,
                object_count: rows.len() as u64,
                pack_size,
            }],
            ReaderLimits {
                max_delta_depth: maximum,
                ..Default::default()
            },
            runtime.clone(),
            identity.clone(),
            1,
        )
        .unwrap();
        reader.inline_locators = Some(Arc::new(
            rows.iter()
                .enumerate()
                .map(|(index, (oid, location))| {
                    (
                        oid.as_bytes().try_into().unwrap(),
                        GitObjectLocator {
                            ordinal: index as u32,
                            pack_id,
                            location: *location,
                            metadata: Default::default(),
                        },
                    )
                })
                .collect(),
        ));
        let reader = Arc::new(reader);
        let budget = OperationBudget::new(crate::OperationLimits::default(), runtime.clone());
        // Poll on a default Tokio worker, rather than the test runner's larger
        // stack. A boxed recursive future still recursively polls its children.
        let result = tokio::spawn(async move {
            let session = GitObjectLocatorSession::without_catalog();
            let result = if batch {
                reader
                    .read_many_with_session(
                        &session,
                        &[target],
                        1,
                        &budget,
                        &CancellationToken::new(),
                    )
                    .await
                    .map(|mut objects| objects.remove(0))
            } else {
                reader
                    .read_with_session(&session, target, &budget, &CancellationToken::new())
                    .await
            };
            session.close().await.unwrap();
            result
        })
        .await
        .unwrap();
        runtime.shutdown().await;
        if maximum == usize::from(DEPTH) {
            assert_eq!(
                result.unwrap().data.as_ref(),
                vec![b'x'; usize::from(DEPTH) + 1]
            );
        } else {
            assert!(
                matches!(
                    result,
                    Err(Error::LimitExceeded {
                        limit: "delta depth",
                        ..
                    })
                ),
                "{result:?}"
            );
        }
    }
}
