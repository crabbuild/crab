//! Git pack-format helpers.
//!
//! This Module owns low-level Git pack wire-format validation. Fetch pipelines,
//! object-store transport, and CLI error presentation live above this Interface.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha1::{Digest, Sha1};

/// SHA-1 trailer length in Git pack files.
pub const PACK_SHA1_LEN: usize = 20;

/// Result alias for Git pack validation.
pub type Result<T> = std::result::Result<T, PackError>;

/// Errors returned while validating Git pack bytes.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum PackError {
    /// A pack is too short to contain the mandatory trailing SHA-1.
    #[error("git pack too short for SHA-1 trailer: {len} bytes")]
    TooShort { len: usize },

    /// The trailing SHA-1 did not match the preceding bytes.
    #[error("git pack SHA-1 mismatch: expected {expected}, computed {computed}")]
    Sha1Mismatch { expected: String, computed: String },

    /// Pack bytes exceeded the caller's intake size cap.
    #[error("git pack too large: {size} bytes exceeds limit {limit}")]
    PackTooLarge { size: u64, limit: u64 },

    /// Caller-provided canonical pack name cannot be used as a pack filename.
    #[error("invalid canonical pack name: {name:?}")]
    InvalidCanonicalName { name: String },

    /// Pack file is not a valid Git pack.
    #[error("invalid git pack file {path}: {reason}")]
    InvalidPackFile { path: PathBuf, reason: String },

    /// Filesystem I/O failed while handling a pack.
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    /// `git index-pack` exited unsuccessfully.
    #[error("git index-pack failed for pack-{git_sha1}: {stderr}")]
    IndexPackFailed { git_sha1: String, stderr: String },

    /// `git index-pack --fsck-objects` rejected an object body.
    #[error("git object validation failed for pack-{git_sha1}: {stderr}")]
    ObjectFsckFailed { git_sha1: String, stderr: String },

    /// `git index-pack` succeeded but did not create the requested index.
    #[error("git index-pack succeeded but produced no index at {path}")]
    IndexMissing { path: PathBuf },

    /// Generated index reports a different pack hash than the pack trailer.
    #[error("pack hash mismatch: trailer says {trailer}, index says {index}")]
    PackHashMismatch { trailer: String, index: String },

    /// Pack index could not be opened for validation.
    #[error("failed to open idx {path} for hash check: {reason}")]
    IndexOpenFailed { path: PathBuf, reason: String },

    /// Pack index checksum did not match its content.
    #[error("pack index checksum failed for {path}: {reason}")]
    IndexChecksumFailed { path: PathBuf, reason: String },

    /// Pack reverse-index generation or validation failed.
    #[error(transparent)]
    ReverseIndex {
        #[from]
        source: crate::pack_locator::PackLocatorError,
    },
}

/// Verify the trailing SHA-1 checksum of a Git pack.
///
/// Git packs end with a 20-byte SHA-1 computed over all preceding bytes.
///
/// # Errors
///
/// Returns [`PackError::TooShort`] when the input cannot hold a trailer, or
/// [`PackError::Sha1Mismatch`] when the trailer does not match the content.
pub fn verify_pack_sha1(pack_bytes: &[u8]) -> Result<()> {
    if pack_bytes.len() < PACK_SHA1_LEN {
        return Err(PackError::TooShort {
            len: pack_bytes.len(),
        });
    }

    let (content, expected_hash) = pack_bytes.split_at(pack_bytes.len() - PACK_SHA1_LEN);

    let mut hasher = Sha1::new();
    hasher.update(content);
    let computed = hasher.finalize();

    if computed.as_slice() != expected_hash {
        return Err(PackError::Sha1Mismatch {
            expected: to_hex(expected_hash),
            computed: to_hex(computed.as_slice()),
        });
    }

    Ok(())
}

