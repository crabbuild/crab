//! Bounded Git delta decoding shared by remote reads and incoming packs.

use std::io::{self, Read, Seek, SeekFrom, Write};

/// Structural failures in Git delta instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DeltaCorruption {
    /// Size header is absent, truncated, overflowing, or not addressable.
    #[error("invalid delta size header")]
    SizeHeader,
    /// Delta declared a base size different from the supplied base.
    #[error("declared base size does not match the base object")]
    BaseSizeMismatch,
    /// Copy offset or length overflows addressable memory.
    #[error("copy range overflows")]
    CopyOverflow,
    /// Copy command reads beyond the base object.
    #[error("copy range exceeds the base object")]
    CopyOutOfBounds,
    /// Zero is a reserved delta command.
    #[error("delta command zero is reserved")]
    ReservedCommand,
    /// Insert command length overflows addressable memory.
    #[error("insert range overflows")]
    InsertOverflow,
    /// Delta instruction bytes are truncated.
    #[error("delta instruction is truncated")]
    InstructionTruncated,
    /// Reconstructed output exceeds or disagrees with its declaration.
    #[error("reconstructed size does not match the delta declaration")]
    ResultSizeMismatch,
}

/// Structural, allocation, limit or cancellation failure in a delta program.
#[derive(Debug, thiserror::Error)]
pub enum DeltaError {
    #[error(transparent)]
    Invalid(#[from] DeltaCorruption),
    #[error("delta result {actual} exceeds limit {maximum}")]
    ResultTooLarge { actual: usize, maximum: usize },
    #[error("cannot allocate {requested} delta bytes")]
    Allocation {
        requested: usize,
        #[source]
        source: std::collections::TryReserveError,
    },
    #[error("delta I/O failed")]
    Io(#[from] io::Error),
    #[error("delta operation cancelled")]
    Cancelled,
}

/// Parsed and size-bounded Git delta program.
pub struct Delta<'a> {
    pub base_size: usize,
    pub result_size: usize,
    pub instructions: &'a [u8],
}

/// Parses delta size headers and rejects results larger than `maximum` before allocation.
pub fn parse(bytes: &[u8], maximum: usize) -> Result<Delta<'_>, DeltaError> {
    let mut input = io::Cursor::new(bytes);
    let (base_size, result_size) = sizes(&mut input, maximum)?;
    Ok(Delta {
        base_size,
        result_size,
        instructions: &bytes[input.position() as usize..],
    })
}

/// Applies a parsed delta, rejecting invalid ranges, sizes and cancellation.
pub fn apply(
    base: &[u8],
    delta: Delta<'_>,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<u8>, DeltaError> {
    if base.len() != delta.base_size {
        return Err(invalid(DeltaCorruption::BaseSizeMismatch));
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(delta.result_size)
        .map_err(|source| DeltaError::Allocation {
            requested: delta.result_size,
            source,
        })?;
    let mut input = io::Cursor::new(delta.instructions);
    let append = |instruction: Instruction<'_>| {
        let source = match instruction {
            Instruction::Copy(range) => &base[range],
            Instruction::Insert(bytes) => bytes,
        };
        output.extend_from_slice(source);
        Ok(())
    };
    walk(
        &mut input,
        delta.base_size,
        delta.result_size,
        cancelled,
        append,
    )?;
    Ok(output)
}

/// Validates every instruction without allocating the reconstructed object.
pub fn validate(delta: &Delta<'_>, cancelled: impl Fn() -> bool) -> Result<(), DeltaError> {
    let mut input = io::Cursor::new(delta.instructions);
    walk(
        &mut input,
        delta.base_size,
        delta.result_size,
        cancelled,
        |_| Ok(()),
    )
}

/// Copy a complete delta program using a seekable base and fixed-size buffers.
///
/// The caller supplies the exact base size, admits output growth and discards
/// output after failure. This validates instructions and lengths, not Git identity.
/// Callers supervise blocked I/O and verify the final object checksum separately.
pub fn copy(
    mut input: impl Read,
    base: &mut (impl Read + Seek),
    base_size: usize,
    output: &mut impl Write,
    maximum: usize,
    cancelled: &dyn Fn() -> bool,
) -> Result<usize, DeltaError> {
    check_cancelled(cancelled)?;
    let (declared_base, result_size) = sizes(&mut input, maximum)?;
    if declared_base != base_size {
        return Err(invalid(DeltaCorruption::BaseSizeMismatch));
    }
    let mut buffer = [0; 64 * 1024];
    let append = |instruction: Instruction<'_>| {
        match instruction {
            Instruction::Insert(bytes) => output.write_all(bytes)?,
            Instruction::Copy(range) => {
                base.seek(SeekFrom::Start(range.start as u64))?;
                let mut remaining = range.len();
                while remaining != 0 {
                    check_cancelled(cancelled)?;
                    let wanted = remaining.min(buffer.len());
                    let count = base.read(&mut buffer[..wanted])?;
                    if count == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "delta base is truncated",
                        )
                        .into());
                    }
                    output.write_all(&buffer[..count])?;
                    remaining -= count;
                }
            }
        }
        Ok(())
    };
    walk(&mut input, base_size, result_size, cancelled, append)?;
    check_cancelled(cancelled)?;
    Ok(result_size)
}

