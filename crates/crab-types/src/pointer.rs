//! Crab pointer wire format.

use std::fmt;

/// First line of every crab pointer without the trailing LF.
pub const VERSION_LINE: &str = "version https://crab.dev/spec/v1";

/// Maximum total byte length of a serialized pointer.
pub const MAX_POINTER_SIZE: usize = 256;

/// Parsed crab pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pointer {
    /// Blake3 hash of the original file content.
    pub file_hash: [u8; 32],
    /// Size of the original file in bytes.
    pub size: u64,
    /// Optional shard hash hint for cold-cache smudge fast path.
    pub shard_hint: Option<[u8; 32]>,
}

/// Parse failure for the Crab pointer wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerParseError {
    message: String,
}

impl PointerParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for PointerParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PointerParseError {}

type Result<T> = std::result::Result<T, PointerParseError>;

impl Pointer {
    /// Deserialize a pointer from its on-wire byte representation.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_POINTER_SIZE {
            return Err(PointerParseError::new(format!(
                "pointer too large: {} bytes (max {})",
                bytes.len(),
                MAX_POINTER_SIZE,
            )));
        }

        let text = std::str::from_utf8(bytes)
            .map_err(|e| PointerParseError::new(format!("pointer is not valid UTF-8: {e}")))?;

        let mut lines = text.split('\n');

        let version_line = lines
            .next()
            .ok_or_else(|| PointerParseError::new("pointer is empty"))?;
        if version_line != VERSION_LINE {
            return Err(PointerParseError::new(format!(
                "unexpected version line: {version_line:?}",
            )));
        }

        let hash_line = lines
            .next()
            .ok_or_else(|| PointerParseError::new("missing file-hash line"))?;
        let hex_str = hash_line
            .strip_prefix("file-hash ")
            .ok_or_else(|| PointerParseError::new(format!("bad file-hash line: {hash_line:?}")))?;
        let file_hash = parse_hex32(hex_str)?;

        let size_line = lines
            .next()
            .ok_or_else(|| PointerParseError::new("missing size line"))?;
        let size_str = size_line
            .strip_prefix("size ")
            .ok_or_else(|| PointerParseError::new(format!("bad size line: {size_line:?}")))?;
        let size: u64 = size_str
            .parse()
            .map_err(|e| PointerParseError::new(format!("invalid size {size_str:?}: {e}")))?;

        let shard_hint = match lines.next() {
            Some(line) if !line.is_empty() => match line.strip_prefix("shard-hint ") {
                Some(hex) => Some(parse_hex32(hex)?),
                None => None,
            },
            Some(_) | None => None,
        };

        Ok(Self {
            file_hash,
            size,
            shard_hint,
        })
    }

    /// Serialize the pointer to its on-wire byte representation.
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let hex = hex_encode(&self.file_hash);
        let mut s = format!("{VERSION_LINE}\nfile-hash {hex}\nsize {}\n", self.size);
        if let Some(ref hint) = self.shard_hint {
            s.push_str("shard-hint ");
            s.push_str(&hex_encode(hint));
            s.push('\n');
        }
        debug_assert!(s.len() <= MAX_POINTER_SIZE);
        s.into_bytes()
    }

    /// Return a copy with the given shard hint attached.
    #[must_use]
    pub fn with_shard_hint(mut self, hint: [u8; 32]) -> Self {
        self.shard_hint = Some(hint);
        self
    }
}

