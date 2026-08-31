//! Git pack-format helpers.
//!
//! This Module owns low-level Git pack wire-format validation. Fetch pipelines,
//! object-store transport, and CLI error presentation live above this Interface.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha1::{Digest, Sha1};

/// SHA-1 trailer length in Git pack files.
pub const PACK_SHA1_LEN: usize = 20;

/// Checksums computed while a complete immutable pack is streamed.
///
/// A caller may carry this identity into a later local install to avoid
/// reading the same pack body again. The source bytes must remain private and
/// unchanged between verification and install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedPackIdentity {
    /// Git's SHA-1 over the pack header and entries, excluding the trailer.
    pub git_sha1: [u8; PACK_SHA1_LEN],
    /// Crab's Blake3 content identity over the complete pack, including trailer.
    pub content_hash: [u8; 32],
}

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

    /// A temporary bare Git object database could not be initialized.
    #[error("failed to initialize bare git object database {path}: {detail}")]
    BareRepositoryInit { path: PathBuf, detail: String },

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

    /// A pack sidecar exceeded the bound implied by its object count.
    #[error("git pack sidecar {path} is too large: {size} bytes exceeds limit {limit}")]
    SidecarTooLarge {
        /// Sidecar path that exceeded its bound.
        path: PathBuf,
        /// Observed sidecar size.
        size: u64,
        /// Maximum permitted sidecar size.
        limit: u64,
    },

    /// The object-count-derived sidecar bound could not be represented.
    #[error("git pack sidecar size bound overflowed for {path}")]
    SidecarSizeOverflow {
        /// Sidecar path whose bound overflowed.
        path: PathBuf,
    },

    /// The pack content did not match its canonical storage identifier.
    #[error("git pack content hash mismatch: expected {expected}, computed {computed}")]
    ContentHashMismatch {
        /// Canonical content identifier supplied by the caller.
        expected: String,
        /// Content identifier computed from the pack bytes.
        computed: String,
    },

    /// Git could not return validated object kinds for a local object database.
    #[error("git object-kind query failed for {path}: {detail}")]
    ObjectKindQuery { path: PathBuf, detail: String },
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