enum Instruction<'a> {
    Copy(std::ops::Range<usize>),
    Insert(&'a [u8]),
}

impl Instruction<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Copy(range) => range.len(),
            Self::Insert(bytes) => bytes.len(),
        }
    }
}

fn walk(
    input: &mut impl Read,
    base_size: usize,
    result_size: usize,
    cancelled: impl Fn() -> bool,
    mut append: impl FnMut(Instruction<'_>) -> Result<(), DeltaError>,
) -> Result<(), DeltaError> {
    let mut inserted = [0; 127];
    let mut output_len = 0usize;
    while let Some(command) = byte(input)? {
        check_cancelled(&cancelled)?;
        let instruction = if command & 0x80 != 0 {
            let mut offset = 0u32;
            let mut size = 0u32;
            for (mask, shift) in [(0x01, 0), (0x02, 8), (0x04, 16), (0x08, 24)] {
                if command & mask != 0 {
                    offset |= u32::from(next(input)?) << shift;
                }
            }
            for (mask, shift) in [(0x10, 0), (0x20, 8), (0x40, 16)] {
                if command & mask != 0 {
                    size |= u32::from(next(input)?) << shift;
                }
            }
            // Git encodes a 64 KiB copy with a zero size field.
            if size == 0 {
                size = 0x1_0000;
            }
            let start = offset as usize;
            let end = start
                .checked_add(size as usize)
                .ok_or_else(|| invalid(DeltaCorruption::CopyOverflow))?;
            if end > base_size {
                return Err(invalid(DeltaCorruption::CopyOutOfBounds));
            }
            Instruction::Copy(start..end)
        } else if command == 0 {
            return Err(invalid(DeltaCorruption::ReservedCommand));
        } else {
            let bytes = &mut inserted[..command as usize];
            // Do not let read_exact retry Interrupted indefinitely; cancellation
            // and source failures must reach the joined physical reader owner.
            let mut read = 0;
            while read < bytes.len() {
                check_cancelled(&cancelled)?;
                let count = input.read(&mut bytes[read..])?;
                if count == 0 {
                    return Err(invalid(DeltaCorruption::InstructionTruncated));
                }
                read += count;
            }
            Instruction::Insert(bytes)
        };
        output_len = output_len
            .checked_add(instruction.len())
            .ok_or_else(|| invalid(DeltaCorruption::ResultSizeMismatch))?;
        if output_len > result_size {
            return Err(invalid(DeltaCorruption::ResultSizeMismatch));
        }
        append(instruction)?;
    }
    if output_len != result_size {
        return Err(invalid(DeltaCorruption::ResultSizeMismatch));
    }
    Ok(())
}

pub(crate) fn sizes(input: &mut impl Read, maximum: usize) -> Result<(usize, usize), DeltaError> {
    let base = decode_size(input)?;
    let result = decode_size(input)?;
    if result > maximum {
        return Err(DeltaError::ResultTooLarge {
            actual: result,
            maximum,
        });
    }
    Ok((base, result))
}

fn decode_size(input: &mut impl Read) -> Result<usize, DeltaError> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        if shift >= u64::BITS {
            return Err(invalid(DeltaCorruption::SizeHeader));
        }
        let byte = next(input)?;
        let component = u64::from(byte & 0x7f);
        if component > (u64::MAX >> shift) {
            return Err(invalid(DeltaCorruption::SizeHeader));
        }
        value |= component << shift;
        if byte & 0x80 == 0 {
            return usize::try_from(value).map_err(|_| invalid(DeltaCorruption::SizeHeader));
        }
        shift += 7;
    }
}

fn byte(input: &mut impl Read) -> Result<Option<u8>, DeltaError> {
    let mut byte = [0];
    Ok((input.read(&mut byte)? != 0).then_some(byte[0]))
}

fn next(input: &mut impl Read) -> Result<u8, DeltaError> {
    byte(input)?.ok_or_else(|| invalid(DeltaCorruption::InstructionTruncated))
}

fn check_cancelled(cancelled: &dyn Fn() -> bool) -> Result<(), DeltaError> {
    if cancelled() {
        return Err(DeltaError::Cancelled);
    }
    Ok(())
}

