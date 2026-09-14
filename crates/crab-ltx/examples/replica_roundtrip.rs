use crab_ltx::{Limits, ManagedDb, Replica};
use crab_storage::{Store, StoreLayout};
use object_store::memory::InMemory;
use std::sync::Arc;

#[tokio::main]
async fn main() -> crab_ltx::Result<()> {
    let source = tempfile::tempdir()?;
    let restored = tempfile::tempdir()?;
    let limits = Limits::default();
    let mut writer = ManagedDb::open(&source.path().join("repository.sqlite"), limits)?;
    writer.transaction(|tx| {
        tx.execute_batch(
            "CREATE TABLE issues(number INTEGER PRIMARY KEY, title TEXT NOT NULL);
             INSERT INTO issues VALUES(1, 'Published through Replica');",
        )
    })?;
    let batch = writer.capture()?;

    // Replace InMemory with Crab's configured S3/RustFS/GCS/Azure Store.
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = StoreLayout::new(store, "repositories/acme-api".into());
    // The service allocates this epoch after acquiring repository ownership.
    let replica = Replica::new(layout, "epoch-1", limits)?;
    let head = replica.replicate(&batch, None).await?;
    println!(
        "Published TXID {} with manifest {}",
        head.position().txid,
        blake3::Hash::from_bytes(head.manifest_digest())
    );

    writer.close()?;
    source.close()?;

    let database = restored.path().join("repository.sqlite");
    replica.restore(&head, &database).await?;
    let connection = crab_ltx::rusqlite::Connection::open(database)?;
    let title: String =
        connection.query_row("SELECT title FROM issues WHERE number = 1", [], |row| {
            row.get(0)
        })?;
    println!("Restored issue #1 after source loss: {title}");
    Ok(())
}
