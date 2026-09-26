mod support;

use crab_ltx::{CellReplica, CellStorageLayout, CrabError, Db, Host, Limits, RootRef};
use crab_storage::Store;
use object_store::path::Path as ObjectPath;
use std::{
    fs::{self, File},
    io::{self, Read as _},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};
use support::rustfs_target;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
const ROW_BYTES: i64 = 1 << 20;
const CUT_BYTES: u64 = 32 * MIB;

#[tokio::main]
async fn main() -> crab_ltx::Result<()> {
    let target_bytes = target_bytes()?;
    if target_bytes < CUT_BYTES {
        return Err(CrabError::InvalidState(
            "CRAB_CELL_LTX_TARGET_BYTES must be at least 32 MiB",
        ));
    }
    let workload_root = workload_root()?;
    let source_directory = temporary_directory(&workload_root, "source")?;
    let recovery_directory = temporary_directory(&workload_root, "recovery")?;
    let scratch_directory = temporary_directory(&workload_root, "compaction")?;
    let limits = limits_for(target_bytes);
    let database = source_directory.path().join("cell.sqlite");
    let mut writer = Db::open(&database, limits)?;
    writer.transaction(|transaction| {
        transaction.execute_batch("CREATE TABLE payload(value BLOB NOT NULL)")
    })?;

    let target = rustfs_target("cell-replica-native-scale")?;
    let layout = CellStorageLayout::new(
        target.store.clone(),
        ObjectPath::from(target.repository_prefix.as_str()),
        [7; 16],
    );
    let replica = CellReplica::new(layout, [8; 32], [9; 16], limits)?;
    let mut root: Option<RootRef> = None;
    let mut prepared: Option<crab_ltx::PreparedRoot> = None;
    let mut sequence = 1_u64;
    let mut written = 0_u64;
    let started = Instant::now();
    while written < target_bytes {
        let batch_bytes = (target_bytes - written).min(CUT_BYTES);
        let rows = i64::try_from(batch_bytes / ROW_BYTES as u64)
            .map_err(|_| CrabError::InvalidState("Cell scale row count"))?;
        let (next_writer, capture) = tokio::task::spawn_blocking(move || {
            let mut writer = writer;
            writer.transaction(|transaction| {
                for _ in 0..rows {
                    transaction.execute(
                        "INSERT INTO payload(value) VALUES(randomblob(?1))",
                        [ROW_BYTES],
                    )?;
                }
                Ok(())
            })?;
            let capture = writer.capture()?;
            Ok::<_, crab_ltx::CrabError>((writer, capture))
        })
        .await
        .map_err(|_| CrabError::InvalidState("Cell scale writer stopped"))??;
        writer = next_writer;

        let next = match replica.prepare(root.as_ref(), &capture, sequence, 1).await {
            Ok(next) => next,
            Err(error) => {
                eprintln!(
                    "CellReplica prepare failed after {written} bytes: {error:?}; capture segments={} position={:?}",
                    capture.segments.len(),
                    capture.position
                );
                if let Some(previous) = &prepared {
                    eprintln!(
                        "  predecessor root txid={} pages={} directory_height={} segments={}",
                        previous.root().position.txid,
                        previous.verified().database_pages(),
                        previous.verified().directory_height(),
                        previous.verified().segment_count()
                    );
                }
                for segment in &capture.segments {
                    eprintln!(
                        "  segment txid={}..{} bytes={} pages={} pre={} post={}",
                        segment.info().min_txid,
                        segment.info().max_txid,
                        segment.info().size_bytes,
                        segment.info().database_pages,
                        segment.info().pre_checksum,
                        segment.info().post_checksum
                    );
                    if let Ok(bytes) = fs::read(segment.path())
                        && bytes.len() >= 106
                    {
                        let page = u32::from_be_bytes(bytes[100..104].try_into().unwrap_or([0; 4]));
                        let flags =
                            u16::from_be_bytes(bytes[104..106].try_into().unwrap_or([0; 2]));
                        eprintln!(
                            "  first frame page={} flags={} length={}",
                            page,
                            flags,
                            bytes.len()
                        );
                    }
                }
                return Err(error);
            }
        };
        root = Some(next.root());
        prepared = Some(next);
        let (next_writer, _) = tokio::task::spawn_blocking(move || {
            let mut writer = writer;
            let removed = writer.prune_captured(&capture)?;
            Ok::<_, crab_ltx::CrabError>((writer, removed))
        })
        .await
        .map_err(|_| CrabError::InvalidState("Cell scale prune stopped"))??;
        writer = next_writer;
        written = written
            .checked_add(batch_bytes)
            .ok_or(CrabError::Limit(crab_ltx::LimitKind::CellScaleBytes))?;
        sequence = sequence
            .checked_add(1)
            .ok_or(CrabError::Limit(crab_ltx::LimitKind::CellScaleSequence))?;
        if written == target_bytes || (written / CUT_BYTES).is_multiple_of(8) {
            println!("prepared {written} / {target_bytes} bytes");
        }
    }

    tokio::task::spawn_blocking(move || writer.close())
        .await
        .map_err(|_| CrabError::InvalidState("Cell scale writer stopped"))??;
    let load_elapsed = started.elapsed();
    let source_digest = stream_digest(&database)?;
    remove_sqlite_artifacts(&database)?;

    let prepared = prepared.ok_or(CrabError::InvalidState("Cell scale produced no root"))?;
    let root = root.ok_or(CrabError::InvalidState("Cell scale produced no root"))?;
    measure_activation(&target, &root, limits, &workload_root).await?;
    let restored = recovery_directory.path().join("restored.sqlite");
    let restore_started = Instant::now();
    prepared.verified().restore(&restored).await?;
    let restore_elapsed = restore_started.elapsed();
    let restored_digest = stream_digest(&restored)?;
    if restored_digest != source_digest {
        return Err(CrabError::ChecksumMismatch);
    }

    let compaction_started = Instant::now();
    let compacted = replica
        .prepare_compaction(
            &root,
            0..prepared.verified().segment_count(),
            9,
            scratch_directory.path(),
        )
        .await?;
    let compaction_elapsed = compaction_started.elapsed();
    let compacted_path = recovery_directory.path().join("compacted.sqlite");
    let compacted_restore_started = Instant::now();
    compacted.verified().restore(&compacted_path).await?;
    let compacted_restore_elapsed = compacted_restore_started.elapsed();
    if stream_digest(&compacted_path)? != source_digest {
        return Err(CrabError::ChecksumMismatch);
    }

    println!("\nRustFS CellReplica native scale load complete");
    println!("target bytes:             {target_bytes}");
    println!(
        "segments:                 {}",
        prepared.verified().segment_count()
    );
    println!("source checksum:          {}", hex_digest(source_digest));
    println!(
        "root digest:              {}",
        blake3::Hash::from_bytes(root.digest).to_hex()
    );
    println!("load wall time:           {load_elapsed:.3?}");
    println!("restore wall time:        {restore_elapsed:.3?}");
    println!("compaction wall time:     {compaction_elapsed:.3?}");
    println!("compacted restore time:   {compacted_restore_elapsed:.3?}");
    println!("source deleted:           true");
    println!("compaction checksum:      exact");
    Ok(())
}

