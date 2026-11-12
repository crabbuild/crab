use super::*;
use crate::incoming_pack::ScratchSpace;
use std::io::SeekFrom;

#[path = "pack_index.rs"]
mod index;
use index::Location;

enum Base {
    Full,
    Offset(u64),
    Reference(Vec<u8>),
}

struct Entry {
    base: Base,
    size: u64,
    data: u64,
}

/// Copy a locally packed blob with bounded buffers and admitted delta scratch.
///
/// Returns false only when no local pack index contains the requested identity.
/// Found data must verify its full captured size, Git checksum and zlib footer.
/// Scratch is created only for deltas; callers own its private root, capacity,
/// cancellation/deadline and delivery after joined I/O. Failed output is untrusted.
pub fn copy_packed_blob(
    objects: &Path,
    oid: &str,
    size: u64,
    output: &mut impl Write,
    scratch: impl FnOnce() -> io::Result<ScratchSpace>,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<bool> {
    copy_observed(objects, oid, size, output, scratch, cancelled).map(|origin| origin.is_some())
}

pub(super) fn copy_observed(
    objects: &Path,
    oid: &str,
    size: u64,
    output: &mut impl Write,
    scratch: impl FnOnce() -> io::Result<ScratchSpace>,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Option<super::capture::Origin>> {
    let bytes = oid_bytes(oid)?;
    let Some(mut location) = locate(objects, &bytes, cancelled)? else {
        return Ok(None);
    };
    let origin = super::capture::Origin::new(&location.path, &location.file)?;
    let entry = header(&mut location, bytes.len())?;
    let copy = |destination: &mut dyn Write| {
        if matches!(entry.base, Base::Full) {
            if entry.size != size {
                return Err(invalid("Git packed blob differs from captured size"));
            }
            return inflate_entry(&mut location, &entry, destination, cancelled);
        }
        let mut decoder = Decoder {
            objects,
            scratch: scratch()?,
            cancelled,
            hash_bytes: bytes.len(),
        };
        decoder.decode(location, entry, size, destination)
    };
    verified(oid, size, output, copy)?;
    check_cancelled(cancelled)?;
    Ok(Some(origin))
}

struct Decoder<'a> {
    objects: &'a Path,
    scratch: ScratchSpace,
    cancelled: &'a dyn Fn() -> bool,
    hash_bytes: usize,
}

struct DeltaStep {
    offset: u64,
    length: u64,
    base_size: usize,
    result_size: usize,
    oid: Option<String>,
}

enum Source {
    Packed(Location, Option<String>),
    Loose(File, String),
}

impl Decoder<'_> {
    fn decode(
        &mut self,
        mut location: Location,
        mut entry: Entry,
        size: u64,
        output: &mut dyn Write,
    ) -> io::Result<()> {
        let mut program = self.scratch.anonymous_file()?;
        let mut steps = Vec::new();
        let mut visited = HashSet::new();
        let mut expected_size = size;
        let mut expected_oid: Option<String> = None;
        // Keep the ancestry on the heap and append all programs to one file.
        // Deep valid chains must not multiply stack frames or file descriptors.
        let (mut base, mut base_size) = loop {
            check_cancelled(self.cancelled)?;
            if visited.len() >= 1024 || !visited.insert((location.path.clone(), location.offset)) {
                return Err(invalid(
                    "Git delta ancestry is cyclic or exceeds its depth limit",
                ));
            }
            if matches!(entry.base, Base::Full) {
                if entry.size != expected_size {
                    return Err(invalid("Git delta base differs from declared size"));
                }
                let mut base = self.scratch.anonymous_file()?;
                let mut copy = |destination: &mut dyn Write| {
                    inflate_entry(&mut location, &entry, destination, self.cancelled)
                };
                if let Some(oid) = expected_oid {
                    verified(&oid, expected_size, &mut base, copy)?;
                } else {
                    copy(&mut base)?;
                }
                break (
                    base,
                    usize::try_from(expected_size).map_err(io::Error::other)?,
                );
            }
            let offset = program.seek(SeekFrom::End(0))?;
            inflate_entry(&mut location, &entry, &mut program, self.cancelled)?;
            program.seek(SeekFrom::Start(offset))?;
            let maximum = usize::try_from(expected_size).map_err(io::Error::other)?;
            let (base_size, result_size) =
                crate::delta::sizes(&mut program, maximum).map_err(delta_error)?;
            if result_size != maximum {
                return Err(invalid("Git delta result differs from captured size"));
            }
            steps.push(DeltaStep {
                offset,
                length: entry.size,
                base_size,
                result_size,
                oid: expected_oid.take(),
            });
            let source = match entry.base {
                Base::Offset(offset) => {
                    location.offset = offset;
                    Source::Packed(location, None)
                }
                Base::Reference(bytes) => self.reference(&location.path, &bytes)?,
                Base::Full => return Err(invalid("Git delta has no base")),
            };
            match source {
                Source::Packed(mut base, oid) => {
                    entry = header(&mut base, self.hash_bytes)?;
                    location = base;
                    expected_size = base_size as u64;
                    expected_oid = oid;
                }
                Source::Loose(file, oid) => {
                    let mut base = self.scratch.anonymous_file()?;
                    copy_loose_blob(file, &oid, base_size as u64, &mut base, self.cancelled)?;
                    break (base, base_size);
                }
            }
        };
        while let Some(step) = steps.pop() {
            check_cancelled(self.cancelled)?;
            if step.base_size != base_size {
                return Err(invalid("Git delta base size changed"));
            }
            let mut next = if steps.is_empty() {
                None
            } else {
                Some(self.scratch.anonymous_file()?)
            };
            let destination: &mut dyn Write = match &mut next {
                Some(file) => file,
                None => &mut *output,
            };
            let mut copy = |destination: &mut dyn Write| {
                program.seek(SeekFrom::Start(step.offset))?;
                let mut input = BufReader::with_capacity(
                    64 * 1024,
                    Read::by_ref(&mut program).take(step.length),
                );
                let mut destination = destination;
                crate::delta::copy(
                    &mut input,
                    &mut base,
                    base_size,
                    &mut destination,
                    step.result_size,
                    self.cancelled,
                )
                .map_err(delta_error)?;
                Ok(())
            };
            if let Some(oid) = step.oid {
                verified(&oid, step.result_size as u64, destination, copy)?;
            } else {
                copy(destination)?;
            }
            if let Some(next) = next {
                base = next;
            }
            base_size = step.result_size;
        }
        Ok(())
    }

    fn reference(&self, pack: &Path, bytes: &[u8]) -> io::Result<Source> {
        let oid = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        // In-pack REF bases precede external aliases, matching Git's identity
        // selection and avoiding cycles introduced by another physical pack.
        let index = pack.with_extension("idx");
        if let Some(location) = index::at(&index, bytes, self.cancelled)? {
            return Ok(Source::Packed(location, Some(oid)));
        }
        if let Some(file) = open_loose_blob(self.objects, &oid, self.cancelled)? {
            return Ok(Source::Loose(file, oid));
        }
        let location = locate(self.objects, bytes, self.cancelled)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "Git delta base is unavailable")
        })?;
        Ok(Source::Packed(location, Some(oid)))
    }
}

