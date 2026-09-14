mod support;

use crab_ltx::{CrabError, Limits, ManagedDb, Replica};
use crab_storage::StoreLayout;
use std::time::Instant;
use support::{
    RECORD_COUNT, expected_object_size_sum, publish_records, records_per_second, rustfs_target,
};

struct QueryReport {
    sqlite_open_micros: u128,
    point_lookup_micros: u128,
    range_query_micros: u128,
    full_scan_micros: u128,
}

#[tokio::main]
async fn main() -> crab_ltx::Result<()> {
    let source_directory = tempfile::tempdir()?;
    let limits = Limits::default();
    let writer = ManagedDb::open(&source_directory.path().join("repository.sqlite"), limits)?;
    let target = rustfs_target("million-record-paged-read")?;
    println!("RustFS object prefix: {}", target.repository_prefix);
    let layout = StoreLayout::new(target.store, target.repository_prefix);
    let replica = Replica::new(layout, "epoch-1", limits)?;
    let report = publish_records(writer, &replica).await?;
    let published_position = report.head.position();
    let manifest_segments = report.head.segment_count();
    let load_elapsed = report.elapsed;
    let sqlite_elapsed = report.sqlite_elapsed;
    let publication_elapsed = report.publication_elapsed;
    let captured_segments = report.captured_segments;
    let pruned_segments = report.pruned_segments;
    let writer = report.writer;
    tokio::task::spawn_blocking(move || writer.close())
        .await
        .map_err(|_| CrabError::InvalidState("performance writer stopped"))??;
    source_directory.close()?;

    // Re-read the head after deleting the source so the measured read path has
    // no live publication receipt or local SQLite fallback.
    let head_started = Instant::now();
    let head = replica
        .head()
        .await?
        .ok_or(CrabError::InvalidState("performance head is missing"))?;
    let head_elapsed = head_started.elapsed();
    if head.position() != published_position {
        return Err(CrabError::InvalidState(
            "observed performance head does not match publication",
        ));
    }

    let page_map_started = Instant::now();
    let paged = replica.paged(&head).await?;
    let page_map_elapsed = page_map_started.elapsed();
    let page_count = paged.page_count();
    let page_size = paged.page_size();
    let expected_sum = expected_object_size_sum();
    let queries = tokio::task::spawn_blocking(move || -> crab_ltx::Result<QueryReport> {
        let sqlite_started = Instant::now();
        let connection = paged.open_sqlite()?;
        let sqlite_open_micros = sqlite_started.elapsed().as_micros();

        let point_started = Instant::now();
        let point_path: String = connection.connection().query_row(
            "SELECT path FROM repository_entries WHERE id = ?1",
            [RECORD_COUNT],
            |row| row.get(0),
        )?;
        let point_lookup_micros = point_started.elapsed().as_micros();
        if point_path != format!("objects/{RECORD_COUNT:010}") {
            return Err(CrabError::InvalidState(
                "million-record point lookup returned wrong row",
            ));
        }

        let range_started = Instant::now();
        let range_count: i64 = connection.connection().query_row(
            "SELECT count(*) FROM repository_entries WHERE repository_id = 42",
            [],
            |row| row.get(0),
        )?;
        let range_query_micros = range_started.elapsed().as_micros();
        if range_count != RECORD_COUNT / 1_000 {
            return Err(CrabError::InvalidState(
                "million-record range query returned wrong count",
            ));
        }

        let scan_started = Instant::now();
        let (record_count, object_size_sum): (i64, i64) = connection.connection().query_row(
            "SELECT count(*), sum(object_size) FROM repository_entries",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let full_scan_micros = scan_started.elapsed().as_micros();
        if record_count != RECORD_COUNT || object_size_sum != expected_sum {
            return Err(CrabError::InvalidState(
                "million-record full scan returned wrong aggregate",
            ));
        }
        if let Some(error) = connection.take_read_error()? {
            return Err(error);
        }
        Ok(QueryReport {
            sqlite_open_micros,
            point_lookup_micros,
            range_query_micros,
            full_scan_micros,
        })
    })
    .await
    .map_err(|_| CrabError::InvalidState("performance reader stopped"))??;

    println!("\nMillion-record paged-read performance complete");
    println!("records:              {RECORD_COUNT}");
    println!("remote SQLite pages:  {page_count} x {page_size} bytes");
    println!("captured segments:    {captured_segments}");
    println!("manifest segments:    {manifest_segments}");
    println!("pruned segments:      {pruned_segments}");
    println!("SQLite + capture:     {sqlite_elapsed:.3?}");
    println!("object publication:   {publication_elapsed:.3?}");
    println!("load preparation:     {load_elapsed:.3?}");
    println!(
        "load throughput:      {:.0} records/s",
        records_per_second(RECORD_COUNT, load_elapsed)
    );
    println!("head read:            {head_elapsed:.3?}");
    println!("page-map construction: {page_map_elapsed:.3?}");
    println!("SQLite VFS open:      {} us", queries.sqlite_open_micros);
    println!("point lookup:         {} us", queries.point_lookup_micros);
    println!("indexed range count:  {} us", queries.range_query_micros);
    println!("full aggregate scan:  {} us", queries.full_scan_micros);
    Ok(())
}