#[derive(Default)]
struct Reads {
    requests: AtomicU64,
    bytes: AtomicU64,
}

impl Reads {
    fn finish(&self, started: Instant) -> serde_json::Value {
        serde_json::json!({
            "elapsed_us": started.elapsed().as_micros(),
            "requests": self.requests.swap(0, Ordering::Relaxed),
            "bytes": self.bytes.swap(0, Ordering::Relaxed),
        })
    }
}

async fn measure_activation(
    target: &support::RustfsTarget,
    root: &RootRef,
    limits: Limits,
    workload_root: &Path,
) -> crab_ltx::Result<()> {
    for round in 0..3 {
        let order = if round % 2 == 0 { [1, 4, 8] } else { [8, 4, 1] };
        for slots in order {
            let reads = Arc::new(Reads::default());
            let requests = reads.clone();
            let bytes = reads.clone();
            // A new Store identity excludes the publication process's immutable
            // caches. Reuse the provider connection pool to isolate metadata work.
            let store = Store::new(target.store.inner().clone())
                .with_read_request_observer(Arc::new(move |_| {
                    requests.requests.fetch_add(1, Ordering::Relaxed);
                }))
                .with_read_byte_observer(Arc::new(move |count| {
                    bytes.bytes.fetch_add(count, Ordering::Relaxed);
                }));
            let layout = CellStorageLayout::new(
                store,
                ObjectPath::from(target.repository_prefix.as_str()),
                [7; 16],
            );
            let host = Host::default().with_io_slots(Arc::new(tokio::sync::Semaphore::new(slots)));
            let replica = CellReplica::new(layout, [8; 32], [9; 16], limits)?.with_host(host);
            for cache in ["cold_metadata", "reused_metadata"] {
                let directory = temporary_directory(workload_root, "activation")?;
                let destination = directory.path().join("active.sqlite");
                let started = Instant::now();
                let verified = replica.open_root(root).await?;
                let root_open = reads.finish(started);
                let pages = verified.database_pages();
                let started = Instant::now();
                let prepared = verified.paged().prepare_writable(&destination).await?;
                let checksums = reads.finish(started);
                let worker_reads = reads.clone();
                let started = Instant::now();
                let (open, query, hydration) = tokio::task::spawn_blocking(move || {
                    let mut database = prepared.open_writable(&destination)?;
                    let open = worker_reads.finish(started);
                    let started = Instant::now();
                    let length: i64 = database
                        .query_with(|db| {
                            db.query_row(
                                "SELECT length(value) FROM payload WHERE rowid = 1",
                                [],
                                |row| row.get(0),
                            )
                        })
                        .map_err(|error| CrabError::Other(Box::new(error)))?;
                    let query = worker_reads.finish(started);
                    if length != ROW_BYTES {
                        return Err(CrabError::ChecksumMismatch);
                    }
                    let hydration = database.hydration()?.ok_or(CrabError::InvalidState(
                        "Cell scale activation is not sparse",
                    ))?;
                    database.close()?;
                    Ok::<_, CrabError>((open, query, hydration))
                })
                .await
                .map_err(|_| CrabError::InvalidState("Cell scale activation worker stopped"))??;
                // Exclude close from the next root-open sample.
                let _ = reads.finish(Instant::now());
                println!(
                    "{}",
                    serde_json::json!({
                        "measurement": "sparse_activation",
                        "round": round,
                        "io_slots": slots,
                        "cache": cache,
                        "database_pages": pages,
                        "sqlite_version": rusqlite::version(),
                        "root_open": root_open,
                        "checksums": checksums,
                        "writable_open": open,
                        "first_query": query,
                        "hydrated_pages": hydration.resolved,
                        "page_faults": hydration.faults,
                    })
                );
            }
        }
    }
    Ok(())
}

