use super::*;
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
    Path((id, selected)): Path<(Uuid, Uuid)>,
    Query(cursor): Query<Cursor>,
) -> Result<Json<Value>> {
    let who = authority::caller(&server, &principal, id).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let history = cells
        .query::<projects::GetHistory>(
            Address::Organization(id),
            None,
            projects::HistoryQuery {
                actor: who,
                project: selected,
                before: cursor.before,
                after: cursor.after,
            },
        )
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::NotFound)?;
    let actors = history
        .items
        .iter()
        .map(|event| match event.actor {
            Actor::Account { id } => Ok(id),
            Actor::Repository { .. } => Err(Error::CellContract(
                "Organization history has a repository actor",
            )),
        })
        .collect::<Result<std::collections::BTreeSet<_>>>()?
        .into_iter()
        .collect::<Vec<_>>();
    let profiles = app::query_output(
        cells
            .query::<crate::cells::directory::LookupProfiles>(
                Address::Directory,
                None,
                actors.clone(),
            )
            .await,
    )?;
    if profiles.len() != actors.len() {
        return Err(Error::CellContract(
            "Project history profile count is invalid",
        ));
    }
    let profiles = actors.into_iter().zip(profiles).collect::<BTreeMap<_, _>>();
    let mut repositories = BTreeMap::new();
    let sources = history
        .items
        .iter()
        .filter_map(|event| match &event.target {
            Target::Repository { id } => Some(*id),
            Target::Item {
                source: Some(source),
                ..
            } => Some(source.repository),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    for id in &sources {
        if let Some(repo) = accessible(&server, &principal, *id).await? {
            repositories.insert(*id, repository(&repo));
        }
    }
    let mut linked = BTreeMap::<Uuid, Vec<Subject>>::new();
    for event in &history.items {
        if let Target::Item {
            source: Some(source),
            ..
        } = &event.target
        {
            let subjects = linked.entry(source.repository).or_default();
            if !subjects.contains(&source.subject) {
                subjects.push(source.subject);
            }
        }
    }
    let mut work = work::Scope {
        server: &server,
        principal: &principal,
        organization: id,
        show_issue_type: false,
        type_scope: None,
    }
    .load(linked)
    .await?;
    work.recheck_moves(&server).await?;
    // Preserve the original history link only while the whole issue transfer
    // path and the original repository grant remain readable.
    for id in sources {
        if accessible(&server, &principal, id).await?.is_none() {
            repositories.remove(&id);
        }
    }
    let items = history.items.into_iter().map(|event| {
        let actor = match event.actor { Actor::Account { id } => id, Actor::Repository { .. } => return Err(Error::CellContract("Organization history has a repository actor")) };
        let (target, restricted) = match event.target {
            Target::Project => (json!({"kind":"project"}), false),
            Target::Properties => (json!({"kind":"properties"}), false),
            Target::Repository { id } => {
                let repo = repositories.get(&id);
                (json!({"kind":"repository","repository":repo,"restricted":repo.is_none()}), repo.is_none())
            }
            Target::Item { id, source } => {
                let current = source.as_ref().and_then(|source| {
                    work.find(source.repository, source.subject)?;
                    repositories.get(&source.repository).map(|repo| {
                        json!({"repository":repo,"subject":source.subject})
                    })
                });
                let restricted = source.is_some() && current.is_none();
                (json!({"kind":"item","id":id,"source":current,"restricted":restricted}), restricted)
            }
        };
        // Linked-item fields can contain notes about work in a private repository.
        let changes = if restricted { Vec::new() } else { event.changes };
        Ok(json!({"version":event.version,"at_ms":event.at_ms,"actor":{"id":actor,"profile":profiles.get(&actor).cloned().flatten()},"target":target,"changes":changes}))
    }).collect::<Result<Vec<_>>>()?;
    project(&server, &principal, id, who, selected).await?;
    app::account(&principal)?;
    Ok(Json(json!({"items":items,"next":history.next})))
}
