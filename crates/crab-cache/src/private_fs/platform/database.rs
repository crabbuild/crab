use rusqlite::{Connection, OpenFlags};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{Directory, OsStr, Path, component_name, io, unsafe_path, validate_permissions};
use crate::private_fs::DatabaseMode;
use crate::{CacheError, Result};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod file;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod generation;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(in crate::private_fs) use generation::Generation;
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod lifetime_tests;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod locking;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod shm;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod vfs;

pub(crate) struct Database {
    connection: std::mem::ManuallyDrop<Connection>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    registration: std::mem::ManuallyDrop<vfs::Registration>,
}

impl std::ops::Deref for Database {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.connection
    }
}

impl Database {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(in crate::private_fs) fn generation(&self) -> Arc<Generation> {
        self.registration.generation()
    }

    // Do not expose &mut Connection: replacing it could let the old SQLite
    // handle outlive its registered callbacks. Transactions borrow the owner.
    #[cfg(feature = "local-cache")]
    pub(crate) fn transaction(&mut self) -> rusqlite::Result<rusqlite::Transaction<'_>> {
        self.connection.transaction()
    }

    pub(crate) fn transaction_with_behavior(
        &mut self,
        behavior: rusqlite::TransactionBehavior,
    ) -> rusqlite::Result<rusqlite::Transaction<'_>> {
        self.connection.transaction_with_behavior(behavior)
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // SAFETY: Drop runs once; ManuallyDrop prevents a second close. Keep
        // VFS callback storage live until SQLite confirms all handles closed.
        let connection = unsafe { std::mem::ManuallyDrop::take(&mut self.connection) };
        if let Err((connection, error)) = connection.close() {
            // A forgotten SQLite statement can make close return BUSY. Leaking
            // both owners in that programming-error case prevents dangling C
            // callback pointers; normal borrowed statements cannot outlive us.
            tracing::error!(%error, "private cache database still has outstanding SQLite handles");
            std::mem::forget(connection);
            return;
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        // SAFETY: SQLite has closed every file using this registration.
        unsafe {
            std::mem::ManuallyDrop::drop(&mut self.registration);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(in crate::private_fs) fn open_database(
    root: &Path,
    path: &Path,
    mode: DatabaseMode,
    busy_timeout: Duration,
) -> Result<Database> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| unsafe_path(path, "entry is outside cache root"))?;
    let root = Directory::root(root, mode == DatabaseMode::Create)?;
    open_database_at(&root, relative, mode, busy_timeout)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(in crate::private_fs) fn open_database_at(
    root: &Directory,
    relative: &Path,
    mode: DatabaseMode,
    busy_timeout: Duration,
) -> Result<Database> {
    open_with_generation(root, relative, mode, busy_timeout, None)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(in crate::private_fs) fn open_database_leased(
    root: &Directory,
    relative: &Path,
    expected: &Generation,
    busy_timeout: Duration,
) -> Result<Database> {
    open_with_generation(
        root,
        relative,
        DatabaseMode::ReadWrite,
        busy_timeout,
        Some(expected),
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(in crate::private_fs) fn validate_database_generation(
    root: &Directory,
    relative: &Path,
    expected: &Generation,
) -> Result<()> {
    let (directory, name) = root.descendant_parent(relative, false)?;
    expected
        .validate(&directory, &name)
        .map_err(|code| CacheError::Index {
            path: root.path.join(relative).display().to_string(),
            source: rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None),
        })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_with_generation(
    root: &Directory,
    relative: &Path,
    mode: DatabaseMode,
    busy_timeout: Duration,
    expected: Option<&Generation>,
) -> Result<Database> {
    let (directory, name) = root.descendant_parent(relative, mode == DatabaseMode::Create)?;
    let path = root.path.join(relative);
    let path = path.as_path();
    // All in-process cache database connections must use this owner. Native
    // POSIX SQLite locks can be lost by closing an unrelated fd for that inode;
    // native SQLite interoperability is supported only across processes.
    let started = Instant::now();
    let _mutation = loop {
        match directory.mutation() {
            Ok(guard) => break guard,
            Err(CacheError::Io(error))
                if error.kind() == io::ErrorKind::WouldBlock
                    && started.elapsed() < busy_timeout =>
            {
                let remaining = busy_timeout.saturating_sub(started.elapsed());
                std::thread::sleep(Duration::from_millis(5).min(remaining));
            }
            Err(error) => return Err(error),
        }
    };
    if let Some(expected) = expected {
        expected
            .validate(&directory, &name)
            .map_err(|code| CacheError::Index {
                path: path.display().to_string(),
                source: rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None),
            })?;
    }
    let filename = path
        .file_name()
        .ok_or_else(|| unsafe_path(path, "database has no filename"))?;
    let mut recovery_files = false;
    for suffix in ["-journal", "-wal", "-shm", "-owner"] {
        let mut side_name = filename.to_os_string();
        side_name.push(suffix);
        let exists = validate_file(&directory, &side_name)?;
        recovery_files |= exists && suffix != "-owner";
    }
    if !validate_file(&directory, filename)? {
        if mode != DatabaseMode::Create {
            return Err(io::Error::from(io::ErrorKind::NotFound).into());
        }
        if recovery_files {
            return Err(unsafe_path(
                path,
                "missing database has recovery side files",
            ));
        }
        // Exclusive creation cannot touch an existing SQLite inode. The fd is
        // closed before SQLite opens, and no later pathname chmod is needed.
        drop(directory.open_component(&name, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, path)?);
        directory.file.sync_all()?;
    }
    // Reopening needs independent file descriptions for SQLite's OFD locks.
    // Retain the old lease for identity, never reuse its main fd for a new connection.
    let generation = Arc::new(Generation::open(&directory, &name, mode)?);
    if let Some(expected) = expected
        && !generation.matches(expected)?
    {
        return Err(unsafe_path(
            path,
            "database generation changed while reopening",
        ));
    }
    drop(_mutation);
    let registration = vfs::Registration::new(directory, name, generation, busy_timeout)?;
    let flags = if mode == DatabaseMode::ReadOnly {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    // SQLite sees only a private virtual name. Every subsequent side-file
    // operation uses the registration's retained directory descriptor.
    let connection =
        Connection::open_with_flags_and_vfs(vfs::VIRTUAL_DATABASE, flags, registration.name()?)
            .map_err(|source| CacheError::Index {
                path: path.display().to_string(),
                source,
            })?;
    connection
        .busy_timeout(busy_timeout)
        .map_err(|source| CacheError::Index {
            path: path.display().to_string(),
            source,
        })?;
    Ok(Database {
        connection: std::mem::ManuallyDrop::new(connection),
        registration: std::mem::ManuallyDrop::new(registration),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(in crate::private_fs) fn open_database(
    _root: &Path,
    path: &Path,
    _mode: DatabaseMode,
    _busy_timeout: Duration,
) -> Result<Database> {
    Err(unsafe_path(
        path,
        "private SQLite locking is unavailable on this platform",
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(in crate::private_fs) fn open_database_at(
    _root: &Directory,
    relative: &Path,
    _mode: DatabaseMode,
    _busy_timeout: Duration,
) -> Result<Database> {
    Err(unsafe_path(
        relative,
        "private SQLite locking is unavailable on this platform",
    ))
}

fn validate_file(directory: &Directory, name: &OsStr) -> Result<bool> {
    let path = directory.path.join(name);
    let name = component_name(name)?;
    let stat = match directory.stat_component(&name) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    validate_permissions(stat.st_mode, stat.st_uid, &path)?;
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG || stat.st_nlink > 1 {
        return Err(unsafe_path(
            &path,
            &format!(
                "database entry is a special file or has another hard link (mode={:o}, links={})",
                stat.st_mode, stat.st_nlink
            ),
        ));
    }
    // macOS can report a regular journal with zero links when another SQLite
    // connection unlinks it during fstatat. This is disappearance, not a hard
    // link or permission failure; SQLite owns subsequent journal resolution.
    Ok(stat.st_nlink == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    #[test]
    fn rollback_after_root_replacement_preserves_replacement_files() {
        assert_replacement_files_survive(true);
    }

    #[test]
    fn connection_drop_after_root_replacement_preserves_replacement_files() {
        assert_replacement_files_survive(false);
    }

    fn assert_replacement_files_survive(explicit_rollback: bool) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let path = root.join("database.sqlite");
        let connection = open_database(&root, &path, DatabaseMode::Create, Duration::ZERO).unwrap();
        connection.execute_batch("CREATE TABLE values_table(value INTEGER); INSERT INTO values_table VALUES (1); BEGIN IMMEDIATE; INSERT INTO values_table VALUES (2);").unwrap();
        assert!(root.join("database.sqlite-journal").exists());
        std::fs::rename(&root, tmp.path().join("moved")).unwrap();
        Directory::root(&root, true).unwrap();
        let journal = root.join("database.sqlite-journal");
        std::fs::write(&path, b"replacement database").unwrap();
        std::fs::write(&journal, b"replacement journal").unwrap();
        if explicit_rollback {
            // Failing closed after replacement is permitted; mutating the
            // new root is not, regardless of the rollback's return value.
            let _ = connection.execute_batch("ROLLBACK");
        }
        drop(connection);
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement database");
        assert_eq!(std::fs::read(&journal).unwrap(), b"replacement journal");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn ofd_reservations_interoperate_with_native_sqlite_across_processes() {
        use std::os::fd::AsRawFd as _;

        const CHILD_PATH: &str = "CRAB_TEST_SQLITE_OFD_PATH";
        const CHILD_MODE: &str = "CRAB_TEST_SQLITE_OFD_MODE";
        fn reserve(file: &std::fs::File) -> io::Result<()> {
            // SAFETY: flock contains only integer fields; zero is valid for
            // its unused fields, including the required OFD l_pid value.
            let mut lock: libc::flock = unsafe { std::mem::zeroed() };
            lock.l_type = libc::F_WRLCK as _;
            lock.l_whence = libc::SEEK_SET as _;
            lock.l_start = 0x4000_0001;
            lock.l_len = 1;
            // SAFETY: the descriptor and flock are live; this is a nonblocking
            // reservation on SQLite's documented RESERVED byte.
            if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &lock) } == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        fn native(path: &Path) -> Connection {
            let connection = Connection::open(path).unwrap();
            connection.busy_timeout(Duration::ZERO).unwrap();
            connection
        }
        fn child(path: &Path, mode: &str) {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "private_fs::platform::database::tests::ofd_reservations_interoperate_with_native_sqlite_across_processes"])
                .env_clear()
                .env(CHILD_PATH, path)
                .env(CHILD_MODE, mode)
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let path = std::path::PathBuf::from(path);
            if std::env::var(CHILD_MODE).unwrap() == "native" {
                let error = native(&path).execute_batch("BEGIN IMMEDIATE").unwrap_err();
                assert_eq!(
                    error.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy)
                );
            } else {
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .unwrap();
                let error = reserve(&file).unwrap_err();
                assert!(matches!(
                    error.raw_os_error(),
                    Some(libc::EACCES | libc::EAGAIN)
                ));
            }
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let path = root.join("database.sqlite");
        let connection = open_database(&root, &path, DatabaseMode::Create, Duration::ZERO).unwrap();
        connection
            .execute_batch("CREATE TABLE values_table(value INTEGER)")
            .unwrap();
        drop(connection);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        reserve(&file).unwrap();
        child(&path, "native");
        drop(file);

        let connection = native(&path);
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        child(&path, "ofd");
        // Closing the other process's descriptor must not release our writer.
        child(&path, "native");
        connection.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn simultaneous_database_creators_preserve_all_rows() {
        const CHILD_ROOT: &str = "CRAB_TEST_SQLITE_CREATOR_ROOT";
        const CHILD_ID: &str = "CRAB_TEST_SQLITE_CREATOR_ID";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = std::path::PathBuf::from(root);
            let path = root.join("database.sqlite");
            let id: u32 = std::env::var(CHILD_ID).unwrap().parse().unwrap();
            let connection =
                open_database(&root, &path, DatabaseMode::Create, Duration::from_secs(2)).unwrap();
            connection
                .execute_batch("CREATE TABLE IF NOT EXISTS creators(id INTEGER PRIMARY KEY)")
                .unwrap();
            connection
                .execute("INSERT INTO creators VALUES (?1)", [id])
                .unwrap();
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let children: Vec<_> = (0..8).map(|id| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "private_fs::platform::database::tests::simultaneous_database_creators_preserve_all_rows"])
                .env_clear()
                .env(CHILD_ROOT, &root)
                .env(CHILD_ID, id.to_string())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn().unwrap()
        }).collect();
        for child in children {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let connection = open_database(
            &root,
            &root.join("database.sqlite"),
            DatabaseMode::ReadOnly,
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM creators", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            8
        );
    }

    #[test]
    fn unsafe_database_entries_are_rejected_without_mutation() {
        for suffix in ["", "-journal", "-wal", "-shm", "-owner"] {
            for unsafe_kind in ["symlink", "hardlink", "public"] {
                let tmp = tempfile::tempdir().unwrap();
                let root = tmp.path().join("cache");
                let path = root.join("database.sqlite");
                Directory::root(&root, true).unwrap();
                let entry = root.join(format!("database.sqlite{suffix}"));
                let sentinel = tmp.path().join("sentinel");
                std::fs::write(&sentinel, b"do not modify").unwrap();
                std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
                match unsafe_kind {
                    "symlink" => symlink(&sentinel, &entry).unwrap(),
                    "hardlink" => std::fs::hard_link(&sentinel, &entry).unwrap(),
                    "public" => {
                        std::fs::write(&entry, b"do not modify").unwrap();
                        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o644))
                            .unwrap();
                    }
                    _ => unreachable!(),
                }
                let before = std::fs::symlink_metadata(&entry)
                    .unwrap()
                    .permissions()
                    .mode();
                for mode in [
                    DatabaseMode::Create,
                    DatabaseMode::ReadWrite,
                    DatabaseMode::ReadOnly,
                ] {
                    assert!(
                        matches!(
                            open_database(&root, &path, mode, Duration::ZERO),
                            Err(CacheError::UnsafeRoot { .. })
                        ),
                        "{suffix} {unsafe_kind}"
                    );
                    assert_eq!(std::fs::read(&sentinel).unwrap(), b"do not modify");
                    assert_eq!(
                        std::fs::symlink_metadata(&entry)
                            .unwrap()
                            .permissions()
                            .mode(),
                        before
                    );
                    if !suffix.is_empty() {
                        assert!(!path.exists());
                    }
                }
            }
        }
    }

    #[test]
    fn database_open_validates_the_entire_cache_owned_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let path = root.join("index/database.sqlite");
        Directory::root(&root.join("index"), true).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let alias = tmp.path().join("alias");
        symlink(&root, &alias).unwrap();
        for (selected_root, selected_path) in [
            (alias.clone(), alias.join("index/database.sqlite")),
            (root.clone(), root.join("../outside/database.sqlite")),
        ] {
            assert!(
                open_database(
                    &selected_root,
                    &selected_path,
                    DatabaseMode::Create,
                    Duration::ZERO
                )
                .is_err()
            );
        }
        symlink(&outside, root.join("linked-index")).unwrap();
        assert!(
            open_database(
                &root,
                &root.join("linked-index/database.sqlite"),
                DatabaseMode::Create,
                Duration::ZERO
            )
            .is_err()
        );
        for directory in [&root, &root.join("index")] {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(matches!(
                open_database(&root, &path, DatabaseMode::Create, Duration::ZERO),
                Err(CacheError::UnsafeRoot { .. })
            ));
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(!outside.join("database.sqlite").exists());
        assert!(!path.exists());
    }

    #[test]
    fn fresh_database_and_sqlite_side_files_are_private_without_chmod() {
        const CHILD: &str = "CRAB_TEST_PRIVATE_SQLITE_UMASK";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "private_fs::platform::database::tests::fresh_database_and_sqlite_side_files_are_private_without_chmod"])
                .env(CHILD, "1")
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // SAFETY: this is an isolated child test process, running only this
        // synchronous test; changing umask cannot affect other test fixtures.
        unsafe {
            libc::umask(0);
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let path = root.join("database.sqlite");
        let connection = open_database(&root, &path, DatabaseMode::Create, Duration::ZERO).unwrap();
        connection.execute_batch("CREATE TABLE values_table(value INTEGER); BEGIN IMMEDIATE; INSERT INTO values_table VALUES (1);").unwrap();
        for path in [
            &path,
            &root.join("database.sqlite-journal"),
            &root.join("database.sqlite-owner"),
        ] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        connection
            .execute_batch("COMMIT; PRAGMA journal_mode=WAL; INSERT INTO values_table VALUES (2);")
            .unwrap();
        for path in [
            &path,
            &root.join("database.sqlite-wal"),
            &root.join("database.sqlite-shm"),
        ] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Reopening must not replace the existing inode or truncate the table.
        drop(open_database(&root, &path, DatabaseMode::Create, Duration::ZERO).unwrap());
        assert_eq!(
            connection
                .query_row("SELECT SUM(value) FROM values_table", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            3
        );
        let missing = root.join("absent.sqlite");
        for mode in [DatabaseMode::ReadOnly, DatabaseMode::ReadWrite] {
            assert!(open_database(&root, &missing, mode, Duration::ZERO).is_err());
            assert!(!missing.exists());
        }
    }
}
