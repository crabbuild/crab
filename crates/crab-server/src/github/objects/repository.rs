//! Repository, ref, commit, label, and milestone objects.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use uuid::Uuid;

use super::{
    Connection, Field, JsonObject, MAX_NATIVE_PAGES, NATIVE_PAGE, R, Result, actors, discussions,
    optional_text, page, text, timestamp, timestamp_seconds,
};
use crate::github::{
    Context,
    graphql::{FieldError, Object},
    ids::{NodeId, database_id},
    native::query,
};

/// A repository the caller can read, built from its native catalog view.
pub(crate) struct Repository {
    pub(crate) id: Uuid,
    pub(crate) owner: String,
    pub(crate) name: String,
    /// The exact catalog view, or a browse row until a field needs more.
    row: Value,
    exact: OnceCell<Value>,
    refs: OnceCell<Value>,
    merge_methods: OnceCell<Option<Value>>,
    // One catalog read serves every issue and pull request in a page.
    labels: OnceCell<Vec<Value>>,
}

fn not_found(owner: &str, name: &str) -> FieldError {
    FieldError::not_found(format!(
        "Could not resolve to a Repository with the name '{owner}/{name}'."
    ))
}

impl Repository {
    fn new(row: Value, exact: bool) -> Option<Self> {
        let id = row["id"].as_str().and_then(|id| Uuid::parse_str(id).ok())?;
        let cell = OnceCell::new();
        if exact {
            let _ = cell.set(row.clone());
        }
        Some(Self {
            id,
            owner: text(&row, "owner").to_owned(),
            name: text(&row, "name").to_owned(),
            row,
            exact: cell,
            refs: OnceCell::new(),
            merge_methods: OnceCell::new(),
            labels: OnceCell::new(),
        })
    }

    /// A browse result, whose permission fields load on demand.
    pub(crate) fn from_row(row: Value) -> Option<Arc<Self>> {
        Self::new(row, false).map(Arc::new)
    }

    /// Loads `owner/name` as the caller sees it, following one rename like
    /// GitHub. Absence and lack of access are indistinguishable.
    pub(crate) async fn lookup(
        context: &Context,
        owner: &str,
        name: &str,
    ) -> crate::github::native::Outcome<Option<Arc<Self>>> {
        let mut address = (owner.to_owned(), name.to_owned());
        for _ in 0..2 {
            let view = context
                .native
                .get(&format!(
                    "/api/repos?{}",
                    query(&[("owner", &address.0), ("name", &address.1)])
                ))
                .await;
            let view = match view {
                Ok(view) => view,
                Err(failure) if failure.status.is_client_error() => return Ok(None),
                Err(failure) => return Err(failure),
            };
            if let Some(row) = view["repositories"]
                .as_array()
                .and_then(|rows| rows.first())
            {
                return Ok(Self::new(row.clone(), true).map(Arc::new));
            }
            match (
                view["redirect"]["owner"].as_str(),
                view["redirect"]["name"].as_str(),
            ) {
                (Some(owner), Some(name)) => address = (owner.to_owned(), name.to_owned()),
                _ => return Ok(None),
            }
        }
        Ok(None)
    }

    pub(crate) async fn find(context: &Context, owner: &str, name: &str) -> Result<Arc<Self>> {
        Self::lookup(context, owner, name)
            .await?
            .ok_or_else(|| not_found(owner, name))
    }

    /// The repository with a stable UUID, if its current address is readable.
    pub(crate) async fn by_id(context: &Context, id: Uuid) -> Result<Option<Arc<Self>>> {
        let Some(entry) = context.server.repositories.by_id(id) else {
            return Ok(None);
        };
        Ok(
            Self::lookup(context, &entry.config.owner, &entry.config.name)
                .await?
                .filter(|repository| repository.id == id),
        )
    }

    pub(crate) fn path(&self) -> String {
        format!("/{}/{}", self.owner, self.name)
    }