/// Validate a Git pack index and return the checksum of its corresponding pack.
///
/// # Errors
///
/// Returns [`PackError::IndexOpenFailed`] for malformed index structure and
/// [`PackError::IndexChecksumFailed`] when the index's trailing checksum does
/// not match its content.
pub fn verify_pack_index_file(idx_path: &Path) -> Result<String> {
    use std::sync::atomic::AtomicBool;

    let idx = gix_pack::index::File::at(idx_path, gix_hash::Kind::Sha1).map_err(|error| {
        PackError::IndexOpenFailed {
            path: idx_path.to_owned(),
            reason: error.to_string(),
        }
    })?;
    let mut progress = gix_features::progress::Discard;
    idx.verify_checksum(&mut progress, &AtomicBool::new(false))
        .map_err(|error| PackError::IndexChecksumFailed {
            path: idx_path.to_owned(),
            reason: error.to_string(),
        })?;
    Ok(idx.pack_checksum().to_hex().to_string())
}

/// Extracts the canonical Crab pack id from a remote pack object filename.
///
/// Returns `None` unless `file_name` is exactly `pack-{id}.pack` and `{id}`
/// is a 64-character hex content hash.
#[must_use]
pub fn canonical_pack_id_from_object_filename(file_name: &str) -> Option<&str> {
    let hash = file_name
        .strip_prefix("pack-")
        .and_then(|rest| rest.strip_suffix(".pack"))?;
    if valid_content_hash(hash) {
        Some(hash)
    } else {
        None
    }
}

/// Outcome of a local pack install.
#[derive(Debug)]
pub struct InstalledPack {
    /// The Git-native pack name: the pack's trailing SHA-1 in hex.
    pub git_sha1: String,
    /// Final `.pack` path, named by the caller's canonical pack id.
    pub pack_path: PathBuf,
    /// Final `.idx` path, named by the caller's canonical pack id.
    pub idx_path: PathBuf,
    /// Final required `.rev` path, named by the caller's canonical pack id.
    pub rev_path: PathBuf,
}

/// Install an already-downloaded pack file into a local Git pack directory.
///
/// The pack is installed as `pack-{canonical_name}.pack` with a matching
/// `pack-{canonical_name}.idx`. The input may live on another filesystem; it
/// is copied into a pack-dir tempfile before indexing so final installation
/// still uses `rename` and never exposes a partially copied pack.
///
/// # Errors
///
/// Returns [`PackError::PackTooLarge`] when `max_input_size` is non-zero and
/// the input exceeds it, [`PackError::InvalidCanonicalName`] for unsafe pack
/// ids, [`PackError::InvalidPackFile`] or [`PackError::Sha1Mismatch`] for
/// malformed pack bytes, [`PackError::ObjectFsckFailed`] when object fsck
/// rejects the pack, and [`PackError::IndexPackFailed`] for other indexing
/// failures.
pub fn install_pack_file_from_path(
    pack_dir: &Path,
    pack_tmp_path: &Path,
    canonical_name: &str,
    max_input_size: u64,
    fsck_objects: bool,
) -> Result<InstalledPack> {
    let size = std::fs::metadata(pack_tmp_path)
        .map_err(|source| io_error(format!("metadata {}", pack_tmp_path.display()), source))?
        .len();
    if max_input_size > 0 && size > max_input_size {
        return Err(PackError::PackTooLarge {
            size,
            limit: max_input_size,
        });
    }

    if !valid_canonical_pack_name(canonical_name) {
        return Err(PackError::InvalidCanonicalName {
            name: canonical_name.to_owned(),
        });
    }

    let final_pack = pack_dir.join(format!("pack-{canonical_name}.pack"));
    let final_idx = pack_dir.join(format!("pack-{canonical_name}.idx"));
    let final_rev = pack_dir.join(format!("pack-{canonical_name}.rev"));

    if final_pack.exists() && final_idx.exists() {
        let installed_size = std::fs::metadata(&final_pack)
            .map_err(|source| io_error(format!("metadata {}", final_pack.display()), source))?
            .len();
        if !final_rev.exists() {
            crate::pack_locator::write_pack_reverse_index(&final_idx, &final_rev)?;
        }
        crate::pack_locator::PackLocationIter::open(&final_idx, &final_rev, installed_size)?;
        return Ok(InstalledPack {
            git_sha1: verify_pack_file_sha1(pack_tmp_path)?,
            pack_path: final_pack,
            idx_path: final_idx,
            rev_path: final_rev,
        });
    }

    std::fs::create_dir_all(pack_dir)
        .map_err(|source| io_error(format!("create {}", pack_dir.display()), source))?;

    let git_sha1 = verify_pack_file_sha1(pack_tmp_path)?;
    let pack_tmp = copy_pack_to_install_temp(pack_dir, pack_tmp_path)?;
    let pack_tmp_path = pack_tmp.into_temp_path();
    install_pack_file_blocking(
        pack_dir,
        pack_tmp_path.as_ref(),
        &git_sha1,
        &final_pack,
        &final_idx,
        &final_rev,
        fsck_objects,
    )
}

