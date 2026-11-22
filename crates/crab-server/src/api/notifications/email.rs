use std::sync::Arc;

use axum::{Extension, Json, extract::State};
use serde::{Deserialize, Serialize};

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address,
        directory::notifications::email::{Preference, PreferenceChange, ReadEmail, SetEmail},
    },
    server::Server,
};

#[derive(Serialize)]
pub(super) struct Settings {
    configured: bool,
    #[serde(flatten)]
    preference: Preference,
}

pub(super) async fn get(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<Settings>> {
    let identity = app::account(&principal)?;
    let configured = server.email.is_some();
    let preference = if configured {
        let cells = server.cells().ok_or(Error::CellUnavailable)?;
        app::query_output(
            cells
                .query::<ReadEmail>(Address::Directory, None, identity)
                .await,
        )?
    } else {
        Preference::default()
    };
    Ok(Json(Settings {
        configured,
        preference,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Change {
    enabled: bool,
    version: u64,
}

pub(super) async fn set(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Json(Change { enabled, version }): Json<Change>,
) -> Result<Json<Settings>> {
    let identity = app::account(&principal)?;
    if server.email.is_none() {
        return Err(Error::Invalid(
            "Email notifications are unavailable on this server",
        ));
    }
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let preference = app::command_output(
        cells
            .command::<SetEmail>(
                Address::Directory,
                app::mutation_identity()?,
                PreferenceChange {
                    identity,
                    enabled,
                    version,
                },
            )
            .await,
    )?
    .ok_or(Error::PageChanged)?;
    Ok(Json(Settings {
        configured: true,
        preference,
    }))
}
