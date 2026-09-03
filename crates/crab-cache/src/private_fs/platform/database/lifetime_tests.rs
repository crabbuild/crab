use std::io::{BufRead as _, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::*;

const DATABASE: &str = "database.sqlite";

fn open(root: &Path) -> Database {
    open_database(
        root,
        &root.join(DATABASE),
        DatabaseMode::Create,
        Duration::from_secs(5),
    )
    .unwrap()
}

fn count(connection: &Connection) -> u64 {
    connection
        .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn root_replacement_cannot_redirect_wal_commit_checkpoint_or_close() {
    for outcome in ["COMMIT", "ROLLBACK"] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let connection = open(&root);
        connection.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE entries(value INTEGER); INSERT INTO entries VALUES(1); BEGIN IMMEDIATE; INSERT INTO entries VALUES(2);").unwrap();
        let moved = tmp.path().join("moved");
        std::fs::rename(&root, &moved).unwrap();
        Directory::root(&root, true).unwrap();
        let sentinels = [
            DATABASE,
            "database.sqlite-journal",
            "database.sqlite-wal",
            "database.sqlite-shm",
        ];
        for name in sentinels {
            std::fs::write(root.join(name), name.as_bytes()).unwrap();
        }
        connection.execute_batch(outcome).unwrap();
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        drop(connection);
        for name in sentinels {
            assert_eq!(
                std::fs::read(root.join(name)).unwrap(),
                name.as_bytes(),
                "{outcome}: {name}"
            );
        }
        assert_eq!(
            count(&open(&moved)),
            if outcome == "COMMIT" { 2 } else { 1 }
        );
    }
}

#[test]
fn first_transaction_uses_the_parent_retained_at_open() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let connection = open(&root);
    let moved = tmp.path().join("moved");
    std::fs::rename(&root, &moved).unwrap();
    Directory::root(&root, true).unwrap();
    connection
        .execute_batch("CREATE TABLE entries(value); INSERT INTO entries VALUES (1)")
        .unwrap();
    drop(connection);
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    assert_eq!(count(&open(&moved)), 1);
}

#[test]
fn main_replacement_preserves_replacement_side_files_during_cleanup() {
    use std::os::unix::fs::PermissionsExt as _;

    for journal in ["DELETE", "WAL"] {
        for outcome in ["COMMIT", "ROLLBACK", "DROP"] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().join("cache");
            let connection = open(&root);
            connection
                .pragma_update(None, "journal_mode", journal)
                .unwrap();
            connection.execute_batch("CREATE TABLE entries(value); INSERT INTO entries VALUES(1); BEGIN IMMEDIATE; INSERT INTO entries VALUES(2)").unwrap();
            let names = [
                DATABASE,
                "database.sqlite-journal",
                "database.sqlite-wal",
                "database.sqlite-shm",
            ];
            for name in names {
                let path = root.join(name);
                if path.exists() {
                    // Rename, never truncate a live mapping or SQLite handle.
                    std::fs::rename(&path, root.join(format!("saved-{name}"))).unwrap();
                }
                std::fs::write(&path, name.as_bytes()).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            if outcome != "DROP" {
                let _ = connection.execute_batch(outcome);
                let _ = connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
            }
            drop(connection);
            for name in names {
                assert_eq!(
                    std::fs::read(root.join(name)).ok().as_deref(),
                    Some(name.as_bytes()),
                    "{journal}/{outcome}: {name}"
                );
            }
        }
    }
}

#[test]
fn replaced_main_cannot_replay_another_database_wal() {
    const ROOT: &str = "CRAB_TEST_REPLACED_DATABASE_ROOT";
    if let Some(root) = std::env::var_os(ROOT) {
        let root = PathBuf::from(root);
        for mode in [
            DatabaseMode::Create,
            DatabaseMode::ReadWrite,
            DatabaseMode::ReadOnly,
        ] {
            assert!(open_database(&root, &root.join(DATABASE), mode, Duration::ZERO).is_err());
        }
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let original = open(&root);
    original.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE entries(value); INSERT INTO entries VALUES('original')").unwrap();
    let replacement_root = tmp.path().join("replacement");
    let replacement = open(&replacement_root);
    replacement.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE entries(value); INSERT INTO entries VALUES('replacement')").unwrap();
    drop(replacement);
    std::fs::rename(root.join(DATABASE), root.join("saved.sqlite")).unwrap();
    std::fs::rename(replacement_root.join(DATABASE), root.join(DATABASE)).unwrap();
    let result = open_database(
        &root,
        &root.join(DATABASE),
        DatabaseMode::ReadWrite,
        Duration::ZERO,
    );
    match result {
        Err(_) => (),
        Ok(connection) => {
            let value = connection.query_row("SELECT value FROM entries", [], |row| {
                row.get::<_, String>(0)
            });
            assert!(
                value.is_err(),
                "a replacement main with an unrelated live WAL must fail closed; read {value:?}"
            );
        }
    }
    drop(original);
    // A different process must reject the stale WAL even after the old owner
    // closes; an in-memory registry would not establish recovery ownership.
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "private_fs::platform::database::lifetime_tests::replaced_main_cannot_replay_another_database_wal"])
        .env_clear().env(ROOT, &root).output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn idle_live_generation_prevents_rebinding_even_without_side_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let original = open(&root);
    original
        .execute_batch("CREATE TABLE entries(value)")
        .unwrap();
    std::fs::rename(root.join(DATABASE), root.join("saved.sqlite")).unwrap();
    let owner_before = std::fs::read(root.join("database.sqlite-owner")).unwrap();
    assert!(
        open_database(
            &root,
            &root.join(DATABASE),
            DatabaseMode::Create,
            Duration::ZERO
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read(root.join("database.sqlite-owner")).unwrap(),
        owner_before
    );
    drop(original);
    let replacement = open(&root);
    replacement
        .execute_batch("CREATE TABLE entries(value); INSERT INTO entries VALUES(1)")
        .unwrap();
    assert_eq!(count(&replacement), 1);
}

