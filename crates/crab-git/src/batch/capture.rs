use super::*;
use crate::incoming_pack::{ScratchSpace, scratch::ScratchFile};
use std::io::SeekFrom;
use std::ops::Range;

const BLOCK_BYTES: usize = 1024 * 1024;

/// Verified immutable bytes owned by an admitted anonymous file.
///
/// Later ranges authenticate copied blocks against the complete Git identity
/// verified at capture. Retain the scratch admission until this owner drops;
/// callers serialize access and supervise cancellation and delivery.
pub struct CapturedBlob {
    file: ScratchFile,
    blocks: Vec<blake3::Hash>,
    size: u64,
    origin: Origin,
}

pub(super) struct Origin {
    path: PathBuf,
    metadata: std::fs::Metadata,
}

impl Origin {
    pub(super) fn new(path: &Path, file: &File) -> io::Result<Self> {
        Ok(Self {
            path: path.to_owned(),
            metadata: file.metadata()?,
        })
    }

    fn unchanged(&self) -> io::Result<bool> {
        let current = match self.path.metadata() {
            Ok(current) => current,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let same = current.is_file()
            && current.len() == self.metadata.len()
            && current.modified().ok() == self.metadata.modified().ok()
            && current.created().ok() == self.metadata.created().ok();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(same
                && current.dev() == self.metadata.dev()
                && current.ino() == self.metadata.ino()
                && current.ctime() == self.metadata.ctime()
                && current.ctime_nsec() == self.metadata.ctime_nsec())
        }
        #[cfg(not(unix))]
        Ok(same)
    }
}

