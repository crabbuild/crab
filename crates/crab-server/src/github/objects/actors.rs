//! User, organization, and repository-owner objects.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::OnceCell;
use uuid::Uuid;

use super::{Connection, Field, R, Result, page, repository, text};
use crate::{
    auth::personal_tokens::Scope,
    cells::{
        Address,
        directory::{LookupActors, LookupProfiles, ResolveHandles, validate_handle},
    },
    github::{
        Context, Viewer,
        graphql::{FieldError, Object},
        ids::{NodeId, database_id},
        native::query,
    },
};

/// A user known by login, with the Directory actor when one exists. Authors
/// without a public profile have only their recorded display name.
#[derive(Clone)]
pub(crate) struct User {
    pub(crate) login: String,
    pub(crate) name: Option<String>,
    pub(crate) actor: Option<Uuid>,
}

impl User {
    fn from_profile(profile: &Value, fallback: &str) -> Option<Self> {
        let login = profile["handle"]
            .as_str()
            .filter(|handle| !handle.is_empty());
        Some(Self {
            login: login.unwrap_or(fallback).to_owned(),
            name: profile["name"]
                .as_str()
                .or(Some(fallback))
                .filter(|name| !name.is_empty())
                .map(str::to_owned),
            actor: profile["id"]
                .as_str()
                .and_then(|id| Uuid::parse_str(id).ok()),
        })
        .filter(|user| !user.login.is_empty())
    }

    /// From a native author view, `{name, profile}`.
    pub(crate) fn from_author(value: &Value) -> Option<Self> {
        Self::from_profile(&value["profile"], text(value, "name"))
    }

    /// From a native assignee view, `{id, name, profile}`.
    pub(crate) fn from_assignee(value: &Value) -> Option<Self> {
        let mut user = Self::from_profile(&value["profile"], text(value, "name"))?;
        user.actor = user
            .actor
            .or_else(|| value["id"].as_str().and_then(|id| Uuid::parse_str(id).ok()));
        Some(user)
    }

    fn from_viewer(viewer: &Viewer) -> Self {
        Self {
            login: viewer.login.clone(),
            name: Some(viewer.name.clone()),
            actor: viewer.actor,
        }
    }

    pub(crate) fn node(&self) -> NodeId {
        // A user without a Directory actor still needs a stable opaque ID.
        NodeId::User(self.actor.unwrap_or_else(|| {
            let digest = blake3::derive_key("crab github user node v1", self.login.as_bytes());
            Uuid::from_bytes(digest[..16].try_into().unwrap_or([1; 16]))
        }))
    }

    pub(crate) async fn is_viewer(&self, context: &Context) -> bool {
        context
            .viewer()
            .await
            .is_ok_and(|viewer| viewer.login == self.login)
    }
}

#[async_trait]
impl Object<Context> for User {
    fn typename(&self) -> &str {
        "User"
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        Ok(match field.name {
            "id" => R::leaf(self.node().encode()),
            "databaseId" => R::leaf(database_id(&self.node())),
            "login" => R::leaf(self.login.as_str()),
            "name" => R::Leaf(self.name.clone().map_or(Value::Null, Value::String)),
            // Crab never discloses account email addresses or profile prose.
            "email" => R::leaf(""),
            "bio" | "company" | "location" => R::null(),
            "isSiteAdmin" => R::leaf(false),
            "isViewer" => R::leaf(self.is_viewer(context).await),
            "url" => R::leaf(context.url(&self.login)),
            "resourcePath" => R::leaf(format!("/{}", self.login)),
            "organizations" => organizations(context, self, field).await?,
            "repositories" => {
                if self.is_viewer(context).await {
                    repository::repositories(context, field, "/api/repos/personal", &[]).await?
                } else if let Some(actor) = self.actor {
                    repository::repositories(
                        context,
                        field,
                        &format!("/api/actors/{actor}/repositories"),
                        &[],
                    )
                    .await?
                } else {
                    R::object(
                        Connection::slice(
                            "RepositoryConnection",
                            "RepositoryEdge",
                            vec![],
                            &page(field)?,
                        )?
                        .scalar("totalDiskUsage", 0),
                    )
                }
            }
            "repository" => R::optional(
                repository::Repository::lookup(
                    context,
                    &self.login,
                    super::required(field, "name")?,
                )
                .await?,
            ),
            _ => R::null(),
        })
    }
}

