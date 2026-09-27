//! Demand-read work on fragmented immutable roots.

use super::*;

struct IsolatedExecutor;

impl crab_ltx::environment::Executor for IsolatedExecutor {
    fn dispatch(&self, job: Box<dyn FnOnce() + Send>) -> std::io::Result<()> {
        tokio::task::spawn_blocking(job);
        Ok(())
    }

    fn start_worker(
        &self,
        job: Box<dyn FnOnce() + Send>,
    ) -> std::io::Result<Box<dyn crab_ltx::environment::Worker>> {
        std::thread::Builder::new()
            .spawn(job)
            .map(|thread| Box::new(thread) as Box<dyn crab_ltx::environment::Worker>)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fragmented_snapshot_demand_reads_do_not_refetch_cached_frames() {
    verify_read_ahead(Arc::new(InMemory::new()), "read-ahead").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the RustFS environment documented in examples/README.md"]
async fn rustfs_fragmented_snapshot_demand_reads_do_not_refetch_cached_frames() {
    let endpoint = std::env::var("CRAB_LTX_TEST_ENDPOINT").unwrap();
    let store = crab_storage::build_explicit_store(
        &std::env::var("CRAB_LTX_TEST_BUCKET").unwrap(),
        crab_storage::ObjectStoreCredentials::Aws {
            access_key_id: std::env::var("AWS_ACCESS_KEY_ID").unwrap(),
            secret_access_key: std::env::var("AWS_SECRET_ACCESS_KEY").unwrap(),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&endpoint),
        endpoint.starts_with("http://"),
    )
    .unwrap();
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let prefix = format!("crab-ltx-tests/read-ahead/{run}");
    verify_read_ahead(store.inner().clone(), &prefix).await;
    eprintln!("RustFS demand read-ahead passed: {prefix}");
}

async fn verify_read_ahead(backend: Arc<dyn ObjectStore>, prefix: &str) {
    for page_size in [512, 4096] {
        let directory = tempfile::TempDir::new().unwrap();
        let source = directory.path().join("source.sqlite");
        let connection = crab_ltx::rusqlite::Connection::open(&source).unwrap();
        connection
            .pragma_update(None, "page_size", page_size)
            .unwrap();
        connection.execute_batch("VACUUM").unwrap();
        drop(connection);
        let mut writer = Db::open(&source, Limits::default()).unwrap();
        let expected = writer
            .transaction(|tx| {
                tx.execute_batch(
                    "CREATE TABLE payload(value BLOB NOT NULL); \
                 CREATE TABLE counter(value INTEGER NOT NULL); INSERT INTO counter VALUES(0)",
                )?;
                let mut expected = blake3::Hasher::new();
                for row in 0u32..128 {
                    let mut bytes = vec![0; 4096];
                    blake3::Hasher::new()
                        .update(&row.to_be_bytes())
                        .finalize_xof()
                        .fill(&mut bytes);
                    tx.execute("INSERT INTO payload VALUES(?1)", [bytes.as_slice()])?;
                    expected.update(&bytes);
                }
                Ok(expected.finalize())
            })
            .unwrap();
        let store = InstrumentedStore::new(backend.clone(), Duration::ZERO);
        // A private driver keeps unrelated concurrent tests from evicting this
        // sub-MiB working set while its exact origin work is measured.
        let host = Host::default().with_executor(Arc::new(IsolatedExecutor));
        let replica = CellReplica::new(
            CellStorageLayout::new(
                Store::new(store.clone()),
                Path::from(format!("{prefix}/{page_size}")),
                [161; 16],
            ),
            [162; 32],
            [163; 16],
            Limits::default(),
        )
        .unwrap()
        .with_host(host);
        let root = replica
            .prepare(None, &writer.capture().unwrap(), 1, 1)
            .await
            .unwrap();
        writer
            .transaction(|tx| tx.execute_batch("UPDATE counter SET value = value + 1"))
            .unwrap();
        let next = replica
            .prepare(Some(&root.root()), &writer.capture().unwrap(), 2, 1)
            .await
            .unwrap();
        writer.close().unwrap();
        let verified = replica.open_root(&next.root()).await.unwrap();
        assert_eq!(verified.paged().page_size(), page_size);
        tokio::task::spawn_blocking(move || {
            store.reset();
            let view = verified.open_read_only(&directory.path().join("view.sqlite")).unwrap();
            assert_eq!(payload_hash(&view.connection().unwrap()), expected);
            let ranges = store.stats.ranges.lock().unwrap().clone();
            assert!(!ranges.is_empty());
            for (index, (path, range)) in ranges.iter().enumerate() {
                assert!(ranges[..index].iter().all(|(previous_path, previous)| {
                    previous_path != path || previous.end <= range.start || range.end <= previous.start
                }), "read-ahead refetched a cached immutable frame at {page_size}-byte pages: {ranges:?}");
            }
            let bytes: u64 = ranges.iter().map(|(_, range)| range.end - range.start).sum();
            eprintln!("Demand scan page_size={page_size} ranges={} bytes={bytes}", ranges.len());
            store.reset();
            assert_eq!(payload_hash(&view.connection().unwrap()), expected);
            assert!(store.stats.ranges.lock().unwrap().is_empty());
        }).await.unwrap();
    }
}

fn payload_hash(connection: &crab_ltx::rusqlite::Connection) -> blake3::Hash {
    let mut statement = connection
        .prepare("SELECT value FROM payload ORDER BY rowid")
        .unwrap();
    let mut rows = statement.query([]).unwrap();
    let mut hash = blake3::Hasher::new();
    while let Some(row) = rows.next().unwrap() {
        hash.update(&row.get::<_, Vec<u8>>(0).unwrap());
    }
    hash.finalize()
}
