//! Git stat-cache updates after verified worktree publication.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bstr::ByteSlice;

use crate::facade::{FacadeError, Result};

/// Expected index content supplied as small bytes or an already verified ID.
pub enum ExpectedBlob {
    /// Hash a small captured blob using the index's object format.
    Bytes(Vec<u8>),
    /// Reuse a verified identity without retaining a complete large blob.
    Id(gix::hash::ObjectId),
}

/// A captured verified stat; the optional blob fences concurrent index changes.
pub struct IndexStatUpdate {
    pub path: PathBuf,
    pub stat: gix::index::entry::Stat,
    pub expected_blob: Option<ExpectedBlob>,
}

impl IndexStatUpdate {
    /// Capture a verified output before publication, retaining its original stat.
    pub fn from_file(path: PathBuf, file: &std::fs::File, blob: &[u8]) -> Result<Self> {
        Self::capture(path, file, ExpectedBlob::Bytes(blob.into()))
    }

    /// Capture output stat fenced by a verified hexadecimal Git blob identity.
    pub fn from_file_oid(path: PathBuf, file: &std::fs::File, oid: &str) -> Result<Self> {
        let oid = gix::hash::ObjectId::from_hex(oid.as_bytes())
            .map_err(|error| operation("parse verified blob identity", error))?;
        Self::capture(path, file, ExpectedBlob::Id(oid))
    }

    fn capture(path: PathBuf, file: &std::fs::File, expected_blob: ExpectedBlob) -> Result<Self> {
        let metadata = gix::index::fs::Metadata::from_file(file)
            .map_err(|error| operation("read published file metadata", error))?;
        let stat = gix::index::entry::Stat::from_fs(&metadata)
            .map_err(|error| operation("read published file stat", error))?;
        Ok(Self {
            path,
            stat,
            expected_blob: Some(expected_blob),
        })
    }
}

/// Refresh verified paths, preserving object IDs, modes and sibling staging.
///
/// Callers must retain the stat captured during verification, rather than
/// capturing a later external edit. Paths are worktree-relative. A changed
/// expected blob is skipped; index lock/read/write failures retain their source.
pub fn refresh(root: &Path, updates: &[IndexStatUpdate]) -> Result<usize> {
    if updates.is_empty() {
        return Ok(0);
    }
    let repo = crate::facade::open_at(root)?;
    let index_path = repo.index_path();
    let lock = gix::lock::File::acquire_to_update_resource(
        &index_path,
        gix::lock::acquire::Fail::Immediately,
        None,
    )
    .map_err(|error| operation("lock Git index stat cache", error))?;
    let mut index = repo
        .open_index()
        .map_err(|error| operation("read Git index stat cache", error))?;
    let timestamp = index.timestamp();
    let updates: HashMap<_, _> = updates
        .iter()
        .map(|update| (path_bytes(&update.path), update))
        .collect();
    let mut updated = 0;
    for (entry, path) in index.entries_mut_with_paths() {
        if entry.stage() != gix::index::entry::Stage::Unconflicted
            || !matches!(
                entry.mode,
                gix::index::entry::Mode::FILE | gix::index::entry::Mode::FILE_EXECUTABLE
            )
        {
            continue;
        }
        // Rewriting advances the index timestamp. Unverified entries that alias
        // its old timestamp must retain Git's zero-size dirty-check marker, or
        // same-size sibling edits can become falsely clean after publication.
        if entry.stat.is_racy(timestamp, Default::default()) {
            entry.stat.size = 0;
        }
        let Some(update) = updates.get(path.as_bytes()) else {
            continue;
        };
        if let Some(expected) = &update.expected_blob {
            let id = match expected {
                ExpectedBlob::Bytes(bytes) => {
                    gix::objs::compute_hash(repo.object_hash(), gix::objs::Kind::Blob, bytes)
                        .map_err(|error| operation("hash expected blob for stat refresh", error))?
                }
                ExpectedBlob::Id(id) => *id,
            };
            if entry.id != id {
                continue;
            }
        }
        entry.stat = update.stat;
        updated += 1;
    }
    if updated == 0 {
        return Ok(0);
    }
    let mut writer = std::io::BufWriter::with_capacity(64 * 1024, lock);
    index
        .write_to(&mut writer, gix::index::write::Options::default())
        .map_err(|error| operation("write Git index stat cache", error))?;
    let lock = writer
        .into_inner()
        .map_err(|error| operation("flush Git index stat cache", error.into_error()))?;
    lock.commit()
        .map_err(|error| operation("commit Git index stat cache", error))?;
    Ok(updated)
}

