//! Sparse writer publication and hydration coalescing.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn exact_cell_root_opens_sparse_writer_and_publishes_incrementally() {
    let source = tempfile::TempDir::new().unwrap();
    let source_path = source.path().join("source.sqlite");
    let mut writer = Db::open(&source_path, Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE messages(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO messages(body) VALUES ('first'), ('second');\
                 CREATE TABLE payload(value BLOB NOT NULL);\
                 INSERT INTO payload VALUES(randomblob(2000000))",
            )
        })
        .unwrap();
    let store = Store::new(Arc::new(InMemory::new()));
    let replica = replica(store, [8; 32], [9; 16]);
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 0, 3)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();
    source.close().unwrap();

    let active = tempfile::TempDir::new().unwrap();
    let active_path = active.path().join("active.sqlite");
    let writable = replica
        .open_root(&root)
        .await
        .unwrap()
        .paged()
        .prepare_writable(&active_path)
        .await
        .unwrap();
    let checksum_bytes = std::fs::metadata(checksum_path(&active_path))
        .unwrap()
        .len();
    assert!(checksum_bytes > 0 && checksum_bytes.is_multiple_of(8));
    let mut writer = tokio::task::spawn_blocking(move || writable.open_writable(&active_path))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(writer.position(), root.position);
    assert!(!writer.hydration().unwrap().unwrap().complete());
    writer
        .transaction(|transaction| {
            assert_eq!(
                transaction.query_row("SELECT count(*) FROM messages", [], |row| row
                    .get::<_, u32>(0))?,
                2
            );
            transaction.execute("INSERT INTO messages(body) VALUES ('third')", [])?;
            Ok(())
        })
        .unwrap();
    let next = replica
        .prepare(Some(&root), &writer.capture().unwrap(), 1, 3)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();
    assert_eq!(next.commit_sequence, 1);

    let replacement = tempfile::TempDir::new().unwrap();
    let replacement_path = replacement.path().join("replacement.sqlite");
    let writable = replica
        .open_root(&next)
        .await
        .unwrap()
        .paged()
        .prepare_writable(&replacement_path)
        .await
        .unwrap();
    let mut replacement =
        tokio::task::spawn_blocking(move || writable.open_writable(&replacement_path))
            .await
            .unwrap()
            .unwrap();
    let count = replacement
        .transaction(|transaction| {
            transaction.query_row("SELECT count(*) FROM messages", [], |row| {
                row.get::<_, u32>(0)
            })
        })
        .unwrap();
    assert_eq!(count, 3);
    replacement.close().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn unproved_sparse_cut_and_lost_sidecar_do_not_advance_the_selected_root() {
    let directory = tempfile::TempDir::new().unwrap();
    let source = directory.path().join("source.sqlite");
    let mut writer = Db::open(&source, Limits::default()).unwrap();
    writer
        .transaction(|tx| {
            tx.execute_batch("CREATE TABLE values_(v INTEGER); INSERT INTO values_ VALUES(1)")
        })
        .unwrap();
    let replica = replica(Store::new(Arc::new(InMemory::new())), [91; 32], [92; 16]);
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();

    let active = directory.path().join("active.sqlite");
    let writable = replica
        .open_root(&root)
        .await
        .unwrap()
        .paged()
        .prepare_writable(&active)
        .await
        .unwrap();
    let mut writer = writable.open_writable(&active).unwrap();
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO values_ VALUES(2)"))
        .unwrap();
    let unproved = writer.capture_deferred().unwrap();
    assert!(unproved.position.txid > root.position.txid);
    writer.close().unwrap();
    std::fs::remove_file(checksum_path(&active)).unwrap();
    std::fs::remove_file(&active).unwrap();

    let recovered = directory.path().join("recovered.sqlite");
    let writable = replica
        .open_root(&root)
        .await
        .unwrap()
        .paged()
        .prepare_writable(&recovered)
        .await
        .unwrap();
    let mut writer = writable.open_writable(&recovered).unwrap();
    assert_eq!(writer.position(), root.position);
    let count: i64 = writer
        .query_with(|db| db.query_row("SELECT count(*) FROM values_", [], |row| row.get(0)))
        .unwrap();
    assert_eq!(count, 1);
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO values_ VALUES(3)"))
        .unwrap();
    assert_eq!(
        writer.capture_deferred().unwrap().position.txid,
        root.position.txid + 1
    );
    writer.close().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn sparse_crash_writer() {
    use std::io::{Read, Write};

    let Ok(store_path) = std::env::var("CRAB_LTX_SPARSE_CRASH_STORE") else {
        return;
    };
    let active = std::path::PathBuf::from(std::env::var("CRAB_LTX_SPARSE_CRASH_ACTIVE").unwrap());
    let digest = std::env::var("CRAB_LTX_SPARSE_CRASH_DIGEST").unwrap();
    let mut root_digest = [0; 32];
    for (index, byte) in root_digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digest[index * 2..index * 2 + 2], 16).unwrap();
    }
    let root = RootRef {
        cell: [91; 32],
        incarnation: [92; 16],
        digest: root_digest,
        position: crab_ltx::Position {
            txid: std::env::var("CRAB_LTX_SPARSE_CRASH_TXID")
                .unwrap()
                .parse()
                .unwrap(),
            checksum: std::env::var("CRAB_LTX_SPARSE_CRASH_CHECKSUM")
                .unwrap()
                .parse()
                .unwrap(),
        },
        commit_sequence: 1,
    };
    let store = Store::new(Arc::new(
        object_store::local::LocalFileSystem::new_with_prefix(store_path).unwrap(),
    ));
    let replica = replica(store, root.cell, root.incarnation);
    let writable = replica
        .open_root(&root)
        .await
        .unwrap()
        .paged()
        .prepare_writable(&active)
        .await
        .unwrap();
    let mut writer = writable.open_writable(&active).unwrap();
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO values_ VALUES(2)"))
        .unwrap();
    let cut = writer.capture_deferred().unwrap();
    assert!(cut.position.txid > root.position.txid);
    println!("SPARSE-CUT {}", cut.position.txid);
    std::io::stdout().flush().unwrap();
    let mut byte = [0];
    std::io::stdin().read_exact(&mut byte).unwrap();
    panic!("parent must kill the sparse writer");
}

