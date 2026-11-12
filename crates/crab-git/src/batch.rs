//! Bounded identity verification of raw Git object streams.

use bstr::ByteSlice as _;
use gix_object::Kind;
use sha2::Digest as _;
use std::borrow::Cow;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

mod packed;
pub use packed::copy_packed_blob;
mod capture;
pub use capture::{CapturedBlob, capture_blob, copy_local_blob};

/// A captured blob header whose body still requires identity verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobHeader {
    pub oid: [u8; 20],
    pub size: u64,
}

/// Verify an ordered raw blob batch against captured OIDs, kinds and sizes.
///
/// Missing, reordered, truncated, extra or checksum-invalid responses fail.
/// The process owner must supervise blocked reads and cancellation separately.
pub fn verify_blob_batch(
    input: impl Read,
    expected: &[BlobHeader],
    cancelled: &dyn Fn() -> bool,
) -> io::Result<()> {
    let mut reader = BufReader::new(input);
    let mut buffer = [0u8; 64 * 1024];
    for blob in expected {
        let oid = gix_hash::ObjectId::Sha1(blob.oid).to_string();
        read_object(
            &mut reader,
            &oid,
            Some(blob.size),
            0,
            None,
            &mut buffer,
            cancelled,
        )?;
    }
    finish(&mut reader, cancelled)
}

/// Copy one raw Git blob batch response while verifying its captured identity.
///
/// Errors can follow partial writes; callers must discard output on failure
/// and supervise blocked I/O and process cancellation separately.
pub fn copy_blob(
    input: impl Read,
    oid: &str,
    size: u64,
    output: &mut impl Write,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<()> {
    let mut reader = BufReader::new(input);
    let mut buffer = [0u8; 64 * 1024];
    read_object(
        &mut reader,
        oid,
        Some(size),
        0,
        Some(output),
        &mut buffer,
        cancelled,
    )?;
    finish(&mut reader, cancelled)
}

/// Copy one loose zlib blob with bounded buffers and captured Git identity checks.
///
/// The complete header, body, zlib footer and input EOF must verify. Errors can
/// follow partial writes; callers discard output and supervise blocked I/O.
pub fn copy_loose_blob(
    input: impl Read,
    oid: &str,
    size: u64,
    output: &mut impl Write,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<()> {
    let mut reader = BufReader::with_capacity(64 * 1024, input);
    let expected = format!("blob {size}\0");
    let mut header_bytes = 0;
    let mut body = ObjectBody::new(
        ObjectHeader {
            oid,
            kind: Kind::Blob,
            size,
        },
        0,
        Some(output),
    )?;
    let accept = |bytes: &[u8]| {
        let count = (expected.len() - header_bytes).min(bytes.len());
        if bytes[..count] != expected.as_bytes()[header_bytes..header_bytes + count] {
            return Err(invalid("Git loose blob header differs from captured size"));
        }
        header_bytes += count;
        body.push(&bytes[count..])
    };
    inflate(&mut reader, cancelled, accept)?;
    if header_bytes != expected.len() {
        return Err(invalid("incomplete Git loose blob header"));
    }
    body.finish()?;
    finish(&mut reader, cancelled)
}

fn inflate(
    reader: &mut impl BufRead,
    cancelled: &dyn Fn() -> bool,
    mut accept: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<u64> {
    let mut decoder = flate2::Decompress::new(true);
    let mut buffer = [0u8; 64 * 1024];
    loop {
        check_cancelled(cancelled)?;
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let status = decoder
            .decompress(
                reader.fill_buf()?,
                &mut buffer,
                flate2::FlushDecompress::None,
            )
            .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?;
        let consumed = (decoder.total_in() - before_in) as usize;
        let produced = (decoder.total_out() - before_out) as usize;
        reader.consume(consumed);
        accept(&buffer[..produced])?;
        if status == flate2::Status::StreamEnd {
            return Ok(decoder.total_out());
        }
        // Only StreamEnd authenticates the footer. Keep cancellation outside
        // high-level read adapters, which may retry Interrupted or accept EOF.
        if consumed == 0 && produced == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete Git zlib object",
            ));
        }
    }
}

