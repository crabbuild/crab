//! Reports the immutable publication cost of one Cell command.
//!
//! Each command commits one SQLite transaction, captures one LTX cut, and
//! publishes one immutable Cell root through `CellReplica`. The runner reports
//! objects, bytes, and wall time per command so the runtime's per-command
//! object-store budget has a measured baseline.
//!
//! The default store is in-memory and synchronous, so its latency is a floor.
//! `--endpoint` (with `--bucket`, `--access-key`, and `--secret-key`) points the
//! same workload at an S3-compatible provider such as RustFS. Object and byte
//! counts are provider independent; only latency changes.

mod filesystem;
mod storage;

use crab_ltx::{CellReplica, CellStorageLayout, Host, Limits};
use crab_storage::{ObjectStoreCredentials, Store};
use object_store::{memory::InMemory, path::Path, ObjectStore};
use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct Config {
    payload_bytes: usize,
    commands: usize,
    warmup: usize,
    sparse: bool,
    random_payload: bool,
    max_capture_bytes: Option<u64>,
    endpoint: Option<String>,
    bucket: String,
    access_key: String,
    secret_key: String,
}

#[derive(Clone, Debug, Serialize)]
struct Sample {
    command: usize,
    objects: u64,
    bytes: u64,
    commit_us: u64,
    capture_us: u64,
    capture_preparation_us: u64,
    capture_schema_check_us: u64,
    capture_wal_existence_us: u64,
    capture_position_resolution_us: u64,
    capture_wal_read_us: u64,
    capture_page_collection_us: u64,
    capture_verification_us: u64,
    capture_encode_us: u64,
    capture_local_write_us: u64,
    capture_fsync_us: u64,
    capture_parent_sync_us: u64,
    capture_checkpoint_us: u64,
    checksum_sync_calls: u64,
    checksum_sync_us: u64,
    wal_read_bytes: u64,
    wal_image_bytes: u64,
    wal_snapshot_reads: u32,
    wal_full_reads: u32,
    elapsed_us: u64,
    prune_us: u64,
    captured_bytes: u64,
    preparation_io: Vec<storage::BackendCost>,
}

#[derive(Debug, Serialize)]
struct Report {
    implementation: &'static str,
    store: &'static str,
    workload: &'static str,
    sqlite_version: &'static str,
    payload_bytes: usize,
    payload_pattern: &'static str,
    measured_commands: usize,
    restored_rows: usize,
    bootstrap_capture_us: u64,
    bootstrap_parent_sync_us: u64,
    objects_per_command: u64,
    bytes_per_command: u64,
    objects_p95: u64,
    bytes_p95: u64,
    elapsed_us_p50: u64,
    elapsed_us_p95: u64,
    elapsed_us_p99: u64,
    elapsed_us_max: u64,
    commit_us_p50: u64,
    commit_us_p95: u64,
    capture_us_p50: u64,
    capture_us_p95: u64,
    checksum_sync_calls: u64,
    checksum_sync_us_p50: u64,
    checksum_sync_us_p95: u64,
    wal_read_bytes: u64,
    wal_image_bytes_max: u64,
    wal_snapshot_reads: u64,
    wal_full_reads: u64,
    samples: Vec<Sample>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_args(std::env::args().skip(1))?;
    let directory = tempfile::TempDir::new()?;
    let database_path = directory.path().join("cell.sqlite");
    let mut limits = Limits::default();
    if let Some(max_capture_bytes) = config.max_capture_bytes {
        limits.max_capture_bytes = max_capture_bytes;
    }
    let (store, store_label, prefix) = open_store(&config)?;
    let backend = Arc::new(storage::StorageCosts::default());
    let store = store.with_storage_observer(backend.clone());
    let syncs = Arc::new(filesystem::ChecksumSyncs::default());
    let host = Host::default().with_filesystem(Arc::new(filesystem::MeasuredFileSystem {
        syncs: syncs.clone(),
    }));
    let replica = CellReplica::new(
        CellStorageLayout::new(store, Path::from(prefix.as_str()), [7; 16]),
        [5; 32],
        [6; 16],
        limits,
    )?
    .with_host(host);
    let mut database = replica.open_new(&database_path)?;

