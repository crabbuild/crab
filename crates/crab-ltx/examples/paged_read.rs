use crab_ltx::{CrabError, Limits, ManagedDb, Replica};
use crab_storage::{Store, StoreLayout};
use object_store::memory::InMemory;
use std::sync::Arc;

#[tokio::main]
async fn main() -> crab_ltx::Result<()> {
    let source = tempfile::tempdir()?;
    let limits = Limits::default();
    let mut writer = ManagedDb::open(&source.path().join("repository.sqlite"), limits)?;
    writer.transaction(|tx| {
        tx.execute_batch(
            "CREATE TABLE issues(number INTEGER PRIMARY KEY, title TEXT NOT NULL);
             INSERT INTO issues VALUES(1, 'First'), (2, 'Second'), (3, 'Third');",
        )
    })?;
    let batch = writer.capture()?;

    let store = Store::new(Arc::new(InMemory::new()));
    let layout = StoreLayout::new(store, "repositories/acme-api".into());
    let replica = Replica::new(layout, "epoch-1", limits)?;
    let head = replica.replicate(&batch, None).await?;
    writer.close()?;
    source.close()?;

    // Building the page map reads authenticated indexes, not full LTX bodies.
    let paged = replica.paged(&head).await?;
    println!(
        "Opened published cut at TXID {} ({} pages of {} bytes)",
        paged.position().txid,
        paged.page_count(),
        paged.page_size()
    );
    let issue_count = tokio::task::spawn_blocking(move || -> crab_ltx::Result<u32> {
        // SQLite faults required pages from the object store through a read-only VFS.
        let sql = paged.open_sqlite()?;
        sql.connection()
            .query_row("SELECT count(*) FROM issues", [], |row| row.get(0))
            .map_err(Into::into)
    })
    .await
    .map_err(|_| CrabError::InvalidState("paged SQL worker stopped"))??;
    println!("Read {issue_count} issues without materializing a local database file");
    Ok(())
}