#[tokio::test(flavor = "multi_thread")]
async fn process_killed_sparse_writer_restores_selected_root_and_commits_again() {
    use std::io::BufRead;
    use std::process::{Command, Stdio};

    let store_dir = tempfile::TempDir::new().unwrap();
    let source_dir = tempfile::TempDir::new().unwrap();
    let store = Store::new(Arc::new(
        object_store::local::LocalFileSystem::new_with_prefix(store_dir.path()).unwrap(),
    ));
    let replica = replica(store, [91; 32], [92; 16]);
    let mut source = Db::open(&source_dir.path().join("source.sqlite"), Limits::default()).unwrap();
    source
        .transaction(|tx| {
            tx.execute_batch("CREATE TABLE values_(v INTEGER); INSERT INTO values_ VALUES(1)")
        })
        .unwrap();
    let root = replica
        .prepare(None, &source.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    source.close().unwrap();

    let active = source_dir.path().join("active.sqlite");
    let digest: String = root
        .digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "cell::roots::sparse::sparse_crash_writer",
            "--exact",
            "--nocapture",
        ])
        .env("CRAB_LTX_SPARSE_CRASH_STORE", store_dir.path())
        .env("CRAB_LTX_SPARSE_CRASH_ACTIVE", &active)
        .env("CRAB_LTX_SPARSE_CRASH_DIGEST", digest)
        .env("CRAB_LTX_SPARSE_CRASH_TXID", root.position.txid.to_string())
        .env(
            "CRAB_LTX_SPARSE_CRASH_CHECKSUM",
            root.position.checksum.to_string(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut child = KillOnDrop(child);
    let _stdin = child.0.stdin.take().unwrap();
    let stdout = child.0.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if line.contains("SPARSE-CUT") {
                let _ = send.send(());
                break;
            }
        }
    });
    receive
        .recv_timeout(std::time::Duration::from_secs(15))
        .unwrap();
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    source_dir.close().unwrap();

    let recovered_dir = tempfile::TempDir::new().unwrap();
    let recovered = recovered_dir.path().join("recovered.sqlite");
    let writable = replica
        .open_root(&root)
        .await
        .unwrap()
        .paged()
        .prepare_writable(&recovered)
        .await
        .unwrap();
    let mut writer = writable.open_writable(&recovered).unwrap();
    let count: i64 = writer
        .query_with(|db| db.query_row("SELECT count(*) FROM values_", [], |row| row.get(0)))
        .unwrap();
    assert_eq!(count, 1);
    writer
        .transaction(|tx| tx.execute_batch("INSERT INTO values_ VALUES(3)"))
        .unwrap();
    let next = writer.capture_deferred().unwrap();
    assert_eq!(next.position.txid, root.position.txid + 1);
    let published = replica.prepare(Some(&root), &next, 2, 1).await.unwrap();
    assert_eq!(published.root().position, next.position);
    writer.close().unwrap();
}

struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn sparse_hydration_coalesces_contiguous_cell_frames() {
    let directory = tempfile::TempDir::new().unwrap();
    let mut writer = Db::open(&directory.path().join("cell.sqlite"), Limits::default()).unwrap();
    writer
        .transaction(|transaction| {
            transaction.execute_batch(
                "CREATE TABLE payload(value BLOB NOT NULL);\
                 INSERT INTO payload VALUES(randomblob(2000000))",
            )
        })
        .unwrap();
    let range_reads = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&range_reads);
    let store =
        Store::new(Arc::new(InMemory::new())).with_read_request_observer(Arc::new(move |kind| {
            if kind == StorageReadKind::Range {
                observed.fetch_add(1, Ordering::SeqCst);
            }
        }));
    let replica = replica(store, [83; 32], [84; 16]);
    let root = replica
        .prepare(None, &writer.capture().unwrap(), 1, 1)
        .await
        .unwrap()
        .root();
    writer.close().unwrap();

    let destination = directory.path().join("sparse.sqlite");
    let writable = replica
        .open_root(&root)
        .await
        .unwrap()
        .paged()
        .prepare_writable(&destination)
        .await
        .unwrap();
    range_reads.store(0, Ordering::SeqCst);
    let (before, after, reads) = tokio::task::spawn_blocking(move || {
        let mut writer = writable.open_writable(&destination).unwrap();
        let before = writer.hydration().unwrap().unwrap();
        let requests = range_reads.load(Ordering::SeqCst);
        let after = writer.hydrate_step(320).unwrap();
        let reads = range_reads.load(Ordering::SeqCst) - requests;
        writer.close().unwrap();
        (before, after, reads)
    })
    .await
    .unwrap();
    let hydrated = after.resolved - before.resolved;
    assert!(hydrated >= 256);
    assert!(reads < u64::from(hydrated));
}