fn invalid(reason: DeltaCorruption) -> DeltaError {
    DeltaError::Invalid(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_and_reconstruction_reject_the_same_malformed_instructions() {
        for (instructions, result_size, reason) in [
            (&[0][..], 0, DeltaCorruption::ReservedCommand),
            (&[0x91][..], 1, DeltaCorruption::InstructionTruncated),
            (&[0x91, 0][..], 1, DeltaCorruption::InstructionTruncated),
            (&[2, b'a'][..], 2, DeltaCorruption::InstructionTruncated),
            (&[0x91, 2, 2][..], 2, DeltaCorruption::CopyOutOfBounds),
            (&[0x90, 3][..], 2, DeltaCorruption::ResultSizeMismatch),
            (&[1, b'a'][..], 2, DeltaCorruption::ResultSizeMismatch),
            (&[2, b'a', b'b'][..], 1, DeltaCorruption::ResultSizeMismatch),
        ] {
            let delta = Delta {
                base_size: 3,
                result_size,
                instructions,
            };
            let validation = validate(&delta, || false);
            let reconstruction = apply(b"abc", delta, || false);
            for result in [validation, reconstruction.map(|_| ())] {
                assert!(matches!(result, Err(DeltaError::Invalid(actual)) if actual == reason));
            }
        }
    }

    #[test]
    fn omitted_copy_size_means_64_kib_in_both_paths() {
        let base = vec![b'x'; 65_536];
        let delta = Delta {
            base_size: base.len(),
            result_size: base.len(),
            instructions: &[0x80],
        };
        validate(&delta, || false).expect("valid implicit copy size");

        assert_eq!(apply(&base, delta, || false).expect("copy base"), base);
    }

    #[test]
    fn rejects_overflowing_size_headers_before_allocation() {
        let mut bytes = vec![0xff; 9];
        bytes.extend_from_slice(&[0x02, 0]);
        assert!(matches!(
            parse(&bytes, usize::MAX),
            Err(DeltaError::Invalid(DeltaCorruption::SizeHeader))
        ));
    }

    #[test]
    fn applies_copy_and_insert_instructions() {
        let bytes = [5, 8, 0x90, 3, 3, b'X', b'Y', b'Z', 0x91, 3, 2];
        let delta = parse(&bytes, 8).expect("parse delta");
        let result = apply(b"abcde", delta, || false).expect("apply delta");
        assert_eq!(result, b"abcXYZde");
    }

    #[test]
    fn rejects_copy_beyond_base() {
        let bytes = [3, 4, 0x91, 2, 4];
        let delta = parse(&bytes, 4).expect("parse delta");
        assert!(apply(b"abc", delta, || false).is_err());
    }

    #[test]
    fn metadata_validation_rejects_copy_beyond_base_without_allocating_output() {
        let bytes = [3, 4, 0x91, 2, 4];
        let delta = parse(&bytes, 4).expect("parse delta");
        assert!(matches!(
            validate(&delta, || false),
            Err(DeltaError::Invalid(DeltaCorruption::CopyOutOfBounds))
        ));
    }

    #[test]
    fn metadata_validation_honors_cancellation() {
        let bytes = [3, 3, 0x90, 3];
        let delta = parse(&bytes, 3).expect("parse delta");

        assert!(matches!(
            validate(&delta, || true),
            Err(DeltaError::Cancelled)
        ));
    }

    #[test]
    fn seekable_delta_copy_uses_the_same_instruction_validation() {
        for encoded in [
            &[5, 8, 0x90, 3, 3, b'X', b'Y', b'Z', 0x91, 3, 2][..],
            &[5, 4, 0x91, 4, 2][..],
            &[5, 0, 0][..],
            &[5, 1, 2, b'x'][..],
            &[5, 3, 1, b'x'][..],
        ] {
            let parsed = parse(encoded, 16).unwrap();
            let buffered = apply(b"abcde", parsed, || false);
            let mut streamed = Vec::new();
            let result = copy(
                encoded,
                &mut io::Cursor::new(b"abcde"),
                5,
                &mut streamed,
                16,
                &|| false,
            );
            match (buffered, result) {
                (Ok(expected), Ok(_)) => assert_eq!(streamed, expected),
                (Err(DeltaError::Invalid(expected)), Err(DeltaError::Invalid(actual))) => {
                    assert_eq!(actual, expected)
                }
                result => panic!("buffered and streamed behavior differs: {result:?}"),
            }
        }
    }

    #[test]
    fn seekable_delta_copy_preserves_source_interruption() {
        struct Interrupted;
        impl Read for Interrupted {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "read interrupted",
                ))
            }
        }
        let error = copy(
            Interrupted,
            &mut io::Cursor::new(b"abc"),
            3,
            &mut Vec::new(),
            16,
            &|| false,
        )
        .unwrap_err();
        assert!(
            matches!(error, DeltaError::Io(source) if source.kind() == io::ErrorKind::Interrupted)
        );
    }
}
