use std::io::Write;
use std::sync::Arc;

use crab_cache_store::CachingStore;
use crab_types::pointer::Pointer;
use crab_xet::hash::MerkleHash;
use xet_client::cas_client::adaptive_concurrency::AdaptiveConcurrencyController;
use xet_client::cas_types::FileRange;
use xet_runtime::core::XetContext;

use crate::{ReadError, Result, StoreClient};

const HYDRATE_CONCURRENCY_TAG: &str = "crab-read";

pub type ReadStoreLayout = crab_storage::StoreLayout<crab_storage::Store>;

/// Read-side hydrator for reconstructing Crab pointers.
pub struct ShardHydrator {
    store: CachingStore,
    router: ReadStoreLayout,
    concurrency: Arc<AdaptiveConcurrencyController>,
    chunk_cache: Option<Arc<dyn xet_client::chunk_cache::ChunkCache>>,
}

impl ShardHydrator {
    #[must_use]
    pub fn with_concurrency(
        store: CachingStore,
        router: ReadStoreLayout,
        concurrency: Arc<AdaptiveConcurrencyController>,
    ) -> Self {
        Self {
            store,
            router,
            concurrency,
            chunk_cache: None,
        }
    }

    /// Create a new hydrator with a fixed download concurrency.
    ///
    /// # Errors
    ///
    /// Returns an error when the Xet runtime context cannot be created.
    pub fn new(
        store: CachingStore,
        router: ReadStoreLayout,
        download_concurrency: usize,
    ) -> Result<Self> {
        let concurrency = fixed_hydrate_concurrency(download_concurrency)?;
        Ok(Self::with_concurrency(store, router, concurrency))
    }

    #[must_use]
    pub fn with_xet_chunk_cache(
        mut self,
        cache: Arc<dyn xet_client::chunk_cache::ChunkCache>,
    ) -> Self {
        self.chunk_cache = Some(cache);
        self
    }

    /// Reconstruct a pointer into memory and verify its whole-file hash.
    pub async fn reconstruct_from_pointer(&self, pointer_bytes: &[u8]) -> Result<Vec<u8>> {
        let ptr = Pointer::parse(pointer_bytes)?;
        self.reconstruct_file(&ptr).await
    }

    /// Reconstruct a pointer into `dest` and verify its whole-file hash.
    pub async fn reconstruct_to_path(&self, ptr: &Pointer, dest: &std::path::Path) -> Result<u64> {
        let file = std::fs::File::create(dest)?;
        self.reconstruct_to_writer(ptr, file).await
    }

    /// Reconstruct a pointer into a blocking writer and verify its hash.
    pub async fn reconstruct_to_writer<W>(&self, ptr: &Pointer, writer: W) -> Result<u64>
    where
        W: Write + Send + 'static,
    {
        let file_hash = MerkleHash::from(ptr.file_hash);
        let client = self.store_client_for_pointer(ptr);
        self.preflight_shard_coverage(&client, ptr).await?;

        let tap_state = Arc::new(std::sync::Mutex::new(GenericHasherTapState {
            writer: Some(writer),
            hasher: blake3::Hasher::new(),
            bytes_written: 0,
        }));
        let writer = GenericHasherTap {
            shared: Arc::clone(&tap_state),
        };

        self.reconstruct_to_writer_unverified(client, file_hash, writer, None)
            .await?;

        let (actual_hash, bytes_written) = {
            let mut guard = tap_state
                .lock()
                .map_err(|_| ReadError::internal("hasher tap poisoned"))?;
            if let Some(writer) = guard.writer.as_mut() {
                writer.flush()?;
            }
            drop(guard.writer.take());
            let hash: [u8; 32] = guard.hasher.finalize().into();
            (hash, guard.bytes_written)
        };

        if actual_hash != ptr.file_hash {
            return Err(ReadError::HashMismatch {
                requested: crab_types::pointer::hex_encode(&ptr.file_hash),
                actual: crab_types::pointer::hex_encode(&actual_hash),
            });
        }

        Ok(bytes_written)
    }

