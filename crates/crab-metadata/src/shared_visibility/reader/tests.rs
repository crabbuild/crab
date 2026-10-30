use super::*;
fn oid(index: usize) -> [u8; 20] {
    decode_oid(&format!("{index:040x}")).unwrap()
}
fn objects(dictionary: Arc<GitVisibilityDictionary>, positions: &[usize]) -> GitVisibilityObjects {
    let mut chunks =
        vec![[0; VISIBILITY_CHUNK_BYTES]; dictionary.objects().len().div_ceil(CHUNK_BITS)];
    for &position in positions {
        chunks[position / CHUNK_BITS][position % CHUNK_BITS / 8] |= 1 << (position % 8);
    }
    GitVisibilityObjects::from_shared_chunks(dictionary, chunks.into_iter().map(Arc::new).collect())
        .unwrap()
}
async fn store(body: &[u8], offset: usize) -> (Store, Path, PageCache) {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let path = Path::from("proof/shared");
    let mut bytes = vec![0; offset];
    bytes.extend_from_slice(body);
    store.put_overwrite(&path, bytes.into()).await.unwrap();
    (store, path, PageCache::default())
}
#[tokio::test]
async fn point_and_bulk_reads_match_exact_sets_with_gaps_duplicates_and_warm_work_bounds() {
    let dictionary =
        GitVisibilityDictionary::new((0..4103).map(|index| format!("{index:040x}")).collect())
            .unwrap();
    let dense = objects(
        dictionary.clone(),
        &(0..4103).filter(|index| index % 2 == 0).collect::<Vec<_>>(),
    );
    let sparse = objects(dictionary.clone(), &[1, 3, 4097]);
    // Make the retained dictionary include the entire fixture and both set directions.
    let all = objects(dictionary, &(0..4103).collect::<Vec<_>>());
    let sets = [dense, sparse, all];
    let prepared = Prepared::new(sets.iter()).unwrap();
    let mut writer = Writer::default();
    let (descriptor, roots) = prepared.encode(&mut writer).unwrap();
    let (store, path, cache) = store(&writer.bytes, 17).await;
    let reader = Reader::new(
        &store,
        &path,
        &cache,
        17,
        writer.bytes.len() as u64,
        &descriptor,
    )
    .unwrap();
    let indexes = [4097, 2, 0, 4102, 4103, 2, 1023, 1024];
    let requested = indexes.map(oid);
    for (set, root) in sets.iter().zip(&roots) {
        let expected = indexes
            .map(|index| set.contains(&format!("{index:040x}")))
            .to_vec();
        for _ in 0..2 {
            assert_eq!(
                reader
                    .lookup(
                        root.as_ref(),
                        &requested,
                        &mut PageWork::new(2 * 1024 * 1024, 0)
                    )
                    .await
                    .unwrap(),
                expected
            );
            assert_eq!(
                reader
                    .objects(root.as_ref(), &mut PageWork::new(2 * 1024 * 1024, 0))
                    .await
                    .unwrap(),
                set.iter()
                    .map(|oid| decode_oid(oid).unwrap())
                    .collect::<Vec<_>>()
            );
        }
        assert!(
            reader
                .lookup(root.as_ref(), &requested, &mut PageWork::new(1, 0))
                .await
                .is_err()
        );
    }
    assert_eq!(
        reader
            .lookup(None, &requested, &mut PageWork::new(1, 0))
            .await
            .unwrap(),
        vec![false; requested.len()]
    );
    assert!(
        reader
            .objects(None, &mut PageWork::new(1, 0))
            .await
            .unwrap()
            .is_empty()
    );
    let repeated = vec![oid(2); 4000];
    assert_eq!(
        reader
            .lookup(
                roots[0].as_ref(),
                &repeated,
                &mut PageWork::new(2 * 1024 * 1024, 0)
            )
            .await
            .unwrap(),
        vec![true; 4000]
    );
}
fn manual(alias: &str, padding: bool) -> (Writer, Descriptor, SetRoot) {
    let mut writer = Writer::default();
    let count = if padding { 3 } else { 2 };
    let dictionary = writer
        .tree(
            Kind::VisibilityDictionary,
            (0..count)
                .map(|index| {
                    (
                        oid(index).to_vec(),
                        (if alias == "forward" { 0 } else { index as u32 })
                            .to_be_bytes()
                            .to_vec(),
                    )
                })
                .collect(),
        )
        .unwrap();
    let reverse = writer
        .tree(
            Kind::VisibilityOrdinals,
            (0..count)
                .map(|index| {
                    (
                        (index as u32).to_be_bytes().to_vec(),
                        oid(if alias == "reverse" { 1 } else { index }).to_vec(),
                    )
                })
                .collect(),
        )
        .unwrap();
    let (chunks, root) = if padding {
        let mut chunk = vec![0; VISIBILITY_CHUNK_BYTES];
        chunk[0] = 0b1111;
        let chunks = writer
            .tree(
                Kind::VisibilityChunkPool,
                vec![(0u32.to_be_bytes().to_vec(), chunk)],
            )
            .unwrap();
        let table = writer
            .tree(
                Kind::VisibilitySetChunks,
                vec![(0u32.to_be_bytes().to_vec(), 0u32.to_be_bytes().to_vec())],
            )
            .unwrap();
        (
            Some(chunks),
            SetRoot {
                encoding: Encoding::Bitmap,
                count: 2,
                root: table,
            },
        )
    } else {
        (
            None,
            SetRoot {
                encoding: Encoding::Sparse,
                count: 1,
                root: writer
                    .tree(
                        Kind::VisibilitySparseSet,
                        vec![(0u32.to_be_bytes().to_vec(), Vec::new())],
                    )
                    .unwrap(),
            },
        )
    };
    (
        writer,
        Descriptor {
            dictionary: Some(dictionary),
            ordinals: Some(reverse),
            chunks,
        },
        root,
    )
}
#[tokio::test]
async fn re_signed_forward_and_reverse_aliases_cannot_grant_a_different_oid() {
    for alias in ["forward", "reverse"] {
        let (writer, descriptor, root) = manual(alias, false);
        let (store, path, cache) = store(&writer.bytes, 0).await;
        let reader = Reader::new(
            &store,
            &path,
            &cache,
            0,
            writer.bytes.len() as u64,
            &descriptor,
        )
        .unwrap();
        let requested = if alias == "forward" { oid(1) } else { oid(0) };
        assert!(
            reader
                .lookup(Some(&root), &[requested], &mut PageWork::new(128 * 1024, 0))
                .await
                .unwrap_err()
                .to_string()
                .contains("exact OID")
        );
        if alias == "reverse" {
            assert!(
                reader
                    .objects(Some(&root), &mut PageWork::new(128 * 1024, 0))
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("exact ordinal")
            );
        }
        assert!(Decoded::new(&writer.bytes, &descriptor, &mut BTreeMap::new()).is_err());
    }
}
#[tokio::test]
async fn re_signed_padding_is_rejected_even_when_the_requested_bit_is_valid() {
    let (writer, descriptor, root) = manual("", true);
    let (store, path, cache) = store(&writer.bytes, 0).await;
    let reader = Reader::new(
        &store,
        &path,
        &cache,
        0,
        writer.bytes.len() as u64,
        &descriptor,
    )
    .unwrap();
    assert!(
        reader
            .lookup(Some(&root), &[oid(0)], &mut PageWork::new(128 * 1024, 0))
            .await
            .unwrap_err()
            .to_string()
            .contains("padding")
    );
    assert!(
        reader
            .objects(Some(&root), &mut PageWork::new(128 * 1024, 0))
            .await
            .unwrap_err()
            .to_string()
            .contains("padding")
    );
}
