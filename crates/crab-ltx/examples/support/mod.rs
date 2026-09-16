use crab_ltx::{CrabError, Limits, ManagedDb, Replica, ReplicaHead};
use crab_storage::{ObjectStoreCredentials, Store};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const REPOSITORY_COUNT: i64 = 1_000;
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

#[derive(Clone, Copy)]
pub struct ScaleProfile {
    pub label: &'static str,
    pub records: i64,
    pub batch_size: i64,
    pub limits: Limits,
}

pub struct LoadReport {
    pub writer: ManagedDb,
    pub head: ReplicaHead,
    pub elapsed: Duration,
    pub sqlite_elapsed: Duration,
    pub publication_elapsed: Duration,
    pub captured_segments: usize,
    pub pruned_segments: usize,
}

pub struct RustfsTarget {
    pub store: Store,
    pub repository_prefix: String,
}

pub fn selected_profile() -> crab_ltx::Result<ScaleProfile> {
    let mut arguments = std::env::args().skip(1);
    let selected = arguments.next();
    if arguments.next().is_some() {
        return Err(CrabError::InvalidState(
            "expected one scale profile: 1m, 10m, or 100m",
        ));
    }
    match selected.as_deref().unwrap_or("1m") {
        "1m" => Ok(profile("1m", 1_000_000, 10_000, 256 * MIB, 2 * GIB)),
        "10m" => Ok(profile("10m", 10_000_000, 100_000, 2 * GIB, 4 * GIB)),
        "100m" => Ok(profile("100m", 100_000_000, 1_000_000, 16 * GIB, 32 * GIB)),
        _ => Err(CrabError::InvalidState(
            "unknown scale profile; use 1m, 10m, or 100m",
        )),
    }
}

pub fn workload_directory(
    profile: ScaleProfile,
    purpose: &str,
) -> crab_ltx::Result<tempfile::TempDir> {
    let root = match std::env::var("CRAB_LTX_WORKLOAD_ROOT") {
        Ok(root) => root,
        Err(_) if profile.records == 1_000_000 => return Ok(tempfile::tempdir()?),
        Err(_) => {
            return Err(CrabError::InvalidState(
                "10m and 100m profiles require CRAB_LTX_WORKLOAD_ROOT",
            ));
        }
    };
    std::fs::create_dir_all(&root)?;
    Ok(tempfile::Builder::new()
        .prefix(&format!("crab-ltx-{}-{purpose}-", profile.label))
        .tempdir_in(root)?)
}

pub fn rustfs_target(workload: &str) -> crab_ltx::Result<RustfsTarget> {
    let bucket = required_environment("CRAB_LTX_TEST_BUCKET")?;
    let endpoint = required_environment("CRAB_LTX_TEST_ENDPOINT")?;
    let access_key_id = required_environment("AWS_ACCESS_KEY_ID")?;
    let secret_access_key = required_environment("AWS_SECRET_ACCESS_KEY")?;
    let allow_http = match endpoint.split_once("://").map(|(scheme, _)| scheme) {
        Some("http") => true,
        Some("https") => false,
        _ => {
            return Err(CrabError::InvalidState(
                "RustFS endpoint must use http or https",
            ));
        }
    };
    let store = crab_storage::build_explicit_store(
        &bucket,
        ObjectStoreCredentials::Aws {
            access_key_id,
            secret_access_key,
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&endpoint),
        allow_http,
    )?;
    let run = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CrabError::InvalidState("system clock is before the Unix epoch"))?
        .as_millis();
    Ok(RustfsTarget {
        store,
        repository_prefix: format!("crab-ltx-examples/{workload}/{run}-{}", std::process::id()),
    })
}