fn operation(
    error_context: &str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> FacadeError {
    FacadeError::Operation {
        operation: error_context.into(),
        source: Box::new(source),
    }
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().into()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Command;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("--literal-pathspecs")
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", root.join("missing-global"))
            .current_dir(root)
            .output()
            .expect("Git runs");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("Git output")
    }

    fn repository() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("repository");
        let root = directory.path();
        git(root, &["init", "--quiet"]);
        git(root, &["config", "user.name", "Fixture"]);
        git(root, &["config", "user.email", "test@example.invalid"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("other.txt"), "original").expect("sibling");
        git(root, &["add", "--", "other.txt"]);
        git(root, &["commit", "--quiet", "-m", "fixture"]);
        directory
    }

    #[test]
    fn publication_refresh_clears_filtered_status_and_keeps_sibling_staging() {
        let directory = repository();
        let root = directory.path();
        git(root, &["config", "filter.demo.clean", "printf pointer"]);
        git(root, &["config", "filter.demo.required", "true"]);
        std::fs::write(root.join(".gitattributes"), "*.bin filter=demo -text\n")
            .expect("attributes");
        std::fs::write(root.join("model[1].bin"), "pointer").expect("pointer");
        git(root, &["add", "--", ".gitattributes", "model[1].bin"]);
        git(root, &["commit", "--quiet", "-m", "pointer"]);
        std::fs::write(root.join("other.txt"), "staged sibling").expect("sibling");
        git(root, &["add", "--", "other.txt"]);
        let entries = git(root, &["ls-files", "--stage"]);
        let mut temporary = tempfile::NamedTempFile::new_in(root).expect("output");
        temporary
            .write_all(&vec![7; 2 * 1024 * 1024])
            .expect("write");
        let update =
            IndexStatUpdate::from_file("model[1].bin".into(), temporary.as_file(), b"pointer")
                .expect("stat");
        temporary
            .persist(root.join("model[1].bin"))
            .expect("publish");
        assert!(git(root, &["status", "--porcelain", "--", "model[1].bin"]).contains(" M "));
        assert_eq!(refresh(root, &[update]).expect("refresh"), 1);
        assert_eq!(
            (
                git(root, &["status", "--porcelain", "--", "model[1].bin"]),
                git(root, &["ls-files", "--stage"])
            ),
            (String::new(), entries)
        );
    }

    #[test]
    fn refresh_skips_a_replaced_index_identity() {
        for by_oid in [false, true] {
            let directory = repository();
            let root = directory.path();
            let file = std::fs::File::open(root.join("other.txt")).expect("file");
            let update = if by_oid {
                let oid = git(root, &["rev-parse", ":other.txt"]);
                IndexStatUpdate::from_file_oid("other.txt".into(), &file, oid.trim())
            } else {
                IndexStatUpdate::from_file("other.txt".into(), &file, b"original")
            }
            .expect("stat");
            std::fs::write(root.join("other.txt"), "replacement").expect("edit");
            git(root, &["add", "--", "other.txt"]);
            let before = std::fs::read(root.join(".git/index")).expect("index");
            assert_eq!(refresh(root, &[update]).expect("refresh"), 0);
            assert_eq!(
                std::fs::read(root.join(".git/index")).expect("index"),
                before
            );
        }
    }

    #[test]
    fn refresh_retains_captured_stat_so_later_edits_stay_visible() {
        let directory = repository();
        let root = directory.path();
        let file = std::fs::File::open(root.join("other.txt")).expect("file");
        let update =
            IndexStatUpdate::from_file("other.txt".into(), &file, b"original").expect("stat");
        std::fs::write(root.join("other.txt"), "modified").expect("same-size edit");
        refresh(root, &[update]).expect("refresh");
        assert!(git(root, &["status", "--porcelain", "--", "other.txt"]).contains(" M "));
    }

    #[test]
    fn refresh_preserves_racy_sibling_edits() {
        for content in [b"modified".as_slice(), b""] {
            let directory = repository();
            let root = directory.path();
            git(root, &["config", "core.trustctime", "false"]);
            git(root, &["config", "core.checkStat", "minimal"]);
            std::fs::write(root.join("verified.txt"), b"verified").expect("verified file");
            git(root, &["add", "verified.txt"]);
            git(root, &["commit", "--quiet", "-m", "verified"]);
            let entries = git(root, &["ls-files", "--stage"]);
            std::fs::write(root.join("other.txt"), content).expect("sibling edit");

            // Reproduce an edit whose stat aliases the staged file, while the
            // old index timestamp still forces Git to inspect its actual bytes.
            let repo = crate::facade::open_at(root).expect("repository");
            let mut index = repo.open_index().expect("index");
            let sibling = std::fs::File::options()
                .read(true)
                .write(true)
                .open(root.join("other.txt"))
                .expect("sibling");
            sibling
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(2))
                .expect("sibling timestamp");
            let metadata = gix::index::fs::Metadata::from_file(&sibling).expect("metadata");
            let stat = gix::index::entry::Stat::from_fs(&metadata).expect("stat");
            for (entry, path) in index.entries_mut_with_paths() {
                if path == "other.txt" {
                    entry.stat = stat;
                }
            }
            let mut temporary =
                tempfile::NamedTempFile::new_in(root.join(".git")).expect("index temporary");
            index
                .write_to(temporary.as_file_mut(), Default::default())
                .expect("write index");
            drop(index);
            drop(sibling);
            temporary.persist(repo.index_path()).expect("publish index");
            std::fs::File::options()
                .write(true)
                .open(repo.index_path())
                .expect("index timestamp")
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
                .expect("racy index");
            let status = || {
                git(
                    root,
                    &[
                        "--no-optional-locks",
                        "status",
                        "--porcelain",
                        "--",
                        "other.txt",
                    ],
                )
            };
            let before = status();
            assert_eq!(before, " M other.txt\n");

            let output = std::fs::File::open(root.join("verified.txt")).expect("verified file");
            let update = IndexStatUpdate::from_file("verified.txt".into(), &output, b"verified")
                .expect("verified stat");
            refresh(root, &[update]).expect("refresh");
            assert_eq!(
                (status(), git(root, &["ls-files", "--stage"])),
                (before, entries)
            );
        }
    }

    #[test]
    fn occupied_index_lock_preserves_index_and_lock_owner() {
        let directory = repository();
        let root = directory.path();
        let before = std::fs::read(root.join(".git/index")).expect("index");
        std::fs::write(root.join(".git/index.lock"), "another writer").expect("lock");
        let file = std::fs::File::open(root.join("other.txt")).expect("file");
        let update =
            IndexStatUpdate::from_file("other.txt".into(), &file, b"original").expect("stat");
        assert!(refresh(root, &[update]).is_err());
        assert_eq!(
            (
                std::fs::read(root.join(".git/index")).expect("index"),
                std::fs::read(root.join(".git/index.lock")).expect("lock")
            ),
            (before, b"another writer".to_vec())
        );
    }
}