fn verified(
    oid: &str,
    size: u64,
    output: &mut dyn Write,
    copy: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    let mut body = ObjectBody::new(
        ObjectHeader {
            oid,
            kind: Kind::Blob,
            size,
        },
        0,
        Some(output),
    )?;
    copy(&mut body)?;
    body.finish()?;
    Ok(())
}

fn locate(
    objects: &Path,
    oid: &[u8],
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Option<Location>> {
    find_in_databases(objects, cancelled, |directory| {
        index::find(directory, oid, cancelled)
    })
}

fn oid_bytes(oid: &str) -> io::Result<Vec<u8>> {
    if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid(
            "Git packed lookup requires a hexadecimal object ID",
        ));
    }
    oid.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = |byte: u8| {
                (byte as char)
                    .to_digit(16)
                    .ok_or_else(|| invalid("invalid Git object ID"))
            };
            Ok((digit(pair[0])? * 16 + digit(pair[1])?) as u8)
        })
        .collect()
}

fn header(location: &mut Location, hash_bytes: usize) -> io::Result<Entry> {
    location.file.seek(SeekFrom::Start(location.offset))?;
    let mut input = Read::by_ref(&mut location.file).take(location.end - location.offset);
    let first = next(&mut input)?;
    let kind = (first >> 4) & 7;
    let mut value = first;
    let mut size = u64::from(first & 15);
    let mut shift = 4;
    let mut length = 1;
    while value & 0x80 != 0 {
        value = next(&mut input)?;
        let component = u64::from(value & 127);
        if shift >= 64 || component > u64::MAX >> shift {
            return Err(invalid("Git packed size overflowed"));
        }
        size |= component << shift;
        shift += 7;
        length += 1;
    }
    let mut canonical = 1;
    let mut remaining = size >> 4;
    while remaining != 0 {
        canonical += 1;
        remaining >>= 7;
    }
    if length != canonical {
        return Err(invalid("Git packed size is not canonical"));
    }
    let base = match kind {
        3 => Base::Full,
        6 => {
            let mut value = next(&mut input)?;
            let mut distance = u64::from(value & 127);
            while value & 0x80 != 0 {
                value = next(&mut input)?;
                distance = distance
                    .checked_add(1)
                    .and_then(|value| value.checked_mul(128))
                    .and_then(|distance| distance.checked_add(u64::from(value & 127)))
                    .ok_or_else(|| invalid("Git delta base offset overflowed"))?;
            }
            let offset = location
                .offset
                .checked_sub(distance)
                .filter(|offset| distance != 0 && *offset >= 12)
                .ok_or_else(|| invalid("Git delta base offset is out of bounds"))?;
            Base::Offset(offset)
        }
        7 => {
            let mut oid = vec![0; hash_bytes];
            input.read_exact(&mut oid)?;
            Base::Reference(oid)
        }
        _ => return Err(invalid("Git packed object is not a blob")),
    };
    Ok(Entry {
        base,
        size,
        data: location.file.stream_position()?,
    })
}

