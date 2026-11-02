use super::*;
use crate::api::profiles::attribution::Authors;
use crate::cells::planning::{Actor, Target};

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Cursor {
    before: Option<u64>,
    after: Option<u64>,
}
pub(super) async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, project)): Path<(String, String, Uuid)>,
    Query(cursor): Query<Cursor>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let cell = app::repository_cell(&server, &repo)?;
    let history = app::query_output(
        cell.client
            .query::<projects::GetHistory>(
                &cell.target,
                None,
                projects::HistoryQuery {
                    project,
                    before: cursor.before,
                    after: cursor.after,
                },
            )
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let mut recorded = Vec::new();
    for event in &history.items {
        match &event.actor {
            Actor::Repository { author } => recorded.push(author),
            Actor::Account { .. } => {
                return Err(Error::CellContract(
                    "Repository history has an organization actor",
                ));
            }
        }
    }
    let authors = Authors::load(&server, &principal, recorded.into_iter()).await?;
    let items = history.items.iter().map(|event| {
        let Actor::Repository { author } = &event.actor else {
            return Err(Error::CellContract("Repository history has an organization actor"));
        };
        let target = match &event.target {
            Target::Project => json!({"kind":"project"}),
            Target::Item { id, source } => {
                if source.as_ref().is_some_and(|source| source.repository != repo.id) {
                    return Err(Error::CellContract("Project history source belongs to another repository"));
                }
                json!({"kind":"item","id":id,"source":source.as_ref().map(|source| json!({"repository":{"id":repo.id,"owner":repo.config.owner,"name":repo.config.name},"subject":source.subject})),"restricted":false})
            }
            _ => return Err(Error::CellContract("Repository history has an organization target")),
        };
        Ok(json!({"version":event.version,"at_ms":event.at_ms,"actor":authors.view(author),"target":target,"changes":event.changes}))
    }).collect::<Result<Vec<_>>>()?;
    // Directory enrichment can outlive a grant; refresh before disclosing history.
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(json!({"items":items,"next":history.next})))
}