fn target_bytes() -> crab_ltx::Result<u64> {
    std::env::var("CRAB_CELL_LTX_TARGET_BYTES")
        .map(|value| {
            value
                .parse()
                .map_err(|_| CrabError::InvalidState("invalid CRAB_CELL_LTX_TARGET_BYTES"))
        })
        .unwrap_or(Ok(5 * GIB))
}

fn workload_root() -> crab_ltx::Result<PathBuf> {
    let root = std::env::var_os("CRAB_LTX_WORKLOAD_ROOT").ok_or(CrabError::InvalidState(
        "CRAB_LTX_WORKLOAD_ROOT is required",
    ))?;
    let root = PathBuf::from(root);
    fs::create_dir_all(&root)?;
    Ok(root)
}

fn temporary_directory(root: &Path, label: &str) -> crab_ltx::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(&format!("crab-cell-ltx-{label}-"))
        .tempdir_in(root)
        .map_err(Into::into)
}

fn limits_for(target_bytes: u64) -> Limits {
    Limits {
        max_database_bytes: target_bytes.saturating_add(512 * MIB),
        max_capture_bytes: 64 * MIB,
        max_file_bytes: target_bytes.saturating_add(512 * MIB),
        max_plan_bytes: target_bytes.saturating_mul(2).saturating_add(512 * MIB),
        max_segments: 1024,
    }
}

fn stream_digest(path: &Path) -> crab_ltx::Result<(u64, [u8; 32])> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 1 << 20];
    let mut length = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        length = length.checked_add(read as u64).ok_or(CrabError::Limit(
            crab_ltx::LimitKind::CellScaleChecksumLength,
        ))?;
    }
    Ok((length, *hasher.finalize().as_bytes()))
}

fn remove_sqlite_artifacts(database: &Path) -> crab_ltx::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut path = database.as_os_str().to_owned();
        path.push(suffix);
        match fs::remove_file(PathBuf::from(path)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn hex_digest(digest: (u64, [u8; 32])) -> String {
    format!(
        "{}:{}",
        digest.0,
        blake3::Hash::from_bytes(digest.1).to_hex()
    )
}