fn copy_pack_to_install_temp(pack_dir: &Path, source: &Path) -> Result<tempfile::NamedTempFile> {
    let pack_tmp = tempfile::Builder::new()
        .prefix(".crab-pack-")
        .suffix(".pack")
        .tempfile_in(pack_dir)
        .map_err(|source| {
            io_error(
                format!("create pack tempfile in {}", pack_dir.display()),
                source,
            )
        })?;
    std::fs::copy(source, pack_tmp.path()).map_err(|source| {
        io_error(
            format!("copy pack into {}", pack_tmp.path().display()),
            source,
        )
    })?;
    Ok(pack_tmp)
}

fn valid_canonical_pack_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn valid_content_hash(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn install_pack_file_blocking(
    pack_dir: &Path,
    pack_tmp_path: &Path,
    git_sha1: &str,
    final_pack: &Path,
    final_idx: &Path,
    final_rev: &Path,
    fsck_objects: bool,
) -> Result<InstalledPack> {
    let idx_tmp = tempfile::Builder::new()
        .prefix(".crab-idx-")
        .suffix(".idx")
        .tempfile_in(pack_dir)
        .map_err(|source| {
            io_error(
                format!("create idx tempfile in {}", pack_dir.display()),
                source,
            )
        })?;
    let idx_tmp_path = idx_tmp.path().to_owned();
    let _idx_guard = idx_tmp;
    let _ = std::fs::remove_file(&idx_tmp_path);

    let mut index_pack = Command::new("git");
    if let Some(git_dir) = pack_dir
        .parent()
        .filter(|objects_dir| {
            objects_dir
                .file_name()
                .is_some_and(|name| name == "objects")
        })
        .and_then(Path::parent)
    {
        // Strict indexing may need base or attribute objects from earlier
        // packs. Bind it to the owning repository instead of ambient cwd.
        index_pack.arg("--git-dir").arg(git_dir);
    }
    index_pack.arg("index-pack").arg("--rev-index");
    if fsck_objects {
        // Validate object bodies during the indexing pass. Broken links are
        // checked separately by the reachability owner after all packs land.
        index_pack.arg("--fsck-objects");
    }
    let output = index_pack
        .arg("-o")
        .arg(&idx_tmp_path)
        .arg(pack_tmp_path)
        .output()
        .map_err(|source| io_error("spawn git index-pack", source))?;

    let rev_tmp_path = idx_tmp_path.with_extension("rev");
    if !output.status.success() {
        let _ = std::fs::remove_file(&rev_tmp_path);
        let error = if fsck_objects {
            PackError::ObjectFsckFailed {
                git_sha1: git_sha1.to_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }
        } else {
            PackError::IndexPackFailed {
                git_sha1: git_sha1.to_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }
        };
        return Err(error);
    }

    if !idx_tmp_path.exists() {
        let _ = std::fs::remove_file(&rev_tmp_path);
        return Err(PackError::IndexMissing { path: idx_tmp_path });
    }

    match parse_idx_pack_hash(&idx_tmp_path) {
        Ok(reported) if reported == git_sha1 => {}
        Ok(reported) => {
            let _ = std::fs::remove_file(&idx_tmp_path);
            let _ = std::fs::remove_file(&rev_tmp_path);
            return Err(PackError::PackHashMismatch {
                trailer: git_sha1.to_owned(),
                index: reported,
            });
        }
        Err(PackError::IndexOpenFailed { .. }) => {}
        Err(error) => return Err(error),
    }

    if !rev_tmp_path.exists() {
        crate::pack_locator::write_pack_reverse_index(&idx_tmp_path, &rev_tmp_path)?;
    }
    crate::pack_locator::PackLocationIter::open(
        &idx_tmp_path,
        &rev_tmp_path,
        std::fs::metadata(pack_tmp_path)
            .map_err(|source| io_error(format!("metadata {}", pack_tmp_path.display()), source))?
            .len(),
    )?;

    std::fs::rename(&idx_tmp_path, final_idx)
        .map_err(|source| io_error(format!("rename {}", idx_tmp_path.display()), source))?;

    if let Err(source) = std::fs::rename(&rev_tmp_path, final_rev) {
        let _ = std::fs::remove_file(final_idx);
        let _ = std::fs::remove_file(&rev_tmp_path);
        return Err(io_error(
            format!("rename {}", rev_tmp_path.display()),
            source,
        ));
    }

    if let Err(source) = std::fs::rename(pack_tmp_path, final_pack) {
        let _ = std::fs::remove_file(final_idx);
        let _ = std::fs::remove_file(final_rev);
        return Err(io_error(
            format!("rename {}", pack_tmp_path.display()),
            source,
        ));
    }

    Ok(InstalledPack {
        git_sha1: git_sha1.to_owned(),
        pack_path: final_pack.to_owned(),
        idx_path: final_idx.to_owned(),
        rev_path: final_rev.to_owned(),
    })
}

fn verify_pack_file_sha1(path: &Path) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};

    const HEADER_LEN: u64 = 12;
    const SHA1_LEN_U64: u64 = PACK_SHA1_LEN as u64;

    let mut file = std::fs::File::open(path)
        .map_err(|source| io_error(format!("open {}", path.display()), source))?;
    let len = file
        .metadata()
        .map_err(|source| io_error(format!("metadata {}", path.display()), source))?
        .len();
    if len < SHA1_LEN_U64 + HEADER_LEN {
        return Err(PackError::InvalidPackFile {
            path: path.to_owned(),
            reason: "too short for PACK header and SHA-1 trailer".to_owned(),
        });
    }

    let mut header = [0u8; 4];
    file.read_exact(&mut header)
        .map_err(|source| io_error(format!("read {}", path.display()), source))?;
    if &header != b"PACK" {
        return Err(PackError::InvalidPackFile {
            path: path.to_owned(),
            reason: "missing PACK header".to_owned(),
        });
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|source| io_error(format!("seek {}", path.display()), source))?;

    let mut remaining = len - SHA1_LEN_U64;
    let mut hasher = Sha1::new();
    let mut buf = [0u8; 1024 * 1024];
    while remaining > 0 {
        let read_len = remaining.min(buf.len() as u64) as usize;
        file.read_exact(&mut buf[..read_len])
            .map_err(|source| io_error(format!("read {}", path.display()), source))?;
        hasher.update(&buf[..read_len]);
        remaining -= read_len as u64;
    }

    let mut expected = [0u8; PACK_SHA1_LEN];
    file.read_exact(&mut expected)
        .map_err(|source| io_error(format!("read {}", path.display()), source))?;
    let computed = hasher.finalize();

    if computed.as_slice() != expected {
        return Err(PackError::Sha1Mismatch {
            expected: to_hex(&expected),
            computed: to_hex(computed.as_slice()),
        });
    }

    Ok(to_hex(&expected))
}

