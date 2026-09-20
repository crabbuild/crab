use celld_ltx::{Db, FileReplicaClient, TXID};
use rusqlite::Connection;
use serde::Serialize;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

const CELLD_SOURCE: &str = "celld 10cb1303dac710dcb3b557e318e08c855261f68b";

#[derive(Debug, Clone, Copy)]
struct Config {
    transactions: usize,
    payload_bytes: usize,
    rounds: usize,
    warmup: usize,
}

#[derive(Debug, Clone, Serialize)]
struct Sample {
    round: usize,
    workload_write_us: u64,
    capture_us: u64,
    verify_us: u64,
    compact_us: u64,
    compact_verify_us: u64,
    restore_us: u64,
    restore_plan_us: u64,
    restore_download_us: u64,
    restore_apply_us: u64,
    recovery_us: u64,
    total_us: u64,
    segments: usize,
    input_ltx_bytes: u64,
    compacted_ltx_bytes: u64,
    source_database_bytes: u64,
    final_txid: u64,
}

#[derive(Debug, Serialize)]
struct Report {
    implementation: &'static str,
    source: &'static str,
    config: ConfigOutput,
    samples: Vec<Sample>,
    median: Summary,
}

#[derive(Debug, Serialize)]
struct ConfigOutput {
    transactions: usize,
    payload_bytes: usize,
    measured_rounds: usize,
    warmup_rounds: usize,
}

#[derive(Debug, Serialize)]
struct Summary {
    workload_write_us: u64,
    capture_us: u64,
    verify_us: u64,
    compact_us: u64,
    compact_verify_us: u64,
    restore_us: u64,
    restore_plan_us: u64,
    restore_download_us: u64,
    restore_apply_us: u64,
    recovery_us: u64,
    total_us: u64,
    segments: usize,
    input_ltx_bytes: u64,
    compacted_ltx_bytes: u64,
    source_database_bytes: u64,
    final_txid: u64,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::from_args(std::env::args().skip(1))?;
    let mut samples = Vec::with_capacity(config.rounds);
    for round in 0..config.warmup + config.rounds {
        let sample = run_round(config, round).await?;
        if round >= config.warmup {
            samples.push(sample);
        }
    }

