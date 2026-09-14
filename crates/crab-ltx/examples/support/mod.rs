use crab_ltx::{CrabError, ManagedDb, Replica, ReplicaHead};
use std::time::{Duration, Instant};

pub const RECORD_COUNT: i64 = 1_000_000;
pub const BATCH_SIZE: i64 = 10_000;
const REPOSITORY_COUNT: i64 = 1_000;
const PROGRESS_INTERVAL: i64 = 100_000;

pub struct LoadReport {
    pub writer: ManagedDb,
    pub head: ReplicaHead,
    pub elapsed: Duration,
    pub sqlite_elapsed: Duration,
    pub publication_elapsed: Duration,
    pub captured_segments: usize,
    pub pruned_segments: usize,
}

pub async fn publish_records(
    mut writer: ManagedDb,
    replica: &Replica,
) -> crab_ltx::Result<LoadReport> {
    let started = Instant::now();
    let mut sqlite_elapsed = Duration::ZERO;
    let mut publication_elapsed = Duration::ZERO;
    let mut captured_segments = 0;
    let mut pruned_segments = 0;
    let mut head = None;
    let mut first = 1;

    while first <= RECORD_COUNT {
        let last = (first + BATCH_SIZE - 1).min(RECORD_COUNT);
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

        if last % PROGRESS_INTERVAL == 0 || last == RECORD_COUNT {
            println!(
                "published {last:>9} / {RECORD_COUNT} records ({:.0} records/s)",
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

pub fn expected_object_size_sum() -> i64 {
    (1..=RECORD_COUNT).map(|id| 4096 + id % 256).sum()
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