/// Initialize a directory as an isolated bare Git object database.
///
/// Callers that install packs into a temporary directory must initialize the
/// repository metadata before invoking Git commands such as `cat-file`.
///
/// # Errors
///
/// Returns [`PackError::Io`] when Git cannot be started or
/// [`PackError::BareRepositoryInit`] when Git rejects the directory.
pub fn initialize_bare_git_dir(path: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["init", "--bare", "--quiet"])
        .arg(path)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .map_err(|source| {
            io_error(
                format!("initialize {} as a bare git repository", path.display()),
                source,
            )
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(PackError::BareRepositoryInit {
        path: path.to_owned(),
        detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
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
    install_pack_file_from_path_with_identity(
        pack_dir,
        pack_tmp_path,
        canonical_name,
        max_input_size,
        fsck_objects,
        None,
    )
}

pub(crate) fn install_pack_file_from_path_with_identity(
    pack_dir: &Path,
    pack_tmp_path: &Path,
    canonical_name: &str,
    max_input_size: u64,
    fsck_objects: bool,
    verified_identity: Option<VerifiedPackIdentity>,
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
    if let Some(identity) = verified_identity {
        let computed_content_hash = blake3::Hash::from_bytes(identity.content_hash)
            .to_hex()
            .to_string();
        if !computed_content_hash.eq_ignore_ascii_case(canonical_name) {
            return Err(PackError::ContentHashMismatch {
                expected: canonical_name.to_owned(),
                computed: computed_content_hash,
            });
        }
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
            git_sha1: verified_identity.map_or_else(
                || verify_pack_file_sha1(pack_tmp_path),
                |identity| Ok(to_hex(&identity.git_sha1)),
            )?,
            pack_path: final_pack,
            idx_path: final_idx,
            rev_path: final_rev,
        });
    }

    std::fs::create_dir_all(pack_dir)
        .map_err(|source| io_error(format!("create {}", pack_dir.display()), source))?;

    let git_sha1 = verified_identity.map_or_else(
        || verify_pack_file_sha1(pack_tmp_path),
        |identity| Ok(to_hex(&identity.git_sha1)),
    )?;
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

/// Install a pack together with its already-verified Git index sidecars.
///
/// The pack body is content-hash and trailer validated once. The supplied
/// index and reverse-index are checked against the pack before immutable
/// hard-link-or-copy staging and atomic per-file installation. This avoids
/// re-running `git index-pack` when a committed Crab inventory already
/// contains both sidecars.
///
/// # Errors
///
/// Returns [`PackError::SidecarTooLarge`] when either sidecar exceeds the
/// object-count-derived bound, [`PackError::SidecarSizeOverflow`] when that
/// bound cannot be represented, or a validation error when the sidecars do not
/// describe the supplied pack.
pub fn install_pack_files_from_paths(
    pack_dir: &Path,
    pack_tmp_path: &Path,
    index_tmp_path: &Path,
    reverse_index_tmp_path: &Path,
    canonical_name: &str,
    max_input_size: u64,
    expected_object_count: u64,
) -> Result<InstalledPack> {
    install_pack_files_from_paths_with_identity(
        pack_dir,
        pack_tmp_path,
        index_tmp_path,
        reverse_index_tmp_path,
        canonical_name,
        max_input_size,
        expected_object_count,
        None,
    )
}

pub(crate) fn install_pack_files_from_paths_with_identity(
    pack_dir: &Path,
    pack_tmp_path: &Path,
    index_tmp_path: &Path,
    reverse_index_tmp_path: &Path,
    canonical_name: &str,
    max_input_size: u64,
    expected_object_count: u64,
    verified_identity: Option<VerifiedPackIdentity>,
) -> Result<InstalledPack> {
    let pack_size = std::fs::metadata(pack_tmp_path)
        .map_err(|source| io_error(format!("metadata {}", pack_tmp_path.display()), source))?
        .len();
    if max_input_size > 0 && pack_size > max_input_size {
        return Err(PackError::PackTooLarge {
            size: pack_size,
            limit: max_input_size,
        });
    }
    if !valid_canonical_pack_name(canonical_name) {
        return Err(PackError::InvalidCanonicalName {
            name: canonical_name.to_owned(),
        });
    }

    let (git_sha1, content_hash, verified_size) = match verified_identity {
        Some(identity) => (to_hex(&identity.git_sha1), identity.content_hash, pack_size),
        None => verify_and_hash_pack_file(pack_tmp_path)?,
    };
    if verified_size != pack_size {
        return Err(PackError::InvalidPackFile {
            path: pack_tmp_path.to_owned(),
            reason: "pack changed while its size was being verified".to_owned(),
        });
    }
    let computed_content_hash = blake3::Hash::from_bytes(content_hash).to_hex().to_string();
    if !computed_content_hash.eq_ignore_ascii_case(canonical_name) {
        return Err(PackError::ContentHashMismatch {
            expected: canonical_name.to_owned(),
            computed: computed_content_hash,
        });
    }

    let index_limit =
        crate::pack_locator::max_pack_index_size(expected_object_count).ok_or_else(|| {
            PackError::SidecarSizeOverflow {
                path: index_tmp_path.to_owned(),
            }
        })?;
    let index_size = std::fs::metadata(index_tmp_path)
        .map_err(|source| io_error(format!("metadata {}", index_tmp_path.display()), source))?
        .len();
    if index_size > index_limit {
        return Err(PackError::SidecarTooLarge {
            path: index_tmp_path.to_owned(),
            size: index_size,
            limit: index_limit,
        });
    }
    let reverse_limit = crate::pack_locator::pack_reverse_index_size(expected_object_count)
        .ok_or_else(|| PackError::SidecarSizeOverflow {
            path: reverse_index_tmp_path.to_owned(),
        })?;
    let reverse_index_size = std::fs::metadata(reverse_index_tmp_path)
        .map_err(|source| {
            io_error(
                format!("metadata {}", reverse_index_tmp_path.display()),
                source,
            )
        })?
        .len();
    if reverse_index_size > reverse_limit {
        return Err(PackError::SidecarTooLarge {
            path: reverse_index_tmp_path.to_owned(),
            size: reverse_index_size,
            limit: reverse_limit,
        });
    }

    let locations = crate::pack_locator::PackLocationIter::open(
        index_tmp_path,
        reverse_index_tmp_path,
        pack_size,
    )?;
    if locations.object_count() != expected_object_count {
        return Err(PackError::InvalidPackFile {
            path: index_tmp_path.to_owned(),
            reason: format!(
                "index has {} objects but caller expects {expected_object_count}",
                locations.object_count()
            ),
        });
    }
    let indexed_sha1 = locations.pack_checksum().to_string();
    if indexed_sha1 != git_sha1 {
        return Err(PackError::PackHashMismatch {
            trailer: git_sha1,
            index: indexed_sha1,
        });
    }

    std::fs::create_dir_all(pack_dir)
        .map_err(|source| io_error(format!("create {}", pack_dir.display()), source))?;
    let final_pack = pack_dir.join(format!("pack-{canonical_name}.pack"));
    let final_idx = pack_dir.join(format!("pack-{canonical_name}.idx"));
    let final_rev = pack_dir.join(format!("pack-{canonical_name}.rev"));
    if final_pack.exists() || final_idx.exists() || final_rev.exists() {
        return Err(PackError::InvalidPackFile {
            path: final_pack,
            reason: "pack installation destination already exists".to_owned(),
        });
    }

    let staged_pack = stage_source_artifact(pack_dir, pack_tmp_path, ".pack")?;
    let staged_idx = stage_source_artifact(pack_dir, index_tmp_path, ".idx")?;
    let staged_rev = stage_source_artifact(pack_dir, reverse_index_tmp_path, ".rev")?;
    let staged_pack_path: &Path = staged_pack.as_ref();
    let staged_idx_path: &Path = staged_idx.as_ref();
    let staged_rev_path: &Path = staged_rev.as_ref();
    if let Err(source) = std::fs::rename(staged_idx_path, &final_idx) {
        return Err(io_error(
            format!("rename {}", staged_idx_path.display()),
            source,
        ));
    }
    if let Err(source) = std::fs::rename(staged_rev_path, &final_rev) {
        let _ = std::fs::remove_file(&final_idx);
        return Err(io_error(
            format!("rename {}", staged_rev_path.display()),
            source,
        ));
    }
    if let Err(source) = std::fs::rename(staged_pack_path, &final_pack) {
        let _ = std::fs::remove_file(&final_idx);
        let _ = std::fs::remove_file(&final_rev);
        return Err(io_error(
            format!("rename {}", staged_pack_path.display()),
            source,
        ));
    }

    Ok(InstalledPack {
        git_sha1,
        pack_path: final_pack,
        idx_path: final_idx,
        rev_path: final_rev,
    })
}

fn stage_source_artifact(
    pack_dir: &Path,
    source: &Path,
    suffix: &str,
) -> Result<tempfile::TempPath> {
    let staged = tempfile::Builder::new()
        .prefix(".crab-pack-source-")
        .suffix(suffix)
        .tempfile_in(pack_dir)
        .map_err(|source_error| {
            io_error(
                format!("create source artifact tempfile in {}", pack_dir.display()),
                source_error,
            )
        })?;
    let staged_path = staged.into_temp_path();
    std::fs::remove_file(&staged_path).map_err(|source_error| {
        io_error(
            format!("prepare source artifact tempfile {}", staged_path.display()),
            source_error,
        )
    })?;
    match std::fs::hard_link(source, &staged_path) {
        Ok(()) => {}
        Err(source_error)
            if matches!(
                source_error.kind(),
                std::io::ErrorKind::CrossesDevices
                    | std::io::ErrorKind::Unsupported
                    | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            std::fs::copy(source, &staged_path).map_err(|copy_error| {
                io_error(
                    format!("copy source artifact {}", source.display()),
                    copy_error,
                )
            })?;
        }
        Err(source_error) => {
            return Err(io_error(
                format!("stage source artifact {}", source.display()),
                source_error,
            ));
        }
    }
    Ok(staged_path)
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
    // Git 2.30, the minimum supported version, predates `--rev-index`.
    // Crab writes its own reverse index below, so indexing must not depend
    // on that newer optional Git sidecar.
    index_pack.arg("index-pack");
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

pub(crate) fn verify_pack_file_sha1(path: &Path) -> Result<String> {
    verify_pack_file(path, false).map(|(git_sha1, _, _)| git_sha1)
}

pub(crate) fn verify_and_hash_pack_file(path: &Path) -> Result<(String, [u8; 32], u64)> {
    let (git_sha1, content_hash, size) = verify_pack_file(path, true)?;
    let content_hash = content_hash.ok_or_else(|| PackError::InvalidPackFile {
        path: path.to_owned(),
        reason: "pack content hash was not computed".to_owned(),
    })?;
    Ok((git_sha1, content_hash, size))
}

fn verify_pack_file(path: &Path, hash_content: bool) -> Result<(String, Option<[u8; 32]>, u64)> {
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
    let mut pack_hasher = Sha1::new();
    let mut content_hasher = hash_content.then(blake3::Hasher::new);
    let mut buf = [0u8; 1024 * 1024];
    while remaining > 0 {
        let read_len = remaining.min(buf.len() as u64) as usize;
        file.read_exact(&mut buf[..read_len])
            .map_err(|source| io_error(format!("read {}", path.display()), source))?;
        pack_hasher.update(&buf[..read_len]);
        if let Some(content_hasher) = &mut content_hasher {
            content_hasher.update(&buf[..read_len]);
        }
        remaining -= read_len as u64;
    }

    let mut expected = [0u8; PACK_SHA1_LEN];
    file.read_exact(&mut expected)
        .map_err(|source| io_error(format!("read {}", path.display()), source))?;
    if let Some(content_hasher) = &mut content_hasher {
        content_hasher.update(&expected);
    }
    let computed = pack_hasher.finalize();

    if computed.as_slice() != expected {
        return Err(PackError::Sha1Mismatch {
            expected: to_hex(&expected),
            computed: to_hex(computed.as_slice()),
        });
    }

    Ok((
        to_hex(&expected),
        content_hasher.map(|hasher| *hasher.finalize().as_bytes()),
        len,
    ))
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

/// Resolve the canonical kind of each requested object through a local Git
/// object database without materializing object bodies.
pub fn object_kinds_from_git_dir(
    git_dir: &Path,
    object_ids: &[gix_hash::ObjectId],
) -> Result<std::collections::HashMap<gix_hash::ObjectId, gix_object::Kind>> {
    use std::io::Write;
    use std::process::Stdio;

    if object_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let mut child = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .arg("cat-file")
        .arg("--batch-check=%(objectname) %(objecttype)")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| PackError::ObjectKindQuery {
            path: git_dir.to_owned(),
            detail: source.to_string(),
        })?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| PackError::ObjectKindQuery {
            path: git_dir.to_owned(),
            detail: "git cat-file did not expose piped stdin".to_owned(),
        })?;
    let requested = object_ids.to_owned();
    let (output, writer_result) = std::thread::scope(|scope| {
        let writer = scope.spawn(move || {
            for oid in &requested {
                writeln!(stdin, "{oid}")?;
            }
            stdin.flush()
        });
        let output = child.wait_with_output();
        (output, writer.join())
    });
    let output = output.map_err(|source| PackError::ObjectKindQuery {
        path: git_dir.to_owned(),
        detail: source.to_string(),
    })?;
    if !output.status.success() {
        return Err(PackError::ObjectKindQuery {
            path: git_dir.to_owned(),
            detail: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let writer_result = writer_result.map_err(|_| PackError::ObjectKindQuery {
        path: git_dir.to_owned(),
        detail: "git cat-file stdin writer thread panicked".to_owned(),
    })?;
    writer_result.map_err(|source| PackError::ObjectKindQuery {
        path: git_dir.to_owned(),
        detail: source.to_string(),
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = stdout.lines().collect::<Vec<_>>();
    if lines.len() != object_ids.len() {
        return Err(PackError::ObjectKindQuery {
            path: git_dir.to_owned(),
            detail: "git cat-file returned the wrong number of object rows".to_owned(),
        });
    }
    let mut kinds = std::collections::HashMap::with_capacity(object_ids.len());
    for (oid, line) in object_ids.iter().zip(lines) {
        let mut fields = line.split_whitespace();
        let returned = fields
            .next()
            .and_then(|value| gix_hash::ObjectId::from_hex(value.as_bytes()).ok());
        let kind = match fields.next() {
            Some("commit") => gix_object::Kind::Commit,
            Some("tree") => gix_object::Kind::Tree,
            Some("blob") => gix_object::Kind::Blob,
            Some("tag") => gix_object::Kind::Tag,
            Some("missing") => {
                return Err(PackError::ObjectKindQuery {
                    path: git_dir.to_owned(),
                    detail: format!("object {oid} is missing"),
                });
            }
            Some(other) => {
                return Err(PackError::ObjectKindQuery {
                    path: git_dir.to_owned(),
                    detail: format!("object {oid} returned unsupported kind {other:?}"),
                });
            }
            None => {
                return Err(PackError::ObjectKindQuery {
                    path: git_dir.to_owned(),
                    detail: format!("object {oid} returned a malformed response"),
                });
            }
        };
        if returned != Some(*oid) || fields.next().is_some() {
            return Err(PackError::ObjectKindQuery {
                path: git_dir.to_owned(),
                detail: format!("object {oid} returned a mismatched response"),
            });
        }
        kinds.insert(*oid, kind);
    }
    Ok(kinds)
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
    fn object_kinds_from_git_dir_returns_validated_kinds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git_dir = dir.path().join("source.git");
        git(
            &["init", "--bare", git_dir.to_str().expect("git path UTF-8")],
            None,
        );
        let blob = String::from_utf8(git(
            &[
                "--git-dir",
                git_dir.to_str().expect("git path UTF-8"),
                "hash-object",
                "-w",
                "--stdin",
            ],
            Some(b"catalogued blob\n"),
        ))
        .expect("blob object id UTF-8")
        .trim()
        .to_owned();
        let tree_input = format!("100644 blob {blob}\tfile.txt\n");
        let tree = String::from_utf8(git(
            &[
                "--git-dir",
                git_dir.to_str().expect("git path UTF-8"),
                "mktree",
            ],
            Some(tree_input.as_bytes()),
        ))
        .expect("tree object id UTF-8")
        .trim()
        .to_owned();
        let object_ids = [
            gix_hash::ObjectId::from_hex(blob.as_bytes()).expect("blob object id"),
            gix_hash::ObjectId::from_hex(tree.as_bytes()).expect("tree object id"),
        ];

        let kinds = object_kinds_from_git_dir(&git_dir, &object_ids).expect("object kinds");

        assert_eq!(kinds.get(&object_ids[0]), Some(&gix_object::Kind::Blob));
        assert_eq!(kinds.get(&object_ids[1]), Some(&gix_object::Kind::Tree));
    }

    #[test]
    fn object_kinds_from_git_dir_rejects_missing_objects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git_dir = dir.path().join("source.git");
        git(
            &["init", "--bare", git_dir.to_str().expect("git path UTF-8")],
            None,
        );
        let missing = gix_hash::ObjectId::from_hex(b"0000000000000000000000000000000000000000")
            .expect("missing object id");

        let error = object_kinds_from_git_dir(&git_dir, &[missing]).expect_err("missing object");

        assert!(matches!(error, PackError::ObjectKindQuery { .. }));
    }

    #[test]
    fn object_kinds_from_git_dir_reports_repository_diagnostics() {
        let dir = tempfile::tempdir().expect("tempdir");
        let object_id = gix_hash::ObjectId::from_hex(b"1111111111111111111111111111111111111111")
            .expect("object id");

        let error = object_kinds_from_git_dir(dir.path(), &[object_id])
            .expect_err("an uninitialized directory is not a Git repository");
        let PackError::ObjectKindQuery { detail, .. } = error else {
            panic!("expected object-kind query error");
        };

        assert!(detail.contains("not a git repository"), "{detail}");
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
    fn file_verification_computes_git_and_storage_hashes_in_one_pass() {
        let (_dir, idx_path, pack_hash) = pack_index_fixture();
        let pack_path = idx_path.with_extension("pack");
        let bytes = std::fs::read(&pack_path).expect("read fixture pack");
        let expected = (
            pack_hash,
            *blake3::hash(&bytes).as_bytes(),
            bytes.len() as u64,
        );

        assert_eq!(
            verify_and_hash_pack_file(&pack_path).expect("verify and hash pack"),
            expected
        );
    }

    #[test]
    fn indexed_pack_install_reuses_verified_sidecars() {
        let (dir, idx_path, _pack_hash) = pack_index_fixture();
        let pack_path = idx_path.with_extension("pack");
        let reverse_path = idx_path.with_extension("rev");
        crate::pack_locator::write_pack_reverse_index(&idx_path, &reverse_path)
            .expect("write fixture reverse index");
        let pack_size = std::fs::metadata(&pack_path)
            .expect("fixture pack metadata")
            .len();
        let locations =
            crate::pack_locator::PackLocationIter::open(&idx_path, &reverse_path, pack_size)
                .expect("open fixture indexes");
        let object_count = locations.object_count();
        let canonical_id = blake3::hash(&std::fs::read(&pack_path).expect("read fixture pack"))
            .to_hex()
            .to_string();
        let destination = dir.path().join("installed");

        let installed = install_pack_files_from_paths(
            &destination,
            &pack_path,
            &idx_path,
            &reverse_path,
            &canonical_id,
            pack_size,
            object_count,
        )
        .expect("install indexed pack");

        assert_eq!(
            std::fs::read(&installed.idx_path).expect("read installed index"),
            std::fs::read(&idx_path).expect("read source index")
        );
        assert_eq!(
            std::fs::read(&installed.rev_path).expect("read installed reverse index"),
            std::fs::read(&reverse_path).expect("read source reverse index")
        );
        assert_eq!(
            std::fs::read(&installed.pack_path).expect("read installed pack"),
            std::fs::read(&pack_path).expect("read source pack")
        );
    }

    #[test]
    fn indexed_pack_install_accepts_streamed_identity() {
        let (dir, idx_path, pack_hash) = pack_index_fixture();
        let pack_path = idx_path.with_extension("pack");
        let reverse_path = idx_path.with_extension("rev");
        crate::pack_locator::write_pack_reverse_index(&idx_path, &reverse_path)
            .expect("write fixture reverse index");
        let bytes = std::fs::read(&pack_path).expect("read fixture pack");
        let pack_size = bytes.len() as u64;
        let locations =
            crate::pack_locator::PackLocationIter::open(&idx_path, &reverse_path, pack_size)
                .expect("open fixture indexes");
        let git_sha1: [u8; PACK_SHA1_LEN] = locations
            .pack_checksum()
            .as_bytes()
            .try_into()
            .expect("fixture pack checksum is SHA-1");
        let identity = VerifiedPackIdentity {
            git_sha1,
            content_hash: *blake3::hash(&bytes).as_bytes(),
        };
        let destination = dir.path().join("installed-with-identity");

        let installed = install_pack_files_from_paths_with_identity(
            &destination,
            &pack_path,
            &idx_path,
            &reverse_path,
            &blake3::hash(&bytes).to_hex(),
            pack_size,
            locations.object_count(),
            Some(identity),
        )
        .expect("install indexed pack with streamed identity");

        assert_eq!(installed.git_sha1, pack_hash);
        assert_eq!(
            std::fs::read(&installed.pack_path).expect("read installed pack"),
            bytes
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
