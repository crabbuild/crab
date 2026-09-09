use super::*;
use crab_xet::shard::{
    FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo, ShardWriter,
    XorbChunkSequenceEntry, XorbChunkSequenceHeader,
};
use crab_xet::xorb::builder::{RunId, XorbBuilder, XorbResult};
use std::io::Read;

async fn publish_xorb(
    store: &Store,
    layout: &StoreLayout<Store>,
    shard: &mut ShardWriter,
    segments: &mut Vec<FileDataSequenceEntry>,
    xorb: XorbResult,
) {
    let mut offset = 0u32;
    let chunks = xorb
        .placements
        .iter()
        .map(|placement| {
            let entry = XorbChunkSequenceEntry::new(
                placement.chunk_hash,
                placement.uncompressed_size,
                offset,
            );
            offset += placement.uncompressed_size;
            entry
        })
        .collect::<Vec<_>>();
    segments.push(FileDataSequenceEntry::new(
        xorb.hash,
        offset,
        0,
        chunks.len() as u32,
    ));
    shard
        .add_xorb(Arc::new(MDBXorbInfo {
            metadata: XorbChunkSequenceHeader::new(xorb.hash, chunks.len(), offset as usize),
            chunks,
        }))
        .unwrap();
    store
        .put(&layout.xorb_path(&xorb.hash), xorb.bytes)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "publishes persistent large qualification data into an empty dedicated bucket"]
