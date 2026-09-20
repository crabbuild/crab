use crab_ltx::{Limits, ManagedDb, Position, VerifiedPlan, compact_exact, restore_exact};
use serde::Serialize;
use std::error::Error;
use std::path::Path;
use std::time::Instant;

const CRAB_SOURCE: &str = "workspace crab-ltx";

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
    end_to_end_us: u64,
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
    end_to_end_us: u64,
    segments: usize,
    input_ltx_bytes: u64,
    compacted_ltx_bytes: u64,
    source_database_bytes: u64,
    final_txid: u64,
}

fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::from_args(std::env::args().skip(1))?;
    let mut samples = Vec::with_capacity(config.rounds);
    for round in 0..config.warmup + config.rounds {
        let sample = run_round(config, round)?;
        if round >= config.warmup {
            samples.push(sample);
        }
    }

    let report = Report {
        implementation: "crab-ltx",
        source: CRAB_SOURCE,
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

fn run_round(config: Config, round: usize) -> Result<Sample, Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("source.sqlite");
    let mut database = ManagedDb::open(&source, Limits::default())?;
    let mut segments = Vec::new();
    let mut position = Position::default();
    let mut workload_write_us = 0;
    let mut capture_us = 0;

    let started = Instant::now();
    database.transaction(|transaction| {
        transaction
            .execute_batch("CREATE TABLE payloads (id INTEGER PRIMARY KEY, value BLOB NOT NULL)")
    })?;
    workload_write_us += elapsed_us(started);
    let started = Instant::now();
    append_capture(&mut database, &mut segments, &mut position)?;
    capture_us += elapsed_us(started);

    for id in 0..config.transactions {
        let payload = payload(id, config.payload_bytes);
        let started = Instant::now();
        database.transaction(|transaction| {
            transaction.execute(
                "INSERT INTO payloads (id, value) VALUES (?1, ?2)",
                crab_ltx::rusqlite::params![id as i64, payload.as_slice()],
            )?;
            Ok(())
        })?;
        workload_write_us += elapsed_us(started);

        let started = Instant::now();
        append_capture(&mut database, &mut segments, &mut position)?;
        capture_us += elapsed_us(started);
    }

    database.close()?;
    let source_database_bytes = std::fs::metadata(&source)?.len();
    let limits = Limits::default();

    let started = Instant::now();
    let plan = VerifiedPlan::new(&segments, position, limits)?;
    let verify_us = elapsed_us(started);

    let compact_path = directory.path().join("compacted.ltx");
    let started = Instant::now();
    let compacted = compact_exact(&plan, &compact_path)?;
    let compact_us = elapsed_us(started);

    let started = Instant::now();
    let compact_plan = VerifiedPlan::new(std::slice::from_ref(&compacted), position, limits)?;
    let compact_verify_us = elapsed_us(started);

    let restored = directory.path().join("restored.sqlite");
    let started = Instant::now();
    restore_exact(&compact_plan, &restored)?;
    let restore_us = elapsed_us(started);
    validate_restore(&restored, config.transactions)?;

    Ok(Sample {
        round,
        workload_write_us,
        capture_us,
        verify_us,
        compact_us,
        compact_verify_us,
        restore_us,
        end_to_end_us: verify_us + compact_us + compact_verify_us + restore_us,
        segments: segments.len(),
        input_ltx_bytes: segments
            .iter()
            .map(|segment| segment.info().size_bytes)
            .sum(),
        compacted_ltx_bytes: compacted.info().size_bytes,
        source_database_bytes,
        final_txid: position.txid,
    })
}

fn append_capture(
    database: &mut ManagedDb,
    segments: &mut Vec<crab_ltx::LocalSegment>,
    position: &mut Position,
) -> crab_ltx::Result<()> {
    let batch = database.capture()?;
    *position = batch.position;
    segments.extend(batch.segments);
    Ok(())
}

fn validate_restore(path: &Path, transactions: usize) -> Result<(), Box<dyn Error>> {
    let connection = crab_ltx::rusqlite::Connection::open(path)?;
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
            end_to_end_us: median(samples.iter().map(|sample| sample.end_to_end_us)),
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

fn median(values: impl Iterator<Item = u64>) -> u64 {
    let mut values = values.collect::<Vec<_>>();
    values.sort_unstable();
    values[values.len() / 2]
}