    let report = Report {
        implementation: "celld-ltx",
        source: CELLD_SOURCE,
        config: ConfigOutput {
            transactions: config.transactions,
            payload_bytes: config.payload_bytes,
            measured_rounds: config.rounds,
            warmup_rounds: config.warmup,
        },
        median: Summary::from_samples(&samples),
        samples,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

impl Config {
    fn from_args<I>(args: I) -> Result<Self, Box<dyn Error>>
    where
        I: IntoIterator<Item = String>,
    {
        let args = args.into_iter().collect::<Vec<_>>();
        let transactions = option(&args, "--transactions")?.unwrap_or(128);
        let payload_bytes = option(&args, "--payload-bytes")?.unwrap_or(4096);
        let rounds = option(&args, "--rounds")?.unwrap_or(5);
        let warmup = option(&args, "--warmup")?.unwrap_or(1);
        if transactions == 0 || payload_bytes == 0 || rounds == 0 {
            return Err("transactions, payload-bytes, and rounds must be positive".into());
        }
        Ok(Self {
            transactions,
            payload_bytes,
            rounds,
            warmup,
        })
    }
}

fn option(args: &[String], name: &str) -> Result<Option<usize>, Box<dyn Error>> {
    let Some(index) = args.iter().position(|arg| arg == name) else {
        return Ok(None);
    };
    let value = args
        .get(index + 1)
        .ok_or_else(|| format!("missing value for {name}"))?
        .parse::<usize>()
        .map_err(|error| format!("invalid value for {name}: {error}"))?;
    Ok(Some(value))
}

async fn run_round(config: Config, round: usize) -> Result<Sample, Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("source.sqlite");
    let mut ltx_db = Db::open(&source)?;
    let mut writer = Connection::open(&source)?;
    writer.busy_timeout(Duration::from_secs(1))?;
    writer.pragma_update(None, "wal_autocheckpoint", 0)?;
    writer.pragma_update(None, "synchronous", "FULL")?;
    writer.pragma_update(None, "foreign_keys", true)?;

    let mut workload_write_us = 0;
    let mut capture_us = 0;
    let started = Instant::now();
    writer.execute_batch("CREATE TABLE payloads (id INTEGER PRIMARY KEY, value BLOB NOT NULL)")?;
    workload_write_us += elapsed_us(started);
    let started = Instant::now();
    ltx_db.sync()?;
    capture_us += elapsed_us(started);

    for id in 0..config.transactions {
        let payload = payload(id, config.payload_bytes);
        let started = Instant::now();
        let transaction = writer.transaction()?;
        transaction.execute(
            "INSERT INTO payloads (id, value) VALUES (?1, ?2)",
            rusqlite::params![id as i64, payload.as_slice()],
        )?;
        transaction.commit()?;
        workload_write_us += elapsed_us(started);

        let started = Instant::now();
        ltx_db.sync()?;
        capture_us += elapsed_us(started);
    }

    let meta_path = ltx_db.meta_path().to_path_buf();
    ltx_db.close()?;
    drop(writer);
    let source_database_bytes = std::fs::metadata(&source)?.len();
    let input_files = list_ltx_files(&meta_path, 0)?;
    let input_ltx_bytes = input_files.iter().map(|(_, bytes)| *bytes).sum::<u64>();
    let segments = input_files.len();
    let client = FileReplicaClient::new(meta_path.to_string_lossy().into_owned());

    let started = Instant::now();
    let output = celld_ltx::replica_compactor::ReplicaCompactor::new(&client)
        .with_local_path(&meta_path)
        .with_verification(true)
        .compact(1)
        .await?
        .ok_or("Celld compaction produced no output")?;
    let compact_us = elapsed_us(started);
    let compacted_ltx_bytes = u64::try_from(output.info.size)
        .map_err(|_| "Celld compaction returned a negative output size")?;

    let restored = directory.path().join("restored.sqlite");
    let started = Instant::now();
    let timing = celld_ltx::replica::restore_timed_with_download_slots(
        &client,
        &restored,
        TXID::ZERO,
        Arc::new(Semaphore::new(1)),
    )
    .await?;
    let restore_us = elapsed_us(started);
    validate_restore(&restored, config.transactions)?;

    let recovery_us = recovery_us(0, compact_us, 0, restore_us);
    let total_us = total_us(workload_write_us, capture_us, recovery_us);

    Ok(Sample {
        round,
        workload_write_us,
        capture_us,
        verify_us: 0,
        compact_us,
        compact_verify_us: 0,
        restore_us,
        restore_plan_us: timing.plan_us,
        restore_download_us: timing.download_us,
        restore_apply_us: timing.apply_us,
        recovery_us,
        total_us,
        segments,
        input_ltx_bytes,
        compacted_ltx_bytes,
        source_database_bytes,
        final_txid: output.info.max_txid.0,
    })
}

fn list_ltx_files(root: &Path, level: u32) -> Result<Vec<(PathBuf, u64)>, Box<dyn Error>> {
    let directory = root.join("ltx").join(level.to_string());
    let mut files = Vec::new();
    if !directory.exists() {
        return Ok(files);
    }
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|extension| extension == "ltx") {
            files.push((path.clone(), std::fs::metadata(path)?.len()));
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

fn validate_restore(path: &Path, transactions: usize) -> Result<(), Box<dyn Error>> {
    let connection = Connection::open(path)?;
    let count: i64 = connection.query_row("SELECT COUNT(*) FROM payloads", [], |row| row.get(0))?;
    if count != transactions as i64 {
        return Err(format!("restored {count} rows, expected {transactions}").into());
    }
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(format!("restored database integrity check: {integrity}").into());
    }
    Ok(())
}

fn payload(id: usize, bytes: usize) -> Vec<u8> {
    (0..bytes)
        .map(|offset| ((id.wrapping_mul(31).wrapping_add(offset)) % 251) as u8)
        .collect()
}

fn elapsed_us(started: Instant) -> u64 {
    started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
}

impl Summary {
    fn from_samples(samples: &[Sample]) -> Self {
        Self {
            workload_write_us: median(samples.iter().map(|sample| sample.workload_write_us)),
            capture_us: median(samples.iter().map(|sample| sample.capture_us)),
            verify_us: median(samples.iter().map(|sample| sample.verify_us)),
            compact_us: median(samples.iter().map(|sample| sample.compact_us)),
            compact_verify_us: median(samples.iter().map(|sample| sample.compact_verify_us)),
            restore_us: median(samples.iter().map(|sample| sample.restore_us)),
            restore_plan_us: median(samples.iter().map(|sample| sample.restore_plan_us)),
            restore_download_us: median(samples.iter().map(|sample| sample.restore_download_us)),
            restore_apply_us: median(samples.iter().map(|sample| sample.restore_apply_us)),
            recovery_us: median(samples.iter().map(|sample| sample.recovery_us)),
            total_us: median(samples.iter().map(|sample| sample.total_us)),
            segments: median(samples.iter().map(|sample| sample.segments as u64)) as usize,
            input_ltx_bytes: median(samples.iter().map(|sample| sample.input_ltx_bytes)),
            compacted_ltx_bytes: median(samples.iter().map(|sample| sample.compacted_ltx_bytes)),
            source_database_bytes: median(
                samples.iter().map(|sample| sample.source_database_bytes),
            ),
            final_txid: median(samples.iter().map(|sample| sample.final_txid)),
        }
    }
}

fn recovery_us(verify_us: u64, compact_us: u64, compact_verify_us: u64, restore_us: u64) -> u64 {
    verify_us
        .saturating_add(compact_us)
        .saturating_add(compact_verify_us)
        .saturating_add(restore_us)
}

fn total_us(workload_write_us: u64, capture_us: u64, recovery_us: u64) -> u64 {
    workload_write_us
        .saturating_add(capture_us)
        .saturating_add(recovery_us)
}

fn median(values: impl Iterator<Item = u64>) -> u64 {
    let mut values = values.collect::<Vec<_>>();
    values.sort_unstable();
    values[values.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::{recovery_us, total_us};

    #[test]
    fn recovery_subtotal_includes_each_phase_once() {
        assert_eq!(recovery_us(11, 13, 17, 19), 60);
    }

    #[test]
    fn total_includes_workload_capture_and_recovery_once() {
        assert_eq!(total_us(23, 29, 31), 83);
    }
}
