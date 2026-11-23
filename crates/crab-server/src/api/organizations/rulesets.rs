use super::*;
use crate::cells::organization::rulesets::{
    Entry, Get, Replace, ReplaceInput, ReplaceOutcome, Repositories, Settings,
};
use crate::repository::settings::{RulesetScope, with_ruleset_lease};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/api/organizations/{id}/rulesets", get(read).put(replace))
        .layer(axum::extract::DefaultBodyLimit::max(128 * 1024))
}

async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<Settings>> {
    let actor = actor(&server, &principal).await?;
    Ok(Json(load(&server, id, actor).await?))
}

async fn load(server: &Server, id: Uuid, actor: Uuid) -> Result<Settings> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    cells
        .query::<Get>(Address::Organization(id), None, Caller { actor })
        .await
        .map(|reply| reply.output)
        .map_err(read_error)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    expected_version: u64,
    rulesets: Vec<Entry>,
}

async fn replace(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Json(input): Json<Input>,
) -> Result<Json<Settings>> {
    let actor = actor(&server, &principal).await?;
    let command = ReplaceInput {
        actor,
        expected_version: input.expected_version,
        rulesets: input.rulesets,
    };
    if !command.valid() {
        return Err(Error::Invalid(
            "Choose valid repository targets, unique names and at least one rule",
        ));
    }
    let current = load(&server, id, actor).await?;
    if current.version != command.expected_version {
        return Err(Error::Conflict);
    }
    let selected = command
        .rulesets
        .iter()
        .filter_map(|rule| match &rule.repositories {
            Repositories::Selected { ids } => Some(ids),
            _ => None,
        })
        .flatten()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    if !selected.is_empty() {
        let (document, _) = server
            .catalog()
            .ok_or(Error::NotFound)?
            .load()
            .await
            .map_err(|error| Error::Membership(Box::new(error)))?;
        let available = document
            .repositories
            .iter()
            .filter(|repo| repo.organization_id() == Some(id))
            .map(|repo| repo.id)
            .collect::<std::collections::HashSet<_>>();
        // Preserve currently saved selections when the repository is gone,
        // so the owner can still change or delete that ruleset.
        let previous = current
            .rulesets
            .iter()
            .filter_map(|rule| match &rule.repositories {
                Repositories::Selected { ids } => Some(ids),
                _ => None,
            })
            .flatten()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        if selected
            .iter()
            .any(|id| !available.contains(id) && !previous.contains(id))
        {
            return Err(Error::Invalid(
                "Choose repositories owned by this organization",
            ));
        }
    }
    let (send, reply) = tokio::sync::oneshot::channel();
    let tracker = server.receives.clone();
    tracker.spawn(async move {
        let server = server.as_ref();
        let outcome = with_ruleset_lease(
            server,
            RulesetScope::Organization(id),
            &server.cancellation,
            |cancel| async move {
                if cancel.is_cancelled() {
                    return Err(Error::MergeBusy);
                }
                let cells = server.cells().ok_or(Error::CellUnavailable)?;
                cells
                    .command::<Replace>(
                        Address::Organization(id),
                        app::mutation_identity()?,
                        command,
                    )
                    .await
                    .map(|reply| reply.output)
                    .map_err(read_error)
            },
        )
        .await;
        let _ = send.send(outcome);
    });
    match reply.await.map_err(|_| Error::MergeBusy)?? {
        ReplaceOutcome::Saved(settings) => Ok(Json(settings)),
        ReplaceOutcome::Conflict => Err(Error::Conflict),
        ReplaceOutcome::UnavailableTeam => Err(Error::Invalid("Choose an active, non-secret team")),
        ReplaceOutcome::UnavailableProperty => Err(Error::Invalid(
            "Choose a defined repository property and an allowed value",
        )),
    }
}
