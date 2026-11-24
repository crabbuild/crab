use super::*;
use crate::cells::{
    application::Read,
    directory::LookupProfiles,
    organization::{Collection, InvitationState, Page, PageCursor, PeopleQuery},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Parameters {
    after: Option<String>,
    limit: Option<u8>,
    q: String,
    role: Option<Role>,
    team: Option<Uuid>,
    status: Option<InvitationState>,
}

pub(super) async fn page<P, T, O>(
    server: &Server,
    principal: &Principal,
    id: Uuid,
    collection: Collection,
    params: Parameters,
    identity: impl Fn(&T) -> Uuid,
    compose: impl Fn(T, Option<ActorSummary>) -> O,
) -> Result<(OrganizationPage<O>, Role)>
where
    P: Read<Input = PeopleQuery, Output = Page<T>>,
{
    let actor = actor(server, principal).await?;
    let address = target(id)?;
    let limit = params.limit.unwrap_or(30);
    let q = params.q.trim().trim_start_matches('@').to_lowercase();
    if !(1..=50).contains(&limit) || q.chars().count() > 100 || q.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "People search requires up to 100 characters and 1–50 results",
        ));
    }
    if (collection == Collection::Members && params.status.is_some())
        || (collection == Collection::Invitations && params.team.is_some())
        || params.team.is_some_and(|team| team.is_nil())
    {
        return Err(Error::Invalid(
            "Choose filters for the selected people view",
        ));
    }
    let mut key = blake3::Hasher::new_keyed(&server.cursor_key);
    key.update(b"crab organization people cursor v1");
    key.update(&serde_json::to_vec(&(
        id,
        actor,
        &collection,
        &q,
        params.role,
        params.team,
        params.status,
    ))?);
    let sign = |cursor: &[u8]| {
        let mut key = key.clone();
        key.update(cursor);
        key.finalize()
    };
    let invalid = || Error::Invalid("Invalid people continuation");
    let mut after: Option<PageCursor> = params
        .after
        .as_deref()
        .map(|value| {
            if value.len() > 512 {
                return Err(invalid());
            }
            let (cursor, signature) = value.split_once('.').ok_or_else(invalid)?;
            let cursor = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| invalid())?;
            let signature = blake3::Hash::from_hex(signature).map_err(|_| invalid())?;
            let page = serde_json::from_slice(&cursor).map_err(|_| invalid())?;
            if signature != sign(&cursor) {
                return Err(Error::PageChanged);
            }
            Ok(page)
        })
        .transpose()?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let mut items = Vec::new();
    let mut revision = None;
    // Names live in Directory. Bound selective searches and continue from
    // the last examined actor so matches beyond this response stay reachable.
    for _ in 0..4 {
        let page = cells
            .query::<P>(
                address,
                None,
                PeopleQuery {
                    actor,
                    after,
                    limit: if q.is_empty() { limit } else { 50 },
                    role: params.role,
                    team: params.team,
                    status: params.status,
                },
            )
            .await
            .map_err(read_error)?
            .output;
        let Page::Page {
            items: batch,
            next,
            revision: current,
        } = page
        else {
            return Err(Error::PageChanged);
        };
        if revision
            .replace(current)
            .is_some_and(|previous| previous != current)
        {
            return Err(Error::PageChanged);
        }
        let ids: Vec<_> = batch.iter().map(&identity).collect();
        let profiles = app::query_output(
            cells
                .query::<LookupProfiles>(Address::Directory, None, ids.clone())
                .await,
        )?;
        if batch.len() != profiles.len() {
            return Err(Error::CellContract(
                "People profile lookup returned an invalid result count",
            ));
        }
        after = next;
        for (index, ((item, profile), person)) in
            batch.into_iter().zip(profiles).zip(ids.iter()).enumerate()
        {
            if q.is_empty()
                || profile.as_ref().is_some_and(|profile| {
                    profile.handle.to_lowercase().contains(&q)
                        || profile.name.to_lowercase().contains(&q)
                })
                || person.to_string().contains(&q)
            {
                items.push(compose(item, profile));
            }
            if items.len() == usize::from(limit) {
                if index + 1 < ids.len() {
                    after = Some(PageCursor {
                        collection: collection.clone(),
                        organization: id,
                        revision: current,
                        after: *person,
                    });
                }
                break;
            }
        }
        if after.is_none() || items.len() == usize::from(limit) {
            break;
        }
    }
    let current = cells
        .query::<GetOrganization>(address, None, Caller { actor })
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::NotFound)?;
    if collection == Collection::Invitations && current.role != Role::Owner {
        return Err(Error::NotFound);
    }
    if revision != Some(current.organization.version) {
        return Err(Error::PageChanged);
    }
    app::account(principal)?;
    let next = if let Some(cursor) = after {
        let bytes = serde_json::to_vec(&cursor)?;
        Some(format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&bytes),
            sign(&bytes)
        ))
    } else {
        None
    };
    Ok((
        OrganizationPage {
            items,
            next,
            revision: current.organization.version,
        },
        current.role,
    ))
}
