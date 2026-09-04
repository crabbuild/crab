use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use crab_storage::test_support::CountingObjectStore;
use crab_types::pointer::Pointer;
use crab_xet::shard::{
    FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo, ShardWriter,
    XorbChunkSequenceEntry, XorbChunkSequenceHeader,
};
use crab_xet::xorb::builder::{RunId, XorbBuilder};
use object_store::memory::InMemory;

use crate::cmd::hydrate::HydrationRuntime;
use crate::core::config::Config;
use crate::core::error::Result;

pub(crate) async fn stored_file(
    cache_root: &Path,
    content: Bytes,
    corrupt_origin: bool,
) -> Result<(HydrationRuntime, Pointer, Arc<CountingObjectStore>)> {
    let file_hash = *blake3::hash(&content).as_bytes();
    let size = content.len();
    let chunk = crab_xet::xorb::format::Chunk::new(content);
    let mut builder = XorbBuilder::new();
    builder.push(&chunk, RunId(0))?;
    let xorb = builder.finalize()?.remove(0);
    let mut shard = ShardWriter::new();
    shard.add_xorb(Arc::new(MDBXorbInfo {
        metadata: XorbChunkSequenceHeader::new(xorb.hash, 1, size),
        chunks: vec![XorbChunkSequenceEntry::new(chunk.hash, size as u32, 0)],
    }))?;
    shard.add_file(MDBFileInfo {
        metadata: FileDataSequenceHeader::new(file_hash.into(), 1, false, false),
        segments: vec![FileDataSequenceEntry::new(xorb.hash, size as u32, 0, 1)],
        verification: vec![],
        metadata_ext: None,
    })?;
    let (shard_bytes, shard_hash) = shard.finalize()?;
    let counted = Arc::new(CountingObjectStore::new(Arc::new(InMemory::new())));
    let store = crate::storage::Store::new(counted.clone());
    let layout = crab_storage::StoreLayout::new(store.clone(), "read-test".into());
    store
        .put(&layout.shard_path(&shard_hash), shard_bytes.into())
        .await?;
    let mut xorb_bytes = xorb.bytes.to_vec();
    if corrupt_origin {
        *xorb_bytes.last_mut().unwrap() ^= 0xff;
    }
    store
        .put(&layout.xorb_path(&xorb.hash), xorb_bytes.into())
        .await?;
    let local = Arc::new(crate::cache::LocalCache::new(cache_root.to_owned()));
    let config = Config::default();
    let caching =
        crab_cache_store::CachingStore::new_with_local_cache(store, &config.cache, local)?;
    let hydrator = super::build_cli_hydrator(caching, layout, &config)?;
    let pointer = Pointer {
        file_hash,
        size: size as u64,
        shard_hint: Some(shard_hash.into()),
    };
    Ok((hydrator, pointer, counted))
}
