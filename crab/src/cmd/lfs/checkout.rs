//! `crab lfs checkout` — replace LFS pointers with actual file content.
//!
//! Reads LFS pointer files in the working tree and replaces them with the
//! actual content from the local LFS cache.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::core::error::{CrabError, Result};
use crate::lfs::batch::PatternFilter;
use crab_git::lfs_pointer::{LfsPointer, MAX_LFS_POINTER_SIZE, hex_encode};
use crab_git::pointer_detect::{PointerKind, classify};

#[derive(Debug, Clone, Default)]
pub struct LfsCheckoutOptions {
    pub paths: Vec<String>,
    pub to: Option<String>,
    pub base: bool,
    pub ours: bool,
    pub theirs: bool,
}

/// Run `crab lfs checkout` with optional path and `--to` flags.
///
/// Without arguments, replaces all LFS pointers in the working tree.
/// With a path, replaces only that file. With `--to`, writes the resolved
/// content to the specified output path.
pub fn run_lfs_checkout(options: LfsCheckoutOptions) -> Result<()> {
    let stage = checkout_stage(&options)?;
    let repo_root = std::env::current_dir()?;

    if let Some(stage) = stage {
        return match (options.paths.as_slice(), options.to.as_deref()) {
            ([file_path], Some(output_path)) => checkout_conflict(
                &repo_root,
                Path::new(file_path),
                Path::new(output_path),
                stage,
            ),
            _ => Err(CrabError::Configuration {
                key: "checkout".to_owned(),
                origin: "--to requires exactly one Git LFS object file path".to_owned(),
            }),
        };
    }

    match (options.paths.as_slice(), options.to.as_deref()) {
        (paths @ [_, ..], None) => {
            checkout_paths(&repo_root, paths)?;
            refresh_index(&repo_root)
        }
        ([], None) => {
            checkout_all(&repo_root)?;
            refresh_index(&repo_root)
        }
        (_, Some(_)) => Err(CrabError::Configuration {
            key: "checkout".to_owned(),
            origin: "--to and exactly one of --theirs, --ours, and --base must be used together"
                .to_owned(),
        }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConflictStage {
    Base,
    Ours,
    Theirs,
}

impl ConflictStage {
    fn index_number(self) -> u8 {
        match self {
            ConflictStage::Base => 1,
            ConflictStage::Ours => 2,
            ConflictStage::Theirs => 3,
        }
    }
}

fn checkout_stage(options: &LfsCheckoutOptions) -> Result<Option<ConflictStage>> {
    let stages = [
        (options.base, ConflictStage::Base),
        (options.ours, ConflictStage::Ours),
        (options.theirs, ConflictStage::Theirs),
    ];
    let selected: Vec<_> = stages
        .into_iter()
        .filter_map(|(enabled, stage)| enabled.then_some(stage))
        .collect();

    if selected.len() > 1 {
        return Err(CrabError::Configuration {
            key: "checkout".to_owned(),
            origin: "at most one of --base, --theirs, and --ours is allowed".to_owned(),
        });
    }
    if (selected.len() == 1) != options.to.is_some() {
        return Err(CrabError::Configuration {
            key: "checkout".to_owned(),
            origin: "--to and exactly one of --theirs, --ours, and --base must be used together"
                .to_owned(),
        });
    }

    Ok(selected.first().copied())
}

/// Resolve LFS content for a given pointer from the local cache.
fn resolve_lfs_content(repo_root: &Path, pointer: &LfsPointer) -> Result<Option<Vec<u8>>> {
    let lfs_dir = crate::lfs::config::LfsConfig::resolve_storage_dir(repo_root)?;
    crate::lfs::cache::read_pointer(&lfs_dir, pointer)
}

fn require_lfs_content(repo_root: &Path, pointer: &LfsPointer) -> Result<Vec<u8>> {
    let oid_hex = hex_encode(&pointer.oid);
    resolve_lfs_content(repo_root, pointer)?.ok_or(CrabError::Configuration {
        key: oid_hex,
        origin: "LFS object is not available in the local cache; run `crab lfs fetch` or `crab lfs pull` first".to_owned(),
    })
}

fn skip_missing_lfs_content(file_path: &Path) {
    eprintln!(
        "Skipped checkout for \"{}\", content not local. Use fetch to download.",
        file_path.display()
    );
}

fn checkout_paths(repo_root: &Path, paths: &[String]) -> Result<u64> {
    if paths.iter().all(|path| !has_glob_metachar(path)) {
        let mut count = 0u64;
        for path in paths {
            if checkout_file(repo_root, Path::new(path))? {
                count += 1;
            }
        }
        return Ok(count);
    }

    checkout_globs(repo_root, paths)
}

fn checkout_globs(repo_root: &Path, patterns: &[String]) -> Result<u64> {
    let classifier = LfsCheckoutClassifier::open(repo_root)?;
    if classifier.is_empty() {
        eprintln!("no LFS-tracked patterns found");
        return Ok(0);
    }

    let filters: Vec<PatternFilter> = patterns
        .iter()
        .map(|pattern| PatternFilter::new(pattern))
        .collect::<Result<_>>()?;
    let files = git_ls_files(repo_root)?;
    let mut count = 0u64;

    for file in files {
        if !classifier.is_tracked(&file) {
            continue;
        }
        if !filters.iter().any(|filter| filter.matches(&file)) {
            continue;
        }
        if checkout_file(repo_root, Path::new(&file))? {
            count += 1;
        }
    }

    eprintln!("checkout: matched {count} file(s)");
    Ok(count)
}

fn has_glob_metachar(path: &str) -> bool {
    path.bytes().any(|b| matches!(b, b'*' | b'?' | b'[' | b']'))
}

/// Check out a single LFS-tracked file.
fn checkout_file(repo_root: &Path, file_path: &Path) -> Result<bool> {
    let full_path = repo_root.join(file_path);
    let repo_path = current_to_repo_path(repo_root, file_path)?;
    let Some(pointer) = index_lfs_pointer(repo_root, &repo_path)? else {
        eprintln!(
            "{}: not an LFS pointer in the index, skipping",
            file_path.display()
        );
        return Ok(false);
    };

    if !full_path.exists() {
        let Some(resolved) = resolve_lfs_content(repo_root, &pointer)? else {
            skip_missing_lfs_content(file_path);
            return Ok(false);
        };
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).map_err(CrabError::Io)?;
        }
        fs::write(&full_path, &resolved).map_err(CrabError::Io)?;
        eprintln!("checkout {}: {} bytes", file_path.display(), resolved.len());
        return Ok(true);
    }

    let content = fs::read(&full_path).map_err(|e| CrabError::Configuration {
        key: file_path.display().to_string(),
        origin: format!("failed to read file: {e}"),
    })?;

    if content.len() > MAX_LFS_POINTER_SIZE {
        eprintln!(
            "{}: not an LFS pointer (file too large)",
            file_path.display()
        );
        return Ok(false);
    }

    match classify(&content) {
        PointerKind::Lfs(worktree_pointer) if worktree_pointer.oid == pointer.oid => {
            let Some(resolved) = resolve_lfs_content(repo_root, &worktree_pointer)? else {
                skip_missing_lfs_content(file_path);
                return Ok(false);
            };
            fs::write(&full_path, &resolved).map_err(CrabError::Io)?;
            eprintln!("checkout {}: {} bytes", file_path.display(), resolved.len(),);
            Ok(true)
        }
        PointerKind::Lfs(_) => {
            eprintln!("{}: modified pointer, skipping", file_path.display());
            Ok(false)
        }
        _ => {
            eprintln!("{}: not an LFS pointer, skipping", file_path.display());
            Ok(false)
        }
    }
}

/// Check out a selected unmerged index stage to a separate output path.
fn checkout_conflict(
    repo_root: &Path,
    file_path: &Path,
    output_path: &Path,
    stage: ConflictStage,
) -> Result<()> {
    let repo_path = current_to_repo_path(repo_root, file_path)?;
    let content = read_index_stage(repo_root, &repo_path, stage)?;

    if content.len() > MAX_LFS_POINTER_SIZE {
        return Err(CrabError::Configuration {
            key: repo_path,
            origin: "conflict stage is not an LFS pointer (too large)".to_owned(),
        });
    }

    if let PointerKind::Lfs(pointer) = classify(&content) {
        let resolved = require_lfs_content(repo_root, &pointer)?;
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(CrabError::Io)?;
        }
        fs::write(output_path, &resolved).map_err(CrabError::Io)?;
        eprintln!(
            "checkout --to {}: {} bytes",
            output_path.display(),
            resolved.len(),
        );
        return Ok(());
    }

    Err(CrabError::Configuration {
        key: repo_path,
        origin: "conflict stage is not an LFS pointer".to_owned(),
    })
}

