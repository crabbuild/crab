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

use crab_ltx::{CellReplica, CellStorageLayout, Db, Limits};
use crab_storage::{ObjectStoreCredentials, Store};
use object_store::{ObjectStore, memory::InMemory, path::Path};
use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct Config {
    payload_bytes: usize,
    commands: usize,
    warmup: usize,
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
    elapsed_us: u64,
}

#[derive(Debug, Serialize)]
struct Report {
    implementation: &'static str,
    store: &'static str,
    payload_bytes: usize,
    measured_commands: usize,
    objects_per_command: u64,
    bytes_per_command: u64,
    objects_p95: u64,
    bytes_p95: u64,
    elapsed_us_p50: u64,
    elapsed_us_p95: u64,
    elapsed_us_p99: u64,
    elapsed_us_max: u64,
    samples: Vec<Sample>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_args(std::env::args().skip(1))?;
    let directory = tempfile::TempDir::new()?;
    let database_path = directory.path().join("cell.sqlite");
    let limits = Limits::default();
    let mut database = Db::open(&database_path, limits)?;
    let (store, store_label, prefix) = open_store(&config)?;
    let replica = CellReplica::new(
        CellStorageLayout::new(store, Path::from(prefix.as_str()), [7; 16]),
        [5; 32],
        [6; 16],
        limits,
    )?;

    database.transaction(|transaction| {
        transaction.execute_batch("CREATE TABLE payload(id INTEGER PRIMARY KEY, value BLOB NOT NULL)")
    })?;
    let first = database.capture()?;
    let mut root = Some(replica.prepare(None, &first, 1, 1).await?.root());
    let _bootstrap = replica.take_publication_cost();

    let mut samples = Vec::with_capacity(config.commands);
    for command in 0..config.commands {
        let payload = payload(command, config.payload_bytes);
        database.transaction(|transaction| {
            transaction.execute("INSERT INTO payload(value) VALUES(?1)", [payload.as_slice()])?;
            Ok(())
        })?;
        let batch = database.capture()?;
        let started = Instant::now();
        let prepared = replica
            .prepare(root.as_ref(), &batch, command as u64 + 2, 1)
            .await?;
        let elapsed = started.elapsed();
        let cost = replica.take_publication_cost();
        root = Some(prepared.root());
        if command >= config.warmup {
            samples.push(Sample {
                command,
                objects: cost.objects,
                bytes: cost.bytes,
                elapsed_us: elapsed.as_micros() as u64,
            });
        }
    }
    database.close()?;

    if samples.is_empty() {
        return Err("at least one measured command is required".into());
    }
    let report = summarize(config, store_label, &samples);
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

/// Deterministic, incompressible-enough payload for one command.
fn payload(command: usize, bytes: usize) -> Vec<u8> {
    (0..bytes)
        .map(|index| ((command * 131 + index) % 251) as u8)
        .collect()
}

fn percentile(values: &[u64], percent: usize) -> u64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[(sorted.len() * percent).div_ceil(100).saturating_sub(1)]
}

fn summarize(config: Config, store: &'static str, samples: &[Sample]) -> Report {
    let objects: Vec<u64> = samples.iter().map(|sample| sample.objects).collect();
    let bytes: Vec<u64> = samples.iter().map(|sample| sample.bytes).collect();
    let elapsed: Vec<u64> = samples.iter().map(|sample| sample.elapsed_us).collect();
    let total_objects: u64 = objects.iter().sum();
    let total_bytes: u64 = bytes.iter().sum();
    Report {
        implementation: "crab-ltx CellReplica",
        store,
        payload_bytes: config.payload_bytes,
        measured_commands: samples.len(),
        objects_per_command: total_objects / samples.len() as u64,
        bytes_per_command: total_bytes / samples.len() as u64,
        objects_p95: percentile(&objects, 95),
        bytes_p95: percentile(&bytes, 95),
        elapsed_us_p50: percentile(&elapsed, 50),
        elapsed_us_p95: percentile(&elapsed, 95),
        elapsed_us_p99: percentile(&elapsed, 99),
        elapsed_us_max: *elapsed.iter().max().unwrap_or(&0),
        samples: samples.to_vec(),
    }
}

impl Config {
    fn from_args(args: impl IntoIterator<Item = String>) -> Result<Self, Box<dyn std::error::Error>> {
        let args: Vec<String> = args.into_iter().collect();
        let payload_bytes = option(&args, "--payload-bytes")?.unwrap_or(4096);
        let commands = option(&args, "--commands")?.unwrap_or(64);
        let warmup = option(&args, "--warmup")?.unwrap_or(4);
        if payload_bytes == 0 || commands == 0 || warmup >= commands {
            return Err("payload-bytes and commands must be positive, and warmup < commands".into());
        }
        let endpoint = value(&args, "--endpoint")?;
        let bucket = value(&args, "--bucket")?.unwrap_or_else(|| "crab-ltx-cost".into());
        let access_key = value(&args, "--access-key")?.unwrap_or_else(|| "crab-ltx-test".into());
        let secret_key = value(&args, "--secret-key")?.unwrap_or_else(|| "crab-ltx-test".into());
        Ok(Self {
            payload_bytes,
            commands,
            warmup,
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
