use crab_ltx::{CrabError, Limits, ManagedDb, Replica};
use crab_storage::{Store, StoreLayout};
use object_store::memory::InMemory;
use std::sync::Arc;

#[tokio::main]
async fn main() -> crab_ltx::Result<()> {
    let source = tempfile::tempdir()?;
    let recovery = tempfile::tempdir()?;
    let limits = Limits::default();
    let mut writer = ManagedDb::open(&source.path().join("repository.sqlite"), limits)?;
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = StoreLayout::new(store, "repositories/acme-api".into());
    let replica = Replica::new(layout, "epoch-1", limits)?;

    let mut head = None;
    for title in ["First", "Second", "Third"] {
        writer.transaction(|tx| {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS issues(
                    number INTEGER PRIMARY KEY,
                    title TEXT NOT NULL
                );",
            )?;
            tx.execute("INSERT INTO issues(title) VALUES(?1)", [title])?;
            Ok(())
        })?;
        let batch = writer.capture()?;
        head = Some(replica.replicate(&batch, head.as_ref()).await?);
    }
    writer.close()?;
    let head = head.ok_or(CrabError::InvalidState("no published head"))?;
    let historical_digest = head.manifest_digest();
    let historical_position = head.position();

    let compacted = replica.compact(&head).await?;
    let historical = replica.open_exact(historical_digest).await?;
    let before = recovery.path().join("historical.sqlite");
    let after = recovery.path().join("compacted.sqlite");
    replica.restore(&historical, &before).await?;
    replica.restore(&compacted, &after).await?;
    if std::fs::read(before)? != std::fs::read(after)? {
        return Err(CrabError::ChecksumMismatch);
    }

    println!(
        "Compacted {} segments to {} at unchanged TXID {}",
        head.segment_count(),
        compacted.segment_count(),
        historical_position.txid
    );
    println!("The immutable historical manifest remains restorable");
    Ok(())
}
