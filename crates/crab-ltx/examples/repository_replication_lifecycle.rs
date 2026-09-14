use crab_ltx::{CrabError, Limits, ManagedDb, Replica};
use crab_storage::{Store, StoreLayout};
use object_store::memory::InMemory;
use std::{path::Path, sync::Arc};

#[tokio::main]
async fn main() -> crab_ltx::Result<()> {
    let source_directory = tempfile::tempdir()?;
    let activation_directory = tempfile::tempdir()?;
    let recovery_directory = tempfile::tempdir()?;
    let limits = Limits::default();

    // The service maps one repository to one StoreLayout prefix and allocates
    // an epoch only after it has obtained and fenced an exclusive writer lease.
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = StoreLayout::new(store, "repositories/acme-api".into());
    let epoch_1 = Replica::new(layout.clone(), "epoch-1", limits)?;

    let source_path = source_directory.path().join("repository.sqlite");
    let mut writer = ManagedDb::open(&source_path, limits)?;
    writer.transaction(|transaction| {
        transaction.execute_batch(
            "CREATE TABLE issues(
                number INTEGER PRIMARY KEY,
                title TEXT NOT NULL
             );
             INSERT INTO issues VALUES(1, 'Initial issue');",
        )
    })?;

    // A successful SQLite transaction is local-only. The CaptureBatch must be
    // retained until conditional head publication returns a durable receipt.
    let initial_batch = writer.capture()?;
    let initial_head = epoch_1.replicate(&initial_batch, None).await?;
    let removed = writer.prune_published(&initial_head)?;
    writer.close()?;
    source_directory.close()?;
    println!(
        "1. Published epoch-1 TXID {} and pruned {removed} acknowledged local segment(s)",
        initial_head.position().txid
    );

    // Re-read the named head as a restarted server would, then query SQLite
    // through authenticated object ranges without materializing a local file.
    let observed_head = epoch_1
        .head()
        .await?
        .ok_or(CrabError::InvalidState("epoch-1 head is missing"))?;
    let paged = epoch_1.paged(&observed_head).await?;
    let page_count = paged.page_count();
    let read_count = tokio::task::spawn_blocking(move || -> crab_ltx::Result<u32> {
        let connection = paged.open_sqlite()?;
        let count =
            connection
                .connection()
                .query_row("SELECT count(*) FROM issues", [], |row| row.get(0))?;
        if let Some(error) = connection.take_read_error()? {
            return Err(error);
        }
        Ok(count)
    })
    .await
    .map_err(|_| CrabError::InvalidState("paged reader stopped"))??;
    if read_count != 1 {
        return Err(CrabError::InvalidState("paged query returned wrong state"));
    }
    println!("2. Queried {read_count} issue over {page_count} remote SQLite page(s)");

    // The application has fenced epoch-1 before allocating epoch-2. inherit()
    // pins the exact predecessor without copying its immutable LTX objects.
    let epoch_2 = Replica::new(layout, "epoch-2", limits)?;
    let inherited_head = epoch_2.inherit(&epoch_1, &observed_head).await?;
    let paged = epoch_2.paged(&inherited_head).await?;
    let activation_path = activation_directory.path().join("repository.sqlite");
    let (mut writer, batch, before, after) =
        tokio::task::spawn_blocking(move || -> crab_ltx::Result<_> {
            let mut writer = paged.open_writable(&activation_path)?;
            let before = writer
                .hydration()?
                .ok_or(CrabError::InvalidState("sparse hydration is missing"))?;
            writer.transaction(|transaction| {
                transaction.execute(
                    "INSERT INTO issues VALUES(2, 'Written by the new owner')",
                    [],
                )?;
                Ok(())
            })?;
            let batch = writer.capture()?;
            let after = writer.hydrate_step(64)?;
            Ok((writer, batch, before, after))
        })
        .await
        .map_err(|_| CrabError::InvalidState("sparse writer stopped"))??;

    let mutation_head = epoch_2.replicate(&batch, Some(&inherited_head)).await?;
    let removed = writer.prune_published(&mutation_head)?;
    tokio::task::spawn_blocking(move || writer.close())
        .await
        .map_err(|_| CrabError::InvalidState("sparse writer stopped"))??;
    println!(
        "3. Inherited epoch-1, resolved {}/{} then {}/{} pages, published epoch-2 TXID {}, and pruned {removed} segment(s)",
        before.resolved,
        before.total,
        after.resolved,
        after.total,
        mutation_head.position().txid
    );

    // Compaction advances the mutable head but preserves the immutable manifest
    // addressed by its digest, so backups and in-flight readers remain pinned.
    let historical_digest = mutation_head.manifest_digest();
    let original_segments = mutation_head.segment_count();
    let compacted_head = epoch_2.compact(&mutation_head).await?;
    let historical_head = epoch_2.open_exact(historical_digest).await?;

    let historical_path = recovery_directory.path().join("historical.sqlite");
    let compacted_path = recovery_directory.path().join("compacted.sqlite");
    epoch_2.restore(&historical_head, &historical_path).await?;
    epoch_2.restore(&compacted_head, &compacted_path).await?;
    if std::fs::read(&historical_path)? != std::fs::read(&compacted_path)? {
        return Err(CrabError::ChecksumMismatch);
    }
    let final_count = issue_count(&compacted_path)?;
    if final_count != 2 {
        return Err(CrabError::InvalidState(
            "restored query returned wrong state",
        ));
    }
    println!(
        "4. Compacted {original_segments} segment(s) to {}, reopened the historical manifest, and exactly restored {final_count} issues",
        compacted_head.segment_count()
    );
    Ok(())
}

fn issue_count(path: &Path) -> crab_ltx::Result<u32> {
    let connection = crab_ltx::rusqlite::Connection::open(path)?;
    Ok(connection.query_row("SELECT count(*) FROM issues", [], |row| row.get(0))?)
}
