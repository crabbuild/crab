//! Git LFS pointer parsing, serialization, and validation.
//!
//! This module owns the Git LFS pointer wire format only. Object storage,
//! transfer-agent behavior, user rendering, and CLI error codes stay above this
//! Interface.

/// Result alias for LFS pointer parsing.
pub type Result<T> = std::result::Result<T, LfsPointerError>;

/// Error raised when a byte slice is not a valid Git LFS pointer.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
#[error("invalid LFS pointer: {reason}")]
pub struct LfsPointerError {
    /// Human-readable validation failure.
    pub reason: String,
}

impl LfsPointerError {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// Canonical version URL for LFS pointer spec v1.
pub const LFS_VERSION_URL: &str = "https://git-lfs.github.com/spec/v1";

/// Legacy version aliases accepted on parse and normalized to [`LFS_VERSION_URL`].
const LEGACY_VERSION_URLS: &[&str] = &[
    "http://git-media.io/v/2",
    "https://hawser.github.com/spec/v1",
];

/// Maximum byte length of a valid LFS pointer.
pub const MAX_LFS_POINTER_SIZE: usize = 1024;

/// SHA-256 of the empty string.
const EMPTY_SHA256: [u8; 32] = [
    0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24,
    0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55,
];

/// Standard Git LFS pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsPointer {
    /// SHA-256 hash of the original file content.
    pub oid: [u8; 32],
    /// Size of the original file in bytes.
    pub size: u64,
    /// Extension entries, sorted by ascending priority.
    pub extensions: Vec<LfsExtension>,
}

/// Extension entry within an LFS pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsExtension {
    /// Human-readable extension name.
    pub name: String,
    /// Numeric priority. Lower values have higher precedence.
    pub priority: u8,
    /// Hash of the extension content.
    pub oid: [u8; 32],
    /// Hash algorithm identifier.
    pub oid_type: String,
}

