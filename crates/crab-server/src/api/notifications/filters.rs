use super::*;
use crate::cells::directory::notifications::filters::{
    Collection, Input, ListFilters, Outcome, Update, UpdateFilters,
};
use axum::extract::rejection::JsonRejection;

#[cfg(test)]
mod tests;

pub(super) async fn get(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<Collection>> {
    let identity = app::account(&principal)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let filters = app::query_output(
        cells
            .query::<ListFilters>(Address::Directory, None, identity)
            .await,
    )?;
    app::account(&principal)?;
    Ok(Json(filters))
}

pub(super) async fn update(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    input: std::result::Result<Json<Update>, JsonRejection>,
) -> Result<Json<Collection>> {
    let identity = app::account(&principal)?;
    let Json(update) = input.map_err(Error::Body)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let result = app::command_output(
        cells
            .command::<UpdateFilters>(
                Address::Directory,
                app::mutation_identity()?,
                Input { identity, update },
            )
            .await,
    )?;
    app::account(&principal)?;
    match result {
        Outcome::Saved(filters) => Ok(Json(filters)),
        Outcome::Conflict => Err(Error::Conflict),
        Outcome::Limit => Err(Error::Invalid(
            "An account can save up to 15 notification filters",
        )),
    }
}