fn read_index_stage(repo_root: &Path, repo_path: &str, stage: ConflictStage) -> Result<Vec<u8>> {
    let refspec = format!(":{}:{repo_path}", stage.index_number());
    let output = Command::new("git")
        .args(["cat-file", "-p", &refspec])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Configuration {
            key: "git cat-file".to_owned(),
            origin: format!("failed to read conflict stage: {e}"),
        })?;

    if output.status.success() {
        return Ok(output.stdout);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(CrabError::Configuration {
        key: "checkout".to_owned(),
        origin: format!("Could not checkout (are you not in the middle of a merge?): {stderr}"),
    })
}

fn index_lfs_pointer(repo_root: &Path, repo_path: &str) -> Result<Option<LfsPointer>> {
    let refspec = format!(":{repo_path}");
    let output = Command::new("git")
        .args(["cat-file", "-p", &refspec])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Configuration {
            key: "git cat-file".to_owned(),
            origin: format!("failed to read index entry: {e}"),
        })?;

    if !output.status.success() || output.stdout.len() > MAX_LFS_POINTER_SIZE {
        return Ok(None);
    }

    match classify(&output.stdout) {
        PointerKind::Lfs(pointer) => Ok(Some(pointer)),
        _ => Ok(None),
    }
}

fn current_to_repo_path(repo_root: &Path, file_path: &Path) -> Result<String> {
    let relative = if file_path.is_absolute() {
        let worktree_root = git_rev_parse(repo_root, "--show-toplevel")?;
        file_path
            .strip_prefix(Path::new(&worktree_root))
            .map(Path::to_path_buf)
            .map_err(|_| CrabError::Configuration {
                key: file_path.display().to_string(),
                origin: "path is outside the current worktree".to_owned(),
            })?
    } else {
        let prefix = git_rev_parse(repo_root, "--show-prefix")?;
        PathBuf::from(prefix).join(file_path)
    };

    let normalized = normalize_repo_relative_path(&relative)?;
    repo_path_string(&normalized)
}