impl LfsPointer {
    /// Parse an LFS pointer from its byte representation.
    ///
    /// Empty input produces an empty pointer with the SHA-256 of empty content.
    /// Legacy version aliases are accepted on parse but serialize to the
    /// canonical version URL.
    ///
    /// # Errors
    ///
    /// Returns [`LfsPointerError`] when the input is too large, not UTF-8, uses
    /// an unsupported version/OID shape, omits required lines, or declares
    /// duplicate extension priorities.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Ok(Self {
                oid: EMPTY_SHA256,
                size: 0,
                extensions: Vec::new(),
            });
        }

        if bytes.len() > MAX_LFS_POINTER_SIZE {
            return Err(lfs_err(format!(
                "pointer too large: {} bytes (max {MAX_LFS_POINTER_SIZE})",
                bytes.len(),
            )));
        }

        let text = std::str::from_utf8(bytes)
            .map_err(|source| lfs_err(format!("pointer is not valid UTF-8: {source}")))?;

        let mut lines = text.lines();
        let version_line = lines.next().ok_or_else(|| lfs_err("pointer is empty"))?;
        Self::validate_version(version_line)?;

        let mut extensions = Vec::new();
        let mut oid: Option<[u8; 32]> = None;
        let mut size: Option<u64> = None;

        for line in lines {
            if line.is_empty() {
                continue;
            }

            if let Some(rest) = line.strip_prefix("oid ") {
                if oid.is_some() {
                    return Err(lfs_err("duplicate oid line"));
                }
                oid = Some(Self::parse_oid(rest)?);
            } else if let Some(rest) = line.strip_prefix("size ") {
                if size.is_some() {
                    return Err(lfs_err("duplicate size line"));
                }
                size = Some(Self::parse_size(rest)?);
            } else if line.starts_with("ext-") {
                extensions.push(Self::parse_extension_line(line)?);
            }
        }

        let oid = oid.ok_or_else(|| lfs_err("missing oid line"))?;
        let size = size.ok_or_else(|| lfs_err("missing size line"))?;
        Self::validate_unique_priorities(&extensions)?;
        extensions.sort_by_key(|ext| ext.priority);

        Ok(Self {
            oid,
            size,
            extensions,
        })
    }

    /// Serialize this pointer to its canonical byte representation.
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        if self.size == 0 {
            return Vec::new();
        }

        let mut buf = String::new();
        buf.push_str("version ");
        buf.push_str(LFS_VERSION_URL);
        buf.push('\n');

        let needs_sort = self
            .extensions
            .windows(2)
            .any(|window| window[0].priority > window[1].priority);
        if needs_sort {
            let mut sorted_exts = self.extensions.clone();
            sorted_exts.sort_by_key(|ext| ext.priority);
            for ext in &sorted_exts {
                write_extension_line(&mut buf, ext);
            }
        } else {
            for ext in &self.extensions {
                write_extension_line(&mut buf, ext);
            }
        }

        buf.push_str("oid sha256:");
        buf.push_str(&hex_encode(&self.oid));
        buf.push('\n');
        buf.push_str("size ");
        buf.push_str(&self.size.to_string());
        buf.push('\n');
        buf.into_bytes()
    }

    /// Return whether `bytes` are in canonical LFS pointer form.
    #[must_use]
    pub fn is_canonical(bytes: &[u8]) -> bool {
        if bytes.contains(&b'\r') {
            return false;
        }
        if bytes.is_empty() {
            return true;
        }
        if bytes.last() != Some(&b'\n') {
            return false;
        }

        let pointer = match Self::parse(bytes) {
            Ok(pointer) => pointer,
            Err(_) => return false,
        };
        pointer.serialize() == bytes
    }

    fn validate_version(line: &str) -> Result<()> {
        let url = line
            .strip_prefix("version ")
            .ok_or_else(|| lfs_err(format!("expected version line, got: {line:?}")))?;

        if url == LFS_VERSION_URL || LEGACY_VERSION_URLS.contains(&url) {
            Ok(())
        } else {
            Err(lfs_err(format!("unsupported version URL: {url:?}")))
        }
    }

    fn parse_oid(value: &str) -> Result<[u8; 32]> {
        let hex = value
            .strip_prefix("sha256:")
            .ok_or_else(|| lfs_err(format!("unsupported OID type: {value:?}")))?;
        parse_hex32(hex).map_err(|_| {
            lfs_err(format!(
                "invalid OID: expected 64 hex chars after sha256:, got {:?}",
                hex,
            ))
        })
    }

    fn parse_size(value: &str) -> Result<u64> {
        if value.starts_with('-') {
            return Err(lfs_err(format!("size must be non-negative, got {value:?}")));
        }
        value
            .parse::<u64>()
            .map_err(|source| lfs_err(format!("invalid size {value:?}: {source}")))
    }

    fn parse_extension_line(line: &str) -> Result<LfsExtension> {
        let rest = line
            .strip_prefix("ext-")
            .ok_or_else(|| lfs_err(format!("bad extension line: {line:?}")))?;
        let (key, value) = rest
            .split_once(' ')
            .ok_or_else(|| lfs_err(format!("bad extension line: {line:?}")))?;
        let (priority_str, name) = key
            .split_once('-')
            .ok_or_else(|| lfs_err(format!("bad extension key: ext-{key}")))?;

        let priority: u8 = priority_str.parse().map_err(|source| {
            lfs_err(format!(
                "invalid extension priority {priority_str:?}: {source}"
            ))
        })?;
        if name.is_empty() {
            return Err(lfs_err("extension name must not be empty"));
        }

        let (oid_type, hex) = value
            .split_once(':')
            .ok_or_else(|| lfs_err(format!("bad extension OID: {value:?}")))?;
        let oid = parse_hex32(hex).map_err(|_| {
            lfs_err(format!(
                "invalid extension OID: expected 64 hex chars, got {:?}",
                hex,
            ))
        })?;

        Ok(LfsExtension {
            name: name.to_owned(),
            priority,
            oid,
            oid_type: oid_type.to_owned(),
        })
    }

    fn validate_unique_priorities(extensions: &[LfsExtension]) -> Result<()> {
        for i in 0..extensions.len() {
            for j in (i + 1)..extensions.len() {
                if extensions[i].priority == extensions[j].priority {
                    return Err(lfs_err(format!(
                        "duplicate extension priority: {}",
                        extensions[i].priority,
                    )));
                }
            }
        }
        Ok(())
    }
}

