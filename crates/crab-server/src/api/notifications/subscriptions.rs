use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, State},
};

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        directory::IdentityKey,
        repository::collaboration::{Subject, SubjectKind, watches},
    },
    repository::Repository,
    server::Server,
};

pub(super) async fn get(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(route): Path<(String, String)>,
) -> Result<Json<watches::Preference>> {
    let repo = app::repository(&server, &principal, &route)?;
    let actor = app::account(&principal)?;
    let preference = read(&server, &repo, actor).await?;
    app::membership(&server, &principal, &repo).await?;
    app::account(&principal)?;
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
    app::membership(&server, &principal, &repo).await?;
    let actor = app::account(&principal)?;
    write(&server, &repo, actor, desired)
        .await?
        .map(Json)
        .ok_or(Error::Conflict)
}

pub(super) async fn write(
    server: &Server,
    repo: &Repository,
    actor: IdentityKey,
    desired: watches::Preference,
) -> Result<Option<watches::Preference>> {
    let cell = app::repository_cell(server, repo)?;
    app::command_output(
        cell.client
            .command::<watches::Set>(
                &cell.target,
                app::mutation_identity()?,
                watches::Change { actor, desired },
            )
            .await,
    )
}

pub(super) async fn read(
    server: &Server,
    repo: &Repository,
    actor: IdentityKey,
) -> Result<watches::Preference> {
    let cell = app::repository_cell(server, repo)?;
    app::query_output(
        cell.client
            .query::<watches::Get>(&cell.target, None, actor)
            .await,
    )
}

fn subject(discussion: &str, number: u64) -> Result<Subject> {
    let kind = match discussion {
        "issues" => SubjectKind::Issue,
        "pulls" => SubjectKind::Pull,
        _ => return Err(Error::NotFound),
    };
    Ok(Subject {
        kind,
        number: app::number(number)?,
    })
}

pub(super) async fn get_thread(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, discussion, number)): Path<(String, String, String, u64)>,
) -> Result<Json<watches::ThreadPreference>> {
    let subject = subject(&discussion, number)?;
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let actor = app::account(&principal)?;
    let cell = app::repository_cell(&server, &repo)?;
    let preference = app::query_output(
        cell.client
            .query::<watches::GetThread>(
                &cell.target,
                None,
                watches::ThreadAccount { subject, actor },
            )
            .await,
    )?;
    app::membership(&server, &principal, &repo).await?;
    app::account(&principal)?;
    preference.map(Json).ok_or(Error::NotFound)
}

pub(super) async fn set_thread(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, discussion, number)): Path<(String, String, String, u64)>,
    Json(desired): Json<watches::ThreadPreference>,
) -> Result<Json<watches::ThreadPreference>> {
    let subject = subject(&discussion, number)?;
    let repo = app::repository(&server, &principal, &(owner, name))?;
    if desired.version > app::MAX_NUMBER {
        return Err(Error::Invalid("Notification preference version is invalid"));
    }
    let cell = app::repository_cell(&server, &repo)?;
    app::membership(&server, &principal, &repo).await?;
    let actor = app::account(&principal)?;
    let outcome = app::command_output(
        cell.client
            .command::<watches::SetThread>(
                &cell.target,
                app::mutation_identity()?,
                watches::ThreadChange {
                    subject,
                    actor,
                    desired,
                },
            )
            .await,
    )?;
    match outcome {
        watches::ThreadOutcome::Saved(preference) => Ok(Json(preference)),
        watches::ThreadOutcome::Missing => Err(Error::NotFound),
        watches::ThreadOutcome::Stale => Err(Error::Conflict),
    }
}