    database.transaction(|transaction| {
        transaction
            .execute_batch("CREATE TABLE payload(id INTEGER PRIMARY KEY, value BLOB NOT NULL)")
    })?;
    let bootstrap_started = Instant::now();
    let first = database.capture()?;
    let bootstrap_capture_us = bootstrap_started.elapsed().as_micros() as u64;
    let bootstrap_parent_sync_us = first.timing.parent_sync_nanos / 1_000;
    let mut root = Some(replica.prepare(None, &first, 1, 1).await?.root());
    database.prune_captured(&first)?;
    let _bootstrap = replica.take_publication_cost();

    let mut database = if config.sparse {
        database.close()?;
        let active = directory.path().join("sparse.sqlite");
        let writable = replica
            .open_root(root.as_ref().ok_or("bootstrap root missing")?)
            .await?
            .paged()
            .prepare_writable(&active)
            .await?;
        tokio::task::spawn_blocking(move || writable.open_writable(&active)).await??
    } else {
        database
    };
    let _activation_syncs = syncs.take();

    let mut samples = Vec::with_capacity(config.commands);
    for command in 0..config.commands {
        let payload = payload(command, config.payload_bytes, config.random_payload);
        let commit_started = Instant::now();
        database.transaction(|transaction| {
            transaction.execute(
                "INSERT INTO payload(value) VALUES(?1)",
                [payload.as_slice()],
            )?;
            Ok(())
        })?;
        let commit_us = commit_started.elapsed().as_micros() as u64;
        let capture_started = Instant::now();
        // Compare activation histories under the runtime's same capture and
        // cleanup boundaries; immediate capture would add another barrier.
        let batch = database.capture_deferred()?;
        let capture_us = capture_started.elapsed().as_micros() as u64;
        let (checksum_sync_calls, checksum_sync_us) = syncs.take();
        // Exclude bootstrap, activation, and sparse reads during commit/capture.
        let _prior_io = backend.take();
        let started = Instant::now();
        let prepared = replica
            .prepare(root.as_ref(), &batch, command as u64 + 2, 1)
            .await?;
        let elapsed = started.elapsed();
        let preparation_io = backend.take();
        let cost = replica.take_publication_cost();
        root = Some(prepared.root());
        let started = Instant::now();
        database.prune_captured(&batch)?;
        let prune_us = started.elapsed().as_micros() as u64;
        if command >= config.warmup {
            samples.push(Sample {
                command,
                objects: cost.objects,
                bytes: cost.bytes,
                commit_us,
                capture_us,
                capture_preparation_us: batch.timing.preparation_nanos / 1_000,
                capture_schema_check_us: batch.timing.schema_check_nanos / 1_000,
                capture_wal_existence_us: batch.timing.wal_existence_nanos / 1_000,
                capture_position_resolution_us: batch.timing.position_resolution_nanos / 1_000,
                capture_wal_read_us: batch.timing.wal_read_nanos / 1_000,
                capture_page_collection_us: batch.timing.page_collection_nanos / 1_000,
                capture_verification_us: batch.timing.verification_nanos / 1_000,
                capture_encode_us: batch.timing.encode_nanos / 1_000,
                capture_local_write_us: batch.timing.local_write_nanos / 1_000,
                capture_fsync_us: batch.timing.fsync_nanos / 1_000,
                capture_parent_sync_us: batch.timing.parent_sync_nanos / 1_000,
                capture_checkpoint_us: batch.timing.checkpoint_nanos / 1_000,
                checksum_sync_calls,
                checksum_sync_us,
                wal_read_bytes: batch.timing.wal_read_bytes,
                wal_image_bytes: batch.timing.wal_image_bytes,
                wal_snapshot_reads: batch.timing.wal_snapshot_reads,
                wal_full_reads: batch.timing.wal_full_reads,
                elapsed_us: elapsed.as_micros() as u64,
                prune_us,
                captured_bytes: batch
                    .segments
                    .iter()
                    .map(|segment| segment.info().size_bytes)
                    .sum(),
                preparation_io,
            });
        }
    }
    database.close()?;

