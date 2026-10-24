use super::{activity, issues, profiles::attribution::Authors, pulls, reactions};
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::collaboration::{
        Subject, SubjectKind,
        conversation::{Cursor, Entry, Key, ListConversation, Position, Query as Input, Source},
    },
    server::Server,
};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    middleware,
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/issues/{number}/conversation",
            get(issue),
        )
        .route(
            "/api/repos/{owner}/{name}/pulls/{number}/conversation",
            get(pull),
        )
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Parameters {
    before: Option<String>,
    after: Option<String>,
    focus: Option<String>,
    limit: Option<u8>,
}
fn decode(value: &str) -> Result<Cursor> {
    if value.len() > 512 {
        return Err(Error::Invalid("Conversation cursor is invalid"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| Error::Invalid("Conversation cursor is invalid"))?;
    serde_json::from_slice(&bytes).map_err(|_| Error::Invalid("Conversation cursor is invalid"))
}
fn encode(value: Option<Cursor>) -> Result<Option<String>> {
    value
        .map(|v| Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&v)?)))
        .transpose()
}
impl Parameters {
    fn position(&self) -> Result<Position> {
        if usize::from(self.before.is_some())
            + usize::from(self.after.is_some())
            + usize::from(self.focus.is_some())
            > 1
        {
            return Err(Error::Invalid("Choose one conversation position"));
        }
        if let Some(value) = &self.before {
            return Ok(Position::Before(decode(value)?));
        }
        if let Some(value) = &self.after {
            return Ok(Position::After(decode(value)?));
        }
        let Some(focus) = &self.focus else {
            return Ok(Position::Latest);
        };
        let (kind, number) = focus
            .split_once(':')
            .ok_or(Error::Invalid("Conversation focus is invalid"))?;
        let source = match kind {
            "comment" => Source::Comment,
            "review" => Source::Review,
            "activity" => Source::Activity,
            _ => return Err(Error::Invalid("Conversation focus is invalid")),
        };
        let number = number
            .parse()
            .map_err(|_| Error::Invalid("Conversation focus is invalid"))?;
        Ok(Position::Focus(Key {
            source,
            number: app::number(number)?,
        }))
    }
}
async fn page(
    server: &Server,
    principal: &Principal,
    key: (String, String),
    subject: Subject,
    params: Parameters,
) -> Result<Json<Value>> {
    let repo = app::repository(server, principal, &key)?;
    app::number(subject.number)?;
    let position = params.position()?;
    let input = Input {
        subject,
        position,
        limit: params.limit.unwrap_or(30),
    };
    input.validate().map_err(Error::Invalid)?;
    let actor = app::actor(principal)?;
    let cell = app::repository_cell(server, &repo)?;
    let page = app::query_output(
        cell.client
            .query::<ListConversation>(&cell.target, None, input)
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let head = match &page.source {
        Some(source) => match &source.merged_head {
            Some(oid) => Some(oid.clone()),
            None => repo.ref_oid(&source.head_ref).await?,
        },
        None => None,
    };
    let targets = page
        .items
        .iter()
        .filter(|entry| !matches!(entry, Entry::Activity(_)))
        .map(Entry::target)
        .collect();
    let mut reactions = reactions::read(server, &repo, &actor, targets)
        .await?
        .into_iter();
    let actors: Vec<_> = page
        .items
        .iter()
        .flat_map(|entry| {
            std::iter::once(entry.author()).chain(match entry {
                Entry::Review(detail) => detail.dismissal.as_ref().map(|value| &value.author),
                _ => None,
            })
        })
        .collect();
    let authors = Authors::load(server, principal, actors.into_iter()).await?;
    let principal = app::membership(server, principal, &repo).await?.principal;
    let items = page
        .items
        .into_iter()
        .map(|entry| {
            if let Entry::Activity(event) = entry {
                return Ok(json!({"kind":"activity","value":activity::view(&authors,&event)}));
            }
            let reaction = reactions.next().ok_or(Error::CellContract(
                "Conversation reaction count is invalid",
            ))?;
            let (kind, value) = match entry {
                Entry::IssueComment(comment) => (
                    "comment",
                    issues::view::comment(&authors, &comment, &actor, &reaction),
                ),
                Entry::PullComment(comment) => {
                    let comment = pulls::storage::from_comment(comment);
                    (
                        "comment",
                        pulls::comments::comment_view(&authors, &comment, &actor, &reaction),
                    )
                }
                Entry::Review(review) => (
                    "review",
                    pulls::reviews::review_view(
                        &authors,
                        &review,
                        &actor,
                        head.as_deref(),
                        &reaction,
                        &principal,
                        &repo,
                    ),
                ),
                Entry::Activity(_) => {
                    return Err(Error::CellContract("Conversation activity was not handled"));
                }
            };
            Ok(json!({"kind":kind,"value":value}))
        })
        .collect::<Result<Vec<_>>>()?;
    if reactions.next().is_some() {
        return Err(Error::CellContract(
            "Conversation reaction count is invalid",
        ));
    }
    Ok(Json(
        json!({"items":items,"start":encode(page.start)?,"end":encode(page.end)?,"has_earlier":page.has_earlier,"has_later":page.has_later,"head_oid":head}),
    ))
}
async fn issue(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(params): Query<Parameters>,
) -> Result<Json<Value>> {
    page(
        &server,
        &principal,
        (owner, name),
        Subject {
            kind: SubjectKind::Issue,
            number,
        },
        params,
    )
    .await
}
async fn pull(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    Query(params): Query<Parameters>,
) -> Result<Json<Value>> {
    page(
        &server,
        &principal,
        (owner, name),
        Subject {
            kind: SubjectKind::Pull,
            number,
        },
        params,
    )
    .await
}

#[cfg(test)]
mod tests;