fn git_rev_parse(repo_root: &Path, arg: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", arg])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Configuration {
            key: "git rev-parse".to_owned(),
            origin: format!("failed to run git rev-parse {arg}: {e}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(CrabError::Configuration {
            key: "git rev-parse".to_owned(),
            origin: stderr,
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn normalize_repo_relative_path(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(CrabError::Configuration {
                        key: path.display().to_string(),
                        origin: "path escapes the current worktree".to_owned(),
                    });
                }
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(CrabError::Configuration {
                    key: path.display().to_string(),
                    origin: "path must be relative to the current worktree".to_owned(),
                });
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err(CrabError::Configuration {
            key: path.display().to_string(),
            origin: "path must not be empty".to_owned(),
        });
    }

    Ok(normalized)
}

fn repo_path_string(path: &Path) -> Result<String> {
    let Some(path) = path.to_str() else {
        return Err(CrabError::Configuration {
            key: path.display().to_string(),
            origin: "path is not valid UTF-8".to_owned(),
        });
    };
    Ok(path.replace(std::path::MAIN_SEPARATOR, "/"))
}

/// Check out all LFS pointers in the working tree.
fn checkout_all(repo_root: &Path) -> Result<()> {
    let classifier = LfsCheckoutClassifier::open(repo_root)?;
    if classifier.is_empty() {
        eprintln!("no LFS-tracked patterns found");
        return Ok(());
    }

    let file_list = git_ls_files(repo_root)?;
    let mut count = 0u64;

    for file in file_list {
        if !classifier.is_tracked(&file) {
            continue;
        }

        match checkout_file(repo_root, Path::new(&file)) {
            Ok(true) => count += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(path = %file, error = %e, "failed to checkout LFS content");
            }
        }
    }

    eprintln!("checkout: replaced {count} LFS pointer(s) with content");
    Ok(())
}

