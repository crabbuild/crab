use std::{cmp::Reverse, collections::HashMap};

use axum::extract::Query;

use super::*;
use crate::cells::organization::teams::repository_access::ActorTeams;

const PAGE: usize = 20;
type Position = (i64, Uuid);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Parameters {
    before: Option<String>,
}

#[derive(Serialize)]
pub(super) struct Page {
    items: Vec<View>,
    next: Option<String>,
}

fn position(offer: &Offer) -> Position {
    (offer.expires_at_ms, offer.id)
}

fn parse_position(value: &str) -> Result<Position> {
    let (time, id) = value
        .split_once(':')
        .and_then(|(time, id)| Some((time.parse::<i64>().ok()?, Uuid::parse_str(id).ok()?)))
        .filter(|(time, id)| *time > 0 && !id.is_nil())
        .ok_or(Error::Invalid("Repository transfer position is invalid"))?;
    Ok((time, id))
}

pub(super) async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Query(parameters): Query<Parameters>,
) -> Result<Json<Page>> {
    let actor = account(&principal)?;
    let before = parameters
        .before
        .as_deref()
        .map(parse_position)
        .transpose()?;
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (document, _) = catalog.load_snapshot().await.map_err(catalog_error)?;
    let mut rows = pending(&document, now()?, before).collect::<Vec<_>>();
    rows.sort_unstable_by_key(|(_, offer)| Reverse(position(offer)));
    let grants =
        access::organization_grants(&server, &principal, destinations(rows.iter().copied()))
            .await?;
    let now = now()?;
    let mut visible = rows.into_iter().filter(|(record, offer)| {
        offer.available(record, now) && allowed(&offer.recipient, &actor, &grants)
    });
    let items: Vec<_> = visible
        .by_ref()
        .take(PAGE)
        .map(|(record, offer)| View::new(record, offer, now, false, true))
        .collect();
    // A continuation is derived only from an offer visible to this recipient.
    let next = visible.next().and_then(|_| {
        items
            .last()
            .map(|item| format!("{}:{}", item.expires_at_ms, item.id))
    });
    account(&principal)?;
    Ok(Json(Page { items, next }))
}

pub(in crate::api) async fn has_pending(server: &Server, principal: &Principal) -> Result<bool> {
    if !principal.is_user() {
        return Ok(false);
    }
    let Some(catalog) = server.catalog() else {
        return Ok(false);
    };
    let actor = account(principal)?;
    let (document, _) = catalog.load_snapshot().await.map_err(catalog_error)?;
    let mut offers = pending(&document, now()?, None);
    if offers.clone().any(|(_, offer)| {
        matches!(&offer.recipient, Recipient::Account { identity } if identity == &actor)
    }) {
        return Ok(true);
    }
    let grants =
        access::organization_grants(server, principal, destinations(offers.clone())).await?;
    let now = now()?;
    account(principal)?;
    Ok(offers.any(|(record, offer)| {
        offer.available(record, now) && allowed(&offer.recipient, &actor, &grants)
    }))
}

fn pending(
    catalog: &CatalogDocument,
    now: i64,
    before: Option<Position>,
) -> impl Iterator<Item = (&CatalogRecord, &Offer)> + Clone {
    catalog.repositories.iter().filter_map(move |record| {
        let offer = record.transfer.as_ref()?;
        (offer.available(record, now) && before.is_none_or(|before| position(offer) < before))
            .then_some((record, offer))
    })
}

fn destinations<'a>(
    offers: impl Iterator<Item = (&'a CatalogRecord, &'a Offer)>,
) -> HashMap<Uuid, bool> {
    offers
        .filter_map(|(_, offer)| match offer.recipient {
            Recipient::Organization { id } => Some((id, false)),
            Recipient::Account { .. } => None,
        })
        .collect()
}

fn allowed(
    recipient: &Recipient,
    actor: &MembershipActor,
    grants: &HashMap<Uuid, ActorTeams>,
) -> bool {
    match recipient {
        Recipient::Account { identity } => identity == actor,
        Recipient::Organization { id } => grants.get(id).is_some_and(|grant| grant.owner),
    }
}
