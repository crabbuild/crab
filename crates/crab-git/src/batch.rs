//! Exact-identity verification of native Git's raw object batch stream.

use gix_object::Kind;
use sha2::Digest as _;
use std::io::{self, BufRead, BufReader, Read};

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
            &mut buffer,
            cancelled,
        )?;
    }
    finish(&mut reader, cancelled)
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
            &mut buffer,
            cancelled,
        )? {
            visitor(index, &body)?;
        }
    }
    finish(&mut reader, cancelled)
}

fn read_object(
    reader: &mut impl BufRead,
    oid: &str,
    blob_size: Option<u64>,
    capture_limit: usize,
    buffer: &mut [u8],
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Option<Vec<u8>>> {
    check_cancelled(cancelled)?;
    let mut header = Vec::with_capacity(100);
    reader.take(100).read_until(b'\n', &mut header)?;
    let mismatch = || {
        invalid(format!(
            "Git {} batch header differs for {oid}",
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
    let mut hasher = ObjectHasher::new(oid)?;
    hasher.update(format!("{} {size}\0", parts[1]).as_bytes());
    let mut body = (kind == Kind::Blob && size <= capture_limit as u64)
        .then(|| Vec::with_capacity(size as usize));
    let mut remaining = size;
    while remaining != 0 {
        check_cancelled(cancelled)?;
        let wanted = remaining.min(buffer.len() as u64) as usize;
        reader.read_exact(&mut buffer[..wanted])?;
        hasher.update(&buffer[..wanted]);
        if let Some(body) = &mut body {
            body.extend_from_slice(&buffer[..wanted]);
        }
        remaining -= wanted as u64;
    }
    if hasher.finalize()? != oid {
        return Err(invalid(format!(
            "Git {} checksum differs from {oid}",
            parts[1]
        )));
    }
    let mut terminator = [0];
    reader.read_exact(&mut terminator)?;
    if terminator != *b"\n" {
        return Err(invalid("Git object batch has no body terminator"));
    }
    Ok(body)
}

#[expect(
    clippy::large_enum_variant,
    reason = "Only one stack hasher is live; boxing adds an allocation for every streamed object"
)]
enum ObjectHasher {
    Sha1(gix_hash::Hasher),
    Sha256(sha2::Sha256),
}

impl ObjectHasher {
    fn new(oid: &str) -> io::Result<Self> {
        if !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid("Git batch request has an invalid object ID"));
        }
        match oid.len() {
            40 => Ok(Self::Sha1(gix_hash::hasher(gix_hash::Kind::Sha1))),
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
            Self::Sha1(hasher) => Ok(hasher.try_finalize().map_err(io::Error::other)?.to_string()),
            Self::Sha256(hasher) => Ok(format!("{:x}", hasher.finalize())),
        }
    }
}

fn finish(reader: &mut impl Read, cancelled: &dyn Fn() -> bool) -> io::Result<()> {
    check_cancelled(cancelled)?;
    if reader.read(&mut [0])? != 0 {
        return Err(invalid("Git object batch contains an unrequested response"));
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
mod tests;