/// Fast detection heuristic: classify a blob as a Crab pointer without
/// fully parsing it.
#[must_use]
pub fn is_pointer(bytes: &[u8]) -> bool {
    if bytes.len() > MAX_POINTER_SIZE {
        return false;
    }

    if bytes.last() != Some(&b'\n') {
        return false;
    }

    let version_bytes = VERSION_LINE.as_bytes();
    if !bytes.starts_with(version_bytes) {
        return false;
    }
    if bytes.get(version_bytes.len()) != Some(&b'\n') {
        return false;
    }

    #[allow(clippy::naive_bytecount)]
    let line_count = bytes.iter().filter(|&&b| b == b'\n').count();
    if line_count != 3 && line_count != 4 {
        return false;
    }

    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };

    let lines: Vec<&str> = text.split('\n').collect();
    if lines.last() != Some(&"") {
        return false;
    }
    let content_lines = &lines[..lines.len() - 1];

    let Some(hex_str) = content_lines
        .get(1)
        .and_then(|l| l.strip_prefix("file-hash "))
    else {
        return false;
    };
    if hex_str.len() != 64 || !hex_str.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }

    let Some(size_str) = content_lines.get(2).and_then(|l| l.strip_prefix("size ")) else {
        return false;
    };
    if size_str.is_empty() || !size_str.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }

    if let Some(line4) = content_lines.get(3)
        && let Some(hint_hex) = line4.strip_prefix("shard-hint ")
        && (hint_hex.len() != 64 || !hint_hex.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return false;
    }

    true
}

fn parse_hex32(hex: &str) -> Result<[u8; 32]> {
    if hex.len() != 64 {
        return Err(PointerParseError::new(format!(
            "file-hash must be 64 hex chars, got {}",
            hex.len(),
        )));
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = hex_nibble(hex.as_bytes()[i * 2])?;
        let lo = hex_nibble(hex.as_bytes()[i * 2 + 1])?;
        *byte = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(PointerParseError::new(format!(
            "invalid hex character: {b:#04x}",
        ))),
    }
}