/// Open a regular loose object in the primary or a reachable alternate database.
///
/// Only absence selects another database. Paths follow Git's quoting, relative
/// bases, duplicate suppression and recursion limit; environment overrides belong
/// to the caller that selected `objects_directory`. Returned bytes are unverified;
/// use `copy_loose_blob` before delivery. Metadata and retained paths share 8 MiB.
pub fn open_loose_blob(
    objects_directory: &Path,
    oid: &str,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Option<File>> {
    Ok(open_loose_at(objects_directory, oid, cancelled)?.map(|(file, _)| file))
}

fn open_loose_at(
    objects_directory: &Path,
    oid: &str,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Option<(File, PathBuf)>> {
    if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("Git loose lookup requires a hexadecimal object ID"));
    }
    find_in_databases(objects_directory, cancelled, |directory| {
        let path = directory.join(&oid[..2]).join(&oid[2..]);
        match path.metadata() {
            Ok(metadata) if !metadata.is_file() => {
                return Err(invalid("Git loose object is not a regular file"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
        match File::open(&path) {
            Ok(file) if file.metadata()?.is_file() => Ok(Some((file, path))),
            Ok(_) => Err(invalid("Git loose object is not a regular file")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    })
}

fn find_in_databases<T>(
    objects_directory: &Path,
    cancelled: &dyn Fn() -> bool,
    mut find: impl FnMut(&Path) -> io::Result<Option<T>>,
) -> io::Result<Option<T>> {
    let mut pending = vec![(objects_directory.to_owned(), 0)];
    let mut visited = HashSet::new();
    const MAX_ALTERNATE_BYTES: usize = 8 * 1024 * 1024;
    let mut remaining = MAX_ALTERNATE_BYTES;
    let mut spend = |bytes: usize| {
        remaining = remaining
            .checked_sub(bytes)
            .ok_or_else(|| invalid("Git alternates exceed the 8 MiB metadata budget"))?;
        Ok::<_, io::Error>(())
    };
    while let Some((directory, depth)) = pending.pop() {
        check_cancelled(cancelled)?;
        let directory = match directory.canonicalize() {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if !visited.insert(directory.clone()) {
            continue;
        }
        spend(directory.as_os_str().as_encoded_bytes().len())?;
        if let Some(value) = find(&directory)? {
            return Ok(Some(value));
        }
        // Git links entries at depths 0..=5, then searches those databases
        // without following their own alternates. Cycles are duplicate paths.
        if depth > 5 {
            continue;
        }
        let alternates = match File::open(directory.join("info/alternates")) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        alternates
            .take(MAX_ALTERNATE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        spend(bytes.len())?;
        for path in alternate_paths(&bytes)?.into_iter().rev() {
            let path = directory.join(path);
            spend(path.as_os_str().as_encoded_bytes().len())?;
            pending.push((path, depth + 1));
        }
    }
    Ok(None)
}

fn alternate_paths(mut input: &[u8]) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    while !input.is_empty() {
        let end = input
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap_or(input.len());
        let next_line = (end + 1).min(input.len());
        if input[0] == b'#' || end == 0 {
            input = &input[next_line..];
            continue;
        }
        // Native Git treats failed C quoting as a literal path beginning with
        // a quote; valid quoted entries consume exactly their closing quote.
        let (path, consumed) = if input[0] == b'"' {
            match gix_quote::ansi_c::undo(input.as_bstr()) {
                Ok((path, consumed)) if input.get(consumed - 1) == Some(&b'"') => {
                    (path, (consumed + 1).min(input.len()))
                }
                _ => (Cow::Borrowed(input[..end].as_bstr()), next_line),
            }
        } else {
            (Cow::Borrowed(input[..end].as_bstr()), next_line)
        };
        if !path.is_empty() {
            let path = path
                .to_os_str()
                .map_err(|source| io::Error::new(io::ErrorKind::InvalidData, source))?;
            paths.push(PathBuf::from(path));
        }
        input = &input[consumed..];
    }
    Ok(paths)
}

/// Stream and verify exact blob bodies from one local Git object database.
///
/// The object database must already be isolated from untrusted alternates.
/// Transport is disabled, so a missing object fails instead of triggering a
/// lazy fetch. Cancellation is cooperative between local reads.
pub fn verify_git_dir_blobs(
    git_dir: &Path,
    expected: &[BlobHeader],
    cancelled: &dyn Fn() -> bool,
) -> io::Result<()> {
    if expected.is_empty() {
        return Ok(());
    }
    check_cancelled(cancelled)?;
    // Feed requests from a file so a large first response cannot fill stdout
    // while the parent is still blocked writing a many-object stdin pipe.
    let mut input = tempfile::tempfile()?;
    for blob in expected {
        check_cancelled(cancelled)?;
        writeln!(input, "{}", gix_hash::ObjectId::Sha1(blob.oid))?;
    }
    input.rewind()?;
    let mut child = crate::raw_object_command(git_dir)
        .args(["cat-file", "--batch"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .stdin(Stdio::from(input))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let operation = (|| {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| invalid("Git blob verifier has no stdout"))?;
        verify_blob_batch(stdout, expected, cancelled)?;
        let status = child.wait()?;
        if !status.success() {
            return Err(invalid("Git blob verifier exited unsuccessfully"));
        }
        Ok(())
    })();
    if operation.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    operation
}

/// Visit small blob bodies after verifying every requested object's identity.
///
/// The visitor receives the zero-based request ordinal and raw body. Larger
/// objects and non-blobs are streamed and hashed without retaining their body.
/// SHA-1 uses Git-compatible collision detection; SHA-256 OIDs are also accepted.
/// Callers own aggregate inventory limits and must discard results on error.
pub fn visit_small_blobs<'a>(
    input: impl Read,
    expected: impl IntoIterator<Item = &'a str>,
    capture_limit: usize,
    cancelled: &dyn Fn() -> bool,
    mut visitor: impl FnMut(usize, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let mut reader = BufReader::new(input);
    let mut buffer = [0u8; 64 * 1024];
    for (index, oid) in expected.into_iter().enumerate() {
        if let Some(body) = read_object(
            &mut reader,
            oid,
            None,
            capture_limit,
            None,
            &mut buffer,
            cancelled,
        )? {
            visitor(index, &body)?;
        }
    }
    finish(&mut reader, cancelled)
}

fn read_object<'a>(
    reader: &mut impl BufRead,
    oid: &'a str,
    blob_size: Option<u64>,
    capture_limit: usize,
    output: Option<&'a mut dyn Write>,
    buffer: &mut [u8],
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Option<Vec<u8>>> {
    check_cancelled(cancelled)?;
    let mut header = Vec::with_capacity(100);
    reader.take(100).read_until(b'\n', &mut header)?;
    let mismatch = || {
        invalid(format!(
            "Git {} batch header differs for {oid}: expected size {blob_size:?}, header bytes {header:?}",
            if blob_size.is_some() {
                "blob"
            } else {
                "object"
            }
        ))
    };
    let line = std::str::from_utf8(&header).map_err(|_| mismatch())?;
    let parts = line
        .strip_suffix('\n')
        .ok_or_else(mismatch)?
        .split(' ')
        .collect::<Vec<_>>();
    if parts.len() != 3 || parts[0] != oid {
        return Err(mismatch());
    }
    let kind = Kind::from_bytes(parts[1].as_bytes()).map_err(|_| mismatch())?;
    let size = parts[2].parse::<u64>().map_err(|_| mismatch())?;
    if parts[2] != size.to_string()
        || blob_size.is_some_and(|expected| kind != Kind::Blob || size != expected)
    {
        return Err(mismatch());
    }
    let mut body = ObjectBody::new(ObjectHeader { oid, kind, size }, capture_limit, output)?;
    while body.remaining != 0 {
        check_cancelled(cancelled)?;
        let wanted = body.remaining.min(buffer.len() as u64) as usize;
        reader.read_exact(&mut buffer[..wanted])?;
        body.push(&buffer[..wanted])?;
    }
    let body = body.finish()?;
    let mut terminator = [0];
    reader.read_exact(&mut terminator)?;
    if terminator != *b"\n" {
        return Err(invalid("Git object batch has no body terminator"));
    }
    Ok(body)
}

struct ObjectHeader<'a> {
    oid: &'a str,
    kind: Kind,
    size: u64,
}

struct ObjectBody<'a> {
    oid: &'a str,
    kind: Kind,
    remaining: u64,
    hasher: ObjectHasher,
    output: Option<&'a mut dyn Write>,
    captured: Option<Vec<u8>>,
}

impl<'a> ObjectBody<'a> {
    fn new(
        header: ObjectHeader<'a>,
        capture_limit: usize,
        output: Option<&'a mut dyn Write>,
    ) -> io::Result<Self> {
        let ObjectHeader { oid, kind, size } = header;
        let mut hasher = ObjectHasher::new(oid)?;
        hasher.update(format!("{kind} {size}\0").as_bytes());
        let captured = (kind == Kind::Blob && size <= capture_limit as u64)
            .then(|| Vec::with_capacity(size as usize));
        Ok(Self {
            oid,
            kind,
            remaining: size,
            hasher,
            output,
            captured,
        })
    }

    fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() as u64 > self.remaining {
            return Err(invalid("Git object body exceeds captured size"));
        }
        self.hasher.update(bytes);
        if let Some(output) = &mut self.output {
            output.write_all(bytes)?;
        }
        if let Some(captured) = &mut self.captured {
            captured.extend_from_slice(bytes);
        }
        self.remaining -= bytes.len() as u64;
        Ok(())
    }

    fn finish(self) -> io::Result<Option<Vec<u8>>> {
        if self.remaining != 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete Git object body",
            ));
        }
        if self.hasher.finalize()? != self.oid {
            return Err(invalid(format!(
                "Git {} checksum differs from {}",
                self.kind, self.oid
            )));
        }
        Ok(self.captured)
    }
}

impl Write for ObjectBody<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.push(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

enum ObjectHasher {
    Sha1(sha1dc::Hasher),
    Sha256(sha2::Sha256),
}

impl ObjectHasher {
    fn new(oid: &str) -> io::Result<Self> {
        if !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid("Git batch request has an invalid object ID"));
        }
        match oid.len() {
            40 => Ok(Self::Sha1(sha1dc::Hasher::new())),
            64 => Ok(Self::Sha256(sha2::Sha256::new())),
            _ => Err(invalid("Git batch request has an unsupported object ID")),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::Sha1(hasher) => hasher.update(bytes),
            Self::Sha256(hasher) => hasher.update(bytes),
        }
    }

    fn finalize(self) -> io::Result<String> {
        match self {
            Self::Sha1(hasher) => {
                hasher
                    .finalize()
                    .map(|digest| digest.to_string())
                    .map_err(|collision| {
                        // Match generated-object hashing's Gitoxide error contract;
                        // hardware acceleration must retain collision rejection and
                        // the original digest rather than a mitigated object ID.
                        io::Error::other(gix_hash::hasher::Error::CollisionAttack {
                            digest: gix_hash::ObjectId::Sha1(*collision.digest().as_bytes()),
                        })
                    })
            }
            Self::Sha256(hasher) => Ok(format!("{:x}", hasher.finalize())),
        }
    }
}

fn finish(reader: &mut impl Read, cancelled: &dyn Fn() -> bool) -> io::Result<()> {
    check_cancelled(cancelled)?;
    if reader.read(&mut [0])? != 0 {
        return Err(invalid("Git object stream has trailing bytes"));
    }
    check_cancelled(cancelled)
}

fn check_cancelled(cancelled: &dyn Fn() -> bool) -> io::Result<()> {
    if cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "Git blob verification cancelled",
        ));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
#[path = "batch_tests.rs"]
mod tests;
