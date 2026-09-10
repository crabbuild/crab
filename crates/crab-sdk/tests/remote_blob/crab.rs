use bytes::Bytes;
use crab_sdk::GitPath;
use crab_sdk::operation::ReadOptions;
use crab_sdk::remote::Snapshot;
use crab_storage::{Store, StoreLayout};
use crab_xet::hash::MerkleHash;
use crab_xet::shard::{
    FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo, ShardWriter,
    XorbChunkSequenceEntry, XorbChunkSequenceHeader,
};
use crab_xet::xorb::builder::{RunId, XorbBuilder};
use std::sync::Arc;

pub(super) struct Fixture {
    pub pointer: crab_types::pointer::Pointer,
    pub shard: (MerkleHash, Bytes),
    xorb: (MerkleHash, Bytes),
}

impl Fixture {
    pub fn new(data: &[u8]) -> Self {
        let chunk = crab_xet::xorb::format::Chunk::new(Bytes::copy_from_slice(data));
        let mut builder = XorbBuilder::new();
        builder.push(&chunk, RunId(0)).unwrap();
        let xorb = builder.finalize().unwrap().remove(0);
        let file_hash = *blake3::hash(data).as_bytes();
        let mut shard = ShardWriter::new();
        shard
            .add_xorb(Arc::new(MDBXorbInfo {
                metadata: XorbChunkSequenceHeader::new(xorb.hash, 1, data.len()),
                chunks: vec![XorbChunkSequenceEntry::new(
                    chunk.hash,
                    data.len() as u32,
                    0,
                )],
            }))
            .unwrap();
        shard
            .add_file(MDBFileInfo {
                metadata: FileDataSequenceHeader::new(file_hash.into(), 1, false, false),
                segments: vec![FileDataSequenceEntry::new(
                    xorb.hash,
                    data.len() as u32,
                    0,
                    1,
                )],
                verification: vec![],
                metadata_ext: None,
            })
            .unwrap();
        let (bytes, hash) = shard.finalize().unwrap();
        Self {
            pointer: crab_types::pointer::Pointer {
                file_hash,
                size: data.len() as u64,
                shard_hint: Some(hash.into()),
            },
            shard: (hash, bytes.into()),
            xorb: (xorb.hash, xorb.bytes),
        }
    }
    pub async fn publish(&self, store: &Store, layout: &StoreLayout<Store>) {
        store
            .put(&layout.shard_path(&self.shard.0), self.shard.1.clone())
            .await
            .unwrap();
        store
            .put(&layout.xorb_path(&self.xorb.0), self.xorb.1.clone())
            .await
            .unwrap_or_else(|error| {
                let mut source = std::error::Error::source(&error);
                while let Some(value) = source {
                    eprintln!("close source: {value:?}");
                    source = value.source();
                }
                panic!("close failed: {error:?}");
            });
    }
    pub async fn verify_corruption(
        &self,
        storage: &Store,
        options: crab_sdk::storage::DirectStoreOptions,
        layout: &StoreLayout<Store>,
        cache: &std::path::Path,
    ) {
        let object = layout.xorb_path(&self.xorb.0);
        let mut corrupt = self.xorb.1.to_vec();
        *corrupt.last_mut().unwrap() ^= 0xff;
        storage
            .put_overwrite(&object, corrupt.into())
            .await
            .unwrap();
        std::fs::create_dir(cache).unwrap();
        let client = crab_sdk::Client::builder()
            .direct_store(options)
            .content_cache(crab_sdk::storage::ContentCache::new(cache, 4 * 1024 * 1024).unwrap())
            .build()
            .unwrap();
        let repository = client
            .open(crab_sdk::OpenOptions::remote(
                crab_sdk::RepositoryLocator::new("repository").unwrap(),
            ))
            .await
            .unwrap();
        let snapshot = repository
            .remote()
            .unwrap()
            .snapshot(crab_sdk::Revision::branch("main").unwrap())
            .await
            .unwrap();
        let mut stream = snapshot
            .open_file(GitPath::new(b"z-crab".to_vec()).unwrap())
            .await
            .unwrap();
        let error = loop {
            match stream.next().await {
                Ok(Some(_)) => continue,
                Ok(None) => panic!("corrupt xorb reached successful EOF"),
                Err(error) => break error,
            }
        };
        assert_eq!(error.kind(), crab_sdk::ErrorKind::Corruption);
        super::archive::fails_without_completing_entry(
            &snapshot,
            b"z-crab",
            crab_sdk::ErrorKind::Corruption,
        )
        .await;
        stream.close().await.unwrap();
        client.close().await.unwrap();
        storage
            .put_overwrite(&object, self.xorb.1.clone())
            .await
            .unwrap();
    }
}

