use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::{AgentError, ReadScope, Result, command};

const APPLICATION_ID: u32 = 0x4352_5042;

struct Lease(File);

impl Drop for Lease {
    fn drop(&mut self) {
        // A forked child must not extend a publication's ownership after the
        // native writer has joined its workers and released the workspace.
        if let Err(error) = self.0.unlock() {
            tracing::warn!(%error, "publication lease release failed");
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
struct Identity {
    volume: u64,
    file: [u8; 16],
    created: Option<Duration>,
}

#[derive(Deserialize, Serialize)]
struct Payload {
    path: PathBuf,
    identity: Identity,
}

#[derive(Deserialize, Serialize)]
struct Record {
    root: PathBuf,
    root_identity: Identity,
    parent_identity: Identity,
    path: PathBuf,
    identity: Identity,
    files: Vec<Payload>,
}

pub(super) struct Workspace {
    record: Record,
    journal: PathBuf,
    _lease: Lease,
    registry: crate::file_publications::Registry,
    publications: Vec<crate::file_publications::Publication>,
}

impl Workspace {
    pub(super) async fn create(
        root: &Path,
        parent: PathBuf,
        scope: &ReadScope,
    ) -> Result<(Self, tempfile::NamedTempFile)> {
        let index = command::index_path(root, scope).await?;
        let control = index.parent().ok_or_else(changed)?.to_owned();
        let root = tokio::fs::canonicalize(root).await?;
        let registry = crate::file_publications::registry(&root).await;
        let scope = scope.clone();
        tokio::task::spawn_blocking(move || Self::begin(root, parent, control, &scope, &registry))
            .await
            .map_err(std::io::Error::other)?
    }

    fn begin(
        root: PathBuf,
        parent: PathBuf,
        control: PathBuf,
        scope: &ReadScope,
        registry: &crate::file_publications::Registry,
    ) -> Result<(Self, tempfile::NamedTempFile)> {
        let root = root.canonicalize()?;
        if !parent.starts_with(&root) || parent.canonicalize()? != parent {
            return Err(changed());
        }
        let lease_path = control.join("crab-desktop-publication.lock");
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lease_path)?;
        if !fs::symlink_metadata(&lease_path)?.is_file()
            || fs::symlink_metadata(&lease_path)?.is_symlink()
        {
            return Err(changed());
        }
        loop {
            scope.check()?;
            match lease.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(25)),
                Err(TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        let lease = Lease(lease);
        let journal = control.join("crab-desktop-publication.sqlite3");
        let db = database(&journal)?;
        for record in records(&db)? {
            scope.check()?;
            if record.root != root {
                return Err(changed());
            }
            remove(&record)?;
            db.execute(
                "DELETE FROM workspaces WHERE path=?",
                [path_key(&record.path)?],
            )
            .map_err(sqlite)?;
        }
        let root_identity = directory_identity(&root)?;
        let parent_identity = directory_identity(&parent)?;
        let mut builder = tempfile::Builder::new();
        builder.prefix(".crab-publication-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        let directory = builder.tempdir_in(&parent)?;
        // No payload bytes are written until the ownership record commits.
        // Allocation interrupted before that point can leave empty unclaimed
        // artifacts; recovery never guesses ownership from their names.
        let path = directory.keep();
        let record = Record {
            root,
            root_identity,
            parent_identity,
            identity: directory_identity(&path)?,
            files: Vec::new(),
            path,
        };
        check(&record)?;
        if let Err(error) = db
            .execute(
                "INSERT INTO workspaces(path,record) VALUES(?,?)",
                [path_key(&record.path)?, serde_json::to_string(&record)?],
            )
            .map_err(sqlite)
        {
            return match remove(&record) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(AgentError::Cleanup {
                    operation: Box::new(error),
                    cleanup: Box::new(cleanup),
                }),
            };
        }
        let mut workspace = Self {
            record,
            journal,
            _lease: lease,
            registry: registry.clone(),
            publications: Vec::new(),
        };
        let prepared = (|| {
            scope.check()?;
            let mut builder = tempfile::Builder::new();
            builder.prefix(".gitignore").rand_bytes(0);
            let mut ignore = workspace.new_temporary(&builder)?;
            // Partial content must never enter an ordinary Stage All operation,
            // including from Git outside the desktop's filtered Changes view.
            ignore.write_all(b"*\n")?;
            ignore.as_file().sync_all()?;
            scope.check()?;
            workspace.new_temporary(&tempfile::Builder::new())
        })();
        let temporary = match prepared {
            Ok(temporary) => temporary,
            Err(operation) => {
                return match workspace.close() {
                    Ok(()) => Err(operation),
                    Err(cleanup) => Err(AgentError::Cleanup {
                        operation: Box::new(operation),
                        cleanup: Box::new(cleanup),
                    }),
                };
            }
        };
        Ok((workspace, temporary))
    }

    pub(super) async fn additional_temporary(mut self) -> Result<(Self, tempfile::NamedTempFile)> {
        tokio::task::spawn_blocking(
            move || match self.new_temporary(&tempfile::Builder::new()) {
                Ok(temporary) => Ok((self, temporary)),
                Err(operation) => match self.close() {
                    Ok(()) => Err(operation),
                    Err(cleanup) => Err(AgentError::Cleanup {
                        operation: Box::new(operation),
                        cleanup: Box::new(cleanup),
                    }),
                },
            },
        )
        .await
        .map_err(std::io::Error::other)?
    }

    fn new_temporary(
        &mut self,
        builder: &tempfile::Builder<'_, '_>,
    ) -> Result<tempfile::NamedTempFile> {
        check(&self.record)?;
        if directory_identity(&self.record.path)? != self.record.identity {
            return Err(changed());
        }
        let (mut temporary, publication) =
            crate::file_publications::owned_temporary(&self.registry, builder, &self.record.path)?;
        // The journal is the only cleanup authority, including early errors.
        // A path-based Drop must not delete an object substituted by another writer.
        temporary.disable_cleanup(true);
        self.record.files.push(Payload {
            path: temporary.path().to_owned(),
            identity: object_identity(temporary.path(), false)?,
        });
        self.publications.push(publication);
        database(&self.journal)?
            .execute(
                "UPDATE workspaces SET record=? WHERE path=?",
                [
                    serde_json::to_string(&self.record)?,
                    path_key(&self.record.path)?,
                ],
            )
            .map_err(sqlite)?;
        Ok(temporary)
    }

    pub(super) async fn finish(self, result: Result<u64>) -> Result<u64> {
        let cleanup = match tokio::task::spawn_blocking(move || self.close()).await {
            Ok(result) => result,
            Err(error) => Err(std::io::Error::other(error).into()),
        };
        match (result, cleanup) {
            (Ok(written_bytes), Err(cleanup)) => Err(AgentError::PublicationCleanup {
                written_bytes,
                cleanup: Box::new(cleanup),
            }),
            (Err(operation), Err(cleanup)) => Err(AgentError::Cleanup {
                operation: Box::new(operation),
                cleanup: Box::new(cleanup),
            }),
            (result, Ok(())) => result,
        }
    }

    fn close(self) -> Result<()> {
        remove(&self.record)?;
        database(&self.journal)?
            .execute(
                "DELETE FROM workspaces WHERE path=?",
                [path_key(&self.record.path)?],
            )
            .map_err(sqlite)?;
        Ok(())
    }
}

fn database(path: &Path) -> Result<Connection> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (!metadata.is_file() || metadata.is_symlink())
    {
        return Err(changed());
    }
    let db = Connection::open(path).map_err(sqlite)?;
    let application: u32 = db
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(sqlite)?;
    if application != APPLICATION_ID && (application != 0 || fs::metadata(path)?.len() != 0) {
        return Err(changed());
    }
    db.pragma_update(None, "journal_mode", "WAL")
        .map_err(sqlite)?;
    db.pragma_update(None, "synchronous", "FULL")
        .map_err(sqlite)?;
    db.execute_batch(&format!("BEGIN IMMEDIATE; PRAGMA application_id={APPLICATION_ID}; CREATE TABLE IF NOT EXISTS workspaces(path TEXT PRIMARY KEY,record TEXT NOT NULL); COMMIT;")).map_err(sqlite)?;
    Ok(db)
}

fn path_key(path: &Path) -> Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(changed)
}

fn records(db: &Connection) -> Result<Vec<Record>> {
    let mut query = db
        .prepare("SELECT record FROM workspaces")
        .map_err(sqlite)?;
    query
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(sqlite)?
        .map(|row| Ok(serde_json::from_str(&row.map_err(sqlite)?)?))
        .collect()
}

fn check(record: &Record) -> Result<()> {
    let parent = record.path.parent().ok_or_else(changed)?;
    if !parent.starts_with(&record.root)
        || record.root.canonicalize()? != record.root
        || parent.canonicalize()? != parent
        || directory_identity(&record.root)? != record.root_identity
        || directory_identity(parent)? != record.parent_identity
    {
        return Err(changed());
    }
    Ok(())
}

fn remove(record: &Record) -> Result<()> {
    let result = (|| -> Result<()> {
        check(record)?;
        match fs::symlink_metadata(&record.path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        if directory_identity(&record.path)? != record.identity {
            return Err(changed());
        }
        for file in &record.files {
            if file.path.parent() != Some(record.path.as_path()) {
                return Err(changed());
            }
            match fs::symlink_metadata(&file.path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
                Ok(_) => {
                    if object_identity(&file.path, false)? != file.identity {
                        return Err(changed());
                    }
                    fs::remove_file(&file.path)?;
                }
            }
        }
        // Only the recorded payload is disposable. Unexpected or replaced files
        // remain visible as a cleanup failure; never recursively delete them.
        fs::remove_dir(&record.path)?;
        Ok(())
    })();
    result.map_err(|source| AgentError::Preview {
        context: format!(
            "Publication scratch at {} was retained; inspect its contents before retrying",
            record.path.display()
        ),
        source: Box::new(source),
    })
}

fn changed() -> AgentError {
    AgentError::Other("Publication workspace identity changed. Retained scratch data requires review before retrying.".into())
}

fn sqlite(source: rusqlite::Error) -> AgentError {
    AgentError::Preview {
        context: "Publication journal is unavailable".into(),
        source: Box::new(source),
    }
}

fn directory_identity(path: &Path) -> Result<Identity> {
    object_identity(path, true)
}

fn object_identity(path: &Path, directory: bool) -> Result<Identity> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() != directory
        || (!directory && !metadata.is_file())
        || metadata.is_symlink()
    {
        return Err(changed());
    }
    #[cfg(unix)]
    let (volume, file) = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), u128::from(metadata.ino()).to_le_bytes())
    };
    #[cfg(windows)]
    let (volume, file) = windows_identity(path)?;
    let created = metadata
        .created()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok());
    Ok(Identity {
        volume,
        file,
        created,
    })
}