async fn publish_large_qualification_repository() {
    let bucket = std::env::var("CRAB_SDK_LARGE_BUCKET").unwrap();
    let inputs = std::path::PathBuf::from(std::env::var_os("CRAB_SDK_LARGE_INPUTS").unwrap());
    let store =
        crab_storage::build_static_env_store(&bucket, crab_storage::StorageProviderKind::S3)
            .unwrap();
    assert!(
        store
            .inner()
            .list(None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .is_empty()
    );
    let layout = StoreLayout::new(store.clone(), "repository".to_owned());
    let mut file = std::fs::File::open(inputs.join("crab")).unwrap();
    let mut builder = XorbBuilder::new();
    let mut shard = ShardWriter::new();
    let mut segments = Vec::new();
    let mut hash = blake3::Hasher::new();
    let mut sha = sha2::Sha256::new();
    let mut total = 0u64;
    loop {
        let mut bytes = vec![0; 64 * 1024];
        let count = file.read(&mut bytes).unwrap();
        if count == 0 {
            break;
        }
        bytes.truncate(count);
        total += count as u64;
        hash.update(&bytes);
        sha.update(&bytes);
        builder
            .push(&crab_xet::xorb::format::Chunk::new(bytes.into()), RunId(0))
            .unwrap();
        while let Some(xorb) = builder.take_completed() {
            publish_xorb(&store, &layout, &mut shard, &mut segments, xorb).await;
        }
    }
    assert_eq!(total, 1024 * 1024 * 1024);
    assert_eq!(
        format!("{:x}", sha.finalize()),
        "9fd9c7f85fc053365cb38a217ddab35a1b22ea647a293c96f85c7bde0ef23b2a"
    );
    for xorb in builder.finalize().unwrap() {
        publish_xorb(&store, &layout, &mut shard, &mut segments, xorb).await;
    }
    assert!(segments.len() > 1);
    let file_hash = *hash.finalize().as_bytes();
    shard
        .add_file(MDBFileInfo {
            metadata: FileDataSequenceHeader::new(file_hash.into(), segments.len(), false, false),
            segments,
            verification: vec![],
            metadata_ext: None,
        })
        .unwrap();
    let (shard_bytes, shard_hash) = shard.finalize().unwrap();
    store
        .put(&layout.shard_path(&shard_hash), shard_bytes.into())
        .await
        .unwrap();
    let pointer = crab_types::pointer::Pointer {
        file_hash,
        size: total,
        shard_hint: Some(shard_hash.into()),
    };
    let lfs = std::fs::read(inputs.join("lfs")).unwrap();
    let lfs_oid: [u8; 32] = sha2::Sha256::digest(&lfs).into();
    assert_eq!(lfs.len(), 32 * 1024 * 1024);
    assert_eq!(
        format!("{:x}", sha2::Sha256::digest(&lfs)),
        "fcb9475ae4ec6d3eb19b7ca158d538360917528686c9260d8a2c56d5d0ef7f5f"
    );
    let lfs_hash = blake3::hash(&lfs);
    store
        .put(
            &crab_lfs::LfsObjectStore::object_path_for_prefix("repository", &lfs_oid),
            lfs.into(),
        )
        .await
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "--initial-branch=main"]);
    std::fs::write(source.join("crab"), pointer.serialize()).unwrap();
    std::fs::write(
        source.join("lfs"),
        format!(
            "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize {}\n",
            lfs_oid
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            32 * 1024 * 1024
        ),
    )
    .unwrap();
    std::fs::copy(inputs.join("ordinary"), source.join("ordinary")).unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "large qualification fixture"]);
    let commit = git(&source, &["rev-parse", "HEAD"]);
    let base = directory.path().join("fixture");
    let pack_hash = git(
        &source,
        &[
            "pack-objects",
            "--all",
            "--index-version=2",
            base.to_str().unwrap(),
        ],
    );
    let pack = directory.path().join(format!("fixture-{pack_hash}.pack"));
    let index = pack.with_extension("idx");
    let reverse = pack.with_extension("rev");
    crab_git::write_pack_reverse_index(&index, &reverse).unwrap();
    let bytes = std::fs::read(&pack).unwrap();
    let pack_size = bytes.len() as u64;
    let pack_id = MerkleHash::from_hex(blake3::hash(&bytes).to_hex().as_str()).unwrap();
    let locations = crab_git::PackLocationIter::open(&index, &reverse, pack_size)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let object_count = locations.len() as u64;
    store
        .put(&layout.pack_path(&pack_id), bytes.into())
        .await
        .unwrap();
    store
        .put(
            &layout.pack_index_path(&pack_id),
            std::fs::read(index).unwrap().into(),
        )
        .await
        .unwrap();
    store
        .put(
            &layout.pack_reverse_index_path(&pack_id),
            std::fs::read(reverse).unwrap().into(),
        )
        .await
        .unwrap();
    let (pack_index_hash, _, pack_index) = compact_pack_index(
        1,
        &[PackManifestEntry {
            pack_id: pack_id.to_string(),
            size: pack_size,
            content_hash: pack_id.to_string(),
            ref_tips: vec![commit.clone()],
            object_count,
        }],
    )
    .unwrap();
    let (shard_index_hash, _, shard_index) = compact_shard_index(1, &[shard_hash.hex()]).unwrap();
    upload_segmented_bulk(
        &store,
        &layout,
        &BulkData {
            shard_index,
            pack_index,
        },
    )
    .await
    .unwrap();
    let mut manifest = Manifest::default_for_repo("refs/heads/main");
    manifest.generation = 1;
    manifest
        .refs
        .insert("refs/heads/main".to_owned(), commit.clone());
    manifest.pack_index_hash = pack_index_hash;
    manifest.shard_index_hash = shard_index_hash;
    manifest.seal_git_validation();
    create_manifest(&store, &layout, &manifest).await.unwrap();
    let coverage = GitLocatorCoverage {
        generation: 1,
        pack_index_hash: MerkleHash::from_hex(&manifest.pack_index_hash).unwrap(),
    };
    let record = GitPackLocatorRecord {
        pack_id,
        committed_generation: 1,
        pack_index_hash: coverage.pack_index_hash,
        object_count,
        pack_size,
    };
    let entries = locations
        .into_iter()
        .map(|entry| GitObjectLocatorEntry {
            oid: entry.oid.as_bytes().try_into().unwrap(),
            location: GitObjectLocation {
                pack_offset: entry.pack_offset,
                entry_len: entry.entry_len,
                crc32: entry.crc32,
            },
            metadata: Default::default(),
        })
        .collect::<Vec<_>>();
    publish_catalog(&store, coverage, record, &entries).await;
    directory.close().unwrap();
    println!(
        "commit={commit} crab_bytes={total} crab_blake3={} lfs_blake3={lfs_hash}",
        hash.finalize()
    );
}