pub(super) async fn verify(snapshot: &Snapshot, original: &[u8]) {
    let path = GitPath::new(b"z-crab".to_vec()).unwrap();
    let size = original.len() as u64;
    for range in [0..size, 0..0, size..size, 1..size - 1, 65530..65547] {
        let mut stream = snapshot
            .open_file(path.clone())
            .with_options(ReadOptions::default().with_range(range.clone()).unwrap())
            .await
            .unwrap();
        let mut actual = Vec::new();
        while let Some(bytes) = stream.next().await.unwrap() {
            assert!(bytes.len() <= 64 * 1024);
            actual.extend_from_slice(&bytes);
        }
        assert_eq!(actual, original[range.start as usize..range.end as usize]);
        stream.close().await.unwrap();
    }
    for advance in [false, true] {
        let mut stream = snapshot.open_file(path.clone()).await.unwrap();
        if advance {
            assert!(stream.next().await.unwrap().is_some());
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), stream.close())
            .await
            .unwrap()
            .unwrap_or_else(|error| {
                let mut source = std::error::Error::source(&error);
                while let Some(value) = source {
                    eprintln!("close source: {value:?}");
                    source = value.source();
                }
                panic!("close failed: {error:?}");
            });
    }
    let mut stream = snapshot
        .open_file(path)
        .with_options(
            ReadOptions::default()
                .with_timeout(std::time::Duration::from_millis(200))
                .unwrap(),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let error = loop {
        match stream.next().await {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("idle Crab stream escaped its deadline"),
            Err(error) => break error,
        }
    };
    assert_eq!(error.kind(), crab_sdk::ErrorKind::Timeout);
    stream.close().await.unwrap();
}

pub(super) async fn verify_limits(
    options: crab_sdk::storage::DirectStoreOptions,
    cache: &std::path::Path,
) {
    std::fs::create_dir(cache).unwrap();
    let client = crab_sdk::Client::builder()
        .direct_store(options)
        .content_cache(crab_sdk::storage::ContentCache::new(cache, 4 * 1024 * 1024).unwrap())
        .build()
        .unwrap();
    let repository = client
        .open(crab_sdk::OpenOptions::remote(
            crab_sdk::RepositoryLocator::new("repository").unwrap(),
        ))
        .await
        .unwrap();
    let snapshot = repository
        .remote()
        .unwrap()
        .snapshot(crab_sdk::Revision::branch("main").unwrap())
        .await
        .unwrap();
    for limits in [
        crab_sdk::operation::ReadLimits {
            max_fetched_bytes: 1,
            ..Default::default()
        },
        crab_sdk::operation::ReadLimits {
            max_storage_requests: 1,
            ..Default::default()
        },
    ] {
        super::ranges::verify_hydration_limit(
            &snapshot,
            GitPath::new(b"z-crab".to_vec()).unwrap(),
            ReadOptions::default().with_limits(limits).unwrap(),
        )
        .await;
    }
    let mut stream = snapshot
        .open_file(GitPath::new(b"z-crab".to_vec()).unwrap())
        .await
        .unwrap();
    while stream.next().await.unwrap().is_some() {}
    stream.close().await.unwrap();
    client.close().await.unwrap();
}

pub(super) async fn verify_pinned_lookup(snapshot: &Snapshot, original: &[u8]) {
    for name in [
        b"z-crab-no-hint".as_slice(),
        b"z-crab-stale-hint".as_slice(),
    ] {
        for range in [0..original.len() as u64, 1..original.len() as u64 - 1] {
            let mut stream = snapshot
                .open_file(GitPath::new(name.to_vec()).unwrap())
                .with_options(ReadOptions::default().with_range(range.clone()).unwrap())
                .await
                .unwrap();
            let mut actual = Vec::new();
            while let Some(bytes) = stream.next().await.unwrap() {
                actual.extend_from_slice(&bytes);
            }
            stream.close().await.unwrap();
            assert_eq!(actual, original[range.start as usize..range.end as usize]);
        }
    }
}

pub(super) async fn verify_lookup_missing(snapshot: &Snapshot) {
    for name in [
        b"z-crab-no-hint".as_slice(),
        b"z-crab-stale-hint".as_slice(),
    ] {
        let mut stream = snapshot
            .open_file(GitPath::new(name.to_vec()).unwrap())
            .await
            .unwrap();
        let error = loop {
            match stream.next().await {
                Err(error) => break error,
                Ok(Some(_)) => continue,
                Ok(None) => panic!("missing pinned recipe reached EOF"),
            }
        };
        assert_eq!(error.kind(), crab_sdk::ErrorKind::NotFound);
        stream.close().await.unwrap();
    }
}
