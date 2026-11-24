use uuid::Uuid;

use super::*;
use crate::{
    catalog::{CatalogDocument, transfer::Recipient},
    cells::directory::{
        IdentityKey,
        notifications::email::{QueueTransferMail, TRANSFER_BATCH, TransferMail, TransferSource},
    },
};

pub(super) async fn run(server: &Server, cells: &CellClient) {
    let mut captured = None;
    loop {
        let result = tokio::select! {
            _ = server.cancellation.cancelled() => return,
            result = reconcile(server, cells, &mut captured) => result,
        };
        if let Err(error) = result {
            tracing::warn!(%error, "Transfer email reconciliation is temporarily unavailable");
        }
        tokio::select! {
            _ = server.cancellation.cancelled() => return,
            _ = tokio::time::sleep(IDLE) => (),
        }
    }
}

pub(super) async fn reconcile(
    server: &Server,
    cells: &CellClient,
    captured: &mut Option<Arc<CatalogDocument>>,
) -> app::Result<()> {
    let Some(catalog) = server.catalog() else {
        return Ok(());
    };
    let (document, _) = catalog
        .load_snapshot()
        .await
        .map_err(app::repository_access_error)?;
    if captured
        .as_ref()
        .is_some_and(|previous| Arc::ptr_eq(previous, &document))
    {
        return Ok(());
    }

    // The committed offer is the durable source. Re-reading it after a lost
    // Directory reply or restart is safe because enqueue binds the same job ID.
    let now = crate::cells::unix_now_ms().map_err(Error::Cell)?;
    let mut pending = document.repositories.iter().filter_map(|record| {
        let offer = record.transfer.as_ref()?;
        let Recipient::Account { identity } = &offer.recipient else {
            return None;
        };
        offer.available(record, now).then_some(TransferMail {
            recipient: IdentityKey {
                issuer: identity.issuer.clone(),
                subject: identity.subject.clone(),
            },
            repository: record.id,
            source: TransferSource {
                offer: offer.id,
                created_at_ms: u64::try_from(offer.created_at_ms()).ok()?,
            },
        })
    });
    loop {
        let batch = pending.by_ref().take(TRANSFER_BATCH).collect::<Vec<_>>();
        if batch.is_empty() {
            break;
        }
        app::command_output(
            cells
                .command::<QueueTransferMail>(Address::Directory, app::mutation_identity()?, batch)
                .await,
        )?;
    }
    *captured = Some(document);
    Ok(())
}

pub(super) async fn still_pending(
    server: &Server,
    repository: Uuid,
    source: TransferSource,
    recipient: &IdentityKey,
) -> app::Result<bool> {
    let Some(catalog) = server.catalog() else {
        return Ok(false);
    };
    let (document, _) = catalog
        .load_snapshot()
        .await
        .map_err(app::repository_access_error)?;
    let now = crate::cells::unix_now_ms().map_err(Error::Cell)?;
    let Some(record) = document.repository_by_id(repository) else {
        return Ok(false);
    };
    Ok(record.transfer.as_ref().is_some_and(|offer| {
        offer.id == source.offer
            && u64::try_from(offer.created_at_ms()) == Ok(source.created_at_ms)
            && offer.available(record, now)
            && matches!(&offer.recipient, Recipient::Account { identity }
                if identity.issuer == recipient.issuer && identity.subject == recipient.subject)
    }))
}
