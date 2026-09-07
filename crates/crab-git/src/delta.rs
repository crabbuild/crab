//! Bounded Git delta decoding shared by remote reads and incoming packs.

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
pub fn parse(bytes: &[u8], maximum: usize) -> std::result::Result<Delta<'_>, DeltaError> {
    let mut cursor = 0;
    let base_size = decode_size(bytes, &mut cursor)?;
    let result_size = decode_size(bytes, &mut cursor)?;
    if result_size > maximum {
        return Err(DeltaError::ResultTooLarge {
            actual: result_size,
            maximum,
        });
    }
    Ok(Delta {
        base_size,
        result_size,
        instructions: &bytes[cursor..],
    })
}

/// Applies a parsed delta, rejecting invalid ranges, sizes and cancellation.
pub fn apply(
    base: &[u8],
    delta: Delta<'_>,
    cancelled: impl Fn() -> bool,
) -> std::result::Result<Vec<u8>, DeltaError> {
    if base.len() != delta.base_size {
        return Err(DeltaError::Invalid(DeltaCorruption::BaseSizeMismatch));
    }

    let mut output = Vec::new();
    output
        .try_reserve_exact(delta.result_size)
        .map_err(|source| DeltaError::Allocation {
            requested: delta.result_size,
            source,
        })?;
    let mut cursor = 0;
    while let Some(&command) = delta.instructions.get(cursor) {
        if cancelled() {
            return Err(DeltaError::Cancelled);
        }
        cursor += 1;
        if command & 0x80 != 0 {
            let mut offset = 0u32;
            let mut size = 0u32;
            for (mask, shift) in [(0x01, 0), (0x02, 8), (0x04, 16), (0x08, 24)] {
                if command & mask != 0 {
                    offset |= u32::from(next(delta.instructions, &mut cursor)?) << shift;
                }
            }
            for (mask, shift) in [(0x10, 0), (0x20, 8), (0x40, 16)] {
                if command & mask != 0 {
                    size |= u32::from(next(delta.instructions, &mut cursor)?) << shift;
                }
            }
            if size == 0 {
                size = 0x1_0000;
            }
            let start = offset as usize;
            let end = start
                .checked_add(size as usize)
                .ok_or(DeltaError::Invalid(DeltaCorruption::CopyOverflow))?;
            let source = base
                .get(start..end)
                .ok_or(DeltaError::Invalid(DeltaCorruption::CopyOutOfBounds))?;
            extend_bounded(&mut output, source, delta.result_size)?;
        } else if command == 0 {
            return Err(DeltaError::Invalid(DeltaCorruption::ReservedCommand));
        } else {
            let end = cursor
                .checked_add(command as usize)
                .ok_or(DeltaError::Invalid(DeltaCorruption::InsertOverflow))?;
            let inserted = delta
                .instructions
                .get(cursor..end)
                .ok_or(DeltaError::Invalid(DeltaCorruption::InstructionTruncated))?;
            extend_bounded(&mut output, inserted, delta.result_size)?;
            cursor = end;
        }
    }
    if output.len() != delta.result_size {
        return Err(DeltaError::Invalid(DeltaCorruption::ResultSizeMismatch));
    }
    Ok(output)
}

/// Validates every instruction without allocating the reconstructed object.
pub fn validate(
    delta: &Delta<'_>,
    cancelled: impl Fn() -> bool,
) -> std::result::Result<(), DeltaError> {
    let mut cursor = 0;
    let mut output_len = 0usize;
    while let Some(&command) = delta.instructions.get(cursor) {
        if cancelled() {
            return Err(DeltaError::Cancelled);
        }
        cursor += 1;
        let appended = if command & 0x80 != 0 {
            let mut offset = 0u32;
            let mut size = 0u32;
            for (mask, shift) in [(0x01, 0), (0x02, 8), (0x04, 16), (0x08, 24)] {
                if command & mask != 0 {
                    offset |= u32::from(next(delta.instructions, &mut cursor)?) << shift;
                }
            }
            for (mask, shift) in [(0x10, 0), (0x20, 8), (0x40, 16)] {
                if command & mask != 0 {
                    size |= u32::from(next(delta.instructions, &mut cursor)?) << shift;
                }
            }
            if size == 0 {
                size = 0x1_0000;
            }
            let end = (offset as usize)
                .checked_add(size as usize)
                .ok_or(DeltaError::Invalid(DeltaCorruption::CopyOverflow))?;
            if end > delta.base_size {
                return Err(DeltaError::Invalid(DeltaCorruption::CopyOutOfBounds));
            }
            size as usize
        } else if command == 0 {
            return Err(DeltaError::Invalid(DeltaCorruption::ReservedCommand));
        } else {
            let inserted = command as usize;
            cursor = cursor
                .checked_add(inserted)
                .ok_or(DeltaError::Invalid(DeltaCorruption::InsertOverflow))?;
            if cursor > delta.instructions.len() {
                return Err(DeltaError::Invalid(DeltaCorruption::InstructionTruncated));
            }
            inserted
        };
        output_len = output_len
            .checked_add(appended)
            .ok_or(DeltaError::Invalid(DeltaCorruption::ResultSizeMismatch))?;
        if output_len > delta.result_size {
            return Err(DeltaError::Invalid(DeltaCorruption::ResultSizeMismatch));
        }
    }
    if output_len != delta.result_size {
        return Err(DeltaError::Invalid(DeltaCorruption::ResultSizeMismatch));
    }
    Ok(())
}

fn decode_size(bytes: &[u8], cursor: &mut usize) -> std::result::Result<usize, DeltaError> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        if shift >= u64::BITS {
            return Err(invalid(DeltaCorruption::SizeHeader));
        }
        let byte = next(bytes, cursor)?;
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

fn next(bytes: &[u8], cursor: &mut usize) -> std::result::Result<u8, DeltaError> {
    let byte = *bytes
        .get(*cursor)
        .ok_or_else(|| invalid(DeltaCorruption::InstructionTruncated))?;
    *cursor += 1;
    Ok(byte)
}

fn extend_bounded(
    output: &mut Vec<u8>,
    bytes: &[u8],
    maximum: usize,
) -> std::result::Result<(), DeltaError> {
    let end = output
        .len()
        .checked_add(bytes.len())
        .ok_or_else(|| invalid(DeltaCorruption::ResultSizeMismatch))?;
    if end > maximum {
        return Err(invalid(DeltaCorruption::ResultSizeMismatch));
    }
    output.extend_from_slice(bytes);
    Ok(())
}

fn invalid(reason: DeltaCorruption) -> DeltaError {
    DeltaError::Invalid(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
