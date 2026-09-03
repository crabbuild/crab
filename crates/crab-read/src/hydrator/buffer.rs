use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use crab_xet::hash::MerkleHash;

use crate::{ReadError, Result};

pub(super) struct ReconstructionBuffer(Arc<Mutex<State>>);

struct State {
    bytes: Option<Vec<u8>>,
    expected: usize,
    exceeded: bool,
}

impl ReconstructionBuffer {
    pub(super) fn new(size: u64) -> Result<Self> {
        let expected = usize::try_from(size)
            .map_err(|source| ReadError::Io(io::Error::new(io::ErrorKind::OutOfMemory, source)))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(expected)
            .map_err(|source| ReadError::Io(io::Error::new(io::ErrorKind::OutOfMemory, source)))?;
        Ok(Self(Arc::new(Mutex::new(State {
            bytes: Some(bytes),
            expected,
            exceeded: false,
        }))))
    }

    pub(super) fn writer(&self) -> impl Write + Send + 'static {
        BufferWriter(Arc::clone(&self.0))
    }

    pub(super) fn finish(self, outcome: Result<()>, file_hash: &MerkleHash) -> Result<Vec<u8>> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| ReadError::internal("reconstruction buffer poisoned"))?;
        let bytes = state
            .bytes
            .take()
            .ok_or_else(|| ReadError::internal("reconstruction buffer closed"))?;
        // This writer's bound is an integrity failure, not an arbitrary I/O
        // failure. Other source failures retain precedence over short output.
        if state.exceeded {
            return Err(ReadError::CorruptObject {
                path: file_hash.hex(),
                reason: format!(
                    "reconstruction exceeds declared output size of {} bytes",
                    state.expected
                ),
            });
        }
        outcome?;
        if bytes.len() != state.expected {
            return Err(ReadError::CorruptObject {
                path: file_hash.hex(),
                reason: format!(
                    "reconstruction size mismatch: expected {}, got {}",
                    state.expected,
                    bytes.len()
                ),
            });
        }
        Ok(bytes)
    }
}

impl Drop for ReconstructionBuffer {
    fn drop(&mut self) {
        // Background writers can outlive a cancelled reconstruction future.
        // They retain the closed handle, not its potentially large allocation.
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(state.bytes.take());
    }
}

struct BufferWriter(Arc<Mutex<State>>);

impl Write for BufferWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self
            .0
            .lock()
            .map_err(|_| io::Error::other("reconstruction buffer poisoned"))?;
        let State {
            bytes: output,
            expected,
            exceeded,
        } = &mut *state;
        let output = output.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "reconstruction buffer closed")
        })?;
        if *exceeded || bytes.len() > *expected - output.len() {
            *exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "reconstructed output exceeds declared size",
            ));
        }
        // Capacity was reserved fallibly before reconstruction. This checked
        // append cannot reallocate in response to an oversized source write.
        output.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash() -> MerkleHash {
        [1; 32].into()
    }

    #[test]
    fn exact_output_preserves_bytes() {
        let buffer = ReconstructionBuffer::new(5).unwrap();
        let mut writer = buffer.writer();
        writer.write_all(b"ab").unwrap();
        writer.write_all(b"cde").unwrap();
        assert_eq!(buffer.finish(Ok(()), &hash()).unwrap(), b"abcde");
    }

    #[test]
    fn overlong_output_cannot_grow_the_buffer() {
        let buffer = ReconstructionBuffer::new(3).unwrap();
        let mut writer = buffer.writer();
        let capacity = buffer.0.lock().unwrap().bytes.as_ref().unwrap().capacity();
        writer.write_all(b"ab").unwrap();
        assert!(writer.write_all(b"cd").is_err());
        assert_eq!(
            buffer.0.lock().unwrap().bytes.as_ref().unwrap().capacity(),
            capacity
        );
        assert_eq!(buffer.0.lock().unwrap().bytes.as_ref().unwrap(), b"ab");
        assert!(matches!(
            buffer.finish(Ok(()), &hash()),
            Err(ReadError::CorruptObject { .. })
        ));
    }

    #[test]
    fn short_success_is_an_integrity_failure() {
        let buffer = ReconstructionBuffer::new(3).unwrap();
        buffer.writer().write_all(b"ab").unwrap();
        assert!(matches!(
            buffer.finish(Ok(()), &hash()),
            Err(ReadError::CorruptObject { .. })
        ));
    }

    #[test]
    fn source_failure_is_not_replaced_by_short_output() {
        let buffer = ReconstructionBuffer::new(3).unwrap();
        let source = ReadError::Io(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(
            matches!(buffer.finish(Err(source), &hash()), Err(ReadError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied)
        );
    }

    #[test]
    fn impossible_capacity_returns_an_error_without_panicking() {
        assert!(
            matches!(ReconstructionBuffer::new(u64::MAX), Err(ReadError::Io(error)) if error.kind() == io::ErrorKind::OutOfMemory)
        );
    }

    #[test]
    fn dropping_owner_releases_buffer_despite_a_live_writer() {
        let buffer = ReconstructionBuffer::new(1024).unwrap();
        let state = Arc::clone(&buffer.0);
        let mut writer = buffer.writer();
        drop(buffer);
        assert!(state.lock().unwrap().bytes.is_none());
        assert_eq!(
            writer.write(b"late").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn finishing_buffer_closes_a_retained_writer() {
        let buffer = ReconstructionBuffer::new(1).unwrap();
        let mut writer = buffer.writer();
        writer.write_all(b"a").unwrap();
        assert_eq!(buffer.finish(Ok(()), &hash()).unwrap(), b"a");
        assert_eq!(
            writer.write(b"late").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }
}
