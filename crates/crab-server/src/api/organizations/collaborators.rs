use super::*;
use std::collections::{BTreeMap, BTreeSet};

use crate::{
    RepositoryAccess, RepositoryMember,
    api::{members::roster, read_admission},
    catalog::{CatalogDocument, CatalogRecord},
    cells::{
        directory::IdentityKey,
        organization::{ActiveMembers, MemberBatch},
    },
};
use axum::response::{IntoResponse, Response};

mod removal;
#[cfg(test)]
pub(in crate::api) use removal::{RemovalPlan, RemovalPreview};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/api/organizations/{id}/outside-collaborators", get(list))
        .route(
            "/api/organizations/{id}/outside-collaborators/removal-preview",
            axum::routing::post(removal::preview),
        )
        .route(
            "/api/organizations/{id}/outside-collaborators/removals",
            axum::routing::post(removal::remove),
        )
        .route(
            "/api/organizations/{id}/outside-collaborators/{key}/repositories",
            get(repositories),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    after: Option<String>,
    limit: Option<u8>,
    q: Option<String>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) struct OutsideCollaborator {
    key: String,
    name: String,
    profile: Option<ActorSummary>,
    repository_count: usize,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) struct OutsideCollaboratorRepository {
    id: Uuid,
    owner: String,
    name: String,
    access: RepositoryAccess,
}

struct Snapshot<'a> {
    server: &'a Server,
    principal: &'a Principal,
    document: CatalogDocument,
    id: Uuid,
    actor: Uuid,
    version: u64,
}

impl<'a> Snapshot<'a> {
    async fn load(server: &'a Server, principal: &'a Principal, id: Uuid) -> Result<Self> {
        let actor = actor(server, principal).await?;
        let version = owner_version(server, id, actor).await?;
        let catalog = server.catalog().ok_or(Error::NotFound)?;
        let (document, _) = catalog.load().await.map_err(app::repository_access_error)?;
        Ok(Self {
            server,
            principal,
            document,
            id,
            actor,
            version,
        })
    }

    fn grants(&self) -> BTreeMap<&str, Vec<(&CatalogRecord, &RepositoryMember)>> {
        let mut grants = BTreeMap::<_, Vec<_>>::new();
        for repo in self
            .document
            .repositories
            .iter()
            .filter(|repo| repo.holds_organization_grants(self.id))
        {
            for member in &repo.members {
                grants
                    .entry(member.subject.as_str())
                    .or_default()
                    .push((repo, member));
            }
        }
        grants
    }

    fn collaborator_key(&self, subject: &str) -> String {
        let mut digest = blake3::Hasher::new_keyed(&self.server.cursor_key);
        digest.update(b"crab.organization.outside-collaborator.v1");
        digest.update(self.id.as_bytes());
        digest.update(subject.as_bytes());
        digest.finalize().to_hex().to_string()
    }

    async fn outsiders<'b>(&self, subjects: &[&'b str]) -> Result<BTreeMap<&'b str, Option<Uuid>>> {
        let identity = app::account(self.principal)?;
        let keys = subjects
            .iter()
            .map(|subject| IdentityKey {
                issuer: identity.issuer.clone(),
                subject: (*subject).to_owned(),
            })
            .collect::<Vec<_>>();
        let ids = roster::accounts(self.server, &keys).await?;
        let cells = self.server.cells().ok_or(Error::CellUnavailable)?;
        let active = cells
            .query::<ActiveMembers>(
                target(self.id)?,
                None,
                MemberBatch {
                    actor: self.actor,
                    version: self.version,
                    candidates: ids.iter().flatten().copied().collect(),
                },
            )
            .await
            .map_err(read_error)?
            .output
            .ok_or(Error::PageChanged)?;
        let active: BTreeSet<_> = active.into_iter().collect();
        // A catalog grant can predate sign-in. Without a Directory account it
        // cannot belong to an active organization member.
        Ok(subjects
            .iter()
            .copied()
            .zip(ids)
            .filter(|(_, id)| id.is_none_or(|id| !active.contains(&id)))
            .collect())
    }

    fn cursor(&self, scope: &str, position: usize) -> String {
        let mut digest = blake3::Hasher::new_keyed(&self.server.cursor_key);
        digest.update(b"crab.organization.outside-collaborators-page.v1");
        digest.update(self.id.as_bytes());
        digest.update(self.actor.as_bytes());
        digest.update(&self.document.version.to_le_bytes());
        digest.update(&self.version.to_le_bytes());
        digest.update(&(position as u64).to_le_bytes());
        digest.update(scope.as_bytes());
        format!("{position}.{}", digest.finalize().to_hex())
    }

    fn offset(&self, scope: &str, after: Option<&str>) -> Result<usize> {
        let Some(after) = after else { return Ok(0) };
        let invalid = Error::Invalid("Invalid outside collaborator continuation");
        let Some((position, _)) = after
            .split_once('.')
            .filter(|(pos, tag)| pos.len() <= 10 && tag.len() == 64)
        else {
            return Err(invalid);
        };
        let position = position.parse::<usize>().map_err(|_| invalid)?;
        if self.cursor(scope, position) != after {
            return Err(Error::PageChanged);
        }
        Ok(position)
    }

    async fn verify(&self) -> Result<()> {
        let catalog = self.server.catalog().ok_or(Error::NotFound)?;
        let (current, _) = catalog.load().await.map_err(app::repository_access_error)?;
        if current.version != self.document.version
            || owner_version(self.server, self.id, self.actor).await? != self.version
        {
            return Err(Error::PageChanged);
        }
        app::account(self.principal)?;
        Ok(())
    }
}

