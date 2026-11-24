use super::*;
use crate::cells::directory::{
    ActorSummary, CompleteHandles, LookupIdentities, LookupProfiles,
    notifications::filters::Author, validate_handle_prefix,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Search {
    prefix: String,
}

#[derive(Serialize)]
pub(super) struct Matches {
    items: Vec<ActorSummary>,
    truncated: bool,
}

pub(super) async fn complete(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Query(input): Query<Search>,
) -> Result<Json<Matches>> {
    app::account(&principal)?;
    let prefix = input
        .prefix
        .trim()
        .trim_start_matches('@')
        .to_ascii_lowercase();
    validate_handle_prefix(&prefix).map_err(Error::Invalid)?;
    if prefix.is_empty() {
        return Ok(Json(Matches {
            items: Vec::new(),
            truncated: false,
        }));
    }
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let result = app::query_output(
        cells
            .query::<CompleteHandles>(Address::Directory, None, prefix)
            .await,
    )?;
    app::account(&principal)?;
    Ok(Json(Matches {
        items: result.items,
        truncated: result.truncated,
    }))
}

pub(super) struct BoundAuthor {
    pub identity: IdentityKey,
    pub discussion: [u8; 32],
}

pub(super) async fn bind(
    server: &Server,
    viewer: &IdentityKey,
    author: Author,
) -> Result<Option<BoundAuthor>> {
    let identity = match author {
        Author::Me => viewer.clone(),
        Author::Actor(id) => {
            let cells = server.cells().ok_or(Error::CellUnavailable)?;
            let (profiles, identities) = tokio::join!(
                cells.query::<LookupProfiles>(Address::Directory, None, vec![id]),
                cells.query::<LookupIdentities>(Address::Directory, None, vec![id]),
            );
            let profiles = app::query_output(profiles)?;
            let identities = app::query_output(identities)?;
            match (profiles.as_slice(), identities.as_slice()) {
                ([Some(_)], [Some(identity)]) => identity.clone(),
                ([_], [_]) => return Ok(None),
                _ => {
                    return Err(Error::CellContract(
                        "Author lookup returned an invalid result count",
                    ));
                }
            }
        }
    };
    let discussion =
        crate::cells::repository::search::people::identity(&identity.issuer, &identity.subject)
            .map_err(Error::Cell)?;
    Ok(Some(BoundAuthor {
        identity,
        discussion,
    }))
}