#[cfg(windows)]
fn windows_identity(path: &Path) -> Result<(u64, [u8; 16])> {
    use std::os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    };
    #[repr(C)]
    struct FileId {
        volume: u64,
        file: [u8; 16],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "GetFileInformationByHandleEx"]
        fn file_information(
            handle: *mut std::ffi::c_void,
            class: i32,
            result: *mut std::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const REPARSE_POINT: u32 = 0x400;
    const FILE_ID_INFO: i32 = 18;
    // Open the object itself, including reparse points, so a link cannot
    // substitute another filesystem object for the recorded workspace.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if metadata.file_attributes() & REPARSE_POINT != 0 {
        return Err(changed());
    }
    let mut result = FileId {
        volume: 0,
        file: [0; 16],
    };
    // SAFETY: the live file handle and initialized FILE_ID_INFO buffer remain
    // valid for this synchronous Win32 call; the length matches its C layout.
    if unsafe {
        file_information(
            file.as_raw_handle(),
            FILE_ID_INFO,
            (&mut result as *mut FileId).cast(),
            std::mem::size_of::<FileId>() as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((result.volume, result.file))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .arg(&root)
                .status()
                .unwrap()
                .success()
        );
        (directory, root)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn abandoned_payload_is_recovered_without_touching_working_files() {
        let (_directory, root) = repository();
        fs::write(root.join("working"), b"retain working content").unwrap();
        let (workspace, mut file) = Workspace::create(&root, root.clone(), &ReadScope::default())
            .await
            .unwrap();
        let path = file.path().to_owned();
        file.write_all(&vec![7; 2 * 1024 * 1024]).unwrap();
        file.as_file().sync_all().unwrap();
        drop(file);
        drop(workspace);
        assert_eq!(fs::metadata(&path).unwrap().len(), 2 * 1024 * 1024);
        let (recovered, file) = Workspace::create(&root, root.clone(), &ReadScope::default())
            .await
            .unwrap();
        assert!(!path.exists());
        assert_eq!(
            fs::read(root.join("working")).unwrap(),
            b"retain working content"
        );
        drop(file);
        recovered.finish(Ok(0)).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recovery_preserves_replaced_payloads_and_their_moved_original() {
        let (_directory, root) = repository();
        let (workspace, file) = Workspace::create(&root, root.clone(), &ReadScope::default())
            .await
            .unwrap();
        let path = file.path().to_owned();
        fs::rename(&path, root.join("moved")).unwrap();
        fs::write(&path, b"new owner").unwrap();
        drop(file);
        drop(workspace);
        assert!(
            Workspace::create(&root, root.clone(), &ReadScope::default())
                .await
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"new owner");
        assert!(root.join("moved").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cleanup_preserves_a_replaced_workspace_and_its_moved_payload() {
        let (_directory, root) = repository();
        let (workspace, mut file) = Workspace::create(&root, root.clone(), &ReadScope::default())
            .await
            .unwrap();
        file.write_all(b"owned partial content").unwrap();
        let path = workspace.record.path.clone();
        let moved = root.join("moved");
        let name = file.path().file_name().unwrap().to_owned();
        // Windows prevents moving a directory while its payload writer is open.
        // Release that OS protection so this case reaches journal-owned cleanup.
        #[cfg(windows)]
        drop(file);
        fs::rename(&path, &moved).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("foreign"), b"retain foreign content").unwrap();
        #[cfg(not(windows))]
        drop(file);
        assert!(workspace.finish(Ok(0)).await.is_err());
        assert_eq!(
            fs::read(moved.join(name)).unwrap(),
            b"owned partial content"
        );
        assert_eq!(
            fs::read(path.join("foreign")).unwrap(),
            b"retain foreign content"
        );
        assert!(
            Workspace::create(&root, root.clone(), &ReadScope::default())
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cleanup_failure_retains_publication_count_and_unlisted_content() {
        let (_directory, root) = repository();
        let (workspace, file) = Workspace::create(&root, root.clone(), &ReadScope::default())
            .await
            .unwrap();
        let foreign = workspace.record.path.join("new-owner.txt");
        fs::write(&foreign, b"retain new content").unwrap();
        drop(file);
        let error = workspace.finish(Ok(123)).await.unwrap_err();
        assert_eq!(error.published_bytes(), Some(123));
        assert_eq!(fs::read(foreign).unwrap(), b"retain new content");
        assert!(
            Workspace::create(&root, root.clone(), &ReadScope::default())
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_live_lease_cannot_be_recovered_and_queued_admission_is_cancellable() {
        let (_directory, root) = repository();
        let (workspace, file) = Workspace::create(&root, root.clone(), &ReadScope::default())
            .await
            .unwrap();
        let scope = ReadScope::default();
        let queued_root = root.clone();
        let queued_scope = scope.clone();
        let queued = tokio::spawn(async move {
            Workspace::create(&queued_root, queued_root.clone(), &queued_scope).await
        });
        scope.cancel();
        assert!(queued.await.unwrap().err().unwrap().is_cancelled());
        assert!(file.path().exists());
        drop(file);
        workspace.finish(Ok(0)).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_snapshots_hide_only_live_owned_payloads_through_cleanup() {
        let (_directory, root) = repository();
        let before = crate::file_publications::status_snapshot(&root).await;
        let (workspace, file) = Workspace::create(&root, root.clone(), &ReadScope::default())
            .await
            .unwrap();
        let (workspace, index) = workspace.additional_temporary().await.unwrap();
        let expected = [
            file.path().to_owned(),
            index.path().to_owned(),
            workspace.record.path.join(".gitignore"),
        ]
        .into_iter()
        .collect();
        assert_eq!(before.finish().await, expected);
        let staged = std::process::Command::new("git")
            .args(["add", "--all"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(staged.success());
        let tracked = std::process::Command::new("git")
            .arg("ls-files")
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(tracked.status.success() && tracked.stdout.is_empty());
        drop(file);
        drop(index);
        workspace.finish(Ok(0)).await.unwrap();
        assert!(
            crate::file_publications::status_snapshot(&root)
                .await
                .finish()
                .await
                .is_empty()
        );
    }
}