async fn owner_version(server: &Server, id: Uuid, actor: Uuid) -> Result<u64> {
    server
        .cells()
        .ok_or(Error::CellUnavailable)?
        .query::<GetOrganization>(target(id)?, None, Caller { actor })
        .await
        .map_err(read_error)?
        .output
        .filter(|view| view.role == Role::Owner)
        .map(|view| view.organization.version)
        .ok_or(Error::NotFound)
}

fn limit(value: Option<u8>) -> Result<usize> {
    let value = usize::from(value.unwrap_or(30));
    (1..=50)
        .contains(&value)
        .then_some(value)
        .ok_or(Error::Invalid(
            "Choose an outside collaborator page size from 1 to 50",
        ))
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(params): Query<Parameters>,
) -> Result<Response> {
    let limit = limit(params.limit)?;
    let q = params
        .q
        .as_deref()
        .unwrap_or("")
        .trim()
        .trim_start_matches('@')
        .to_lowercase();
    if q.chars().count() > 100 || q.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "Search outside collaborators with up to 100 characters",
        ));
    }
    let Ok(admission) = read_admission::acquire_bulk(&server) else {
        return Ok(read_admission::busy());
    };
    let snapshot = Snapshot::load(&server, &principal, id).await?;
    let scope = format!("people:{q}");
    let mut position = snapshot.offset(&scope, params.after.as_deref())?;
    let grants: Vec<_> = snapshot.grants().into_iter().collect();
    let mut items = Vec::with_capacity(limit);

    // Bound Directory and Cell work even when few subjects match. The cursor
    // advances past examined subjects, including active organization members.
    for batch in grants
        .iter()
        .skip(position)
        .take(200)
        .collect::<Vec<_>>()
        .chunks(50)
    {
        let subjects: Vec<_> = batch.iter().map(|(subject, _)| *subject).collect();
        let outsiders = snapshot.outsiders(&subjects).await?;
        let ids = outsiders.values().flatten().copied().collect();
        let profiles = roster::profiles(&server, &ids).await?;
        for (subject, repositories) in batch {
            position += 1;
            let Some(actor) = outsiders.get(subject) else {
                continue;
            };
            let profile = actor.and_then(|id| profiles.get(&id));
            let name = profile.map_or_else(
                || repositories[0].1.name.clone(),
                |profile| profile.name.clone(),
            );
            if name.to_lowercase().contains(&q)
                || repositories
                    .iter()
                    .any(|(repository, _)| repository.name.to_lowercase().contains(&q))
                || profile.is_some_and(|profile| profile.handle.to_lowercase().contains(&q))
            {
                items.push(OutsideCollaborator {
                    key: snapshot.collaborator_key(subject),
                    name,
                    profile: profile.cloned(),
                    repository_count: repositories.len(),
                });
            }
            if items.len() == limit {
                break;
            }
        }
        if items.len() == limit {
            break;
        }
    }
    let next = (position < grants.len()).then(|| snapshot.cursor(&scope, position));
    snapshot.verify().await?;
    Ok(read_admission::retain(
        Json(OrganizationPage {
            items,
            next,
            revision: snapshot.document.version,
        })
        .into_response(),
        admission,
    ))
}

async fn repositories(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, key)): Path<(Uuid, String)>,
    Query(params): Query<Parameters>,
) -> Result<Response> {
    let limit = limit(params.limit)?;
    if params.q.is_some() || key.len() != 64 || blake3::Hash::from_hex(&key).is_err() {
        return Err(Error::NotFound);
    }
    let Ok(admission) = read_admission::acquire_bulk(&server) else {
        return Ok(read_admission::busy());
    };
    let snapshot = Snapshot::load(&server, &principal, id).await?;
    let scope = format!("repositories:{key}");
    let position = snapshot.offset(&scope, params.after.as_deref())?;
    let (subject, mut grants) = snapshot
        .grants()
        .into_iter()
        .find(|(subject, _)| snapshot.collaborator_key(subject) == key)
        .ok_or(Error::NotFound)?;
    if !snapshot.outsiders(&[subject]).await?.contains_key(subject) {
        return Err(Error::NotFound);
    }
    grants.sort_unstable_by(|(a, _), (b, _)| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    let items: Vec<_> = grants
        .iter()
        .skip(position)
        .take(limit)
        .map(|(repo, grant)| OutsideCollaboratorRepository {
            id: repo.id,
            owner: repo.owner.clone(),
            name: repo.name.clone(),
            access: grant.access,
        })
        .collect();
    let end = position + items.len();
    let next = (end < grants.len()).then(|| snapshot.cursor(&scope, end));
    snapshot.verify().await?;
    Ok(read_admission::retain(
        Json(OrganizationPage {
            items,
            next,
            revision: snapshot.document.version,
        })
        .into_response(),
        admission,
    ))
}