pub async fn publish_records(
    mut writer: ManagedDb,
    replica: &Replica,
    profile: ScaleProfile,
) -> crab_ltx::Result<LoadReport> {
    let started = Instant::now();
    let mut sqlite_elapsed = Duration::ZERO;
    let mut publication_elapsed = Duration::ZERO;
    let mut captured_segments = 0;
    let mut pruned_segments = 0;
    let mut head = None;
    let mut first = 1;

    while first <= profile.records {
        let last = (first + profile.batch_size - 1).min(profile.records);
        let (next_writer, batch, local_elapsed) =
            tokio::task::spawn_blocking(move || -> crab_ltx::Result<_> {
                let local_started = Instant::now();
                insert_batch(&mut writer, first, last)?;
                let batch = writer.capture()?;
                Ok((writer, batch, local_started.elapsed()))
            })
            .await
            .map_err(|_| CrabError::InvalidState("load writer stopped"))??;
        writer = next_writer;
        sqlite_elapsed += local_elapsed;
        captured_segments += batch.segments.len();

        let publication_started = Instant::now();
        let next_head = replica.replicate(&batch, head.as_ref()).await?;
        publication_elapsed += publication_started.elapsed();

        let prune_head = next_head.clone();
        let (next_writer, removed) = tokio::task::spawn_blocking(move || -> crab_ltx::Result<_> {
            let removed = writer.prune_published(&prune_head)?;
            Ok((writer, removed))
        })
        .await
        .map_err(|_| CrabError::InvalidState("load writer stopped"))??;
        writer = next_writer;
        pruned_segments += removed;
        head = Some(next_head);

        if last % (profile.records / 10) == 0 || last == profile.records {
            println!(
                "published {last:>9} / {} records ({:.0} records/s)",
                profile.records,
                records_per_second(last, started.elapsed())
            );
        }
        first = last + 1;
    }

    Ok(LoadReport {
        writer,
        head: head.ok_or(CrabError::InvalidState("load produced no replica head"))?,
        elapsed: started.elapsed(),
        sqlite_elapsed,
        publication_elapsed,
        captured_segments,
        pruned_segments,
    })
}

pub fn records_per_second(records: i64, elapsed: Duration) -> f64 {
    if elapsed.is_zero() {
        return 0.0;
    }
    records as f64 / elapsed.as_secs_f64()
}

pub fn expected_object_size_sum(records: i64) -> i64 {
    let cycles = records / 256;
    let remainder = records % 256;
    records * 4096 + cycles * 32_640 + remainder * (remainder + 1) / 2
}

fn profile(
    label: &'static str,
    records: i64,
    batch_size: i64,
    max_database_bytes: u64,
    max_plan_bytes: u64,
) -> ScaleProfile {
    ScaleProfile {
        label,
        records,
        batch_size,
        limits: Limits {
            max_database_bytes,
            max_capture_bytes: 64 * MIB,
            max_file_bytes: 512 * MIB,
            max_plan_bytes,
            max_segments: 512,
        },
    }
}

fn insert_batch(writer: &mut ManagedDb, first: i64, last: i64) -> crab_ltx::Result<()> {
    writer.transaction(|transaction| {
        if first == 1 {
            transaction.execute_batch(
                "CREATE TABLE repository_entries(
                    id INTEGER PRIMARY KEY,
                    repository_id INTEGER NOT NULL,
                    path TEXT NOT NULL UNIQUE,
                    object_size INTEGER NOT NULL,
                    generation INTEGER NOT NULL
                 ) STRICT;
                 CREATE INDEX repository_entries_by_repository
                    ON repository_entries(repository_id, id);",
            )?;
        }

        let mut statement = transaction.prepare(
            "INSERT INTO repository_entries(
                id, repository_id, path, object_size, generation
             ) VALUES(?1, ?2, ?3, ?4, ?5)",
        )?;
        for id in first..=last {
            statement.execute((
                id,
                id % REPOSITORY_COUNT,
                format!("objects/{id:010}"),
                4096 + id % 256,
                id % 7,
            ))?;
        }
        Ok(())
    })?;
    Ok(())
}

fn required_environment(name: &str) -> crab_ltx::Result<String> {
    std::env::var(name).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("missing required environment variable {name}"),
        )
        .into()
    })
}
