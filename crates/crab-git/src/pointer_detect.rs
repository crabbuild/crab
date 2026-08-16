//! Dual pointer detection for LFS and Crab formats.
//!
//! Classifies a blob as an LFS pointer, a Crab pointer, or not a pointer
//! by inspecting the version line prefix. Uses a fast path that checks the
//! first 256 bytes before attempting a full parse.

use crab_types::pointer::Pointer;

use crate::lfs_pointer::{LfsPointer, MAX_LFS_POINTER_SIZE};

/// Version line prefixes used for fast-path detection.
const LFS_VERSION_PREFIX: &str = "version https://git-lfs.github.com/spec/v1";
const LEGACY_GIT_MEDIA_PREFIX: &str = "version http://git-media.io/v/2";
const LEGACY_HAWSER_PREFIX: &str = "version https://hawser.github.com/spec/v1";
const CRAB_VERSION_PREFIX: &str = "version https://crab.dev/spec/v1";

/// Result of classifying a blob's pointer format.
#[derive(Debug)]
pub enum PointerKind {
    /// Standard Git LFS pointer (spec v1).
    Lfs(LfsPointer),
    /// Crab-native pointer.
    Crab(Pointer),
    /// Not a recognized pointer format.
    NotAPointer,
}

/// Classify a blob by inspecting the version line prefix.
///
/// Checks the first 256 bytes for a known version line before attempting a
/// full parse. Empty or oversized blobs are classified as `NotAPointer`.
#[must_use]
pub fn classify(bytes: &[u8]) -> PointerKind {
    if bytes.is_empty() || bytes.len() > MAX_LFS_POINTER_SIZE {
        return PointerKind::NotAPointer;
    }

    let window = &bytes[..bytes.len().min(256)];
    let first_line = match window.iter().position(|&b| b == b'\n') {
        Some(pos) => &window[..pos],
        None => return PointerKind::NotAPointer,
    };

    if first_line == LFS_VERSION_PREFIX.as_bytes()
        || first_line == LEGACY_GIT_MEDIA_PREFIX.as_bytes()
        || first_line == LEGACY_HAWSER_PREFIX.as_bytes()
    {
        match LfsPointer::parse(bytes) {
            Ok(pointer) => PointerKind::Lfs(pointer),
            Err(_) => PointerKind::NotAPointer,
        }
    } else if first_line == CRAB_VERSION_PREFIX.as_bytes() {
        match Pointer::parse(bytes) {
            Ok(pointer) => PointerKind::Crab(pointer),
            Err(_) => PointerKind::NotAPointer,
        }
    } else {
        PointerKind::NotAPointer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lfs_pointer::{LFS_VERSION_URL, hex_encode};

    fn sample_oid() -> [u8; 32] {
        let mut h = [0u8; 32];
        for (i, byte) in h.iter_mut().enumerate() {
            *byte = i as u8;
        }
        h
    }

    fn sample_oid_hex() -> &'static str {
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
    }

    #[test]
    fn classify_lfs_pointer() {
        let raw = format!(
            "version {LFS_VERSION_URL}\noid sha256:{}\nsize 1048576\n",
            sample_oid_hex(),
        );
        match classify(raw.as_bytes()) {
            PointerKind::Lfs(p) => {
                assert_eq!(p.oid, sample_oid());
                assert_eq!(p.size, 1_048_576);
            }
            other => panic!("expected Lfs, got {other:?}"),
        }
    }

    #[test]
    fn classify_lfs_legacy_git_media() {
        let raw = format!(
            "version http://git-media.io/v/2\noid sha256:{}\nsize 42\n",
            sample_oid_hex(),
        );
        assert!(matches!(classify(raw.as_bytes()), PointerKind::Lfs(_)));
    }

    #[test]
    fn classify_lfs_legacy_hawser() {
        let raw = format!(
            "version https://hawser.github.com/spec/v1\noid sha256:{}\nsize 99\n",
            sample_oid_hex(),
        );
        assert!(matches!(classify(raw.as_bytes()), PointerKind::Lfs(_)));
    }

    #[test]
    fn classify_crab_pointer() {
        let hash = sample_oid();
        let raw = format!(
            "version https://crab.dev/spec/v1\nfile-hash {}\nsize 512\n",
            hex_encode(&hash),
        );
        match classify(raw.as_bytes()) {
            PointerKind::Crab(p) => {
                assert_eq!(p.file_hash, hash);
                assert_eq!(p.size, 512);
            }
            other => panic!("expected Crab, got {other:?}"),
        }
    }

    #[test]
    fn classify_empty_is_not_a_pointer() {
        assert!(matches!(classify(b""), PointerKind::NotAPointer));
    }

    #[test]
    fn classify_too_large_is_not_a_pointer() {
        let mut raw = format!(
            "version {LFS_VERSION_URL}\noid sha256:{}\nsize 1\n",
            sample_oid_hex(),
        )
        .into_bytes();
        raw.resize(MAX_LFS_POINTER_SIZE + 1, b' ');
        assert!(matches!(classify(&raw), PointerKind::NotAPointer));
    }

    #[test]
    fn classify_random_bytes_is_not_a_pointer() {
        assert!(matches!(
            classify(b"hello world, this is not a pointer"),
            PointerKind::NotAPointer,
        ));
    }

    #[test]
    fn classify_unknown_version_is_not_a_pointer() {
        let raw = b"version https://example.com/v99\noid sha256:0000000000000000000000000000000000000000000000000000000000000000\nsize 1\n";
        assert!(matches!(classify(raw), PointerKind::NotAPointer));
    }

    #[test]
    fn classify_lfs_with_bad_body_is_not_a_pointer() {
        let raw = format!("version {LFS_VERSION_URL}\ngarbage\n");
        assert!(matches!(classify(raw.as_bytes()), PointerKind::NotAPointer));
    }

    #[test]
    fn classify_crab_with_bad_body_is_not_a_pointer() {
        let raw = b"version https://crab.dev/spec/v1\ngarbage\n";
        assert!(matches!(classify(raw), PointerKind::NotAPointer));
    }

    #[test]
    fn classify_no_newline_is_not_a_pointer() {
        let raw = b"version https://git-lfs.github.com/spec/v1";
        assert!(matches!(classify(raw), PointerKind::NotAPointer));
    }
}