    pub(crate) fn api(&self, tail: &str) -> String {
        format!("/api/repos/{}/{}/{tail}", self.owner, self.name)
    }

    pub(crate) fn url(&self, context: &Context) -> String {
        context.url(&self.path())
    }

    pub(crate) fn organization(&self) -> Option<Uuid> {
        self.row["organization_id"]
            .as_str()
            .and_then(|id| Uuid::parse_str(id).ok())
    }

    pub(crate) fn is_public(&self) -> bool {
        self.row["visibility"] == "public"
    }

    pub(crate) async fn exact(&self, context: &Context) -> Result<&Value> {
        self.exact
            .get_or_try_init(|| async {
                Self::lookup(context, &self.owner, &self.name)
                    .await?
                    .filter(|current| current.id == self.id)
                    .map(|current| current.row.clone())
                    .ok_or_else(|| not_found(&self.owner, &self.name))
            })
            .await
    }

    /// GitHub's permission name for the caller's current role.
    pub(crate) async fn permission(&self, context: &Context) -> Result<&'static str> {
        let view = self.exact(context).await?;
        Ok(if view["can_admin"] == true {
            "ADMIN"
        } else if view["can_maintain"] == true {
            "MAINTAIN"
        } else if view["access"] == "write" {
            "WRITE"
        } else {
            "READ"
        })
    }

    pub(crate) async fn refs(&self, context: &Context) -> Result<&Value> {
        self.refs
            .get_or_try_init(|| async { Ok(context.native.get(&self.api("refs")).await?) })
            .await
    }

    /// One ref by its full name, such as `refs/heads/main`.
    pub(crate) async fn find_ref(
        self: &Arc<Self>,
        context: &Context,
        name: &str,
    ) -> Result<Option<Ref>> {
        let refs = self.refs(context).await?;
        Ok(refs["refs"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|entry| entry["name"] == name)
            .map(|entry| {
                Ref::new(
                    self,
                    name.to_owned(),
                    entry["oid"].as_str().map(str::to_owned),
                )
            }))
    }

    pub(crate) async fn default_branch(&self, context: &Context) -> Result<Option<String>> {
        let refs = self.refs(context).await?;
        Ok(refs["head"]["name"]
            .as_str()
            .or_else(|| refs["unborn_head"].as_str())
            .and_then(|name| name.strip_prefix("refs/heads/"))
            .map(str::to_owned))
    }

    /// Merge settings are readable at Maintain. Without them, the server's
    /// merge check still enforces the configured methods.
    async fn merge_methods(&self, context: &Context) -> Result<Option<&Value>> {
        Ok(self
            .merge_methods
            .get_or_try_init(|| async {
                match context
                    .native
                    .get(&self.api("settings/merge-methods"))
                    .await
                {
                    Ok(value) => Ok(Some(value)),
                    Err(failure) if failure.status.is_client_error() => Ok(None),
                    Err(failure) => Err(FieldError::from(failure)),
                }
            })
            .await?
            .as_ref())
    }

    async fn allows(&self, context: &Context, method: &str) -> Result<bool> {
        Ok(match self.merge_methods(context).await? {
            Some(settings) => settings["enabled"]
                .as_array()
                .is_some_and(|enabled| enabled.iter().any(|value| value == method)),
            None => true,
        })
    }

    async fn setting(&self, context: &Context, name: &str) -> Result<bool> {
        Ok(self
            .merge_methods(context)
            .await?
            .is_some_and(|settings| settings[name] == true))
    }
}

pub(super) fn repository_list(rows: &[Value]) -> Vec<Arc<dyn Object<Context>>> {
    rows.iter()
        .filter_map(|row| Repository::from_row(row.clone()))
        .map(|repository| Arc::new(repository) as Arc<dyn Object<Context>>)
        .collect()
}