    // Validate the final selected root independently of the writer and its
    // pruned local cuts. This work stays outside all measured phases.
    let restored = directory.path().join("restored.sqlite");
    replica
        .open_root(root.as_ref().ok_or("final root missing")?)
        .await?
        .restore(&restored)
        .await?;
    let connection = crab_ltx::rusqlite::Connection::open_with_flags(
        restored,
        crab_ltx::rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut statement = connection.prepare("SELECT id, value FROM payload ORDER BY id")?;
    let mut rows = statement.query([])?;
    let mut restored_rows = 0;
    while let Some(row) = rows.next()? {
        let id: u64 = row.get(0)?;
        let value: Vec<u8> = row.get(1)?;
        if id != restored_rows as u64 + 1
            || value != payload(restored_rows, config.payload_bytes, config.random_payload)
        {
            return Err("restored root changed a committed payload".into());
        }
        restored_rows += 1;
    }
    if restored_rows != config.commands {
        return Err("restored root lost committed payloads".into());
    }

    if samples.is_empty() {
        return Err("at least one measured command is required".into());
    }
    let report = summarize(
        config,
        store_label,
        &samples,
        bootstrap_capture_us,
        bootstrap_parent_sync_us,
        restored_rows,
    );
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Opens the configured store and returns it with its label and prefix.
fn open_store(
    config: &Config,
) -> Result<(Store, &'static str, String), Box<dyn std::error::Error>> {
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    let prefix = format!("cost/{}-{run}", std::process::id());
    let Some(endpoint) = config.endpoint.as_deref() else {
        let backend: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        return Ok((Store::new(backend), "object_store in-memory", prefix));
    };
    let allow_http = match endpoint.split_once("://") {
        Some(("http", _)) => true,
        Some(("https", _)) => false,
        _ => return Err("endpoint must use http or https".into()),
    };
    let store = crab_storage::build_explicit_store(
        &config.bucket,
        ObjectStoreCredentials::Aws {
            access_key_id: config.access_key.clone(),
            secret_access_key: config.secret_key.clone(),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(endpoint),
        allow_http,
    )?;
    // The label is bounded and static so the report stays comparable.
    let label = if allow_http {
        "S3-compatible endpoint (http)"
    } else {
        "S3-compatible endpoint (https)"
    };
    Ok((store, label, prefix))
}

fn payload(command: usize, bytes: usize, random: bool) -> Vec<u8> {
    if !random {
        // Retain the periodic fixture so historical cost rows are reproducible.
        return (0..bytes)
            .map(|index| ((command * 131 + index) % 251) as u8)
            .collect();
    }
    let mut state = (command as u64).wrapping_add(1);
    let mut payload = vec![0; bytes];
    for chunk in payload.chunks_mut(8) {
        // A fixed command seed makes this high-entropy workload repeatable.
        // This generator is only test data, never a security primitive.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
    }
    payload
}

fn percentile(values: &[u64], percent: usize) -> u64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[(sorted.len() * percent).div_ceil(100).saturating_sub(1)]
}

fn summarize(
    config: Config,
    store: &'static str,
    samples: &[Sample],
    bootstrap_capture_us: u64,
    bootstrap_parent_sync_us: u64,
    restored_rows: usize,
) -> Report {
    let objects: Vec<u64> = samples.iter().map(|sample| sample.objects).collect();
    let bytes: Vec<u64> = samples.iter().map(|sample| sample.bytes).collect();
    let elapsed: Vec<u64> = samples.iter().map(|sample| sample.elapsed_us).collect();
    let commits: Vec<u64> = samples.iter().map(|sample| sample.commit_us).collect();
    let captures: Vec<u64> = samples.iter().map(|sample| sample.capture_us).collect();
    let checksum_syncs: Vec<u64> = samples
        .iter()
        .map(|sample| sample.checksum_sync_us)
        .collect();
    let total_objects: u64 = objects.iter().sum();
    let total_bytes: u64 = bytes.iter().sum();
    Report {
        implementation: "crab-ltx CellReplica",
        store,
        sqlite_version: crab_ltx::rusqlite::version(),
        workload: if config.sparse {
            "sparse deferred capture"
        } else {
            "fresh deferred capture"
        },
        payload_bytes: config.payload_bytes,
        payload_pattern: if config.random_payload {
            "xorshift64-command-seeded"
        } else {
            "periodic-251"
        },
        measured_commands: samples.len(),
        restored_rows,
        bootstrap_capture_us,
        bootstrap_parent_sync_us,
        objects_per_command: total_objects / samples.len() as u64,
        bytes_per_command: total_bytes / samples.len() as u64,
        objects_p95: percentile(&objects, 95),
        bytes_p95: percentile(&bytes, 95),
        elapsed_us_p50: percentile(&elapsed, 50),
        elapsed_us_p95: percentile(&elapsed, 95),
        elapsed_us_p99: percentile(&elapsed, 99),
        elapsed_us_max: *elapsed.iter().max().unwrap_or(&0),
        commit_us_p50: percentile(&commits, 50),
        commit_us_p95: percentile(&commits, 95),
        capture_us_p50: percentile(&captures, 50),
        capture_us_p95: percentile(&captures, 95),
        checksum_sync_calls: samples
            .iter()
            .map(|sample| sample.checksum_sync_calls)
            .sum(),
        checksum_sync_us_p50: percentile(&checksum_syncs, 50),
        checksum_sync_us_p95: percentile(&checksum_syncs, 95),
        wal_read_bytes: samples.iter().map(|sample| sample.wal_read_bytes).sum(),
        wal_image_bytes_max: samples
            .iter()
            .map(|sample| sample.wal_image_bytes)
            .max()
            .unwrap_or(0),
        wal_snapshot_reads: samples
            .iter()
            .map(|sample| u64::from(sample.wal_snapshot_reads))
            .sum(),
        wal_full_reads: samples
            .iter()
            .map(|sample| u64::from(sample.wal_full_reads))
            .sum(),
        samples: samples.to_vec(),
    }
}

impl Config {
    fn from_args(
        args: impl IntoIterator<Item = String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let args: Vec<String> = args.into_iter().collect();
        let payload_bytes = option(&args, "--payload-bytes")?.unwrap_or(4096);
        let commands = option(&args, "--commands")?.unwrap_or(64);
        let warmup = option(&args, "--warmup")?.unwrap_or(4);
        let sparse = args.iter().any(|arg| arg == "--sparse");
        let random_payload = args.iter().any(|arg| arg == "--random-payload");
        let max_capture_bytes = option(&args, "--max-capture-bytes")?.map(|bytes| bytes as u64);
        if payload_bytes == 0 || commands == 0 || warmup >= commands {
            return Err(
                "payload-bytes and commands must be positive, and warmup < commands".into(),
            );
        }
        let endpoint = value(&args, "--endpoint")?;
        let bucket = value(&args, "--bucket")?.unwrap_or_else(|| "crab-ltx-cost".into());
        let access_key = value(&args, "--access-key")?.unwrap_or_else(|| "crab-ltx-test".into());
        let secret_key = value(&args, "--secret-key")?.unwrap_or_else(|| "crab-ltx-test".into());
        Ok(Self {
            payload_bytes,
            commands,
            warmup,
            sparse,
            random_payload,
            max_capture_bytes,
            endpoint,
            bucket,
            access_key,
            secret_key,
        })
    }
}

fn value(args: &[String], name: &str) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let Some(position) = args.iter().position(|arg| arg == name) else {
        return Ok(None);
    };
    Ok(Some(
        args.get(position + 1)
            .ok_or_else(|| format!("{name} needs a value"))?
            .clone(),
    ))
}

fn option(args: &[String], name: &str) -> Result<Option<usize>, Box<dyn std::error::Error>> {
    let Some(position) = args.iter().position(|arg| arg == name) else {
        return Ok(None);
    };
    let value = args
        .get(position + 1)
        .ok_or_else(|| format!("{name} needs a value"))?;
    Ok(Some(value.parse()?))
}

#[allow(dead_code)]
fn _duration_helper(duration: Duration) -> u64 {
    duration.as_micros() as u64
}
