use crab_metadata::capsule_protocol::resolve_capsule_plan_receipt;

use crate::{
    app::{self, Result},
    cells::repository::collaboration::links::commits::{
        Next, Publication, Reservation, Reserve, Resolution, Resolve,
    },
    repository::Repository,
    server::Server,
};

pub(crate) async fn reserve(
    server: &Server,
    repo: &Repository,
    reservation: Reservation,
) -> Result<bool> {
    let cell = app::repository_cell(server, repo)?;
    app::command_output(
        cell.client
            .command::<Reserve>(cell.target, app::mutation_identity()?, reservation)
            .await,
    )
}

async fn reconcile(
    server: &Server,
    repo: &Repository,
    publication: Publication,
    due: Option<u64>,
) -> Result<()> {
    let receipt = resolve_capsule_plan_receipt(&repo.store, &repo.layout, &publication.plan)
        .await
        .map_err(|source| crate::Error::Settings {
            source: Box::new(source),
        })?;
    let resolution = if let Some(receipt) = receipt {
        if !receipt.transaction().edits().iter().any(|edit| {
            edit.ref_name() == publication.reference
                && edit.expected_old() == publication.before.as_deref()
                && edit.new_oid() == Some(publication.after.as_str())
        }) {
            return Err(app::Error::CellContract(
                "issue closure receipt has a different ref edit",
            ));
        }
        Resolution::Committed(publication.plan)
    } else if let Some(due) = due {
        Resolution::Uncommitted {
            plan: publication.plan,
            due,
        }
    } else {
        return Ok(());
    };
    let cell = app::repository_cell(server, repo)?;
    app::command_output(
        cell.client
            .command::<Resolve>(cell.target, app::mutation_identity()?, resolution)
            .await,
    )
}

pub(crate) async fn finish(server: &Server, repo: &Repository, publication: Publication) {
    if let Err(error) = reconcile(server, repo, publication, None).await {
        tracing::warn!(repository = %repo.id, %error, "issue closure will retry");
    }
}

pub(crate) async fn recover(server: &Server, repo: &Repository) -> Result<()> {
    let cell = app::repository_cell(server, repo)?;
    for _ in 0..8 {
        if server.cancellation.is_cancelled() {
            break;
        }
        let Some((due, publication)) =
            app::query_output(cell.client.query::<Next>(cell.target, None, ()).await)?
        else {
            break;
        };
        reconcile(server, repo, publication, Some(due)).await?;
    }
    Ok(())
}