/// Lists repositories from a native browse endpoint into a GitHub page,
/// applying GitHub's filters the native browse does not take itself.
pub(super) async fn repositories(
    context: &Context,
    field: &Field<'_>,
    path: &str,
    base: &[(&str, &str)],
) -> Result<R> {
    let page = page(field)?;
    let privacy = field
        .string("privacy")
        .or(field.string("visibility"))
        .map(str::to_ascii_lowercase);
    let fork = field.argument("isFork").and_then(Value::as_bool);
    let archived = field.argument("isArchived").and_then(Value::as_bool);
    let keep = |row: &Value| {
        privacy
            .as_deref()
            .is_none_or(|privacy| row["visibility"] == privacy)
            && fork.is_none_or(|fork| row["fork"] == fork)
            && archived.is_none_or(|archived| row["archived"] == archived)
    };
    let fetch = |cursor: Option<String>, limit: usize| async move {
        let mut pairs = base.to_vec();
        let limit = limit.to_string();
        pairs.push(("limit", &limit));
        if let Some(cursor) = &cursor {
            pairs.push(("after", cursor));
        }
        let response = context
            .native
            .get(&format!("{path}?{}", query(&pairs)))
            .await?;
        let rows = response["repositories"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok::<_, FieldError>((rows, response["next"].as_str().map(str::to_owned)))
    };
    let mut rows = Vec::new();
    let mut cursor = page.after.clone();
    let mut reads = 0;
    // Each native page is no larger than the rows still needed, so the
    // native cursor always resumes exactly after this GitHub page.
    while rows.len() < page.size && reads < MAX_NATIVE_PAGES {
        let (batch, next) =
            fetch(cursor.clone(), (page.size - rows.len()).min(NATIVE_PAGE)).await?;
        reads += 1;
        rows.extend(batch.into_iter().filter(|row| keep(row)));
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    let has_next = cursor.is_some();
    let mut total = rows.len();
    if has_next && field.selects(&["totalCount"]) {
        let mut scan = cursor.clone();
        while let Some(after) = scan.take() {
            if reads == MAX_NATIVE_PAGES {
                break;
            }
            let (batch, next) = fetch(Some(after), NATIVE_PAGE).await?;
            reads += 1;
            total += batch.iter().filter(|row| keep(row)).count();
            scan = next;
        }
    }
    let end = cursor.unwrap_or_default();
    let nodes = repository_list(&rows)
        .into_iter()
        .map(|node| (node, end.clone()))
        .collect();
    Ok(R::object(
        Connection::new(
            "RepositoryConnection",
            "RepositoryEdge",
            nodes,
            total,
            has_next,
        )
        .scalar("totalDiskUsage", 0),
    ))
}

#[async_trait]
impl Object<Context> for Arc<Repository> {
    fn typename(&self) -> &str {
        "Repository"
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        let row = &self.row;
        let node = NodeId::Repository(self.id);
        Ok(match field.name {
            "id" => R::leaf(node.encode()),
            "databaseId" => R::leaf(database_id(&node)),
            "name" => R::leaf(self.name.as_str()),
            "nameWithOwner" => R::leaf(format!("{}/{}", self.owner, self.name)),
            "owner" => actors::repository_owner(self),
            "description" => R::Leaf(optional_text(row, "description")),
            "homepageUrl" => R::Leaf(optional_text(row, "homepage")),
            // Crab records neither when a repository was archived or pushed
            // nor which template generated it.
            "archivedAt" | "pushedAt" | "diskUsage" | "templateRepository" => R::null(),
            "url" => R::leaf(self.url(context)),
            "resourcePath" => R::leaf(self.path()),
            "createdAt" => R::Leaf(timestamp(&row["created_at_ms"])),
            "updatedAt" => R::Leaf(match timestamp(&row["metadata_updated_at_ms"]) {
                Value::Null => timestamp(&row["created_at_ms"]),
                value => value,
            }),
            "isArchived" => R::leaf(row["archived"] == true),
            "isFork" => R::leaf(row["fork"] == true),
            "isPrivate" => R::leaf(!self.is_public()),
            "visibility" => R::leaf(if self.is_public() {
                "PUBLIC"
            } else {
                "PRIVATE"
            }),
            "isInOrganization" => R::leaf(self.organization().is_some()),
            "isEmpty" => R::leaf(self.refs(context).await?["head"].is_null()),
            "isTemplate" => R::leaf(row["is_template"] == true),
            // Crab repositories always have issues and projects and never
            // have wikis, discussions, stars, or locks.
            "hasIssuesEnabled" | "hasProjectsEnabled" | "isBlankIssuesEnabled" => R::leaf(true),
            "hasWikiEnabled"
            | "hasDiscussionsEnabled"
            | "isLocked"
            | "isDisabled"
            | "isUserConfigurationRepository"
            | "viewerHasStarred" => R::leaf(false),
            "stargazerCount" => R::leaf(0),
            "viewerCanAdminister" => R::leaf(self.exact(context).await?["can_admin"] == true),
            "viewerPermission" => R::leaf(self.permission(context).await?),
            "mergeCommitAllowed" => R::leaf(self.allows(context, "merge_commit").await?),
            "squashMergeAllowed" => R::leaf(self.allows(context, "squash").await?),
            "rebaseMergeAllowed" => R::leaf(self.allows(context, "rebase").await?),
            "autoMergeAllowed" => R::leaf(self.setting(context, "auto_merge").await?),
            "deleteBranchOnMerge" => R::leaf(self.setting(context, "delete_head_branches").await?),
            "viewerDefaultMergeMethod" => R::leaf(if self.allows(context, "merge_commit").await? {
                "MERGE"
            } else if self.allows(context, "squash").await? {
                "SQUASH"
            } else {
                "REBASE"
            }),
            "parent" => match row["forked_from"]["owner"].as_str() {
                Some(owner) => R::optional(
                    Repository::lookup(context, owner, text(&row["forked_from"], "name")).await?,
                ),
                None => match self.exact(context).await?["forked_from"]["owner"].as_str() {
                    Some(owner) => {
                        let exact = self.exact(context).await?;
                        R::optional(
                            Repository::lookup(context, owner, text(&exact["forked_from"], "name"))
                                .await?,
                        )
                    }
                    None => R::null(),
                },
            },
            "defaultBranchRef" => match self.default_branch(context).await? {
                Some(branch) => {
                    let oid = self.refs(context).await?["head"]["oid"]
                        .as_str()
                        .map(str::to_owned);
                    R::object(Ref::new(self, format!("refs/heads/{branch}"), oid))
                }
                None => R::null(),
            },
            "ref" => {
                let name = super::required(field, "qualifiedName")?;
                for candidate in [
                    name.to_owned(),
                    format!("refs/heads/{name}"),
                    format!("refs/tags/{name}"),
                ] {
                    if let Some(found) = self.find_ref(context, &candidate).await? {
                        return Ok(R::object(found));
                    }
                }
                R::null()
            }
            "refs" => {
                let prefix = super::required(field, "refPrefix")?;
                let search = field.string("query").unwrap_or_default();
                let refs = self.refs(context).await?;
                let mut names: Vec<(String, Option<String>)> = refs["refs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| {
                        let name = entry["name"].as_str()?;
                        let short = name.strip_prefix(prefix)?;
                        short
                            .contains(search)
                            .then(|| (name.to_owned(), entry["oid"].as_str().map(str::to_owned)))
                    })
                    .collect();
                names.sort();
                if field.string("direction") == Some("DESC") {
                    names.reverse();
                }
                let items = names
                    .into_iter()
                    .map(|(name, oid)| {
                        Arc::new(Ref::new(self, name, oid)) as Arc<dyn Object<Context>>
                    })
                    .collect();
                R::object(Connection::slice(
                    "RefConnection",
                    "RefEdge",
                    items,
                    &page(field)?,
                )?)
            }
            "repositoryTopics" => {
                let topics: Vec<Arc<dyn Object<Context>>> = row["topics"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|topic| {
                        Arc::new(Topic {
                            repository: self.url(context),
                            name: topic.to_owned(),
                        }) as Arc<dyn Object<Context>>
                    })
                    .collect();
                R::object(Connection::slice(
                    "RepositoryTopicConnection",
                    "RepositoryTopicEdge",
                    topics,
                    &page(field)?,
                )?)
            }
            "labels" => {
                let labels = labels(context, self).await?;
                let search = field.string("query").map(str::to_lowercase);
                let mut items: Vec<Value> = labels
                    .into_iter()
                    .filter(|label| {
                        search.as_deref().is_none_or(|search| {
                            text(label, "name").to_lowercase().contains(search)
                        })
                    })
                    .collect();
                if field
                    .argument("orderBy")
                    .and_then(|order| order["field"].as_str())
                    == Some("NAME")
                {
                    items.sort_by_key(|label| text(label, "name").to_lowercase());
                    if field
                        .argument("orderBy")
                        .and_then(|order| order["direction"].as_str())
                        == Some("DESC")
                    {
                        items.reverse();
                    }
                }
                let items = items
                    .into_iter()
                    .map(|label| Arc::new(Label::new(self, label)) as Arc<dyn Object<Context>>)
                    .collect();
                R::object(Connection::slice(
                    "LabelConnection",
                    "LabelEdge",
                    items,
                    &page(field)?,
                )?)
            }
            "label" => {
                let name = super::required(field, "name")?.to_lowercase();
                R::optional(
                    labels(context, self)
                        .await?
                        .into_iter()
                        .find(|label| text(label, "name").to_lowercase() == name)
                        .map(|label| Label::new(self, label)),
                )
            }
            "milestones" => milestones(context, self, field).await?,
            "milestone" => {
                let number = field.int("number").unwrap_or(0);
                match context
                    .native
                    .get(&self.api(&format!("milestones/{number}")))
                    .await
                {
                    Ok(value) => R::object(Milestone::new(self, value)),
                    Err(failure) if failure.status.is_client_error() => R::null(),
                    Err(failure) => return Err(failure.into()),
                }
            }
            "assignableUsers" | "mentionableUsers" => {
                let roster = context.native.get(&self.api("assignees")).await?;
                let search = field.string("query").map(str::to_lowercase);
                let items = roster["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(actors::User::from_assignee)
                    .filter(|user| {
                        search.as_deref().is_none_or(|search| {
                            user.login.to_lowercase().contains(search)
                                || user
                                    .name
                                    .as_deref()
                                    .is_some_and(|name| name.to_lowercase().contains(search))
                        })
                    })
                    .map(|user| Arc::new(user) as Arc<dyn Object<Context>>)
                    .collect();
                R::object(Connection::slice(
                    "UserConnection",
                    "UserEdge",
                    items,
                    &page(field)?,
                )?)
            }
            "issues" => discussions::issues(context, self, field).await?,
            "issue" => R::optional(
                discussions::Issue::find(
                    context,
                    self,
                    field.int("number").unwrap_or(0).max(0) as u64,
                )
                .await?,
            ),
            "pullRequests" => discussions::pull_requests(context, self, field).await?,
            "pullRequest" => R::optional(
                discussions::PullRequest::find(
                    context,
                    self,
                    field.int("number").unwrap_or(0).max(0) as u64,
                )
                .await?,
            ),
            "issueOrPullRequest" => {
                let number = field.int("number").unwrap_or(0).max(0) as u64;
                match discussions::Issue::find(context, self, number).await? {
                    Some(issue) => R::object(issue),
                    None => match discussions::PullRequest::find(context, self, number).await? {
                        Some(pull) => R::object(pull),
                        None => {
                            return Err(FieldError::not_found(format!(
                                "Could not resolve to an issue or pull request with the number of {number}."
                            )));
                        }
                    },
                }
            }
            _ => R::null(),
        })
    }
}

pub(crate) async fn labels(context: &Context, repository: &Repository) -> Result<Vec<Value>> {
    Ok(repository
        .labels
        .get_or_try_init(|| async {
            Ok::<_, FieldError>(
                context.native.get(&repository.api("labels")).await?["items"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            )
        })
        .await?
        .clone())
}

pub(super) async fn label_by_number(
    context: &Context,
    repository: &Arc<Repository>,
    number: u64,
) -> Result<Option<R>> {
    Ok(labels(context, repository)
        .await?
        .into_iter()
        .find(|label| label["id"].as_u64() == Some(number))
        .map(|label| R::object(Label::new(repository, label))))
}

async fn milestones(
    context: &Context,
    repository: &Arc<Repository>,
    field: &Field<'_>,
) -> Result<R> {
    let page = page(field)?;
    let states: Vec<String> = field
        .argument("states")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_ascii_lowercase)
        .collect();
    let state = match states.as_slice() {
        [single] => single.as_str(),
        _ => "all",
    };
    let limit = NATIVE_PAGE.min(page.size.max(1)).to_string();
    let mut pairs = vec![("state", state), ("limit", limit.as_str())];
    if let Some(after) = &page.after {
        pairs.push(("before", after));
    }
    let response = context
        .native
        .get(&format!(
            "{}?{}",
            repository.api("milestones"),
            query(&pairs)
        ))
        .await?;
    let next = response["next"].as_u64().map(|next| next.to_string());
    let search = field.string("query").map(str::to_lowercase);
    let nodes: Vec<_> = response["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|milestone| {
            search
                .as_deref()
                .is_none_or(|search| text(milestone, "title").to_lowercase().contains(search))
        })
        .take(page.size)
        .map(|milestone| {
            let cursor = milestone["number"].to_string();
            (
                Arc::new(Milestone::new(repository, milestone.clone())) as Arc<dyn Object<Context>>,
                cursor,
            )
        })
        .collect();
    let count = nodes.len();
    Ok(R::object(Connection::new(
        "MilestoneConnection",
        "MilestoneEdge",
        nodes,
        count + usize::from(next.is_some()),
        next.is_some(),
    )))
}

pub(crate) struct Ref {
    repository: Arc<Repository>,
    name: String,
    oid: Option<String>,
    /// A pull request's merge requirements, when this is its base branch.
    requirements: Option<Value>,
}

impl Ref {
    pub(super) fn new(repository: &Arc<Repository>, name: String, oid: Option<String>) -> Self {
        Self {
            repository: Arc::clone(repository),
            name,
            oid,
            requirements: None,
        }
    }

    /// Binds the merge requirements of a pull request based on this branch.
    pub(super) fn based(mut self, requirements: &Value) -> Self {
        self.requirements = Some(requirements.clone());
        self
    }

    pub(super) fn oid(&self) -> Option<&str> {
        self.oid.as_deref()
    }

    /// GitHub's branch protection as one pull request's requirements show
    /// it. Crab evaluates protection and rulesets per pull request, so other
    /// refs cannot answer without guessing.
    fn protection(&self) -> Result<R> {
        let Some(requirements) = &self.requirements else {
            return Err(FieldError::unprocessable(
                "Crab reports branch protection through a pull request's baseRef",
            ));
        };
        let approvals = requirements["required_approvals"].as_u64().unwrap_or(0);
        // Freshness is evaluated only when the base requires an up-to-date head.
        let strict = !requirements["head_up_to_date"].is_null();
        if requirements["protected"] != true && approvals == 0 && !strict {
            return Ok(R::null());
        }
        Ok(R::object(JsonObject::new(
            "BranchProtectionRule",
            json!({
                "requiredApprovingReviewCount": approvals,
                "requiresApprovingReviews": approvals > 0,
                "requiresStrictStatusChecks": strict,
            }),
        )))
    }
}

#[async_trait]
impl Object<Context> for Ref {
    fn typename(&self) -> &str {
        "Ref"
    }

    async fn resolve(&self, _: &Context, field: &Field<'_>) -> Result<R> {
        let (prefix, short) = ["refs/heads/", "refs/tags/"]
            .into_iter()
            .find_map(|prefix| Some((prefix, self.name.strip_prefix(prefix)?)))
            .unwrap_or(("", self.name.as_str()));
        Ok(match field.name {
            "id" => R::leaf(NodeId::Ref(self.repository.id, self.name.clone()).encode()),
            "name" => R::leaf(short),
            "prefix" => R::leaf(prefix),
            "repository" => R::object(Arc::clone(&self.repository)),
            "target" => R::optional(
                self.oid
                    .clone()
                    .map(|oid| Commit::new(&self.repository, oid, None)),
            ),
            "branchProtectionRule" => self.protection()?,
            _ => R::null(),
        })
    }
}

/// A commit, loaded from the native commit read when a field needs it.
pub(crate) struct Commit {
    repository: Arc<Repository>,
    oid: String,
    detail: OnceCell<Value>,
}

impl Commit {
    pub(crate) fn new(repository: &Arc<Repository>, oid: String, detail: Option<Value>) -> Self {
        let cell = OnceCell::new();
        if let Some(detail) = detail {
            let _ = cell.set(detail);
        }
        Self {
            repository: Arc::clone(repository),
            oid,
            detail: cell,
        }
    }

    async fn detail(&self, context: &Context) -> Result<&Value> {
        self.detail
            .get_or_try_init(|| async {
                Ok(context
                    .native
                    .get(&format!(
                        "{}?{}",
                        self.repository.api("commit"),
                        query(&[("rev", &self.oid)])
                    ))
                    .await?)
            })
            .await
    }

    async fn actor(&self, context: &Context) -> Result<JsonObject> {
        let detail = self.detail(context).await?;
        Ok(JsonObject::new(
            "GitActor",
            json!({
                "name": detail["author"],
                "email": null,
                "date": detail["author_seconds"].as_u64().map_or(Value::Null, timestamp_seconds),
                "user": null,
            }),
        ))
    }
}

#[async_trait]
impl Object<Context> for Commit {
    fn typename(&self) -> &str {
        "Commit"
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        let path = format!("{}/commit/{}", self.repository.path(), self.oid);
        Ok(match field.name {
            "id" => R::leaf(NodeId::Commit(self.repository.id, self.oid.clone()).encode()),
            "oid" => R::leaf(self.oid.as_str()),
            "abbreviatedOid" => R::leaf(self.oid.chars().take(7).collect::<String>()),
            "url" | "commitUrl" => R::leaf(context.url(&path)),
            "resourcePath" => R::leaf(path),
            "repository" => R::object(Arc::clone(&self.repository)),
            "message" => R::leaf(text(self.detail(context).await?, "message")),
            "messageHeadline" => R::leaf(
                text(self.detail(context).await?, "message")
                    .lines()
                    .next()
                    .unwrap_or_default(),
            ),
            "messageBody" => {
                let message = text(self.detail(context).await?, "message");
                R::leaf(
                    message
                        .split_once('\n')
                        .map_or("", |(_, body)| body)
                        .trim_start_matches('\n')
                        .trim_end(),
                )
            }
            // Crab records one Git signature time per commit summary.
            "authoredDate" | "committedDate" => R::Leaf(
                self.detail(context).await?["author_seconds"]
                    .as_u64()
                    .map_or(Value::Null, timestamp_seconds),
            ),
            "author" | "committer" => R::object(self.actor(context).await?),
            "authors" => R::object(Connection::slice(
                "GitActorConnection",
                "GitActorEdge",
                vec![Arc::new(self.actor(context).await?) as Arc<dyn Object<Context>>],
                &page(field)?,
            )?),
            "statusCheckRollup" => {
                discussions::status_check_rollup(context, &self.repository, &self.oid).await?
            }
            _ => R::null(),
        })
    }
}

pub(crate) struct Label {
    repository: Arc<Repository>,
    value: Value,
}

impl Label {
    pub(crate) fn new(repository: &Arc<Repository>, value: Value) -> Self {
        Self {
            repository: Arc::clone(repository),
            value,
        }
    }
}

#[async_trait]
impl Object<Context> for Label {
    fn typename(&self) -> &str {
        "Label"
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        let name = text(&self.value, "name");
        let path = format!(
            "{}/labels/{}",
            self.repository.path(),
            url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>()
        );
        Ok(match field.name {
            "id" => R::leaf(
                NodeId::Label(self.repository.id, self.value["id"].as_u64().unwrap_or(0)).encode(),
            ),
            "name" => R::leaf(name),
            "color" => R::leaf(text(&self.value, "color").trim_start_matches('#')),
            "description" => R::Leaf(optional_text(&self.value, "description")),
            "isDefault" => R::leaf(false),
            "url" => R::leaf(context.url(&path)),
            "resourcePath" => R::leaf(path),
            _ => R::null(),
        })
    }
}

pub(crate) struct Milestone {
    repository: Arc<Repository>,
    value: Value,
}

impl Milestone {
    pub(crate) fn new(repository: &Arc<Repository>, value: Value) -> Self {
        Self {
            repository: Arc::clone(repository),
            value,
        }
    }
}

/// Milestone due dates are calendar days; GitHub reports them at midnight UTC.
fn due_on(value: &Value) -> Value {
    match value {
        Value::String(day) if day.len() == 10 => Value::String(format!("{day}T00:00:00Z")),
        Value::Number(_) => timestamp(value),
        _ => Value::Null,
    }
}

#[async_trait]
impl Object<Context> for Milestone {
    fn typename(&self) -> &str {
        "Milestone"
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        let value = &self.value;
        let number = value["number"].as_u64().unwrap_or(0);
        let path = format!("{}/milestone/{number}", self.repository.path());
        let closed = value["state"] == "closed";
        Ok(match field.name {
            "id" => R::leaf(NodeId::Milestone(self.repository.id, number).encode()),
            "number" => R::leaf(number),
            "title" => R::leaf(text(value, "title")),
            "description" => R::Leaf(optional_text(value, "description")),
            "dueOn" => R::Leaf(due_on(&value["due_on"])),
            "state" => R::leaf(if closed { "CLOSED" } else { "OPEN" }),
            "closed" => R::leaf(closed),
            "closedAt" => R::Leaf(timestamp(&value["closed_at"])),
            "createdAt" => R::Leaf(timestamp(&value["created_at"])),
            "updatedAt" => R::Leaf(timestamp(&value["updated_at"])),
            "progressPercentage" => {
                let open = value["open_count"].as_f64().unwrap_or(0.0);
                let done = value["closed_count"].as_f64().unwrap_or(0.0);
                R::leaf(if open + done > 0.0 {
                    done * 100.0 / (open + done)
                } else {
                    0.0
                })
            }
            "url" => R::leaf(context.url(&path)),
            "resourcePath" => R::leaf(path),
            _ => R::null(),
        })
    }
}

struct Topic {
    repository: String,
    name: String,
}

#[async_trait]
impl Object<Context> for Topic {
    fn typename(&self) -> &str {
        "RepositoryTopic"
    }

    async fn resolve(&self, _: &Context, field: &Field<'_>) -> Result<R> {
        let id = blake3::hash(format!("topic\0{}", self.name).as_bytes()).to_hex();
        Ok(match field.name {
            "id" => R::leaf(format!("RT_{}", &id[..24])),
            "topic" => R::object(JsonObject::new(
                "Topic",
                json!({"id": format!("T_{}", &id[..24]), "name": self.name}),
            )),
            "url" => R::leaf(self.repository.clone()),
            "resourcePath" => R::leaf(format!("/topics/{}", self.name)),
            _ => R::null(),
        })
    }
}
