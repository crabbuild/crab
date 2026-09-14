mod support;

use crab_ltx::{CrabError, Limits, ManagedDb, Replica};
use crab_storage::StoreLayout;
use std::time::Instant;
use support::{
    RECORD_COUNT, expected_object_size_sum, publish_records, records_per_second, rustfs_target,
};

#[tokio::main]
async fn main() -> crab_ltx::Result<()> {
    let source_directory = tempfile::tempdir()?;
    let recovery_directory = tempfile::tempdir()?;
    let limits = Limits::default();
    let database = source_directory.path().join("repository.sqlite");
    let writer = ManagedDb::open(&database, limits)?;

    let target = rustfs_target("million-record-replication-load")?;
    println!("RustFS object prefix: {}", target.repository_prefix);
    let layout = StoreLayout::new(target.store, target.repository_prefix);
    let replica = Replica::new(layout, "epoch-1", limits)?;
    let report = publish_records(writer, &replica).await?;
    let database_bytes = std::fs::metadata(report.writer.path())?.len();
    let position = report.head.position();
    let manifest_segments = report.head.segment_count();
    let published_head = report.head;
    let writer = report.writer;
    tokio::task::spawn_blocking(move || writer.close())
        .await
        .map_err(|_| CrabError::InvalidState("load writer stopped"))??;
    source_directory.close()?;

    let restored = recovery_directory.path().join("repository.sqlite");
    let recovery_started = Instant::now();
    replica.restore(&published_head, &restored).await?;
    let recovery_elapsed = recovery_started.elapsed();
    let (restored_count, restored_size) =
        tokio::task::spawn_blocking(move || -> crab_ltx::Result<(i64, i64)> {
            let connection = crab_ltx::rusqlite::Connection::open(restored)?;
            Ok(connection.query_row(
                "SELECT count(*), sum(object_size) FROM repository_entries",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .await
        .map_err(|_| CrabError::InvalidState("load verifier stopped"))??;
    if restored_count != RECORD_COUNT || restored_size != expected_object_size_sum() {
        return Err(CrabError::InvalidState(
            "restored million-record aggregate does not match",
        ));
    }

    println!("\nMillion-record replication load complete");
    println!("records:                 {restored_count}");
    println!("logical object bytes:    {restored_size}");
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
        records_per_second(RECORD_COUNT, report.elapsed)
    );
    println!("exact recovery time:      {recovery_elapsed:.3?}");
    Ok(())
}
