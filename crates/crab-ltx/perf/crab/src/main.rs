use crab_ltx::{
    CaptureTiming, Limits, Db, Position, VerifiedPlan, compact_exact, restore_exact,
};
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
    capture_position_us: u64,
    capture_wal_read_us: u64,
    capture_page_collection_us: u64,
    capture_verification_us: u64,
    capture_encode_us: u64,
    capture_local_write_us: u64,
    capture_fsync_us: u64,
    capture_parent_sync_us: u64,
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
    capture_position_us: u64,
    capture_wal_read_us: u64,
    capture_page_collection_us: u64,
    capture_verification_us: u64,
    capture_encode_us: u64,
    capture_local_write_us: u64,
    capture_fsync_us: u64,
    capture_parent_sync_us: u64,
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
    let mut database = Db::open(&source, Limits::default())?;
    let mut segments = Vec::new();
    let mut position = Position::default();
    let mut capture_phases = CapturePhases::default();
    let mut workload_write_us = 0;
    let mut capture_us = 0;

    let started = Instant::now();
    database.transaction(|transaction| {
        transaction
            .execute_batch("CREATE TABLE payloads (id INTEGER PRIMARY KEY, value BLOB NOT NULL)")
    })?;
    workload_write_us += elapsed_us(started);
    let started = Instant::now();
    append_capture(
        &mut database,
        &mut segments,
        &mut position,
        &mut capture_phases,
    )?;
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
        append_capture(
            &mut database,
            &mut segments,
            &mut position,
            &mut capture_phases,
        )?;
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
        capture_position_us: capture_phases.position_us(),
        capture_wal_read_us: capture_phases.wal_read_us(),
        capture_page_collection_us: capture_phases.page_collection_us(),
        capture_verification_us: capture_phases.verification_us(),
        capture_encode_us: capture_phases.encode_us(),
        capture_local_write_us: capture_phases.local_write_us(),
        capture_fsync_us: capture_phases.fsync_us(),
        capture_parent_sync_us: capture_phases.parent_sync_us(),
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
    database: &mut Db,
    segments: &mut Vec<crab_ltx::LocalSegment>,
    position: &mut Position,
    phases: &mut CapturePhases,
) -> crab_ltx::Result<()> {
    let batch = database.capture()?;
    *position = batch.position;
    phases.add(batch.timing);
    segments.extend(batch.segments);
    Ok(())
}

#[derive(Debug, Default)]
struct CapturePhases {
    position_nanos: u64,
    wal_read_nanos: u64,
    page_collection_nanos: u64,
    verification_nanos: u64,
    encode_nanos: u64,
    local_write_nanos: u64,
    fsync_nanos: u64,
    parent_sync_nanos: u64,
}

impl CapturePhases {
    fn add(&mut self, timing: CaptureTiming) {
        self.position_nanos = self
            .position_nanos
            .saturating_add(timing.position_resolution_nanos);
        self.wal_read_nanos = self.wal_read_nanos.saturating_add(timing.wal_read_nanos);
        self.page_collection_nanos = self
            .page_collection_nanos
            .saturating_add(timing.page_collection_nanos);
        self.verification_nanos = self
            .verification_nanos
            .saturating_add(timing.verification_nanos);
        self.encode_nanos = self.encode_nanos.saturating_add(timing.encode_nanos);
        self.local_write_nanos = self
            .local_write_nanos
            .saturating_add(timing.local_write_nanos);
        self.fsync_nanos = self.fsync_nanos.saturating_add(timing.fsync_nanos);
        self.parent_sync_nanos = self
            .parent_sync_nanos
            .saturating_add(timing.parent_sync_nanos);
    }

    fn position_us(&self) -> u64 {
        nanos_to_us(self.position_nanos)
    }

    fn wal_read_us(&self) -> u64 {
        nanos_to_us(self.wal_read_nanos)
    }

    fn page_collection_us(&self) -> u64 {
        nanos_to_us(self.page_collection_nanos)
    }

    fn verification_us(&self) -> u64 {
        nanos_to_us(self.verification_nanos)
    }

    fn encode_us(&self) -> u64 {
        nanos_to_us(self.encode_nanos)
    }

    fn local_write_us(&self) -> u64 {
        nanos_to_us(self.local_write_nanos)
    }

    fn fsync_us(&self) -> u64 {
        nanos_to_us(self.fsync_nanos)
    }

    fn parent_sync_us(&self) -> u64 {
        nanos_to_us(self.parent_sync_nanos)
    }
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

fn nanos_to_us(nanos: u64) -> u64 {
    nanos / 1_000
}

impl Summary {
    fn from_samples(samples: &[Sample]) -> Self {
        Self {
            workload_write_us: median(samples.iter().map(|sample| sample.workload_write_us)),
            capture_us: median(samples.iter().map(|sample| sample.capture_us)),
            capture_position_us: median(samples.iter().map(|sample| sample.capture_position_us)),
            capture_wal_read_us: median(samples.iter().map(|sample| sample.capture_wal_read_us)),
            capture_page_collection_us: median(
                samples
                    .iter()
                    .map(|sample| sample.capture_page_collection_us),
            ),
            capture_verification_us: median(
                samples.iter().map(|sample| sample.capture_verification_us),
            ),
            capture_encode_us: median(samples.iter().map(|sample| sample.capture_encode_us)),
            capture_local_write_us: median(
                samples.iter().map(|sample| sample.capture_local_write_us),
            ),
            capture_fsync_us: median(samples.iter().map(|sample| sample.capture_fsync_us)),
            capture_parent_sync_us: median(
                samples.iter().map(|sample| sample.capture_parent_sync_us),
            ),
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