#[test]
fn incomplete_owner_can_reinitialize_only_without_recovery_files() {
    use std::os::unix::fs::PermissionsExt as _;

    for side in [None, Some("-journal"), Some("-wal"), Some("-shm")] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        drop(open(&root));
        let owner = root.join("database.sqlite-owner");
        std::fs::write(&owner, b"partial").unwrap();
        if let Some(suffix) = side {
            let path = root.join(format!("{DATABASE}{suffix}"));
            std::fs::write(&path, b"recovery").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let result = open_database(
            &root,
            &root.join(DATABASE),
            DatabaseMode::Create,
            Duration::ZERO,
        );
        if let Some(suffix) = side {
            assert!(result.is_err());
            assert_eq!(std::fs::read(&owner).unwrap(), b"partial");
            assert_eq!(
                std::fs::read(root.join(format!("{DATABASE}{suffix}"))).unwrap(),
                b"recovery"
            );
        } else {
            let connection = result.unwrap();
            connection
                .execute_batch("CREATE TABLE entries(value)")
                .unwrap();
            assert_eq!(count(&connection), 0);
        }
    }
}

#[test]
fn native_and_private_sqlite_writers_exclude_each_other_across_processes() {
    const ROOT: &str = "CRAB_TEST_DATABASE_LOCK_ROOT";
    const OWNER: &str = "CRAB_TEST_DATABASE_LOCK_OWNER";
    if let Some(root) = std::env::var_os(ROOT) {
        let root = PathBuf::from(root);
        let error = if std::env::var(OWNER).unwrap() == "native" {
            let connection = Connection::open(root.join(DATABASE)).unwrap();
            connection.busy_timeout(Duration::ZERO).unwrap();
            connection.execute_batch("BEGIN IMMEDIATE").unwrap_err()
        } else {
            let connection = open_database(
                &root,
                &root.join(DATABASE),
                DatabaseMode::ReadWrite,
                Duration::ZERO,
            )
            .unwrap();
            connection.execute_batch("BEGIN IMMEDIATE").unwrap_err()
        };
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy)
        );
        return;
    }
    for journal in ["DELETE", "WAL"] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let connection = open(&root);
        connection
            .pragma_update(None, "journal_mode", journal)
            .unwrap();
        connection
            .execute_batch("CREATE TABLE entries(value)")
            .unwrap();
        drop(connection);
        for child_owner in ["native", "private"] {
            let private = (child_owner == "native").then(|| open(&root));
            let native =
                (child_owner == "private").then(|| Connection::open(root.join(DATABASE)).unwrap());
            let connection: &Connection = private.as_deref().or(native.as_ref()).unwrap();
            connection
                .execute_batch("BEGIN IMMEDIATE; INSERT INTO entries VALUES(1)")
                .unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "private_fs::platform::database::lifetime_tests::native_and_private_sqlite_writers_exclude_each_other_across_processes"])
                .env_clear().env(ROOT, &root).env(OWNER, child_owner).output().unwrap();
            assert!(
                output.status.success(),
                "{journal}/{child_owner}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            connection.execute_batch("COMMIT").unwrap();
        }
        assert_eq!(count(&open(&root)), 2);
    }
}

