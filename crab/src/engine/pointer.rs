//! Crab pointer format: parse and serialize.
//!
//! A pointer is three or four LF-terminated lines totalling ≤256 bytes:
//!
//! ```text
//! version https://crab.dev/spec/v1
//! file-hash {64-hex-blake3}
//! size {decimal-bytes}
//! shard-hint {64-hex-blake3}   (optional)
//! ```

use crate::core::error::Result;
use std::io::Read;
use std::path::Path;

use crab_types::pointer::{MAX_POINTER_SIZE, Pointer};

/// Hydration state of a tracked file in the working tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HydrationState {
    /// File contains a pointer blob (unhydrated).
    Pointer,
    /// File contains full content matching the committed pointer's size.
    Hydrated,
    /// File contains full content but its size differs from the committed pointer.
    Modified,
}

/// Determine the hydration state of a working-tree file relative to its
/// committed pointer.
///
/// 1. If the file is a valid pointer on disk → [`HydrationState::Pointer`].
/// 2. If the file size matches `pointer.size` → [`HydrationState::Hydrated`].
/// 3. Otherwise → [`HydrationState::Modified`].
///
/// Propagates I/O errors from metadata / file reads.
pub fn detect_hydration_state(path: &Path, pointer: &Pointer) -> Result<HydrationState> {
    if is_working_tree_pointer(path)? {
        return Ok(HydrationState::Pointer);
    }

    let meta = std::fs::metadata(path)?;
    if meta.len() == pointer.size {
        Ok(HydrationState::Hydrated)
    } else {
        Ok(HydrationState::Modified)
    }
}

/// Check if a working-tree file is an unhydrated crab pointer.
///
/// Reads at most 256 bytes from the file. Returns `true` when the file
/// size is within the pointer budget and `Pointer::parse` succeeds on
/// the contents.
///
/// Returns `Ok(false)` for empty files or files larger than
/// [`MAX_POINTER_SIZE`]. Propagates I/O errors from open/read.
pub fn is_working_tree_pointer(path: &Path) -> Result<bool> {
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_POINTER_SIZE as u64 {
        return Ok(false);
    }

    let mut file = std::fs::File::open(path)?;
    let mut buf = [0u8; MAX_POINTER_SIZE];
    let n = file.read(&mut buf)?;
    if n == 0 {
        return Ok(false);
    }

    Ok(Pointer::parse(&buf[..n]).is_ok())
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

    fn sample_pointer() -> Pointer {
        Pointer {
            file_hash: sample_hash(),
            size: 1_048_576,
            shard_hint: None,
        }
    }

    #[test]
    fn working_tree_pointer_detects_valid_pointer_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ptr.bin");
        let p = sample_pointer();
        std::fs::write(&path, p.serialize()).unwrap();
        assert!(is_working_tree_pointer(&path).unwrap());
    }

    #[test]
    fn working_tree_pointer_rejects_large_hydrated_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        std::fs::write(&path, vec![0u8; MAX_POINTER_SIZE + 1]).unwrap();
        assert!(!is_working_tree_pointer(&path).unwrap());
    }

    #[test]
    fn working_tree_pointer_rejects_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty");
        std::fs::write(&path, b"").unwrap();
        assert!(!is_working_tree_pointer(&path).unwrap());
    }

    #[test]
    fn working_tree_pointer_rejects_small_non_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("readme.txt");
        std::fs::write(&path, b"hello world\n").unwrap();
        assert!(!is_working_tree_pointer(&path).unwrap());
    }

    #[test]
    fn working_tree_pointer_returns_io_error_for_missing_file() {
        let result = is_working_tree_pointer(Path::new("/nonexistent/pointer.bin"));
        assert!(result.is_err());
    }

    // --- detect_hydration_state tests ---

    #[test]
    fn hydration_state_pointer_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ptr.bin");
        let p = sample_pointer();
        std::fs::write(&path, p.serialize()).unwrap();
        assert_eq!(
            detect_hydration_state(&path, &p).unwrap(),
            HydrationState::Pointer
        );
    }

    #[test]
    fn hydration_state_hydrated_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        let p = Pointer {
            file_hash: sample_hash(),
            size: 4096,
            shard_hint: None,
        };
        // Write non-pointer content whose size matches pointer.size.
        std::fs::write(&path, vec![0xAB; 4096]).unwrap();
        assert_eq!(
            detect_hydration_state(&path, &p).unwrap(),
            HydrationState::Hydrated
        );
    }

    #[test]
    fn hydration_state_modified_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        let p = Pointer {
            file_hash: sample_hash(),
            size: 4096,
            shard_hint: None,
        };
        // Write content whose size differs from pointer.size.
        std::fs::write(&path, vec![0xAB; 9999]).unwrap();
        assert_eq!(
            detect_hydration_state(&path, &p).unwrap(),
            HydrationState::Modified
        );
    }

    #[test]
    fn hydration_state_io_error_for_missing_file() {
        let p = sample_pointer();
        let result = detect_hydration_state(Path::new("/nonexistent/data.bin"), &p);
        assert!(result.is_err());
    }
}