    /// Reconstruct bytes covering `[start, end)` of the original file.
    pub async fn reconstruct_range_from_pointer(
        &self,
        pointer_bytes: &[u8],
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>> {
        let ptr = Pointer::parse(pointer_bytes)?;
        let client = self.store_client_for_pointer(&ptr);

        if end < start {
            return Err(ReadError::internal(format!(
                "reconstruct_range_from_pointer: end ({end}) < start ({start})"
            )));
        }

        let end = end.min(ptr.size);
        let start = start.min(ptr.size);
        if start >= end {
            return Ok(Vec::new());
        }

        #[expect(
            clippy::cast_possible_truncation,
            reason = "range bounds fit usize on every platform crab runs on"
        )]
        let capacity = (end - start) as usize;
        let buffer: std::io::Cursor<Vec<u8>> = std::io::Cursor::new(Vec::with_capacity(capacity));
        let shared = Arc::new(std::sync::Mutex::new(buffer));
        let writer = SharedCursorWriter(Arc::clone(&shared));
        let file_hash = MerkleHash::from(ptr.file_hash);
        let range = FileRange::new(start, end);

        self.reconstruct_to_writer_unverified(client, file_hash, writer, Some(range))
            .await?;

        let content = {
            let mut guard = shared
                .lock()
                .map_err(|_| ReadError::internal("reconstruction writer poisoned"))?;
            std::mem::take(guard.get_mut())
        };

        Ok(content)
    }

    fn store_client_for_pointer(&self, ptr: &Pointer) -> Arc<dyn xet_client::cas_client::Client> {
        let file_hash = MerkleHash::from(ptr.file_hash);
        let client = StoreClient::new(
            self.store.clone(),
            self.router.clone(),
            Arc::clone(&self.concurrency),
        );
        let client = match ptr.shard_hint {
            Some(hint) => client.with_shard_hint(file_hash, MerkleHash::from(hint)),
            None => client,
        };
        Arc::new(client)
    }

    async fn reconstruct_file(&self, ptr: &Pointer) -> Result<Vec<u8>> {
        let file_hash = MerkleHash::from(ptr.file_hash);
        let client = self.store_client_for_pointer(ptr);
        self.preflight_shard_coverage(&client, ptr).await?;

        #[expect(
            clippy::cast_possible_truncation,
            reason = "pointer size fits usize on all platforms crab runs on"
        )]
        let expected_size = ptr.size as usize;
        let buffer: std::io::Cursor<Vec<u8>> =
            std::io::Cursor::new(Vec::with_capacity(expected_size));
        let shared = Arc::new(std::sync::Mutex::new(buffer));
        let writer = SharedCursorWriter(Arc::clone(&shared));

        self.reconstruct_to_writer_unverified(client, file_hash, writer, None)
            .await?;

        let content = {
            let mut guard = shared
                .lock()
                .map_err(|_| ReadError::internal("reconstruction writer poisoned"))?;
            std::mem::take(guard.get_mut())
        };

        let actual_hash: [u8; 32] = *blake3::hash(&content).as_bytes();
        if actual_hash != ptr.file_hash {
            return Err(ReadError::HashMismatch {
                requested: crab_types::pointer::hex_encode(&ptr.file_hash),
                actual: crab_types::pointer::hex_encode(&actual_hash),
            });
        }

        Ok(content)
    }

    async fn reconstruct_to_writer_unverified<W>(
        &self,
        client: Arc<dyn xet_client::cas_client::Client>,
        file_hash: MerkleHash,
        writer: W,
        range: Option<FileRange>,
    ) -> Result<()>
    where
        W: Write + Send + 'static,
    {
        let xet_context = XetContext::default().map_err(|error| {
            ReadError::internal(format!("failed to initialize xet context: {error}"))
        })?;
        let reconstructor =
            xet_data::file_reconstruction::FileReconstructor::new(&xet_context, &client, file_hash);
        let reconstructor = match range {
            Some(range) => reconstructor.with_byte_range(range),
            None => reconstructor,
        };
        let reconstructor = match self.chunk_cache.clone() {
            Some(cache) => reconstructor.with_chunk_cache(cache),
            None => reconstructor,
        };

        reconstructor
            .reconstruct_to_writer(writer)
            .await
            .map_err(|e| {
                ReadError::internal(format!(
                    "file reconstruction failed for {}: {e}",
                    file_hash.hex()
                ))
            })?;
        Ok(())
    }

    async fn preflight_shard_coverage(
        &self,
        client: &Arc<dyn xet_client::cas_client::Client>,
        ptr: &Pointer,
    ) -> Result<()> {
        let file_hash = MerkleHash::from(ptr.file_hash);
        let Ok(Some((info, _))) = client.get_file_reconstruction_info(&file_hash).await else {
            return Ok(());
        };

        let covered: u64 = info
            .segments
            .iter()
            .map(|s| u64::from(s.unpacked_segment_bytes))
            .sum();
        if covered >= ptr.size {
            return Ok(());
        }

        let example = info.segments.first().map_or_else(
            || (0, String::new()),
            |s| (s.chunk_index_start, s.xorb_hash.hex()),
        );

        Err(ReadError::IncompleteShardReconstruction {
            file_hash: file_hash.hex(),
            uncovered_chunks: 1,
            example_chunk_hash: example.1,
            example_chunk_index: example.0,
        })
    }
}