/// Encode 32 bytes as a 64-character lowercase hex string.
#[must_use]
pub fn hex_encode(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hash() -> [u8; 32] {
        let mut h = [0u8; 32];
        for (i, byte) in h.iter_mut().enumerate() {
            *byte = i as u8;
        }
        h
    }

    fn sample_shard_hint() -> [u8; 32] {
        let mut h = [0u8; 32];
        for (i, byte) in h.iter_mut().enumerate() {
            *byte = 0xff - i as u8;
        }
        h
    }

    fn sample_pointer() -> Pointer {
        Pointer {
            file_hash: sample_hash(),
            size: 1_048_576,
            shard_hint: None,
        }
    }

    #[test]
    fn pointer_round_trips() {
        let pointer = sample_pointer();

        let bytes = pointer.serialize();
        assert_eq!(Pointer::parse(&bytes), Ok(pointer));
    }

    #[test]
    fn pointer_round_trips_with_shard_hint() {
        let pointer = sample_pointer().with_shard_hint(sample_shard_hint());

        let bytes = pointer.serialize();
        assert_eq!(Pointer::parse(&bytes), Ok(pointer));
    }

    #[test]
    fn parses_valid_pointer() {
        let raw = b"version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1048576\n";

        let pointer = Pointer::parse(raw).unwrap();

        assert_eq!(pointer.file_hash, sample_hash());
        assert_eq!(pointer.size, 1_048_576);
        assert_eq!(pointer.shard_hint, None);
    }

    #[test]
    fn parses_pointer_with_shard_hint() {
        let raw = b"version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1048576\n\
shard-hint fffefdfcfbfaf9f8f7f6f5f4f3f2f1f0efeeedecebeae9e8e7e6e5e4e3e2e1e0\n";

        let pointer = Pointer::parse(raw).unwrap();

        assert_eq!(pointer.file_hash, sample_hash());
        assert_eq!(pointer.size, 1_048_576);
        assert_eq!(pointer.shard_hint, Some(sample_shard_hint()));
    }

    #[test]
    fn parses_unknown_trailing_line_without_shard_hint() {
        let raw = b"version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1048576\n\
future-ext some-value\n";

        let pointer = Pointer::parse(raw).unwrap();

        assert_eq!(pointer.shard_hint, None);
    }

    #[test]
    fn rejects_wrong_version() {
        let raw = b"version https://crab.dev/spec/v2\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1048576\n";

        let err = Pointer::parse(raw).unwrap_err();

        assert!(err.to_string().contains("unexpected version line"));
    }

    #[test]
    fn rejects_invalid_hex() {
        let raw = b"version https://crab.dev/spec/v1\n\
file-hash zz0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1048576\n";

        let err = Pointer::parse(raw).unwrap_err();
        assert!(err.to_string().contains("invalid hex character"));
    }

    #[test]
    fn rejects_short_hex() {
        let raw = b"version https://crab.dev/spec/v1\n\
file-hash abcd\n\
size 1048576\n";

        let err = Pointer::parse(raw).unwrap_err();

        assert!(err.to_string().contains("64 hex chars"));
    }

    #[test]
    fn rejects_invalid_size() {
        let raw = b"version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size notanumber\n";

        let err = Pointer::parse(raw).unwrap_err();

        assert!(err.to_string().contains("invalid size"));
    }

    #[test]
    fn rejects_negative_size() {
        let raw = b"version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size -42\n";

        let err = Pointer::parse(raw).unwrap_err();

        assert!(err.to_string().contains("invalid size"));
    }

    #[test]
    fn serializes_three_line_format() {
        let bytes = sample_pointer().serialize();
        let text = std::str::from_utf8(&bytes).unwrap();

        assert_eq!(
            text,
            "version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1048576\n"
        );
    }

    #[test]
    fn serializes_four_line_format_with_shard_hint() {
        let bytes = sample_pointer()
            .with_shard_hint(sample_shard_hint())
            .serialize();
        let text = std::str::from_utf8(&bytes).unwrap();

        assert_eq!(
            text,
            "version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1048576\n\
shard-hint fffefdfcfbfaf9f8f7f6f5f4f3f2f1f0efeeedecebeae9e8e7e6e5e4e3e2e1e0\n"
        );
    }

    #[test]
    fn serialized_pointer_stays_within_size_budget() {
        let pointer = Pointer {
            file_hash: [0xff; 32],
            size: u64::MAX,
            shard_hint: Some([0xaa; 32]),
        };

        assert!(pointer.serialize().len() <= MAX_POINTER_SIZE);
    }

    #[test]
    fn with_shard_hint_attaches_hint_without_changing_identity() {
        let pointer = sample_pointer();

        let hinted = pointer.with_shard_hint(sample_shard_hint());

        assert_eq!(hinted.file_hash, sample_hash());
        assert_eq!(hinted.size, 1_048_576);
        assert_eq!(hinted.shard_hint, Some(sample_shard_hint()));
    }

    #[test]
    fn detects_valid_pointer() {
        let pointer = sample_pointer();

        assert!(is_pointer(&pointer.serialize()));
    }

    #[test]
    fn detects_valid_pointer_with_shard_hint() {
        let pointer = sample_pointer().with_shard_hint(sample_shard_hint());

        assert!(is_pointer(&pointer.serialize()));
    }

    #[test]
    fn rejects_non_pointer_blobs() {
        assert!(!is_pointer(b""));
        assert!(!is_pointer(&(0..128).collect::<Vec<u8>>()));
        assert!(!is_pointer(b"hello world\n"));
    }

    #[test]
    fn rejects_pointer_without_trailing_lf() {
        let raw = b"version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1048576";

        assert!(!is_pointer(raw));
    }

    #[test]
    fn rejects_pointer_over_size_budget() {
        let mut large = sample_pointer().serialize();
        large.resize(MAX_POINTER_SIZE + 1, b' ');

        assert!(!is_pointer(&large));
    }

    #[test]
    fn rejects_pointer_with_wrong_line_count() {
        let two_lines = b"version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n";
        let five_lines = b"version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1048576\n\
extra1 foo\n\
extra2 bar\n";

        assert!(!is_pointer(two_lines));
        assert!(!is_pointer(five_lines));
    }

    #[test]
    fn rejects_pointer_with_bad_hash_or_size_grammar() {
        let bad_hash = b"version https://crab.dev/spec/v1\n\
file-hash zz0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1048576\n";
        let bad_size = b"version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size abc\n";

        assert!(!is_pointer(bad_hash));
        assert!(!is_pointer(bad_size));
    }

    #[test]
    fn rejects_pointer_with_bad_shard_hint_grammar() {
        let bad_hex = b"version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1048576\n\
shard-hint zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz\n";
        let short_hex = b"version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1048576\n\
shard-hint abcd\n";

        assert!(!is_pointer(bad_hex));
        assert!(!is_pointer(short_hex));
    }
}