/// Refresh Git's stat cache after replacing pointer files with content.
/// Git may report unrelated dirty paths with a non-zero status even though
/// it performed the refresh, so only process-launch failures are fatal.
fn refresh_index(repo_root: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["update-index", "-q", "--refresh"])
        .current_dir(repo_root)
        .output()
        .map_err(|error| CrabError::Configuration {
            key: "git update-index".to_owned(),
            origin: format!("failed to refresh the index after LFS checkout: {error}"),
        })?;
    if !output.status.success() {
        tracing::debug!(
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "git update-index reported paths needing refresh"
        );
    }
    Ok(())
}

fn git_ls_files(repo_root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["ls-files"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| CrabError::Configuration {
            key: "git ls-files".to_owned(),
            origin: format!("failed to run git ls-files: {e}"),
        })?;

    if !output.status.success() {
        return Err(CrabError::Configuration {
            key: "git ls-files".to_owned(),
            origin: "git ls-files failed".to_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// Classifier that decides whether a path is LFS-tracked.
///
/// Under `gix-pathmatch`, this wraps the consolidated
/// [`core::attrs::TrackedClassifier`] (backed by `gix_attributes::Search`).
/// Otherwise it falls back to the hand-rolled pattern list + suffix matcher.
#[cfg(feature = "gix-pathmatch")]
struct LfsCheckoutClassifier(crate::core::attrs::TrackedClassifier);

#[cfg(not(feature = "gix-pathmatch"))]
struct LfsCheckoutClassifier {
    patterns: Vec<String>,
}

impl LfsCheckoutClassifier {
    fn open(repo_root: &Path) -> Result<Self> {
        #[cfg(feature = "gix-pathmatch")]
        {
            Ok(LfsCheckoutClassifier(
                crate::core::attrs::TrackedClassifier::open(repo_root, "lfs")?,
            ))
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            Ok(LfsCheckoutClassifier {
                patterns: crate::lfs::track::list(repo_root)?,
            })
        }
    }

    fn is_empty(&self) -> bool {
        #[cfg(feature = "gix-pathmatch")]
        {
            false
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            self.patterns.is_empty()
        }
    }

    fn is_tracked(&self, path: &str) -> bool {
        #[cfg(feature = "gix-pathmatch")]
        {
            self.0.is_tracked(path)
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            is_lfs_tracked_legacy(path, &self.patterns)
        }
    }
}

/// Check if a file path matches any of the tracked LFS patterns.
#[cfg(not(feature = "gix-pathmatch"))]
fn is_lfs_tracked_legacy(path: &str, patterns: &[String]) -> bool {
    for pattern in patterns {
        if let Some(suffix) = pattern.strip_prefix('*') {
            if path.ends_with(suffix) {
                return true;
            }
        } else if let Some(prefix) = pattern.strip_suffix("/**") {
            if path.starts_with(prefix) {
                return true;
            }
        } else if path == pattern {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Stdio;

    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    use crab_git::lfs_pointer::LfsPointer;

    #[test]
    fn has_glob_metachar_detects_patterns() {
        assert!(has_glob_metachar("assets/*.bin"));
        assert!(has_glob_metachar("assets/file?.bin"));
        assert!(has_glob_metachar("assets/[ab].bin"));
        assert!(!has_glob_metachar("assets/file.bin"));
    }

    #[test]
    fn checkout_options_reject_multiple_conflict_stages() {
        let options = LfsCheckoutOptions {
            base: true,
            ours: true,
            ..LfsCheckoutOptions::default()
        };

        assert!(checkout_stage(&options).is_err());
    }

    #[test]
    fn checkout_options_reject_to_without_conflict_stage() {
        let options = LfsCheckoutOptions {
            to: Some("ours.dat".to_owned()),
            paths: vec!["asset.bin".to_owned()],
            ..LfsCheckoutOptions::default()
        };

        assert!(checkout_stage(&options).is_err());
    }

    #[test]
    fn checkout_options_reject_conflict_stage_without_to() {
        let options = LfsCheckoutOptions {
            ours: true,
            paths: vec!["asset.bin".to_owned()],
            ..LfsCheckoutOptions::default()
        };

        assert!(checkout_stage(&options).is_err());
    }

    #[test]
    fn checkout_options_accept_conflict_stage_with_to() {
        let options = LfsCheckoutOptions {
            ours: true,
            to: Some("ours.dat".to_owned()),
            paths: vec!["asset.bin".to_owned()],
            ..LfsCheckoutOptions::default()
        };

        assert_eq!(checkout_stage(&options).unwrap(), Some(ConflictStage::Ours));
    }

    #[test]
    fn normalizes_repo_relative_paths() {
        let normalized = normalize_repo_relative_path(Path::new("dir/../asset.bin")).unwrap();
        assert_eq!(repo_path_string(&normalized).unwrap(), "asset.bin");
        assert!(normalize_repo_relative_path(Path::new("../asset.bin")).is_err());
    }

    #[test]
    fn checkout_conflict_resolves_selected_stage_to_output() {
        let repo = temp_git_repo();
        let base_pointer = write_lfs_pointer_blob(repo.path(), b"base\n");
        let ours_pointer = write_lfs_pointer_blob(repo.path(), b"ours\n");
        let theirs_pointer = write_lfs_pointer_blob(repo.path(), b"theirs\n");
        write_conflict_index(
            repo.path(),
            "asset.bin",
            &base_pointer.blob_oid,
            &ours_pointer.blob_oid,
            &theirs_pointer.blob_oid,
        );
        let output = repo.path().join("ours.dat");

        checkout_conflict(
            repo.path(),
            Path::new("asset.bin"),
            &output,
            ConflictStage::Ours,
        )
        .unwrap();

        assert_eq!(fs::read(output).unwrap(), b"ours\n");
    }

    #[test]
    fn checkout_file_restores_missing_file_from_index() {
        let repo = temp_git_repo();
        let pointer = write_lfs_pointer_blob(repo.path(), b"content\n");
        write_index_entry(repo.path(), "asset.bin", &pointer.blob_oid);

        let checked_out = checkout_file(repo.path(), Path::new("asset.bin")).unwrap();

        assert!(checked_out);
        assert_eq!(
            fs::read(repo.path().join("asset.bin")).unwrap(),
            b"content\n"
        );
    }

    #[test]
    fn checkout_file_does_not_overwrite_materialized_content() {
        let repo = temp_git_repo();
        let pointer = write_lfs_pointer_blob(repo.path(), b"content\n");
        write_index_entry(repo.path(), "asset.bin", &pointer.blob_oid);
        fs::write(repo.path().join("asset.bin"), b"edited\n").unwrap();

        let checked_out = checkout_file(repo.path(), Path::new("asset.bin")).unwrap();

        assert!(!checked_out);
        assert_eq!(
            fs::read(repo.path().join("asset.bin")).unwrap(),
            b"edited\n"
        );
    }

    #[test]
    fn checkout_file_skips_missing_local_content() {
        let repo = temp_git_repo();
        let pointer = write_lfs_pointer_blob(repo.path(), b"content\n");
        write_index_entry(repo.path(), "asset.bin", &pointer.blob_oid);
        fs::remove_file(local_lfs_object_path(repo.path(), &pointer.oid_hex)).unwrap();
        fs::write(repo.path().join("asset.bin"), &pointer.bytes).unwrap();

        let checked_out = checkout_file(repo.path(), Path::new("asset.bin")).unwrap();

        assert!(!checked_out);
        assert_eq!(
            fs::read(repo.path().join("asset.bin")).unwrap(),
            pointer.bytes
        );
    }

    #[test]
    fn checkout_file_rejects_corrupt_local_content() {
        let repo = temp_git_repo();
        let pointer = write_lfs_pointer_blob(repo.path(), b"content\n");
        write_index_entry(repo.path(), "asset.bin", &pointer.blob_oid);
        fs::write(
            local_lfs_object_path(repo.path(), &pointer.oid_hex),
            b"corrupt\n",
        )
        .unwrap();
        fs::write(repo.path().join("asset.bin"), &pointer.bytes).unwrap();

        let error = checkout_file(repo.path(), Path::new("asset.bin")).unwrap_err();

        assert!(matches!(error, CrabError::LfsObjectCorrupt { .. }));
        assert_eq!(
            fs::read(repo.path().join("asset.bin")).unwrap(),
            pointer.bytes
        );
    }

    struct PointerBlob {
        blob_oid: String,
        oid_hex: String,
        bytes: Vec<u8>,
    }

    fn temp_git_repo() -> TempDir {
        let repo = tempfile::tempdir().unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .unwrap();
        assert!(status.success());
        repo
    }

    fn write_lfs_pointer_blob(repo_root: &Path, content: &[u8]) -> PointerBlob {
        let oid: [u8; 32] = Sha256::digest(content).into();
        let oid_hex = hex_encode(&oid);
        let pointer = LfsPointer {
            oid,
            size: content.len() as u64,
            extensions: Vec::new(),
        }
        .serialize();
        let local_path = local_lfs_object_path(repo_root, &oid_hex);
        fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        fs::write(local_path, content).unwrap();

        let pointer_path = repo_root.join(format!("{oid_hex}.pointer"));
        fs::write(&pointer_path, &pointer).unwrap();
        let output = Command::new("git")
            .args(["hash-object", "-w"])
            .arg(&pointer_path)
            .current_dir(repo_root)
            .output()
            .unwrap();
        assert!(output.status.success());

        PointerBlob {
            blob_oid: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            oid_hex,
            bytes: pointer,
        }
    }

    fn local_lfs_object_path(repo_root: &Path, oid_hex: &str) -> PathBuf {
        repo_root
            .join(".git")
            .join("lfs")
            .join("objects")
            .join(&oid_hex[..2])
            .join(&oid_hex[2..4])
            .join(oid_hex)
    }

    fn write_index_entry(repo_root: &Path, path: &str, blob_oid: &str) {
        let mut child = Command::new("git")
            .args(["update-index", "--index-info"])
            .current_dir(repo_root)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let input = format!("100644 {blob_oid}\t{path}\n");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let status = child.wait().unwrap();
        assert!(status.success());
    }

    fn write_conflict_index(
        repo_root: &Path,
        path: &str,
        base_oid: &str,
        ours_oid: &str,
        theirs_oid: &str,
    ) {
        let mut child = Command::new("git")
            .args(["update-index", "--index-info"])
            .current_dir(repo_root)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let input = format!(
            "100644 {base_oid} 1\t{path}\n100644 {ours_oid} 2\t{path}\n100644 {theirs_oid} 3\t{path}\n"
        );
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let status = child.wait().unwrap();
        assert!(status.success());
    }
}