pub fn fixed_hydrate_concurrency(concurrency: usize) -> Result<Arc<AdaptiveConcurrencyController>> {
    let context = XetContext::default().map_err(|error| {
        ReadError::internal(format!("failed to initialize xet context: {error}"))
    })?;
    Ok(AdaptiveConcurrencyController::new_fixed(
        context,
        HYDRATE_CONCURRENCY_TAG,
        concurrency,
    ))
}

struct SharedCursorWriter(Arc<std::sync::Mutex<std::io::Cursor<Vec<u8>>>>);

impl Write for SharedCursorWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("shared cursor mutex poisoned"))?
            .write(buf)
    }

    fn write_vectored(&mut self, bufs: &[std::io::IoSlice<'_>]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("shared cursor mutex poisoned"))?
            .write_vectored(bufs)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("shared cursor mutex poisoned"))?
            .flush()
    }
}

struct GenericHasherTapState<W: Write> {
    writer: Option<W>,
    hasher: blake3::Hasher,
    bytes_written: u64,
}

struct GenericHasherTap<W: Write> {
    shared: Arc<std::sync::Mutex<GenericHasherTapState<W>>>,
}

fn hash_vectored_prefix(
    hasher: &mut blake3::Hasher,
    bufs: &[std::io::IoSlice<'_>],
    mut written: usize,
) {
    for buf in bufs {
        if written == 0 {
            break;
        }
        let take = written.min(buf.len());
        hasher.update(&buf[..take]);
        written -= take;
    }
}

impl<W: Write> Write for GenericHasherTap<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| std::io::Error::other("hasher tap mutex poisoned"))?;
        let written = guard
            .writer
            .as_mut()
            .ok_or_else(|| std::io::Error::other("hasher tap: writer already taken"))?
            .write(buf)?;
        guard.hasher.update(&buf[..written]);
        guard.bytes_written = guard.bytes_written.saturating_add(written as u64);
        Ok(written)
    }

    fn write_vectored(&mut self, bufs: &[std::io::IoSlice<'_>]) -> std::io::Result<usize> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| std::io::Error::other("hasher tap mutex poisoned"))?;
        let written = guard
            .writer
            .as_mut()
            .ok_or_else(|| std::io::Error::other("hasher tap: writer already taken"))?
            .write_vectored(bufs)?;
        hash_vectored_prefix(&mut guard.hasher, bufs, written);
        guard.bytes_written = guard.bytes_written.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| std::io::Error::other("hasher tap mutex poisoned"))?;
        match guard.writer.as_mut() {
            Some(writer) => writer.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{IoSlice, Write};
    use std::sync::{Arc, Mutex};

    use super::{GenericHasherTap, GenericHasherTapState};

    struct PartialVectoredWriter;

    impl Write for PartialVectoredWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len().min(5))
        }

        fn write_vectored(&mut self, _bufs: &[IoSlice<'_>]) -> std::io::Result<usize> {
            Ok(5)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn generic_hasher_tap_hashes_only_partial_vectored_write() {
        let shared = Arc::new(Mutex::new(GenericHasherTapState {
            writer: Some(PartialVectoredWriter),
            hasher: blake3::Hasher::new(),
            bytes_written: 0,
        }));
        let mut tap = GenericHasherTap {
            shared: Arc::clone(&shared),
        };
        let bufs = [IoSlice::new(b"abc"), IoSlice::new(b"defg")];

        let written = tap.write_vectored(&bufs).expect("partial vectored write");
        let guard = shared.lock().expect("hasher state");
        let actual = *guard.hasher.finalize().as_bytes();

        assert_eq!(
            (written, guard.bytes_written, actual),
            (5, 5, *blake3::hash(b"abcde").as_bytes())
        );
    }
}
