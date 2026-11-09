use std::{
    io::{Read, Seek, SeekFrom},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use bytes::Bytes;
use parquet::{
    errors::{ParquetError, Result as ParquetResult},
    file::reader::{ChunkReader, Length},
};

use crate::repository::FileSource;

const WINDOW_BYTES: u32 = 1024 * 1024;
const PREVIEW_READ_BUDGET: u64 = 128 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct PreviewReader {
    source: Arc<FileSource>,
    runtime: tokio::runtime::Handle,
    remaining: Arc<AtomicU64>,
    offset: u64,
    window: Bytes,
    window_offset: u64,
}

impl PreviewReader {
    pub(crate) fn new(source: FileSource) -> Self {
        Self {
            source: Arc::new(source),
            runtime: tokio::runtime::Handle::current(),
            remaining: Arc::new(AtomicU64::new(PREVIEW_READ_BUDGET)),
            offset: 0,
            window: Bytes::new(),
            window_offset: 0,
        }
    }
}

impl Read for PreviewReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() || self.offset >= self.source.size {
            return Ok(0);
        }
        if self.offset < self.window_offset
            || self.offset >= self.window_offset + self.window.len() as u64
        {
            let length = u64::from(WINDOW_BYTES).min(self.source.size - self.offset);
            let mut remaining = self.remaining.load(Ordering::Acquire);
            loop {
                let next = remaining.checked_sub(length).ok_or_else(|| {
                    std::io::Error::other("preview exceeded its 128 MiB read budget")
                })?;
                match self.remaining.compare_exchange_weak(
                    remaining,
                    next,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(actual) => remaining = actual,
                }
            }
            // Parsers run on spawn_blocking; the async source retains SDK and
            // native cleanup ownership on the original Tokio runtime.
            self.window = self
                .runtime
                .block_on(self.source.read_range(self.offset, length as u32))
                .map_err(std::io::Error::other)?;
            self.window_offset = self.offset;
        }
        let start = (self.offset - self.window_offset) as usize;
        let length = output.len().min(self.window.len() - start);
        output[..length].copy_from_slice(&self.window[start..start + length]);
        self.offset += length as u64;
        Ok(length)
    }
}

impl Seek for PreviewReader {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let offset = match position {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::End(offset) => i128::from(self.source.size) + i128::from(offset),
            SeekFrom::Current(offset) => i128::from(self.offset) + i128::from(offset),
        };
        self.offset = u64::try_from(offset).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid preview seek")
        })?;
        Ok(self.offset)
    }
}

impl Length for PreviewReader {
    fn len(&self) -> u64 {
        self.source.size
    }
}

impl ChunkReader for PreviewReader {
    type T = Self;

    fn get_read(&self, start: u64) -> ParquetResult<Self::T> {
        let mut reader = self.clone();
        reader.seek(SeekFrom::Start(start))?;
        Ok(reader)
    }

    fn get_bytes(&self, start: u64, length: usize) -> ParquetResult<Bytes> {
        if start > self.source.size || length as u64 > self.source.size - start {
            return Err(ParquetError::EOF("preview range exceeds file size".into()));
        }
        if length as u64 > self.remaining.load(Ordering::Acquire) {
            return Err(ParquetError::General(
                "preview exceeded its 128 MiB read budget".into(),
            ));
        }
        let mut reader = self.get_read(start)?;
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes)?;
        Ok(bytes.into())
    }
}
