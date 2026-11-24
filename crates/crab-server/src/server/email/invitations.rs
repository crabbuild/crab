use super::*;
use crate::cells::{
    directory::{
        notifications::email::{InvitationMail, InvitationSource, QueueInvitationMail},
        organizations::{Candidate, Candidates, Query},
    },
    organization::{
        Caller,
        discovery::{GetLanding, Landing},
    },
};
use futures_util::{StreamExt, TryStreamExt, stream};
use uuid::Uuid;

pub(super) async fn run(server: &Server, cells: &CellClient) {
    let mut after = None;
    loop {
        let result = tokio::select! {
            _ = server.cancellation.cancelled() => return,
            result = reconcile(cells, after.clone()) => result,
        };
        match result {
            Ok(next) => {
                after = next;
                if after.is_some() {
                    tokio::task::yield_now().await;
                    continue;
                }
            }
            Err(error) => {
                tracing::warn!(%error, "Invitation email reconciliation is temporarily unavailable")
            }
        }
        tokio::select! {
            _ = server.cancellation.cancelled() => return,
            _ = tokio::time::sleep(IDLE) => (),
        }
    }
}

pub(super) async fn reconcile(
    cells: &CellClient,
    after: Option<String>,
) -> app::Result<Option<String>> {
    let page = app::query_output(
        cells
            .query::<Candidates>(Address::Directory, None, Query { actor: None, after })
            .await,
    )?;
    let batch: Vec<_> = stream::iter(page.items)
        .map(
            |Candidate {
                 actor,
                 organization,
             }| async move {
                Ok::<_, Error>(
                    pending(cells, organization, actor)
                        .await?
                        .map(|source| InvitationMail { actor, source }),
                )
            },
        )
        .buffered(8)
        .try_filter_map(|entry| async { Ok(entry) })
        .try_collect()
        .await?;
    if !batch.is_empty() {
        app::command_output(
            cells
                .command::<QueueInvitationMail>(
                    Address::Directory,
                    app::mutation_identity()?,
                    batch,
                )
                .await,
        )?;
    }
    Ok(page.next)
}

pub(super) async fn pending(
    cells: &CellClient,
    organization: Uuid,
    actor: Uuid,
) -> app::Result<Option<InvitationSource>> {
    let landing = match cells
        .query::<GetLanding>(Address::Organization(organization), None, Caller { actor })
        .await
    {
        Ok(result) => result.output,
        Err(crate::cells::Error::NotInitialized) => None,
        Err(error) => return Err(Error::Cell(error)),
    };
    Ok(match landing {
        Some(Landing::Invitation { invitation, .. }) => Some(InvitationSource {
            organization,
            version: invitation.version,
            created_at_ms: invitation.created_at_ms,
        }),
        _ => None,
    })
}
