mod support;

use crab_ltx::{CrabError, ManagedDb, Replica};
use crab_storage::StoreLayout;
use std::time::Instant;
use support::{
    expected_object_size_sum, publish_records, records_per_second, rustfs_target, selected_profile,
    workload_directory,
};

#[tokio::main]
async fn main() -> crab_ltx::Result<()> {
    let profile = selected_profile()?;
    let source_directory = workload_directory(profile, "source")?;
    let recovery_directory = workload_directory(profile, "recovery")?;
    let limits = profile.limits;
    let database = source_directory.path().join("repository.sqlite");
    let writer = ManagedDb::open(&database, limits)?;

    let target = rustfs_target(&format!("{}-replication-load", profile.label))?;
    println!(
        "Scale profile: {} records in batches of {}",
        profile.records, profile.batch_size
    );
    println!("RustFS object prefix: {}", target.repository_prefix);
    let layout = StoreLayout::new(target.store, target.repository_prefix);
    let replica = Replica::new(layout, "epoch-1", limits)?;
    let report = publish_records(writer, &replica, profile).await?;
    let database_bytes = std::fs::metadata(report.writer.path())?.len();
    let position = report.head.position();
    let manifest_segments = report.head.segment_count();
    let published_head = report.head;
    let writer = report.writer;
    tokio::task::spawn_blocking(move || writer.close())
        .await
        .map_err(|_| CrabError::InvalidState("load writer stopped"))??;
    source_directory.close()?;

    let verification = if profile.records < 100_000_000 {
        verify_exact_restore(&replica, &published_head, &recovery_directory).await?
    } else {
        verify_paged(&replica, &published_head).await?
    };
    if verification.records != profile.records
        || verification.object_size != expected_object_size_sum(profile.records)
    {
        return Err(CrabError::InvalidState(
            "restored scale aggregate does not match",
        ));
    }

    println!("\nRustFS replication scale load complete");
    println!("profile:                 {}", profile.label);
    println!("records:                 {}", verification.records);
    println!("logical object bytes:    {}", verification.object_size);
    println!("database bytes:           {database_bytes}");
    println!("final TXID:               {}", position.txid);
    println!("captured LTX segments:    {}", report.captured_segments);
    println!("manifest segments:        {manifest_segments}");
    println!("pruned local segments:    {}", report.pruned_segments);
    println!("SQLite + capture time:    {:.3?}", report.sqlite_elapsed);
    println!(
        "object publication time: {:.3?}",
        report.publication_elapsed
    );
    println!("load wall time:           {:.3?}", report.elapsed);
    println!(
        "load throughput:         {:.0} records/s",
        records_per_second(profile.records, report.elapsed)
    );
    println!(
        "{} verification time: {:.3?}",
        verification.kind, verification.elapsed
    );
    Ok(())
}

struct Verification {
    kind: &'static str,
    records: i64,
    object_size: i64,
    elapsed: std::time::Duration,
}

async fn verify_exact_restore(
    replica: &Replica,
    head: &crab_ltx::ReplicaHead,
    directory: &tempfile::TempDir,
) -> crab_ltx::Result<Verification> {
    let restored = directory.path().join("repository.sqlite");
    let started = Instant::now();
    replica.restore(head, &restored).await?;
    let (records, object_size) = aggregate_file(restored).await?;
    Ok(Verification {
        kind: "exact restore",
        records,
        object_size,
        elapsed: started.elapsed(),
    })
}

async fn verify_paged(
    replica: &Replica,
    head: &crab_ltx::ReplicaHead,
) -> crab_ltx::Result<Verification> {
    let started = Instant::now();
    let paged = replica.paged(head).await?;
    let (records, object_size) = tokio::task::spawn_blocking(move || {
        let connection = paged.open_sqlite()?;
        let aggregate = connection.connection().query_row(
            "SELECT count(*), sum(object_size) FROM repository_entries",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if let Some(error) = connection.take_read_error()? {
            return Err(error);
        }
        Ok::<_, CrabError>(aggregate)
    })
    .await
    .map_err(|_| CrabError::InvalidState("paged scale verifier stopped"))??;
    Ok(Verification {
        kind: "paged",
        records,
        object_size,
        elapsed: started.elapsed(),
    })
}

async fn aggregate_file(path: std::path::PathBuf) -> crab_ltx::Result<(i64, i64)> {
    tokio::task::spawn_blocking(move || {
        let connection = crab_ltx::rusqlite::Connection::open(path)?;
        Ok(connection.query_row(
            "SELECT count(*), sum(object_size) FROM repository_entries",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    })
    .await
    .map_err(|_| CrabError::InvalidState("scale verifier stopped"))?
}
