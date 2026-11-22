use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, State, rejection::JsonRejection},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address,
        directory::saved_replies::{
            DeleteInput, DeleteOutcome, DeleteSavedReply, ListSavedReplies, PutInput, PutOutcome,
            PutSavedReply, ReplyEdit, SavedReply,
        },
    },
    server::Server,
};

#[derive(Serialize)]
pub(super) struct Replies {
    items: Vec<SavedReply>,
}

pub(super) async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<Replies>> {
    let identity = app::account(&principal)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let items = app::query_output(
        cells
            .query::<ListSavedReplies>(Address::Directory, None, identity)
            .await,
    )?;
    app::account(&principal)?;
    Ok(Json(Replies { items }))
}

pub(super) async fn put(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    input: std::result::Result<Json<ReplyEdit>, JsonRejection>,
) -> Result<Json<SavedReply>> {
    let identity = app::account(&principal)?;
    let Json(edit) = input.map_err(Error::Body)?;
    edit.validate().map_err(Error::Invalid)?;
    if id.is_nil() {
        return Err(Error::Invalid("Saved reply ID is invalid"));
    }
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let output = app::command_output(
        cells
            .command::<PutSavedReply>(
                Address::Directory,
                app::mutation_identity()?,
                PutInput { identity, id, edit },
            )
            .await,
    )?;
    app::account(&principal)?;
    match output {
        PutOutcome::Saved(reply) => Ok(Json(reply)),
        PutOutcome::Conflict => Err(Error::Conflict),
        PutOutcome::Missing => Err(Error::NotFound),
        PutOutcome::Limit => Err(Error::Invalid("An account can save up to 24 replies")),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeleteRequest {
    version: u64,
}

pub(super) async fn delete(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    input: std::result::Result<Json<DeleteRequest>, JsonRejection>,
) -> Result<Json<()>> {
    let identity = app::account(&principal)?;
    let Json(input) = input.map_err(Error::Body)?;
    if id.is_nil() || !(1..=app::MAX_NUMBER).contains(&input.version) {
        return Err(Error::Invalid("Saved reply ID or version is invalid"));
    }
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let output = app::command_output(
        cells
            .command::<DeleteSavedReply>(
                Address::Directory,
                app::mutation_identity()?,
                DeleteInput {
                    identity,
                    id,
                    version: input.version,
                },
            )
            .await,
    )?;
    app::account(&principal)?;
    match output {
        DeleteOutcome::Deleted => Ok(Json(())),
        DeleteOutcome::Conflict => Err(Error::Conflict),
        DeleteOutcome::Missing => Err(Error::NotFound),
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::http::Harness;
    use axum::http::{StatusCode, header};
    use reqwest::Method;
    use serde_json::{Value, json};
    use uuid::Uuid;

    const ROUTE: &str = "/api/profile/saved-replies";

    #[tokio::test]
    async fn versioned_replies_are_private_and_survive_profile_changes() {
        let h = Harness::new(false).await;
        let alice = h.login().await;
        let id = Uuid::now_v7();
        let path = format!("{ROUTE}/{id}");
        let create = json!({"version":0,"title":"Triage","body":"Thanks, %cursor%!"});
        assert_eq!(
            h.http
                .put(format!("{}{path}", h.origin))
                .header(header::COOKIE, &alice)
                .json(&create)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
        let created: Value = h
            .write(Method::PUT, &path, &alice, create.clone())
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(
            created,
            json!({"id":id,"version":1,"title":"Triage","body":"Thanks, %cursor%!"})
        );
        let edit = json!({"version":1,"title":"Triage","body":"Thanks for the report!"});
        let edited: Value = h
            .write(Method::PUT, &path, &alice, edit.clone())
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(edited["version"], 2);
        assert_eq!(
            h.write(Method::PUT, &path, &alice, edit)
                .await
                .json::<Value>()
                .await
                .unwrap(),
            edited
        );
        assert_eq!(
            h.write(
                Method::PUT,
                &path,
                &alice,
                json!({"version":1,"title":"Conflict","body":"Different"})
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
        for (method, body) in [
            (
                Method::POST,
                json!({"request_id":Uuid::now_v7(),"handle":"alice","name":"Alice"}),
            ),
            (
                Method::PATCH,
                json!({"version":1,"handle":"alice-renamed","name":"Alice"}),
            ),
        ] {
            h.write(method, "/api/profile", &alice, body)
                .await
                .error_for_status()
                .unwrap();
        }
        let second_session = h.login().await;
        assert_eq!(
            h.json(ROUTE, &second_session).await,
            json!({"items":[edited]})
        );
        *h.provider.mode.lock().await = "member".into();
        let bob = h.login().await;
        assert_eq!(h.json(ROUTE, &bob).await, json!({"items":[]}));
        assert_eq!(
            h.write(Method::DELETE, &path, &bob, json!({"version":2}))
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        for version in [2, 2] {
            assert!(
                h.write(Method::DELETE, &path, &alice, json!({"version":version}))
                    .await
                    .status()
                    .is_success()
            );
        }
        assert_eq!(
            h.write(Method::PUT, &path, &alice, create).await.status(),
            StatusCode::CONFLICT
        );
        assert_eq!(h.json(ROUTE, &alice).await, json!({"items":[]}));
        h.close().await;
    }
}