fn write_extension_line(buf: &mut String, ext: &LfsExtension) {
    buf.push_str("ext-");
    buf.push_str(&ext.priority.to_string());
    buf.push('-');
    buf.push_str(&ext.name);
    buf.push(' ');
    buf.push_str(&ext.oid_type);
    buf.push(':');
    buf.push_str(&hex_encode(&ext.oid));
    buf.push('\n');
}

fn lfs_err(reason: impl Into<String>) -> LfsPointerError {
    LfsPointerError::new(reason)
}

fn parse_hex32(hex: &str) -> std::result::Result<[u8; 32], ()> {
    if hex.len() != 64 {
        return Err(());
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = hex_nibble(hex.as_bytes()[i * 2])?;
        let lo = hex_nibble(hex.as_bytes()[i * 2 + 1])?;
        *byte = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> std::result::Result<u8, ()> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(()),
    }
}

/// Encode 32 bytes as a 64-character lowercase hex string.
#[must_use]
pub fn hex_encode(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_oid() -> [u8; 32] {
        let mut hash = [0u8; 32];
        for (i, byte) in hash.iter_mut().enumerate() {
            *byte = i as u8;
        }
        hash
    }

    fn sample_oid_hex() -> &'static str {
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
    }

    fn pointer_bytes(version: &str, oid_hex: &str, size: &str) -> Vec<u8> {
        format!("version {version}\noid sha256:{oid_hex}\nsize {size}\n").into_bytes()
    }

    #[test]
    fn parse_valid_pointer() {
        let raw = pointer_bytes(LFS_VERSION_URL, sample_oid_hex(), "1048576");
        let pointer = LfsPointer::parse(&raw).unwrap();
        assert_eq!(pointer.oid, sample_oid());
        assert_eq!(pointer.size, 1_048_576);
        assert!(pointer.extensions.is_empty());
    }

    #[test]
    fn parse_empty_input_returns_empty_pointer() {
        let pointer = LfsPointer::parse(b"").unwrap();
        assert_eq!(pointer.oid, EMPTY_SHA256);
        assert_eq!(pointer.size, 0);
        assert!(pointer.extensions.is_empty());
    }

    #[test]
    fn parse_accepts_legacy_version_aliases() {
        for version in LEGACY_VERSION_URLS {
            let raw = pointer_bytes(version, sample_oid_hex(), "42");
            let pointer = LfsPointer::parse(&raw).unwrap();
            assert_eq!(pointer.oid, sample_oid());
            assert_eq!(pointer.size, 42);
        }
    }

    #[test]
    fn parse_rejects_too_large() {
        let mut raw = pointer_bytes(LFS_VERSION_URL, sample_oid_hex(), "1");
        raw.resize(MAX_LFS_POINTER_SIZE + 1, b' ');
        let err = LfsPointer::parse(&raw).unwrap_err();
        assert!(err.reason.contains("too large"));
    }

    #[test]
    fn parse_rejects_bad_shape() {
        let cases = [
            pointer_bytes("https://example.com/v99", sample_oid_hex(), "1"),
            pointer_bytes(LFS_VERSION_URL, "abcd", "1"),
            format!(
                "version {LFS_VERSION_URL}\noid md5:{}\nsize 1\n",
                sample_oid_hex()
            )
            .into_bytes(),
            pointer_bytes(LFS_VERSION_URL, sample_oid_hex(), "-42"),
            format!("version {LFS_VERSION_URL}\nsize 1\n").into_bytes(),
            format!(
                "version {LFS_VERSION_URL}\noid sha256:{}\n",
                sample_oid_hex()
            )
            .into_bytes(),
        ];

        for raw in cases {
            assert!(LfsPointer::parse(&raw).is_err());
        }
    }

    #[test]
    fn parse_extensions_sorted_by_priority() {
        let ext_oid = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let raw = format!(
            "version {LFS_VERSION_URL}\n\
             ext-3-gamma sha256:{ext_oid}\n\
             ext-1-alpha sha256:{ext_oid}\n\
             ext-2-beta sha256:{ext_oid}\n\
             oid sha256:{}\n\
             size 100\n",
            sample_oid_hex(),
        )
        .into_bytes();
        let pointer = LfsPointer::parse(&raw).unwrap();
        assert_eq!(pointer.extensions.len(), 3);
        assert_eq!(pointer.extensions[0].priority, 1);
        assert_eq!(pointer.extensions[1].priority, 2);
        assert_eq!(pointer.extensions[2].priority, 3);
    }

    #[test]
    fn parse_rejects_duplicate_priorities() {
        let ext_oid = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let raw = format!(
            "version {LFS_VERSION_URL}\n\
             ext-1-alpha sha256:{ext_oid}\n\
             ext-1-beta sha256:{ext_oid}\n\
             oid sha256:{}\n\
             size 100\n",
            sample_oid_hex(),
        )
        .into_bytes();
        let err = LfsPointer::parse(&raw).unwrap_err();
        assert!(err.reason.contains("duplicate extension priority"));
    }

    #[test]
    fn serialize_produces_canonical_format() {
        let pointer = LfsPointer {
            oid: sample_oid(),
            size: 1_048_576,
            extensions: Vec::new(),
        };
        let bytes = pointer.serialize();
        let expected = format!(
            "version {LFS_VERSION_URL}\noid sha256:{}\nsize 1048576\n",
            sample_oid_hex(),
        );
        assert_eq!(bytes, expected.as_bytes());
    }

    #[test]
    fn serialize_with_extensions_sorts_by_priority() {
        let ext_oid = [0xaa; 32];
        let ext_oid_hex = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let pointer = LfsPointer {
            oid: sample_oid(),
            size: 100,
            extensions: vec![
                LfsExtension {
                    name: "gamma".into(),
                    priority: 3,
                    oid: ext_oid,
                    oid_type: "sha256".into(),
                },
                LfsExtension {
                    name: "alpha".into(),
                    priority: 1,
                    oid: ext_oid,
                    oid_type: "sha256".into(),
                },
            ],
        };
        let bytes = pointer.serialize();
        let expected = format!(
            "version {LFS_VERSION_URL}\n\
             ext-1-alpha sha256:{ext_oid_hex}\n\
             ext-3-gamma sha256:{ext_oid_hex}\n\
             oid sha256:{}\n\
             size 100\n",
            sample_oid_hex(),
        );
        assert_eq!(bytes, expected.as_bytes());
    }

    #[test]
    fn serialize_zero_size_returns_empty() {
        let pointer = LfsPointer {
            oid: EMPTY_SHA256,
            size: 0,
            extensions: Vec::new(),
        };
        assert!(pointer.serialize().is_empty());
    }

    #[test]
    fn canonical_form_checks_exact_bytes() {
        let pointer = LfsPointer {
            oid: sample_oid(),
            size: 42,
            extensions: Vec::new(),
        };
        let bytes = pointer.serialize();
        assert!(LfsPointer::is_canonical(&bytes));
        assert!(LfsPointer::is_canonical(b""));
        assert!(!LfsPointer::is_canonical(&bytes[..bytes.len() - 1]));

        let legacy = pointer_bytes("http://git-media.io/v/2", sample_oid_hex(), "1");
        assert!(!LfsPointer::is_canonical(&legacy));
    }

    #[test]
    fn hex_encode_round_trip() {
        let bytes = sample_oid();
        let hex = hex_encode(&bytes);
        assert_eq!(hex, sample_oid_hex());
        let decoded = parse_hex32(&hex).unwrap();
        assert_eq!(decoded, bytes);
    }
}