/// Memberships are the viewer's own; GitHub hides them without `read:org`.
async fn organizations(context: &Context, user: &User, field: &Field<'_>) -> Result<R> {
    let page = page(field)?;
    let readable = context
        .principal()
        .personal_token()
        .is_none_or(|token| token.has(Scope::ReadOrg));
    let mut items: Vec<Arc<dyn Object<Context>>> = Vec::new();
    if readable && user.is_viewer(context).await {
        let mut cursor: Option<String> = None;
        // Membership lists are short; a bound keeps hostile ones finite.
        for _ in 0..20 {
            let path = match &cursor {
                Some(after) => format!("/api/organizations?{}", query(&[("after", after)])),
                None => "/api/organizations".to_owned(),
            };
            let response = context.native.get(&path).await?;
            for entry in response["items"].as_array().into_iter().flatten() {
                // Invitations are not memberships.
                if entry.get("role").is_none() {
                    continue;
                }
                if let Some(organization) = Organization::from_view(entry) {
                    items.push(Arc::new(organization));
                }
            }
            cursor = response["next"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
    }
    Ok(R::object(Connection::slice(
        "OrganizationConnection",
        "OrganizationEdge",
        items,
        &page,
    )?))
}

/// An organization from its native view, `{organization, role}`.
pub(crate) struct Organization {
    id: Uuid,
    login: String,
    view: Value,
}

impl Organization {
    pub(crate) fn from_view(view: &Value) -> Option<Self> {
        let organization = &view["organization"];
        Some(Self {
            id: organization["id"]
                .as_str()
                .and_then(|id| Uuid::parse_str(id).ok())?,
            login: text(organization, "handle").to_owned(),
            view: view.clone(),
        })
    }
}

#[async_trait]
impl Object<Context> for Organization {
    fn typename(&self) -> &str {
        "Organization"
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        let organization = &self.view["organization"];
        let node = NodeId::Organization(self.id);
        Ok(match field.name {
            "id" => R::leaf(node.encode()),
            "databaseId" => R::leaf(database_id(&node)),
            "login" => R::leaf(self.login.as_str()),
            "name" => R::Leaf(super::optional_text(organization, "name")),
            "description" => R::Leaf(super::optional_text(organization, "description")),
            "email" => R::Leaf(super::optional_text(organization, "email")),
            "url" => R::leaf(context.url(&self.login)),
            "resourcePath" => R::leaf(format!("/{}", self.login)),
            "viewerIsAMember" => R::leaf(self.view.get("role").is_some_and(|role| !role.is_null())),
            "viewerCanAdminister" => R::leaf(self.view["role"] == "owner"),
            "repositories" => {
                let id = self.id.to_string();
                repository::repositories(context, field, "/api/repos", &[("organization", &id)])
                    .await?
            }
            "repository" => R::optional(
                repository::Repository::lookup(
                    context,
                    &self.login,
                    super::required(field, "name")?,
                )
                .await?
                .filter(|repository| repository.organization() == Some(self.id)),
            ),
            _ => R::null(),
        })
    }
}

pub(super) async fn viewer(context: &Context) -> Result<R> {
    Ok(R::object(User::from_viewer(context.viewer().await?)))
}

fn handle(login: &str) -> Option<String> {
    let handle = login.to_ascii_lowercase();
    validate_handle(&handle).is_ok().then_some(handle)
}

/// Resolves a login to the account that holds it, through the Directory.
pub(crate) async fn find_user(context: &Context, login: &str) -> Result<Option<User>> {
    let principal = context.principal();
    if !(principal.is_user() || principal.is_local()) {
        return Ok(None);
    }
    let viewer = context.viewer().await?;
    if viewer.login.eq_ignore_ascii_case(login) {
        return Ok(Some(User::from_viewer(viewer)));
    }
    let Some(handle) = handle(login) else {
        return Ok(None);
    };
    let cells = context
        .server
        .cells()
        .ok_or_else(crate::github::unavailable)?;
    let identity = cells
        .query::<ResolveHandles>(Address::Directory, None, vec![handle])
        .await
        .map_err(|_| crate::github::unavailable())?
        .output
        .into_iter()
        .next()
        .flatten();
    let Some(identity) = identity else {
        return Ok(None);
    };
    let summary = cells
        .query::<LookupActors>(Address::Directory, None, vec![identity])
        .await
        .map_err(|_| crate::github::unavailable())?
        .output
        .into_iter()
        .next()
        .flatten();
    Ok(summary.map(|summary| User {
        login: summary.handle,
        name: Some(summary.name),
        actor: Some(summary.id),
    }))
}

async fn find_organization(context: &Context, login: &str) -> Result<Option<Organization>> {
    let Some(handle) = handle(login) else {
        return Ok(None);
    };
    match context
        .native
        .get(&format!("/api/organizations/by-handle/{handle}"))
        .await
    {
        Ok(view) => Ok(Organization::from_view(&view)),
        Err(failure) if failure.status.is_client_error() => Ok(None),
        Err(failure) => Err(failure.into()),
    }
}

pub(super) async fn owner(context: &Context, login: &str) -> Result<R> {
    if let Some(organization) = find_organization(context, login).await? {
        return Ok(R::object(organization));
    }
    Ok(R::optional(find_user(context, login).await?))
}

pub(super) async fn user(context: &Context, login: &str) -> Result<R> {
    match find_user(context, login).await? {
        Some(user) => Ok(R::object(user)),
        None => Err(FieldError::not_found(format!(
            "Could not resolve to a User with the login of '{login}'."
        ))),
    }
}

pub(super) async fn organization(context: &Context, login: &str) -> Result<R> {
    match find_organization(context, login).await? {
        Some(organization) => Ok(R::object(organization)),
        None => Err(FieldError::not_found(format!(
            "Could not resolve to an Organization with the login of '{login}'."
        ))),
    }
}

pub(super) async fn user_by_actor(context: &Context, actor: Uuid) -> Result<R> {
    let principal = context.principal();
    if !(principal.is_user() || principal.is_local()) {
        return Ok(R::null());
    }
    let cells = context
        .server
        .cells()
        .ok_or_else(crate::github::unavailable)?;
    let summary = cells
        .query::<LookupProfiles>(Address::Directory, None, vec![actor])
        .await
        .map_err(|_| crate::github::unavailable())?
        .output
        .into_iter()
        .next()
        .flatten();
    Ok(R::optional(summary.map(|summary| User {
        login: summary.handle,
        name: Some(summary.name),
        actor: Some(summary.id),
    })))
}

pub(super) async fn organization_by_id(context: &Context, id: Uuid) -> Result<R> {
    match context
        .native
        .get(&format!("/api/organizations/{id}"))
        .await
    {
        Ok(view) => Ok(R::optional(Organization::from_view(&view))),
        Err(failure) if failure.status.is_client_error() => Ok(R::null()),
        Err(failure) => Err(failure.into()),
    }
}

/// A team. Crab teams have no slug, so the slug is the team's UUID, which
/// is stable and unambiguous where GitHub would derive one from the name.
#[derive(Clone)]
pub(crate) struct Team {
    organization: Uuid,
    login: String,
    id: Uuid,
    name: String,
    description: Option<String>,
}

impl Team {
    /// `login` is the organization's handle.
    pub(crate) fn new(
        organization: Uuid,
        login: &str,
        id: Uuid,
        name: &str,
        description: Option<&str>,
    ) -> Self {
        Self {
            organization,
            login: login.to_owned(),
            id,
            name: name.to_owned(),
            description: description
                .filter(|text| !text.is_empty())
                .map(str::to_owned),
        }
    }
}

#[async_trait]
impl Object<Context> for Team {
    fn typename(&self) -> &str {
        "Team"
    }

    async fn resolve(&self, _: &Context, field: &Field<'_>) -> Result<R> {
        let slug = self.id.simple().to_string();
        Ok(match field.name {
            "id" => R::leaf(NodeId::Team(self.organization, self.id).encode()),
            "name" => R::leaf(self.name.as_str()),
            "combinedSlug" => R::leaf(format!("{}/{slug}", self.login)),
            "slug" => R::leaf(slug),
            "description" => R::Leaf(self.description.clone().map_or(Value::Null, Value::String)),
            "organization" => R::object(Owner {
                login: self.login.clone(),
                organization: Some(self.organization),
                resolved: OnceCell::new(),
            }),
            _ => R::null(),
        })
    }
}

pub(super) async fn team_by_id(context: &Context, organization: Uuid, id: Uuid) -> Result<R> {
    let read = |path: String| async move {
        match context.native.get(&path).await {
            Ok(view) => Ok(Some(view)),
            Err(failure) if failure.status.is_client_error() => Ok(None),
            Err(failure) => Err(FieldError::from(failure)),
        }
    };
    let (Some(owner), Some(view)) = (
        read(format!("/api/organizations/{organization}")).await?,
        read(format!("/api/organizations/{organization}/teams/{id}")).await?,
    ) else {
        return Ok(R::null());
    };
    let team = &view["team"];
    Ok(R::object(Team::new(
        organization,
        text(&owner["organization"], "handle"),
        id,
        text(team, "name"),
        team["description"].as_str(),
    )))
}

/// A repository's owner. Its type follows the catalog's organization
/// binding; profile data loads only when a client selects it.
pub(super) fn repository_owner(repository: &Arc<repository::Repository>) -> R {
    R::object(Owner {
        login: repository.owner.clone(),
        organization: repository.organization(),
        resolved: OnceCell::new(),
    })
}

struct Owner {
    login: String,
    organization: Option<Uuid>,
    resolved: OnceCell<Box<dyn Object<Context>>>,
}

impl Owner {
    async fn resolved(&self, context: &Context) -> Result<&dyn Object<Context>> {
        Ok(self
            .resolved
            .get_or_try_init(|| async {
                Ok::<_, FieldError>(match self.organization {
                    Some(id) => {
                        let view = match context
                            .native
                            .get(&format!("/api/organizations/{id}"))
                            .await
                        {
                            Ok(view) => view,
                            // Outside collaborators see an organization's
                            // repositories without reading its profile.
                            Err(failure) if failure.status.is_client_error() => serde_json::json!({
                                "organization": {"id": id, "handle": self.login},
                            }),
                            Err(failure) => return Err(failure.into()),
                        };
                        let organization = Organization::from_view(&view).unwrap_or(Organization {
                            id,
                            login: self.login.clone(),
                            view,
                        });
                        Box::new(organization) as Box<dyn Object<Context>>
                    }
                    None => Box::new(find_user(context, &self.login).await?.unwrap_or(User {
                        login: self.login.clone(),
                        name: None,
                        actor: None,
                    })),
                })
            })
            .await?
            .as_ref())
    }
}

#[async_trait]
impl Object<Context> for Owner {
    fn typename(&self) -> &str {
        if self.organization.is_some() {
            "Organization"
        } else {
            "User"
        }
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        match (field.name, self.organization) {
            ("login", _) => Ok(R::leaf(self.login.as_str())),
            ("id", Some(id)) => Ok(R::leaf(NodeId::Organization(id).encode())),
            ("url", _) => Ok(R::leaf(context.url(&self.login))),
            ("resourcePath", _) => Ok(R::leaf(format!("/{}", self.login))),
            _ => self.resolved(context).await?.resolve(context, field).await,
        }
    }
}
