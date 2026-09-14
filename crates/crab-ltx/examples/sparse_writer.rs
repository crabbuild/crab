use crab_ltx::{CrabError, Limits, ManagedDb, Replica};
use crab_storage::{Store, StoreLayout};
use object_store::memory::InMemory;
use std::sync::Arc;

#[tokio::main]
async fn main() -> crab_ltx::Result<()> {
    let source_directory = tempfile::tempdir()?;
    let activation_directory = tempfile::tempdir()?;
    let recovery_directory = tempfile::tempdir()?;
    let limits = Limits::default();
    let mut source = ManagedDb::open(&source_directory.path().join("repository.sqlite"), limits)?;
    source.transaction(|tx| {
        tx.execute_batch(
            "CREATE TABLE issues(number INTEGER PRIMARY KEY, title TEXT NOT NULL);
             INSERT INTO issues VALUES(1, 'Inherited issue');",
        )
    })?;

    let store = Store::new(Arc::new(InMemory::new()));
    let layout = StoreLayout::new(store, "repositories/acme-api".into());
    let previous = Replica::new(layout.clone(), "epoch-1", limits)?;
    let previous_head = previous.replicate(&source.capture()?, None).await?;
    source.close()?;
    source_directory.close()?;

    // The application must fence epoch-1 before allocating epoch-2.
    let current = Replica::new(layout, "epoch-2", limits)?;
    let inherited = current.inherit(&previous, &previous_head).await?;
    let paged = current.paged(&inherited).await?;
    let sparse_path = activation_directory.path().join("repository.sqlite");
    let (writer, batch) = tokio::task::spawn_blocking(move || -> crab_ltx::Result<_> {
        let mut writer = paged.open_writable(&sparse_path)?;
        writer.transaction(|tx| {
            tx.execute("INSERT INTO issues VALUES(2, 'Sparse write')", [])?;
            Ok(())
        })?;
        let batch = writer.capture()?;
        Ok((writer, batch))
    })
    .await
    .map_err(|_| CrabError::InvalidState("sparse writer stopped"))??;
    let head = current.replicate(&batch, Some(&inherited)).await?;
    tokio::task::spawn_blocking(move || writer.close())
        .await
        .map_err(|_| CrabError::InvalidState("sparse writer stopped"))??;

    let restored = recovery_directory.path().join("repository.sqlite");
    current.restore(&head, &restored).await?;
    let connection = crab_ltx::rusqlite::Connection::open(restored)?;
    let count: u32 = connection.query_row("SELECT count(*) FROM issues", [], |row| row.get(0))?;
    println!(
        "Inherited epoch-1, wrote in epoch-2, and restored {count} issues at TXID {}",
        head.position().txid
    );
    Ok(())
}
