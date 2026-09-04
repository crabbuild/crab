use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    middleware,
    routing::get,
};
use serde_json::{Value, json};

use crate::{
    app::{self, Error, Result},
    auth::{Identity, Principal},
    server::{Repository, Server},
};

const MAX_ASSIGNEES: usize = 10;

#[derive(Clone)]
pub(crate) struct Assignee {
    pub subject: String,
    pub name: String,
}

pub(crate) fn available(repo: &Repository, actor: &Identity) -> Vec<Assignee> {
    if actor.issuer == "urn:crab:local" {
        return vec![Assignee {
            subject: "operator".into(),
            name: "Local operator".into(),
        }];
    }
    repo.config
        .members
        .iter()
        .map(|member| Assignee {
            subject: member.subject.clone(),
            name: member.name.clone(),
        })
        .collect()
}

fn view(assignee: &Assignee) -> Value {
    json!({"subject":assignee.subject,"name":assignee.name})
}

pub(crate) fn selection_view(subjects: &[String], available: &[Assignee]) -> Vec<Value> {
    available
        .iter()
        .filter(|assignee| subjects.contains(&assignee.subject))
        .map(view)
        .collect()
}

pub(crate) fn validate_selection(
    mut subjects: Vec<String>,
    available: &[Assignee],
) -> Result<Vec<String>> {
    if subjects.len() > MAX_ASSIGNEES || subjects.iter().any(String::is_empty) {
        return Err(Error::Invalid("An item supports at most 10 assignees"));
    }
    subjects.sort_unstable();
    if subjects.windows(2).any(|pair| pair[0] == pair[1])
        || subjects.iter().any(|subject| {
            !available
                .iter()
                .any(|assignee| assignee.subject == *subject)
        })
    {
        return Err(Error::Invalid(
            "Assignee selection contains an unknown or duplicate repository member",
        ));
    }
    Ok(subjects)
}

pub(crate) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/assignees", get(list))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key)?;
    let available = available(repo, &app::actor(&principal)?);
    Ok(Json(json!({
        "items":available.iter().map(view).collect::<Vec<_>>(),
        "can_manage":principal.can_write(&repo.config),
    })))
}