fn parse_idx_pack_hash(idx_path: &Path) -> Result<String> {
    verify_pack_index_file(idx_path)
}

fn io_error(context: impl Into<String>, source: std::io::Error) -> PackError {
    PackError::Io {
        context: context.into(),
        source,
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            use std::fmt::Write;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use super::*;

    fn pack_with_sha1(content: &[u8]) -> Vec<u8> {
        let mut hasher = Sha1::new();
        hasher.update(content);
        let hash = hasher.finalize();
        let mut pack = Vec::with_capacity(content.len() + PACK_SHA1_LEN);
        pack.extend_from_slice(content);
        pack.extend_from_slice(&hash);
        pack
    }

    fn git(args: &[&str], stdin: Option<&[u8]>) -> Vec<u8> {
        let mut child = Command::new("git")
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn git");
        if let Some(input) = stdin {
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(input)
                .expect("write git stdin");
        }
        let output = child.wait_with_output().expect("wait for git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn pack_index_fixture() -> (tempfile::TempDir, PathBuf, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let git_dir = dir.path().join("source.git");
        git(
            &["init", "--bare", git_dir.to_str().expect("git path UTF-8")],
            None,
        );
        let oid = String::from_utf8(git(
            &[
                "--git-dir",
                git_dir.to_str().expect("git path UTF-8"),
                "hash-object",
                "-w",
                "--stdin",
            ],
            Some(b"pack index fixture\n"),
        ))
        .expect("object id UTF-8");
        let base = dir.path().join("fixture-pack");
        let pack_hash = String::from_utf8(git(
            &[
                "--git-dir",
                git_dir.to_str().expect("git path UTF-8"),
                "pack-objects",
                "--index-version=2",
                base.to_str().expect("pack base UTF-8"),
            ],
            Some(oid.as_bytes()),
        ))
        .expect("pack hash UTF-8")
        .trim()
        .to_owned();
        let idx_path = dir.path().join(format!("fixture-pack-{pack_hash}.idx"));
        (dir, idx_path, pack_hash)
    }

    #[test]
    fn valid_pack_passes_sha1_verification() {
        let pack = pack_with_sha1(b"PACK valid content here");
        assert!(verify_pack_sha1(&pack).is_ok());
    }

    #[test]
    fn corrupted_pack_fails_sha1_verification() {
        let mut pack = pack_with_sha1(b"PACK valid content here");
        pack[4] ^= 0x01;
        assert!(matches!(
            verify_pack_sha1(&pack),
            Err(PackError::Sha1Mismatch { .. })
        ));
    }

    #[test]
    fn truncated_pack_fails_sha1_verification() {
        assert!(matches!(
            verify_pack_sha1(b"too short"),
            Err(PackError::TooShort { len: 9 })
        ));
    }

    #[test]
    fn valid_pack_index_reports_corresponding_pack_checksum() {
        let (_dir, idx_path, pack_hash) = pack_index_fixture();

        assert_eq!(
            verify_pack_index_file(&idx_path).expect("valid pack index"),
            pack_hash
        );
    }

    #[test]
    fn corrupted_pack_index_checksum_is_rejected() {
        let (dir, idx_path, _pack_hash) = pack_index_fixture();
        let mut bytes = std::fs::read(&idx_path).expect("read pack index");
        let last = bytes.last_mut().expect("pack index checksum byte");
        *last ^= 1;
        let corrupt_path = dir.path().join("corrupt.idx");
        std::fs::write(&corrupt_path, bytes).expect("write corrupt pack index");

        assert!(matches!(
            verify_pack_index_file(&corrupt_path),
            Err(PackError::IndexChecksumFailed { .. })
        ));
    }

    #[test]
    fn canonical_pack_id_from_object_filename_extracts_content_hash() {
        let id = "a".repeat(64);
        let file_name = format!("pack-{id}.pack");

        assert_eq!(
            canonical_pack_id_from_object_filename(&file_name),
            Some(id.as_str())
        );
    }

    #[test]
    fn canonical_pack_id_from_object_filename_rejects_noncanonical_names() {
        for file_name in [
            "",
            "pack-.pack",
            "pack-not-a-hash.pack",
            "pack-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.pack",
            "pack-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.idx",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.pack",
        ] {
            assert_eq!(
                canonical_pack_id_from_object_filename(file_name),
                None,
                "expected {file_name:?} to be rejected"
            );
        }
    }

    #[test]
    fn install_pack_file_from_path_rejects_unsafe_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pack = dir.path().join("input.pack");
        std::fs::write(&pack, b"not a pack").expect("write pack");

        let err = install_pack_file_from_path(dir.path(), &pack, "../escape", 0, false)
            .expect_err("unsafe name should fail");
        assert!(matches!(err, PackError::InvalidCanonicalName { .. }));
    }

    #[test]
    fn install_pack_file_from_path_rejects_invalid_pack_before_installing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pack = dir.path().join("input.pack");
        std::fs::write(&pack, b"not a pack").expect("write pack");

        let err = install_pack_file_from_path(dir.path(), &pack, "bad", 0, false)
            .expect_err("invalid pack should fail");
        assert!(matches!(err, PackError::InvalidPackFile { .. }));
        assert!(!dir.path().join("pack-bad.pack").exists());
        assert!(!dir.path().join("pack-bad.idx").exists());
    }

    #[test]
    fn install_pack_file_always_returns_verified_reverse_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git_dir = dir.path().join("source.git");
        git(
            &["init", "--bare", git_dir.to_str().expect("git path UTF-8")],
            None,
        );
        let oid = String::from_utf8(git(
            &[
                "--git-dir",
                git_dir.to_str().expect("git path UTF-8"),
                "hash-object",
                "-w",
                "--stdin",
            ],
            Some(b"installed object\n"),
        ))
        .expect("object id UTF-8");
        let base = dir.path().join("source-pack");
        let pack_hash = String::from_utf8(git(
            &[
                "--git-dir",
                git_dir.to_str().expect("git path UTF-8"),
                "pack-objects",
                "--index-version=2",
                base.to_str().expect("pack base UTF-8"),
            ],
            Some(oid.as_bytes()),
        ))
        .expect("pack id UTF-8");
        let source = dir
            .path()
            .join(format!("source-pack-{}.pack", pack_hash.trim()));
        let pack_dir = dir.path().join("installed");

        let installed = install_pack_file_from_path(&pack_dir, &source, "canonical", 0, false)
            .expect("install pack");

        assert!(installed.rev_path.exists());
        crate::pack_locator::PackLocationIter::open(
            &installed.idx_path,
            &installed.rev_path,
            std::fs::metadata(&installed.pack_path)
                .expect("pack metadata")
                .len(),
        )
        .expect("verified reverse index");
    }

    #[test]
    fn install_pack_file_fsck_rejects_malformed_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git_dir = dir.path().join("source.git");
        git(
            &["init", "--bare", git_dir.to_str().expect("git path UTF-8")],
            None,
        );
        let tree = String::from_utf8(git(
            &[
                "--git-dir",
                git_dir.to_str().expect("git path UTF-8"),
                "mktree",
            ],
            Some(b""),
        ))
        .expect("tree id UTF-8");
        let commit = String::from_utf8(git(
            &[
                "--git-dir",
                git_dir.to_str().expect("git path UTF-8"),
                "hash-object",
                "-t",
                "commit",
                "--literally",
                "-w",
                "--stdin",
            ],
            Some(format!("tree {}\n\nmalformed\n", tree.trim()).as_bytes()),
        ))
        .expect("commit id UTF-8");
        let base = dir.path().join("malformed-pack");
        let pack_hash = String::from_utf8(git(
            &[
                "--git-dir",
                git_dir.to_str().expect("git path UTF-8"),
                "pack-objects",
                base.to_str().expect("pack base UTF-8"),
            ],
            Some(commit.as_bytes()),
        ))
        .expect("pack id UTF-8");
        let source = dir
            .path()
            .join(format!("malformed-pack-{}.pack", pack_hash.trim()));

        let error = install_pack_file_from_path(
            &dir.path().join("installed"),
            &source,
            "malformed",
            0,
            true,
        )
        .expect_err("fsck must reject malformed commit");

        assert!(matches!(error, PackError::ObjectFsckFailed { .. }));
    }

    #[test]
    fn copy_pack_to_install_temp_places_copy_in_pack_dir() {
        let source_dir = tempfile::tempdir().expect("source tempdir");
        let pack_dir = tempfile::tempdir().expect("pack tempdir");
        let source = source_dir.path().join("input.pack");
        std::fs::write(&source, b"pack bytes").expect("write source pack");

        let copied = copy_pack_to_install_temp(pack_dir.path(), &source).expect("copy pack");

        assert_eq!(copied.path().parent(), Some(pack_dir.path()));
        assert_eq!(
            std::fs::read(copied.path()).expect("read copied pack"),
            b"pack bytes"
        );
        assert_eq!(
            std::fs::read(&source).expect("read source pack"),
            b"pack bytes"
        );
    }
}