#[test]
fn killed_writer_recovers_only_committed_rows_in_both_journal_modes() {
    use std::os::unix::process::ExitStatusExt as _;

    const ROOT: &str = "CRAB_TEST_DATABASE_KILL_ROOT";
    if let Some(root) = std::env::var_os(ROOT) {
        let connection = open(&PathBuf::from(root));
        connection.execute_batch("PRAGMA cache_size=4; BEGIN IMMEDIATE; WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<256) INSERT INTO entries SELECT zeroblob(4096) FROM numbers;").unwrap();
        println!("writer-ready");
        std::io::stdout().flush().unwrap();
        let mut buffer = String::new();
        std::io::stdin().read_line(&mut buffer).unwrap();
        panic!("parent must kill the writer before its connection drops");
    }
    for journal in ["DELETE", "WAL"] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let connection = open(&root);
        connection
            .pragma_update(None, "journal_mode", journal)
            .unwrap();
        connection
            .execute_batch("CREATE TABLE entries(value); INSERT INTO entries VALUES('committed')")
            .unwrap();
        drop(connection);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "private_fs::platform::database::lifetime_tests::killed_writer_recovers_only_committed_rows_in_both_journal_modes", "--nocapture"])
            .env_clear().env(ROOT, &root).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        // Child::wait closes its stdin. Retain both pipes until reaping so an
        // EOF cannot unwind/rollback the writer before SIGKILL is delivered.
        let _stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.as_mut().unwrap();
        let mut ready = false;
        for line in std::io::BufReader::new(stdout).lines() {
            if line.unwrap() == "writer-ready" {
                ready = true;
                break;
            }
        }
        if !ready {
            let _ = child.kill();
        }
        assert!(ready, "{journal}: child failed before dirty-page spill");
        child.kill().unwrap();
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
        let recovered = open(&root);
        assert_eq!(count(&recovered), 1, "{journal}");
        assert_eq!(
            recovered
                .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok",
            "{journal}"
        );
    }
}

#[test]
fn pending_writer_excludes_new_readers_until_existing_readers_finish() {
    use super::locking::DatabaseLock;
    use rusqlite::ffi;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("locks");
    std::fs::write(&path, b"").unwrap();
    let file = || {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap()
    };
    let reader = file();
    let writer = file();
    let newcomer = file();
    let mut reader_lock = DatabaseLock::default();
    let mut writer_lock = DatabaseLock::default();
    let mut newcomer_lock = DatabaseLock::default();
    reader_lock
        .acquire(&reader, ffi::SQLITE_LOCK_SHARED)
        .unwrap();
    writer_lock
        .acquire(&writer, ffi::SQLITE_LOCK_SHARED)
        .unwrap();
    writer_lock
        .acquire(&writer, ffi::SQLITE_LOCK_RESERVED)
        .unwrap();
    assert_eq!(
        writer_lock.acquire(&writer, ffi::SQLITE_LOCK_EXCLUSIVE),
        Err(ffi::SQLITE_BUSY)
    );
    assert_eq!(writer_lock.level, ffi::SQLITE_LOCK_PENDING);
    assert_eq!(
        newcomer_lock.acquire(&newcomer, ffi::SQLITE_LOCK_SHARED),
        Err(ffi::SQLITE_BUSY)
    );
    reader_lock.release(&reader, ffi::SQLITE_LOCK_NONE).unwrap();
    writer_lock
        .acquire(&writer, ffi::SQLITE_LOCK_EXCLUSIVE)
        .unwrap();
    writer_lock
        .release(&writer, ffi::SQLITE_LOCK_SHARED)
        .unwrap();
    newcomer_lock
        .acquire(&newcomer, ffi::SQLITE_LOCK_SHARED)
        .unwrap();
}

#[test]
fn independent_wal_writers_retain_every_committed_transaction() {
    use std::io::Read as _;
    const ROOT: &str = "CRAB_TEST_WAL_WRITER_ROOT";
    if let Some(root) = std::env::var_os(ROOT) {
        let connection = open(&PathBuf::from(root));
        println!("writer-ready");
        std::io::stdout().flush().unwrap();
        std::io::stdin().read_exact(&mut [0]).unwrap();
        for _ in 0..8 {
            connection
                .execute_batch("BEGIN IMMEDIATE; INSERT INTO entries VALUES(1); COMMIT")
                .unwrap();
        }
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let connection = open(&root);
    connection
        .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE entries(value)")
        .unwrap();
    drop(connection);
    let mut children: Vec<_> = (0..8).map(|_| {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "private_fs::platform::database::lifetime_tests::independent_wal_writers_retain_every_committed_transaction", "--nocapture"])
            .env_clear().env(ROOT, &root).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap()
    }).collect();
    for child in &mut children {
        let stdout = child.stdout.as_mut().unwrap();
        let mut ready = false;
        for line in std::io::BufReader::new(stdout).lines() {
            if line.unwrap() == "writer-ready" {
                ready = true;
                break;
            }
        }
        assert!(ready, "child failed before writer barrier");
    }
    for child in &mut children {
        child.stdin.take().unwrap().write_all(b"x").unwrap();
    }
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    assert_eq!(count(&open(&root)), 64);
}

#[test]
fn wal_readers_share_an_index_spanning_multiple_mapping_regions() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("cache");
    let writer = open(&root);
    writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; PRAGMA cache_size=4; CREATE TABLE entries(value); WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<4200) INSERT INTO entries SELECT zeroblob(4096) FROM numbers;").unwrap();
    assert!(
        std::fs::metadata(root.join("database.sqlite-shm"))
            .unwrap()
            .len()
            > 32 * 1024
    );
    let reader = open(&root);
    assert_eq!(count(&reader), 4200);
    drop(writer);
    assert_eq!(count(&reader), 4200);
    drop(reader);
    assert_eq!(count(&open(&root)), 4200);
}
