use super::*;
use crate::cells::{
    directory::organizations::{Candidate, Candidates, Query as CandidateQuery, RecordCandidate},
    organization::discovery::{GetLanding, Landing},
};
use futures_util::{StreamExt, TryStreamExt, stream};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Parameters {
    after: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Entry {
    Member {
        organization: organization::Organization,
        actor: Uuid,
        role: Role,
        can_create: bool,
    },
    Invitation {
        organization: organization::Organization,
        invitation: organization::Invitation,
    },
}

pub(super) async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Query(params): Query<Parameters>,
) -> Result<Json<Value>> {
    let identity = app::account(&principal)?;
    let Some(actor) = crate::repository::access::actor_id(&server, identity).await? else {
        return Ok(Json(json!({"items": [], "next": null})));
    };
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let candidates = app::query_output(
        cells
            .query::<Candidates>(
                Address::Directory,
                None,
                CandidateQuery {
                    actor: Some(actor),
                    after: params.after,
                },
            )
            .await,
    )?;
    let entries: Vec<_> = stream::iter(candidates.items)
        .map(|candidate: Candidate| {
            let cells = &cells;
            async move {
                if candidate.actor != actor {
                    return Err(Error::CellContract(
                        "Organization candidate has the wrong account",
                    ));
                }
                let landing = match cells
                    .query::<GetLanding>(
                        Address::Organization(candidate.organization),
                        None,
                        Caller { actor },
                    )
                    .await
                {
                    Ok(result) => result.output,
                    Err(crate::cells::Error::NotInitialized) => None,
                    Err(error) => return Err(read_error(error)),
                };
                Ok(landing.map(|entry| match entry {
                    Landing::Member { view, can_create } => Entry::Member {
                        organization: view.organization,
                        actor,
                        role: view.role,
                        can_create,
                    },
                    Landing::Invitation {
                        organization,
                        invitation,
                    } => Entry::Invitation {
                        organization,
                        invitation,
                    },
                }))
            }
        })
        .buffered(8)
        .try_filter_map(|entry| async { Ok(entry) })
        .try_collect()
        .await?;
    app::account(&principal)?;
    Ok(Json(json!({"items": entries, "next": candidates.next})))
}

pub(super) async fn record_candidate(
    cells: &CellClient,
    organization: Uuid,
    actor: Uuid,
) -> Result<()> {
    app::command_output(
        cells
            .command::<RecordCandidate>(
                Address::Directory,
                app::mutation_identity()?,
                Candidate {
                    actor,
                    organization,
                },
            )
            .await,
    )
}
