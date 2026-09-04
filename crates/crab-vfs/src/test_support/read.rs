use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use crab_cache::LocalCache;
use crab_cache_store::{CacheConfig, CachingStore};
use crab_read::ReadRuntimeBuilder;
use crab_storage::Store;
use crab_storage::test_support::CountingObjectStore;
use crab_types::pointer::Pointer;
use crab_xet::shard::{
    FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo, ShardWriter,
    XorbChunkSequenceEntry, XorbChunkSequenceHeader,
};
use crab_xet::xorb::builder::{RunId, XorbBuilder};
use object_store::memory::InMemory;

use crate::MountReadContext;

pub(crate) struct StoredPointer {
    pub(crate) context: MountReadContext,
    pub(crate) pointer: Pointer,
    pub(crate) origin: Arc<CountingObjectStore>,
    pub(crate) xorb_path: object_store::path::Path,
}

impl StoredPointer {
    pub(crate) fn xorb_body_requests(&self) -> usize {
        use crab_storage::test_support::ObjectReadKind;

        let path = self.xorb_path.to_string();
        self.origin
            .requests()
            .iter()
            .filter(|request| request.location == path && request.kind != ObjectReadKind::Head)
            .count()
    }

    #[expect(clippy::unwrap_used, reason = "construct real shard/xorb test fixture")]
    pub(crate) async fn new(cache_root: &Path, content: Bytes) -> Self {
        let file_hash = *blake3::hash(&content).as_bytes();
        let size = u32::try_from(content.len()).unwrap();
        let chunk = crab_xet::xorb::format::Chunk::new(content);
        let mut builder = XorbBuilder::new();
        builder.push(&chunk, RunId(0)).unwrap();
        let xorb = builder.finalize().unwrap().remove(0);
        let mut shard = ShardWriter::new();
        shard
            .add_xorb(Arc::new(MDBXorbInfo {
                metadata: XorbChunkSequenceHeader::new(xorb.hash, 1, size as usize),
                chunks: vec![XorbChunkSequenceEntry::new(chunk.hash, size, 0)],
            }))
            .unwrap();
        shard
            .add_file(MDBFileInfo {
                metadata: FileDataSequenceHeader::new(file_hash.into(), 1, false, false),
                segments: vec![FileDataSequenceEntry::new(xorb.hash, size, 0, 1)],
                verification: vec![],
                metadata_ext: None,
            })
            .unwrap();
        let (shard_bytes, shard_hash) = shard.finalize().unwrap();
        let origin = Arc::new(CountingObjectStore::new(Arc::new(InMemory::new())));
        let store = Store::new(origin.clone());
        let layout = crate::StoreLayout::new(store.clone(), "vfs-read-test".into());
        let xorb_path = layout.xorb_path(&xorb.hash);
        store.put(&xorb_path, xorb.bytes).await.unwrap();
        store
            .put(&layout.shard_path(&shard_hash), shard_bytes.into())
            .await
            .unwrap();
        let local = Arc::new(LocalCache::new(cache_root.to_owned()));
        let caching =
            CachingStore::new_with_local_cache(store, CacheConfig::default(), local).unwrap();
        let hydrator = ReadRuntimeBuilder::new(caching, layout.clone(), 2)
            .build()
            .unwrap();
        Self {
            context: MountReadContext {
                store_layout: layout,
                hydrator: Arc::new(hydrator),
            },
            pointer: Pointer {
                file_hash,
                size: u64::from(size),
                shard_hint: Some(shard_hash.into()),
            },
            origin,
            xorb_path,
        }
    }
}