/// Copy a local loose or packed blob through the canonical identity verifier.
///
/// Only local absence returns false; found corruption and I/O errors propagate.
/// Output is untrusted until success. Scratch is created only for delta decoding.
pub fn copy_local_blob(
    objects: &Path,
    oid: &str,
    size: u64,
    output: &mut impl Write,
    scratch: impl FnOnce() -> io::Result<ScratchSpace>,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<bool> {
    copy_observed(objects, oid, size, output, scratch, cancelled).map(|origin| origin.is_some())
}

fn copy_observed(
    objects: &Path,
    oid: &str,
    size: u64,
    output: &mut impl Write,
    scratch: impl FnOnce() -> io::Result<ScratchSpace>,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Option<Origin>> {
    if let Some((file, path)) = open_loose_at(objects, oid, cancelled)? {
        let origin = Origin::new(&path, &file)?;
        copy_loose_blob(file, oid, size, output, cancelled)?;
        return Ok(Some(origin));
    }
    packed::copy_observed(objects, oid, size, output, scratch, cancelled)
}

/// Capture a complete local blob with one Git checksum and bounded block proofs.
///
/// Scratch admission precedes every growth, including delta bases/programs.
/// Only absence returns None. Failure drops the anonymous file and its charge.
pub fn capture_blob(
    objects: &Path,
    oid: &str,
    size: u64,
    scratch: &ScratchSpace,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Option<CapturedBlob>> {
    let mut file = scratch.anonymous_file()?;
    // Admit the known full capture before allocating proof storage or decoding
    // any delta base. All writers share this reservation until the file closes.
    file.reserve(size)?;
    let count = usize::try_from(size.div_ceil(BLOCK_BYTES as u64)).map_err(io::Error::other)?;
    let mut blocks = Vec::new();
    blocks.try_reserve_exact(count).map_err(io::Error::other)?;
    let mut output = Capture {
        file,
        blocks,
        block: blake3::Hasher::new(),
        block_bytes: 0,
    };
    let Some(origin) = copy_observed(
        objects,
        oid,
        size,
        &mut output,
        || Ok(scratch.clone()),
        cancelled,
    )?
    else {
        return Ok(None);
    };
    if output.block_bytes != 0 {
        output.blocks.push(output.block.finalize());
    }
    check_cancelled(cancelled)?;
    Ok(Some(CapturedBlob {
        file: output.file,
        blocks: output.blocks,
        size,
        origin,
    }))
}

struct Capture {
    file: ScratchFile,
    blocks: Vec<blake3::Hash>,
    block: blake3::Hasher,
    block_bytes: usize,
}

impl Write for Capture {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        self.file.write_all(bytes)?;
        let length = bytes.len();
        while !bytes.is_empty() {
            let count = bytes.len().min(BLOCK_BYTES - self.block_bytes);
            self.block.update(&bytes[..count]);
            self.block_bytes += count;
            bytes = &bytes[count..];
            if self.block_bytes == BLOCK_BYTES {
                self.blocks.push(self.block.finalize());
                self.block.reset();
                self.block_bytes = 0;
            }
        }
        Ok(length)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl CapturedBlob {
    /// Invalidate a capture when its observed physical source changes.
    ///
    /// Metadata can reject reuse but never authenticates returned bytes. Each
    /// returned block must still match the verified anonymous capture's proof.
    pub fn source_unchanged(&self) -> io::Result<bool> {
        self.origin.unchanged()
    }

    /// Copy authenticated selected bytes without reconstructing the Git object.
    ///
    /// The range must lie within the captured size. Reject changed/truncated
    /// owned bytes and cancellation; discard output on any returned error.
    pub fn copy_range(
        &mut self,
        range: Range<u64>,
        output: &mut impl Write,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<()> {
        check_cancelled(cancelled)?;
        if range.start > range.end || range.end > self.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Git capture range exceeds its size",
            ));
        }
        if self.file.metadata()?.len() != self.size {
            return Err(invalid("Git capture differs from verified size"));
        }
        if range.is_empty() {
            return check_cancelled(cancelled);
        }
        let mut buffer = vec![0; BLOCK_BYTES];
        let mut offset = range.start / BLOCK_BYTES as u64 * BLOCK_BYTES as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        while offset < range.end {
            let length = (self.size - offset).min(BLOCK_BYTES as u64) as usize;
            let mut read = 0;
            while read < length {
                check_cancelled(cancelled)?;
                let count = self.file.read(&mut buffer[read..length])?;
                if count == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "incomplete Git capture",
                    ));
                }
                read += count;
            }
            if self.blocks.get((offset / BLOCK_BYTES as u64) as usize)
                != Some(&blake3::hash(&buffer[..length]))
            {
                return Err(invalid("Git capture block checksum differs"));
            }
            let from = range.start.saturating_sub(offset) as usize;
            let to = (range.end - offset).min(length as u64) as usize;
            output.write_all(&buffer[from..to])?;
            offset += length as u64;
        }
        check_cancelled(cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(bytes: &[u8]) -> (tempfile::TempDir, String) {
        let root = tempfile::tempdir().unwrap();
        let oid = gix_object::compute_hash(gix_hash::Kind::Sha1, Kind::Blob, bytes)
            .unwrap()
            .to_string();
        let path = root.path().join(&oid[..2]).join(&oid[2..]);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut encoded = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        write!(encoded, "blob {}\0", bytes.len()).unwrap();
        encoded.write_all(bytes).unwrap();
        std::fs::write(path, encoded.finish().unwrap()).unwrap();
        (root, oid)
    }

    #[test]
    fn warm_capture_authenticates_unaligned_blocks_and_empty_eof() {
        let bytes = (0..BLOCK_BYTES + 31)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>();
        let (root, oid) = fixture(&bytes);
        let scratch = ScratchSpace::new(root.path(), 2 * BLOCK_BYTES as u64);
        let mut blob = capture_blob(root.path(), &oid, bytes.len() as u64, &scratch, &|| false)
            .unwrap()
            .unwrap();
        let mut selected = Vec::new();
        blob.copy_range(
            (BLOCK_BYTES - 17) as u64..(BLOCK_BYTES + 20) as u64,
            &mut selected,
            &|| false,
        )
        .unwrap();
        assert_eq!(selected, bytes[BLOCK_BYTES - 17..BLOCK_BYTES + 20]);
        blob.copy_range(
            bytes.len() as u64..bytes.len() as u64,
            &mut std::io::sink(),
            &|| false,
        )
        .unwrap();
    }

    #[test]
    fn corrupt_owned_capture_never_uses_unchanged_origin_metadata_as_proof() {
        let (root, oid) = fixture(b"original captured body");
        let scratch = ScratchSpace::new(root.path(), 1024_u64);
        let mut blob = capture_blob(root.path(), &oid, 22, &scratch, &|| false)
            .unwrap()
            .unwrap();
        blob.file.seek(SeekFrom::Start(21)).unwrap();
        blob.file.write_all(b"!").unwrap();
        assert!(blob.source_unchanged().unwrap());
        let error = blob
            .copy_range(0..4, &mut std::io::sink(), &|| false)
            .unwrap_err();
        assert!(error.to_string().contains("block checksum differs"));
    }

    #[test]
    fn observed_source_replacement_invalidates_a_healthy_capture() {
        let (root, oid) = fixture(b"original captured body");
        let scratch = ScratchSpace::new(root.path(), 1024_u64);
        let blob = capture_blob(root.path(), &oid, 22, &scratch, &|| false)
            .unwrap()
            .unwrap();
        let original = root.path().join(&oid[..2]).join(&oid[2..]);
        std::fs::rename(&original, root.path().join("old-object")).unwrap();
        std::fs::write(original, b"corrupt replacement").unwrap();
        assert!(!blob.source_unchanged().unwrap());
    }

    #[test]
    fn captured_bytes_share_admission_until_the_file_closes() {
        let (root, oid) = fixture(b"12345678");
        let scratch = ScratchSpace::new(root.path(), 8_u64);
        let blob = capture_blob(root.path(), &oid, 8, &scratch, &|| false)
            .unwrap()
            .unwrap();
        assert!(capture_blob(root.path(), &oid, 8, &scratch, &|| false).is_err());
        drop(blob);
        assert!(
            capture_blob(root.path(), &oid, 8, &scratch, &|| false)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn cancelled_capture_releases_admission_for_a_fresh_read() {
        let (root, oid) = fixture(b"12345678");
        let scratch = ScratchSpace::new(root.path(), 8_u64);
        let error = capture_blob(root.path(), &oid, 8, &scratch, &|| true)
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(
            capture_blob(root.path(), &oid, 8, &scratch, &|| false)
                .unwrap()
                .is_some()
        );
    }
}
