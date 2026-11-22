use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, State},
};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{Address, directory::RegisterAccounts, repository::collaboration::watches},
    repository::{Repository, access::actor_id},
    server::Server,
};

pub(super) async fn get(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(route): Path<(String, String)>,
) -> Result<Json<watches::Preference>> {
    let repo = app::repository(&server, &principal, &route)?;
    let actor = actor_id(&server, app::account(&principal)?).await?;
    app::membership(&server, &principal, &repo).await?;
    let preference = match actor {
        Some(actor) => read(&server, &repo, actor).await?,
        None => watches::Preference::default(),
    };
    Ok(Json(preference))
}

pub(super) async fn set(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(route): Path<(String, String)>,
    Json(desired): Json<watches::Preference>,
) -> Result<Json<watches::Preference>> {
    let repo = app::repository(&server, &principal, &route)?;
    if desired.version > app::MAX_NUMBER {
        return Err(Error::Invalid("Notification preference version is invalid"));
    }
    let identity = app::account(&principal)?;
    let cell = app::repository_cell(&server, &repo)?;
    let accounts = app::command_output(
        cell.client
            .command::<RegisterAccounts>(
                Address::Directory,
                app::mutation_identity()?,
                vec![identity],
            )
            .await,
    )?;
    let [actor] = accounts.as_slice() else {
        return Err(Error::CellContract(
            "Notification account registration is inconsistent",
        ));
    };
    app::membership(&server, &principal, &repo).await?;
    app::account(&principal)?;
    let preference = app::command_output(
        cell.client
            .command::<watches::Set>(
                &cell.target,
                app::mutation_identity()?,
                watches::Change {
                    actor: *actor,
                    desired,
                },
            )
            .await,
    )?;
    preference.map(Json).ok_or(Error::Conflict)
}

pub(super) async fn read(
    server: &Server,
    repo: &Repository,
    actor: Uuid,
) -> Result<watches::Preference> {
    let cell = app::repository_cell(server, repo)?;
    app::query_output(
        cell.client
            .query::<watches::Get>(&cell.target, None, actor)
            .await,
    )
}