fn next(input: &mut impl Read) -> io::Result<u8> {
    let mut byte = [0];
    if input.read(&mut byte)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Git pack header is truncated",
        ));
    }
    Ok(byte[0])
}

fn inflate_entry(
    location: &mut Location,
    entry: &Entry,
    output: &mut dyn Write,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<()> {
    location.file.seek(SeekFrom::Start(entry.data))?;
    let mut reader = BufReader::with_capacity(
        64 * 1024,
        Read::by_ref(&mut location.file).take(location.end - entry.data),
    );
    let mut remaining = entry.size;
    let accept = |bytes: &[u8]| {
        if bytes.len() as u64 > remaining {
            return Err(invalid("Git packed body exceeds its declared size"));
        }
        output.write_all(bytes)?;
        remaining -= bytes.len() as u64;
        Ok(())
    };
    inflate(&mut reader, cancelled, accept)?;
    if remaining != 0 {
        return Err(invalid("Git packed body is truncated"));
    }
    Ok(())
}

fn delta_error(error: crate::delta::DeltaError) -> io::Error {
    match error {
        crate::delta::DeltaError::Io(error) => error,
        crate::delta::DeltaError::Cancelled => io::Error::new(io::ErrorKind::Interrupted, error),
        error => io::Error::new(io::ErrorKind::InvalidData, error),
    }
}

#[cfg(test)]
#[path = "packed_tests.rs"]
mod tests;
