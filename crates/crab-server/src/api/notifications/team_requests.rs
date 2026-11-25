use super::*;
use crate::cells::{
    self,
    directory::organizations::{Candidate, Candidates, Query as CandidateQuery},
    organization::teams::hierarchy_requests,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

const PAGE_SIZE: u8 = 20;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Parameters {
    after: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    directory: Option<String>,
    resume: Option<Resume>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Resume {
    organization: Uuid,
    after: Uuid,
}

fn decode(value: &str) -> Result<Cursor> {
    const INVALID: Error = Error::Invalid("Team request continuation is invalid");
    if value.len() > 512 {
        return Err(INVALID);
    }
    let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| INVALID)?;
    serde_json::from_slice(&bytes).map_err(|_| INVALID)
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "TeamRequestInboxItem"))]
pub(in crate::api) struct Item {
    organization: Uuid,
    organization_name: String,
    request: hierarchy_requests::Item,
}

#[derive(Default, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "TeamRequestInboxPage"))]
pub(in crate::api) struct Page {
    items: Vec<Item>,
    next: Option<String>,
}

pub(super) async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Query(params): Query<Parameters>,
) -> Result<Json<Page>> {
    let cursor = params
        .after
        .as_deref()
        .map(decode)
        .transpose()?
        .unwrap_or_default();
    let identity = app::account(&principal)?;
    let Some(actor) = crate::repository::access::actor_id(&server, identity).await? else {
        return Ok(Json(Page::default()));
    };
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let candidates = app::query_output(
        cells
            .query::<Candidates>(
                Address::Directory,
                None,
                CandidateQuery {
                    actor: Some(actor),
                    after: cursor.directory.clone(),
                },
            )
            .await,
    )?;
    let count = candidates.items.len();
    let mut batches = stream::iter(candidates.items.into_iter().enumerate())
        .map(|(index, candidate): (_, Candidate)| {
            let cells = &cells;
            let resume = cursor.resume.as_ref();
            async move {
                if candidate.actor != actor {
                    return Err(Error::CellContract(
                        "Organization candidate has the wrong account",
                    ));
                }
                let after = resume
                    .filter(|position| position.organization == candidate.organization)
                    .map(|position| position.after);
                let page = match cells
                    .query::<hierarchy_requests::List>(
                        Address::Organization(candidate.organization),
                        None,
                        hierarchy_requests::ListInput {
                            actor,
                            team: None,
                            after,
                            request: None,
                            incoming: true,
                            limit: PAGE_SIZE,
                        },
                    )
                    .await
                {
                    Ok(result) => Some(result.output),
                    Err(cells::Error::NotInitialized | cells::Error::AccessDenied) => None,
                    Err(error) => return Err(Error::Cell(error)),
                };
                Ok((index, candidate, page))
            }
        })
        .buffered(8);
    let mut page = Page::default();
    let mut completed = cursor.directory.clone();
    let mut next = None;
    while let Some((index, candidate, result)) = batches.try_next().await? {
        if let Some(result) = result.filter(|result| !result.items.is_empty()) {
            let name = result.organization_name.ok_or(Error::CellContract(
                "Incoming team requests have no organization",
            ))?;
            let room = usize::from(PAGE_SIZE) - page.items.len();
            let more = result.next.is_some() || result.items.len() > room;
            page.items
                .extend(result.items.into_iter().take(room).map(|request| Item {
                    organization: candidate.organization,
                    organization_name: name.clone(),
                    request,
                }));
            if more {
                let last = page.items.last().ok_or(Error::CellContract(
                    "Team request continuation has no result",
                ))?;
                next = Some(Cursor {
                    directory: completed,
                    resume: Some(Resume {
                        organization: candidate.organization,
                        after: last.request.id,
                    }),
                });
                break;
            }
        }
        completed = Some(candidate.position());
        if page.items.len() == usize::from(PAGE_SIZE) {
            if candidates.next.is_some() || index + 1 < count {
                next = Some(Cursor {
                    directory: completed,
                    resume: None,
                });
            }
            break;
        }
    }
    if next.is_none()
        && let Some(directory) = candidates.next
    {
        next = Some(Cursor {
            directory: Some(directory),
            resume: None,
        });
    }
    page.next = next
        .map(|cursor| serde_json::to_vec(&cursor).map(|bytes| URL_SAFE_NO_PAD.encode(bytes)))
        .transpose()?;
    app::account(&principal)?;
    Ok(Json(page))
}

#[cfg(test)]
mod tests;
