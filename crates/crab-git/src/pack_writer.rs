//! Bounded streaming construction of self-contained Git packs.

use std::io::{self, Read, Write};

use gix_object::Kind;

/// Failures constructing a private pack; callers discard output after any error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("pack construction I/O failed")]
    Io(#[from] io::Error),
    #[error("pack checksum rejected")]
    Hash(#[from] gix_hash::hasher::Error),
    #[error("pack exceeds its output byte limit")]
    Limit,
    #[error("pack construction cancelled")]
    Cancelled,
    #[error("pack object count exceeds the Git format")]
    ObjectCount,
    #[error("object contains more bytes than its declared size")]
    ObjectSize,
}

struct Output<W> {
    writer: W,
    hash: gix_hash::Hasher,
    written: u64,
    maximum: u64,
    limit_hit: bool,
}

impl<W: Write> Write for Output<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.maximum.saturating_sub(self.written) {
            self.limit_hit = true;
            return Err(io::Error::other("pack output byte limit"));
        }
        let written = self.writer.write(bytes)?;
        self.hash.update(&bytes[..written]);
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Stream full Git objects into a checksummed pack without retaining compressed output.
///
/// Each input provides kind, exact byte length and a reader ending at that length.
/// The output limit includes headers and the trailer. Cancellation is checked
/// between 64 KiB input chunks; callers provide I/O deadlines and discard partial
/// output on any error. Call on a blocking worker and keep output private until
/// success. Input ownership, graph validation and publication remain with callers.
pub fn write_pack<W, R, I, C>(
    writer: W,
    objects: I,
    maximum: u64,
    cancelled: C,
) -> Result<(), Error>
where
    W: Write,
    R: Read,
    I: ExactSizeIterator<Item = io::Result<(Kind, u64, R)>>,
    C: Fn() -> bool,
{
    let count = u32::try_from(objects.len()).map_err(|_| Error::ObjectCount)?;
    let mut output = Output {
        writer,
        hash: gix_hash::hasher(gix_hash::Kind::Sha1),
        written: 0,
        maximum,
        limit_hit: false,
    };
    let result = (|| {
        check(&cancelled)?;
        output.write_all(b"PACK\0\0\0\x02")?;
        output.write_all(&count.to_be_bytes())?;
        let mut buffer = [0; 64 * 1024];
        for object in objects {
            check(&cancelled)?;
            let (kind, size, mut reader) = object?;
            let header = match kind {
                Kind::Commit => gix_pack::data::entry::Header::Commit,
                Kind::Tree => gix_pack::data::entry::Header::Tree,
                Kind::Blob => gix_pack::data::entry::Header::Blob,
                Kind::Tag => gix_pack::data::entry::Header::Tag,
            };
            header.write_to(size, &mut output)?;
            let mut encoder =
                flate2::write::ZlibEncoder::new(&mut output, flate2::Compression::default());
            let mut remaining = size;
            while remaining != 0 {
                check(&cancelled)?;
                let length = remaining.min(buffer.len() as u64) as usize;
                reader.read_exact(&mut buffer[..length])?;
                encoder.write_all(&buffer[..length])?;
                remaining -= length as u64;
            }
            check(&cancelled)?;
            match reader.read_exact(&mut buffer[..1]) {
                Ok(()) => return Err(Error::ObjectSize),
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {}
                Err(error) => return Err(error.into()),
            }
            encoder.finish()?;
        }
        check(&cancelled)?;
        let checksum = output.hash.clone().try_finalize()?;
        output.write_all(checksum.as_bytes())?;
        output.flush()?;
        Ok(())
    })();
    // Only our own budget rejection sets this flag. Backend I/O errors retain
    // their original source even when they occur near the configured boundary.
    if output.limit_hit {
        Err(Error::Limit)
    } else {
        result
    }
}

fn check(cancelled: &impl Fn() -> bool) -> Result<(), Error> {
    if cancelled() {
        return Err(Error::Cancelled);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_pack_round_trips_through_native_git() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("generated.pack");
        let data = vec![b'x'; 192 * 1024];
        let objects = [
            (Kind::Blob, data.as_slice()),
            (Kind::Blob, b"second".as_slice()),
        ];
        let output = io::BufWriter::new(std::fs::File::create(&path).unwrap());
        write_pack(
            output,
            objects
                .iter()
                .map(|(kind, bytes)| Ok((*kind, bytes.len() as u64, *bytes))),
            1024 * 1024,
            || false,
        )
        .unwrap();
        let pack = std::fs::read(&path).unwrap();
        crate::pack::verify_pack_sha1(&pack).unwrap();
        let index = std::process::Command::new("git")
            .arg("index-pack")
            .arg("--strict")
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            index.status.success(),
            "{}",
            String::from_utf8_lossy(&index.stderr)
        );
        let verify = std::process::Command::new("git")
            .args(["verify-pack", "-v"])
            .arg(path.with_extension("idx"))
            .output()
            .unwrap();
        assert!(
            verify.status.success(),
            "{}",
            String::from_utf8_lossy(&verify.stderr)
        );
        let rows = String::from_utf8(verify.stdout).unwrap();
        for (kind, bytes) in objects {
            let oid = crate::incoming_pack::object_id(kind, bytes);
            let expected = [oid.to_string(), "blob".to_owned(), bytes.len().to_string()];
            assert!(
                rows.lines().any(|line| line
                    .split_whitespace()
                    .take(3)
                    .eq(expected.iter().map(String::as_str))),
                "{rows}"
            );
        }
    }

    #[test]
    fn output_limit_includes_trailer_without_writing_past_limit() {
        let mut complete = Vec::new();
        write_pack(
            &mut complete,
            std::iter::once(Ok((Kind::Blob, 3, b"abc".as_slice()))),
            1024,
            || false,
        )
        .unwrap();
        for maximum in [0, 11, complete.len() as u64 - 1, complete.len() as u64] {
            let mut output = Vec::new();
            let result = write_pack(
                &mut output,
                std::iter::once(Ok((Kind::Blob, 3, b"abc".as_slice()))),
                maximum,
                || false,
            );
            if maximum == complete.len() as u64 {
                assert_eq!(output, complete);
                result.unwrap();
            } else {
                assert!(matches!(result, Err(Error::Limit)));
                assert!(output.len() as u64 <= maximum);
            }
        }
    }

    #[test]
    fn object_length_mismatch_never_produces_a_successful_pack() {
        for size in [2, 4] {
            let result = write_pack(
                Vec::new(),
                std::iter::once(Ok((Kind::Blob, size, b"abc".as_slice()))),
                1024,
                || false,
            );
            assert!(matches!(result, Err(Error::ObjectSize) | Err(Error::Io(_))));
        }
    }

    #[test]
    fn cancellation_stops_reading_between_bounded_chunks() {
        use std::cell::Cell;
        struct Reader<'a>(&'a Cell<usize>);
        impl Read for Reader<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                assert!(bytes.len() <= 64 * 1024);
                bytes.fill(b'x');
                self.0.set(self.0.get() + bytes.len());
                Ok(bytes.len())
            }
        }
        let read = Cell::new(0);
        let result = write_pack(
            Vec::new(),
            std::iter::once(Ok((Kind::Blob, 1024 * 1024, Reader(&read)))),
            1024 * 1024,
            || read.get() != 0,
        );
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(read.get(), 64 * 1024);
    }

    #[test]
    fn short_output_writes_preserve_the_pack_checksum() {
        struct Short(Vec<u8>);
        impl Write for Short {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let count = bytes.len().min(3);
                self.0.extend_from_slice(&bytes[..count]);
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut output = Short(Vec::new());
        write_pack(
            &mut output,
            std::iter::once(Ok((Kind::Blob, 3, b"abc".as_slice()))),
            1024,
            || false,
        )
        .unwrap();
        crate::pack::verify_pack_sha1(&output.0).unwrap();
    }

    #[test]
    fn backend_errors_retain_the_original_io_source() {
        struct Denied;
        impl Write for Denied {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let result = write_pack(
            Denied,
            std::iter::once(Ok((Kind::Blob, 3, b"abc".as_slice()))),
            1024,
            || false,
        );
        assert!(
            matches!(result, Err(Error::Io(source)) if source.kind() == io::ErrorKind::PermissionDenied)
        );
    }
}
