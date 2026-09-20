use crab_ltx::{Limits, LocalSegment, ManagedDb, VerifiedPlan, restore_exact};

fn main() -> crab_ltx::Result<()> {
    let source = tempfile::tempdir()?;
    let replica = tempfile::tempdir()?;
    let restored = tempfile::tempdir()?;
    let limits = Limits::default();
    let mut db = ManagedDb::open(&source.path().join("repository.sqlite"), limits)?;
    db.transaction(|tx| {
        tx.execute(
            "CREATE TABLE issues (number INTEGER PRIMARY KEY, title TEXT NOT NULL)",
            [],
        )?;
        tx.execute(
            "INSERT INTO issues VALUES (1, 'Recover this issue from LTX')",
            [],
        )?;
        Ok(())
    })?;
    let batch = db.capture()?;
    let mut files = Vec::new();
    for segment in batch.segments {
        let path = replica.path().join(format!(
            "{}-{}.ltx",
            segment.info().min_txid,
            segment.info().max_txid
        ));
        // A local copy demonstrates the transport boundary, not cloud durability.
        std::fs::copy(segment.path(), &path)?;
        files.push(LocalSegment::new(path, segment.info().clone()));
    }
    db.close()?;
    source.close()?;
    let plan = VerifiedPlan::new(&files, batch.position, limits)?;
    let path = restored.path().join("repository.sqlite");
    restore_exact(&plan, &path)?;
    let conn = crab_ltx::rusqlite::Connection::open(path)?;
    let title: String = conn.query_row("SELECT title FROM issues WHERE number = 1", [], |row| {
        row.get(0)
    })?;
    println!("Restored issue #1: {title}");
    println!(
        "Verified LTX position: {} / {:016x}",
        batch.position.txid, batch.position.checksum
    );
    Ok(())
}
