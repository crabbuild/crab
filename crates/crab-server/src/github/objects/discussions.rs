//! Issue, pull request, comment, review, and commit-check objects.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::OnceCell;

use super::{
    Connection, Field, JsonObject, MAX_NATIVE_PAGES, NATIVE_PAGE, Page, R, Result,
    actors::{Team, User},
    offset_cursor, page,
    repository::{Commit, Label, Milestone, Repository, labels},
    text, timestamp,
};
use crate::github::{
    Context,
    graphql::{FieldError, Object},
    ids::{Discussion, NodeId, comment_rest_id, database_id, rest_id},
    native::query,
};

/// Bounds the comments, reviews, and commits one connection read collects
/// to present GitHub's oldest-first order over newest-first native pages.
const MAX_COLLECTED: usize = 500;
/// Bounds the changed files whose line counts one pull request computes.
const MAX_STAT_FILES: usize = 300;

fn kind(discussion: Discussion) -> &'static str {
    match discussion {
        Discussion::Issue => "issues",
        Discussion::Pull => "pulls",
    }
}

fn reaction_groups(value: &Value) -> R {
    const CONTENT: [(&str, &str); 8] = [
        ("thumbs_up", "THUMBS_UP"),
        ("thumbs_down", "THUMBS_DOWN"),
        ("laugh", "LAUGH"),
        ("hooray", "HOORAY"),
        ("confused", "CONFUSED"),
        ("heart", "HEART"),
        ("rocket", "ROCKET"),
        ("eyes", "EYES"),
    ];
    let groups = &value["reactions"]["groups"];
    R::List(
        CONTENT
            .iter()
            .filter_map(|(native, content)| {
                let group = groups.get(*native)?;
                let count = group["count"].as_u64().filter(|count| *count > 0)?;
                Some(R::object(ReactionGroup {
                    content,
                    count,
                    reacted: group["reacted"] == true,
                }))
            })
            .collect(),
    )
}

struct ReactionGroup {
    content: &'static str,
    count: u64,
    reacted: bool,
}

#[async_trait]
impl Object<Context> for ReactionGroup {
    fn typename(&self) -> &str {
        "ReactionGroup"
    }

    async fn resolve(&self, _: &Context, field: &Field<'_>) -> Result<R> {
        Ok(match field.name {
            "content" => R::leaf(self.content),
            "viewerHasReacted" => R::leaf(self.reacted),
            // Native summaries count reactors without listing them.
            "users" => R::object(Connection::new(
                "ReactingUserConnection",
                "ReactingUserEdge",
                Vec::new(),
                usize::try_from(self.count).unwrap_or(usize::MAX),
                false,
            )),
            _ => R::null(),
        })
    }
}

fn author(value: &Value) -> R {
    R::optional(User::from_author(value))
}

async fn viewer_authored(context: &Context, author: &Value) -> bool {
    let Ok(viewer) = context.viewer().await else {
        return false;
    };
    let actor = author["profile"]["id"].as_str();
    viewer
        .actor
        .is_some_and(|id| actor == Some(id.to_string().as_str()))
}

fn users(values: &Value, field: &Field<'_>) -> Result<R> {
    let items = values
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(User::from_assignee)
        .map(|user| Arc::new(user) as Arc<dyn Object<Context>>)
        .collect();
    Ok(R::object(Connection::slice(
        "UserConnection",
        "UserEdge",
        items,
        &page(field)?,
    )?))
}

/// Joins a discussion's selected label IDs with the repository catalog, so
/// list summaries report descriptions like detail reads do.
async fn label_connection(
    context: &Context,
    repository: &Arc<Repository>,
    selected: &Value,
    field: &Field<'_>,
) -> Result<R> {
    let ids: Vec<u64> = selected
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|label| label["id"].as_u64())
        .collect();
    let catalog = if ids.is_empty() {
        Vec::new()
    } else {
        labels(context, repository).await?
    };
    let items = ids
        .iter()
        .filter_map(|id| {
            catalog
                .iter()
                .find(|label| label["id"].as_u64() == Some(*id))
        })
        .map(|label| Arc::new(Label::new(repository, label.clone())) as Arc<dyn Object<Context>>)
        .collect();
    Ok(R::object(Connection::slice(
        "LabelConnection",
        "LabelEdge",
        items,
        &page(field)?,
    )?))
}

/// A discussion's native comment pages, newest first, gathered within bounds.
async fn collect(context: &Context, path: &str, limit: usize) -> Result<(Vec<Value>, bool)> {
    let mut items = Vec::new();
    let mut before: Option<String> = None;
    while items.len() < limit {
        let size = (limit - items.len()).min(NATIVE_PAGE).to_string();
        let mut pairs = vec![("limit", size.as_str())];
        if let Some(before) = &before {
            pairs.push(("before", before));
        }
        let response = context
            .native
            .get(&format!("{path}?{}", query(&pairs)))
            .await?;
        items.extend(response["items"].as_array().cloned().unwrap_or_default());
        before = match &response["next"] {
            Value::Number(number) => Some(number.to_string()),
            Value::String(text) => Some(text.clone()),
            _ => None,
        };
        if before.is_none() {
            return Ok((items, false));
        }
    }
    Ok((items, before.is_some()))
}

/// Pages newest-first native entries in GitHub's oldest-first order. Cursors
/// carry the entry number, so a page resumes after exactly that entry.
async fn chronological(
    context: &Context,
    path: &str,
    field: &Field<'_>,
    total: Option<usize>,
) -> Result<(Vec<Value>, usize, bool)> {
    let page = page(field)?;
    // `last: n` is the newest n: one native page in reverse, unless the
    // caller also needs a total the list row did not carry.
    let counted = total.is_some() || !field.selects(&["totalCount"]);
    if page.last && page.after.is_none() && counted {
        let (mut items, _) = collect(context, path, page.size).await?;
        items.reverse();
        return Ok((items, total.unwrap_or_default(), false));
    }
    let after = page
        .after
        .as_deref()
        .map(|cursor| {
            cursor
                .strip_prefix('n')
                .and_then(|number| number.parse::<u64>().ok())
                .ok_or_else(|| {
                    FieldError::unprocessable(format!(
                        "`{cursor}` does not appear to be a valid cursor."
                    ))
                })
        })
        .transpose()?;
    let (mut items, truncated) = collect(context, path, MAX_COLLECTED).await?;
    items.reverse();
    let total = total.unwrap_or(items.len());
    if page.last && after.is_none() {
        let skip = items.len().saturating_sub(page.size);
        return Ok((items.into_iter().skip(skip).collect(), total, false));
    }
    let remaining: Vec<Value> = items
        .into_iter()
        .filter(|item| after.is_none_or(|after| item["number"].as_u64() > Some(after)))
        .collect();
    let has_next = remaining.len() > page.size || truncated;
    Ok((
        remaining.into_iter().take(page.size).collect(),
        total,
        has_next,
    ))
}

fn chronological_cursor(item: &Value) -> String {
    format!("n{}", item["number"].as_u64().unwrap_or(0))
}

async fn comments(
    context: &Context,
    repository: &Arc<Repository>,
    discussion: Discussion,
    number: u64,
    total: Option<usize>,
    field: &Field<'_>,
) -> Result<R> {
    // A count from the list row answers without reading any comments.
    if let Some(total) = total
        && !field.selects(&["nodes"])
        && !field.selects(&["edges"])
        && !field.selects(&["pageInfo"])
    {
        return Ok(R::object(Connection::new(
            "IssueCommentConnection",
            "IssueCommentEdge",
            Vec::new(),
            total,
            total > 0,
        )));
    }
    let path = repository.api(&format!("{}/{number}/comments", kind(discussion)));
    let (items, total, has_next) = chronological(context, &path, field, total).await?;
    let nodes = items
        .into_iter()
        .map(|item| {
            let cursor = chronological_cursor(&item);
            (
                Arc::new(IssueComment {
                    repository: Arc::clone(repository),
                    discussion,
                    number,
                    value: item,
                }) as Arc<dyn Object<Context>>,
                cursor,
            )
        })
        .collect();
    Ok(R::object(Connection::new(
        "IssueCommentConnection",
        "IssueCommentEdge",
        nodes,
        total,
        has_next,
    )))
}

pub(super) async fn comment(
    context: &Context,
    repository: &Arc<Repository>,
    discussion: Discussion,
    number: u64,
    comment: u64,
) -> Result<Option<R>> {
    let path = repository.api(&format!("{}/{number}/comments/{comment}", kind(discussion)));
    match context.native.get(&path).await {
        Ok(value) => Ok(Some(R::object(IssueComment {
            repository: Arc::clone(repository),
            discussion,
            number,
            value,
        }))),
        Err(failure) if failure.status.is_client_error() => Ok(None),
        Err(failure) => Err(failure.into()),
    }
}

pub(crate) struct IssueComment {
    pub(crate) repository: Arc<Repository>,
    pub(crate) discussion: Discussion,
    pub(crate) number: u64,
    pub(crate) value: Value,
}

impl IssueComment {
    pub(crate) fn node(&self) -> NodeId {
        NodeId::Comment {
            repository: self.repository.id,
            discussion: self.discussion,
            number: self.number,
            comment: self.value["number"].as_u64().unwrap_or(0),
        }
    }

    pub(crate) fn rest_id(&self) -> u64 {
        comment_rest_id(
            self.discussion,
            self.number,
            self.value["number"].as_u64().unwrap_or(0),
        )
        .unwrap_or_else(|| rest_id(&self.node()))
    }

    pub(crate) fn path(&self) -> String {
        let segment = match self.discussion {
            Discussion::Issue => "issues",
            Discussion::Pull => "pull",
        };
        format!(
            "{}/{segment}/{}#issuecomment-{}",
            self.repository.path(),
            self.number,
            self.rest_id()
        )
    }
}

#[async_trait]
impl Object<Context> for IssueComment {
    fn typename(&self) -> &str {
        "IssueComment"
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        let value = &self.value;
        let node = self.node();
        Ok(match field.name {
            "id" => R::leaf(node.encode()),
            "databaseId" => R::leaf(database_id(&node)),
            "fullDatabaseId" => R::leaf(self.rest_id().to_string()),
            "author" => author(&value["author"]),
            "authorAssociation" => R::leaf("NONE"),
            "body" => R::leaf(text(value, "body")),
            "createdAt" | "publishedAt" => R::Leaf(timestamp(&value["created_at"])),
            "updatedAt" => R::Leaf(timestamp(&value["updated_at"])),
            "lastEditedAt" => R::Leaf(if value["has_edit_history"] == true {
                timestamp(&value["updated_at"])
            } else {
                Value::Null
            }),
            "includesCreatedEdit" => R::leaf(value["has_edit_history"] == true),
            "isMinimized" => R::leaf(!value["minimization"].is_null()),
            "minimizedReason" => R::Leaf(
                value["minimization"]["reason"]
                    .as_str()
                    .map_or(Value::Null, |reason| {
                        Value::String(reason.to_ascii_uppercase())
                    }),
            ),
            "reactionGroups" => reaction_groups(value),
            "url" => R::leaf(context.url(&self.path())),
            "resourcePath" => R::leaf(self.path()),
            "viewerCanUpdate" => R::leaf(value["can_edit"] == true),
            "viewerCanDelete" => R::leaf(false),
            "viewerDidAuthor" => R::leaf(viewer_authored(context, &value["author"]).await),
            "repository" => R::object(Arc::clone(&self.repository)),
            "issue" => match self.discussion {
                Discussion::Issue => {
                    R::optional(Issue::find(context, &self.repository, self.number).await?)
                }
                Discussion::Pull => {
                    return Err(FieldError::new(
                        "Pull request comments have no separate issue in Crab",
                    ));
                }
            },
            "pullRequest" => match self.discussion {
                Discussion::Pull => {
                    R::optional(PullRequest::find(context, &self.repository, self.number).await?)
                }
                Discussion::Issue => R::null(),
            },
            _ => R::null(),
        })
    }
}

pub(crate) struct Issue {
    pub(crate) repository: Arc<Repository>,
    pub(crate) number: u64,
    row: Value,
    detail: OnceCell<Value>,
    milestone: OnceCell<Value>,
}

impl Issue {
    pub(crate) fn new(repository: &Arc<Repository>, row: Value, detail: bool) -> Self {
        let cell = OnceCell::new();
        if detail {
            let _ = cell.set(row.clone());
        }
        Self {
            repository: Arc::clone(repository),
            number: row["number"].as_u64().unwrap_or(0),
            row,
            detail: cell,
            milestone: OnceCell::new(),
        }
    }

    pub(crate) async fn find(
        context: &Context,
        repository: &Arc<Repository>,
        number: u64,
    ) -> Result<Option<Self>> {
        if number == 0 {
            return Ok(None);
        }
        match context
            .native
            .get(&repository.api(&format!("issues/{number}")))
            .await
        {
            // A moved issue answers with its new address, not its content.
            Ok(value) if value["title"].is_string() => Ok(Some(Self::new(repository, value, true))),
            Ok(_) => Ok(None),
            Err(failure) if failure.status.is_client_error() => Ok(None),
            Err(failure) => Err(failure.into()),
        }
    }

    pub(crate) fn path(&self) -> String {
        format!("{}/issues/{}", self.repository.path(), self.number)
    }

    pub(crate) async fn detail(&self, context: &Context) -> Result<&Value> {
        self.detail
            .get_or_try_init(|| async {
                Ok(context
                    .native
                    .get(&self.repository.api(&format!("issues/{}", self.number)))
                    .await?)
            })
            .await
    }

    async fn milestone(&self, context: &Context) -> Result<&Value> {
        self.milestone
            .get_or_try_init(|| async {
                Ok(context
                    .native
                    .get(
                        &self
                            .repository
                            .api(&format!("issues/{}/milestone", self.number)),
                    )
                    .await?["milestone"]
                    .clone())
            })
            .await
    }
}

#[async_trait]
impl Object<Context> for Issue {
    fn typename(&self) -> &str {
        "Issue"
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        let row = &self.row;
        let node = NodeId::Issue(self.repository.id, self.number);
        let closed = row["state"] == "closed";
        Ok(match field.name {
            "id" => R::leaf(node.encode()),
            "databaseId" => R::leaf(database_id(&node)),
            "fullDatabaseId" => R::leaf(rest_id(&node).to_string()),
            "number" => R::leaf(self.number),
            "title" => R::leaf(text(row, "title")),
            "body" => R::leaf(text(self.detail(context).await?, "body")),
            "state" => R::leaf(if closed { "CLOSED" } else { "OPEN" }),
            "stateReason" => R::Leaf(row["state_reason"].as_str().map_or(Value::Null, |reason| {
                Value::String(reason.to_ascii_uppercase())
            })),
            "closed" => R::leaf(closed),
            "closedAt" => R::Leaf(timestamp(&row["closed_at"])),
            "createdAt" | "publishedAt" => R::Leaf(timestamp(&row["created_at"])),
            "updatedAt" => R::Leaf(timestamp(&row["updated_at"])),
            "lastEditedAt" => R::null(),
            "url" => R::leaf(context.url(&self.path())),
            "resourcePath" => R::leaf(self.path()),
            "author" => author(&row["author"]),
            "authorAssociation" => R::leaf("NONE"),
            "includesCreatedEdit" => {
                R::leaf(self.detail(context).await?["has_edit_history"] == true)
            }
            "assignees" => users(&row["assignees"], field)?,
            "labels" => label_connection(context, &self.repository, &row["labels"], field).await?,
            "milestone" => match self.milestone(context).await? {
                Value::Null => R::null(),
                milestone => R::object(Milestone::new(&self.repository, milestone.clone())),
            },
            "comments" => {
                let total = row["comment_count"].as_u64().map(|count| count as usize);
                comments(
                    context,
                    &self.repository,
                    Discussion::Issue,
                    self.number,
                    total,
                    field,
                )
                .await?
            }
            "reactionGroups" => reaction_groups(self.detail(context).await?),
            "repository" => R::object(Arc::clone(&self.repository)),
            "viewerCanUpdate" => R::leaf(self.detail(context).await?["can_edit"] == true),
            "viewerDidAuthor" => R::leaf(viewer_authored(context, &row["author"]).await),
            _ => R::null(),
        })
    }
}

/// One changed file of a pull request comparison, with both blob sides.
pub(crate) struct FileDiff {
    path: String,
    change: &'static str,
    old: Value,
    new: Value,
    additions: u64,
    deletions: u64,
}

pub(crate) struct PullRequest {
    pub(crate) repository: Arc<Repository>,
    pub(crate) number: u64,
    row: Value,
    detail: OnceCell<Value>,
    head: OnceCell<Option<Arc<Repository>>>,
    files: OnceCell<Vec<FileDiff>>,
    milestone: OnceCell<Value>,
}

impl PullRequest {
    pub(crate) fn new(repository: &Arc<Repository>, row: Value, detail: bool) -> Self {
        let cell = OnceCell::new();
        if detail {
            let _ = cell.set(row.clone());
        }
        Self {
            repository: Arc::clone(repository),
            number: row["number"].as_u64().unwrap_or(0),
            row,
            detail: cell,
            head: OnceCell::new(),
            files: OnceCell::new(),
            milestone: OnceCell::new(),
        }
    }

    pub(crate) async fn find(
        context: &Context,
        repository: &Arc<Repository>,
        number: u64,
    ) -> Result<Option<Self>> {
        if number == 0 {
            return Ok(None);
        }
        match context
            .native
            .get(&repository.api(&format!("pulls/{number}")))
            .await
        {
            Ok(value) => Ok(Some(Self::new(repository, value, true))),
            Err(failure) if failure.status.is_client_error() => Ok(None),
            Err(failure) => Err(failure.into()),
        }
    }

    pub(crate) fn path(&self) -> String {
        format!("{}/pull/{}", self.repository.path(), self.number)
    }

    pub(crate) async fn detail(&self, context: &Context) -> Result<&Value> {
        self.detail
            .get_or_try_init(|| async {
                Ok(context
                    .native
                    .get(&self.repository.api(&format!("pulls/{}", self.number)))
                    .await?)
            })
            .await
    }

    /// The head repository: this one, a readable fork, or none when the
    /// fork is gone or hidden from the caller.
    pub(crate) async fn head_repository(
        &self,
        context: &Context,
    ) -> Result<Option<&Arc<Repository>>> {
        Ok(self
            .head
            .get_or_try_init(|| async {
                match self.row["head_repository"]["owner"].as_str() {
                    Some(owner) => Repository::lookup(
                        context,
                        owner,
                        text(&self.row["head_repository"], "name"),
                    )
                    .await
                    .map_err(FieldError::from),
                    None => Ok(Some(Arc::clone(&self.repository))),
                }
            })
            .await?
            .as_ref())
    }

    /// The pull request's armed auto-merge. Merge-queue entries are not
    /// auto-merge requests, and Crab records who armed it by name only.
    async fn auto_merge(&self, context: &Context) -> Result<R> {
        if self.row["state"] != "open" {
            return Ok(R::null());
        }
        let path = self
            .repository
            .api(&format!("pulls/{}/auto-merge", self.number));
        let response = match context.native.get(&path).await {
            Ok(response) => response,
            // A reader the native view refuses sees no merge automation.
            Err(failure) if failure.status.is_client_error() => return Ok(R::null()),
            Err(failure) => return Err(failure.into()),
        };
        let request = &response["request"];
        if request.is_null() || !request["stopped"].is_null() || request["queue"] == true {
            return Ok(R::null());
        }
        let method = match request["method"].as_str() {
            Some("squash") => "SQUASH",
            Some("rebase") => "REBASE",
            Some("fast_forward") => "FAST_FORWARD",
            _ => "MERGE",
        };
        Ok(R::object(JsonObject::new(
            "AutoMergeRequest",
            json!({"mergeMethod": method, "enabledBy": null, "enabledAt": null, "authorEmail": null, "commitHeadline": null, "commitBody": null}),
        )))
    }

    /// Pending review requests, people first. The native listing pages
    /// people and reports every team request on each page.
    async fn review_requests(&self, context: &Context) -> Result<Vec<Arc<dyn Object<Context>>>> {
        let path = self
            .repository
            .api(&format!("pulls/{}/reviewers", self.number));
        let requested = |value: &Value| value["state"]["kind"] == "requested";
        let request = |key: &Value, code_owner: &Value, reviewer: Reviewer| {
            Arc::new(ReviewRequest {
                pull: self.number,
                repository: self.repository.id,
                key: key.as_str().unwrap_or_default().to_owned(),
                code_owner: *code_owner == true,
                reviewer,
            }) as Arc<dyn Object<Context>>
        };
        let mut requests = Vec::new();
        let mut teams = Vec::new();
        let mut after: Option<String> = None;
        for read in 0..MAX_NATIVE_PAGES {
            let mut pairs = vec![("limit", "50")];
            if let Some(after) = &after {
                pairs.push(("after", after));
            }
            let response = context
                .native
                .get(&format!("{path}?{}", query(&pairs)))
                .await?;
            for item in response["items"].as_array().into_iter().flatten() {
                if requested(&item["request"]) {
                    let reviewer = Reviewer::Person(item["reviewer"].clone());
                    requests.push(request(
                        &item["request"]["id"],
                        &item["request"]["code_owner"],
                        reviewer,
                    ));
                }
            }
            if read == 0
                && let Some(organization) = self.repository.organization()
            {
                for team in response["teams"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|team| requested(team))
                {
                    let Some(id) = team["id"]
                        .as_str()
                        .and_then(|id| uuid::Uuid::parse_str(id).ok())
                    else {
                        continue;
                    };
                    let name = team["name"].as_str().unwrap_or_default();
                    let reviewer = Reviewer::Team(Team::new(
                        organization,
                        &self.repository.owner,
                        id,
                        name,
                        None,
                    ));
                    teams.push(request(&team["id"], &team["code_owner"], reviewer));
                }
            }
            after = response["next"].as_str().map(str::to_owned);
            if after.is_none() {
                break;
            }
        }
        requests.extend(teams);
        Ok(requests)
    }

    pub(crate) fn cross_repository(&self) -> bool {
        self.row["head_repository"].is_object()
    }

    async fn comparison(&self, context: &Context) -> Result<(String, String)> {
        let detail = self.detail(context).await?;
        Ok((
            text(detail, "base_oid").to_owned(),
            text(detail, "head_oid").to_owned(),
        ))
    }

    /// Every changed file with both blob sides. Native diffs carry the
    /// texts; binary and oversized files count zero lines, as on GitHub.
    pub(crate) async fn files(&self, context: &Context) -> Result<&Vec<FileDiff>> {
        self.files
            .get_or_try_init(|| async {
                let (base, head) = self.comparison(context).await?;
                if base.is_empty() || head.is_empty() {
                    return Ok(Vec::new());
                }
                let Some(repository) = self.head_repository(context).await? else {
                    return Ok(Vec::new());
                };
                let mut pairs = vec![
                    ("base", base.as_str()),
                    ("rev", head.as_str()),
                    ("merge_base", "true"),
                ];
                let upstream = self.repository.id.to_string();
                if self.cross_repository() {
                    pairs.push(("upstream", upstream.as_str()));
                }
                let changes = context
                    .native
                    .get(&format!("{}?{}", repository.api("changes"), query(&pairs)))
                    .await?;
                let merge_base = changes["base"].as_str().unwrap_or(&base).to_owned();
                let mut files = Vec::new();
                for change in changes["changes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(MAX_STAT_FILES)
                {
                    let diff = context
                        .native
                        .get(&format!(
                            "{}?{}",
                            self.repository.api("diff"),
                            query(&[
                                ("base", merge_base.as_str()),
                                ("rev", head.as_str()),
                                ("path_hex", text(change, "path_hex")),
                            ])
                        ))
                        .await;
                    let (old, new) = match diff {
                        Ok(diff) => (diff["old"].clone(), diff["new"].clone()),
                        Err(failure) if failure.status.is_client_error() => {
                            (Value::Null, Value::Null)
                        }
                        Err(failure) => return Err(failure.into()),
                    };
                    let (additions, deletions) = line_changes(
                        old["text"].as_str().unwrap_or_default(),
                        new["text"].as_str().unwrap_or_default(),
                    );
                    files.push(FileDiff {
                        path: text(change, "path").to_owned(),
                        change: match text(change, "kind") {
                            "Added" => "ADDED",
                            "Deleted" => "DELETED",
                            "Renamed" => "RENAMED",
                            "Copied" => "COPIED",
                            _ => "MODIFIED",
                        },
                        old,
                        new,
                        additions,
                        deletions,
                    });
                }
                Ok(files)
            })
            .await
    }

    /// The comparison as a Git unified diff, GitHub's `diff` media type.
    pub(crate) async fn unified_diff(&self, context: &Context) -> Result<String> {
        let mut output = String::new();
        for file in self.files(context).await? {
            render_file(&mut output, file);
        }
        Ok(output)
    }

    async fn commits(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        let (base, head) = self.comparison(context).await?;
        let page = page(field)?;
        // `last: 1` is the head commit, which needs no history walk.
        if page.last && page.size == 1 && page.after.is_none() && !field.selects(&["totalCount"]) {
            let commit = PullCommit {
                pull: self.path(),
                repository: Arc::clone(&self.repository),
                oid: head,
                detail: None,
            };
            return Ok(R::object(Connection::new(
                "PullRequestCommitConnection",
                "PullRequestCommitEdge",
                vec![(
                    Arc::new(commit) as Arc<dyn Object<Context>>,
                    offset_cursor(0),
                )],
                1,
                false,
            )));
        }
        let Some(repository) = self.head_repository(context).await? else {
            return Ok(R::object(Connection::slice(
                "PullRequestCommitConnection",
                "PullRequestCommitEdge",
                Vec::new(),
                &page,
            )?));
        };
        let upstream = self.repository.id.to_string();
        let mut commits = Vec::new();
        let mut cursor: Option<String> = None;
        while commits.len() < MAX_COLLECTED {
            let limit = "200".to_owned();
            let mut pairs = vec![
                ("rev", head.as_str()),
                ("base", base.as_str()),
                ("limit", limit.as_str()),
            ];
            if self.cross_repository() {
                pairs.push(("upstream", upstream.as_str()));
            }
            if let Some(cursor) = &cursor {
                pairs.push(("cursor", cursor));
            }
            let response = context
                .native
                .get(&format!("{}?{}", repository.api("commits"), query(&pairs)))
                .await?;
            commits.extend(response["items"].as_array().cloned().unwrap_or_default());
            cursor = response["next"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        // Native history is newest first; GitHub lists commits oldest first.
        commits.reverse();
        let items = commits
            .into_iter()
            .map(|commit| {
                Arc::new(PullCommit {
                    pull: self.path(),
                    repository: Arc::clone(repository),
                    oid: text(&commit, "oid").to_owned(),
                    detail: Some(commit),
                }) as Arc<dyn Object<Context>>
            })
            .collect();
        Ok(R::object(Connection::slice(
            "PullRequestCommitConnection",
            "PullRequestCommitEdge",
            items,
            &page,
        )?))
    }

    async fn reviews(&self, context: &Context, field: &Field<'_>, latest: bool) -> Result<R> {
        let path = self
            .repository
            .api(&format!("pulls/{}/reviews", self.number));
        let (mut items, _) = collect(context, &path, MAX_COLLECTED).await?;
        items.reverse();
        let states: Option<Vec<String>> =
            field
                .argument("states")
                .and_then(Value::as_array)
                .map(|states| {
                    states
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                });
        let reviewer = field.string("author");
        let mut reviews: Vec<Value> = items
            .into_iter()
            .filter(|review| {
                states
                    .as_ref()
                    .is_none_or(|states| states.iter().any(|state| state == review_state(review)))
            })
            .filter(|review| {
                reviewer.is_none_or(|login| {
                    User::from_author(&review["author"]).is_some_and(|user| user.login == login)
                })
            })
            .collect();
        if latest {
            // The latest review per author that approved or requested changes.
            let mut seen = std::collections::HashSet::new();
            reviews.reverse();
            reviews.retain(|review| {
                review_state(review) != "COMMENTED"
                    && seen.insert(User::from_author(&review["author"]).map(|user| user.login))
            });
            reviews.reverse();
        }
        let items = reviews
            .into_iter()
            .map(|review| {
                Arc::new(Review::new(&self.repository, self.number, review))
                    as Arc<dyn Object<Context>>
            })
            .collect();
        Ok(R::object(Connection::slice(
            "PullRequestReviewConnection",
            "PullRequestReviewEdge",
            items,
            &page(field)?,
        )?))
    }

    async fn milestone(&self, context: &Context) -> Result<&Value> {
        self.milestone
            .get_or_try_init(|| async {
                Ok(context
                    .native
                    .get(
                        &self
                            .repository
                            .api(&format!("pulls/{}/milestone", self.number)),
                    )
                    .await?["milestone"]
                    .clone())
            })
            .await
    }
}

fn short(oid: &Value) -> String {
    oid.as_str()
        .map_or_else(|| "0000000".to_owned(), |oid| oid.chars().take(7).collect())
}

/// Renders one file like `git diff`, with headers for added, deleted, and
/// binary files and three lines of hunk context.
fn render_file(output: &mut String, file: &FileDiff) {
    use std::fmt::Write as _;
    let path = &file.path;
    let _ = writeln!(output, "diff --git a/{path} b/{path}");
    let mode = file.new["mode"]
        .as_str()
        .or(file.old["mode"].as_str())
        .unwrap_or("100644");
    match file.change {
        "ADDED" => {
            let _ = writeln!(output, "new file mode {mode}");
            let _ = writeln!(output, "index 0000000..{}", short(&file.new["oid"]));
        }
        "DELETED" => {
            let _ = writeln!(output, "deleted file mode {mode}");
            let _ = writeln!(output, "index {}..0000000", short(&file.old["oid"]));
        }
        _ => {
            let _ = writeln!(
                output,
                "index {}..{} {mode}",
                short(&file.old["oid"]),
                short(&file.new["oid"])
            );
        }
    }
    let readable = |side: &Value| side.is_null() || side["text"].is_string();
    if !readable(&file.old) || !readable(&file.new) {
        let _ = writeln!(output, "Binary files a/{path} and b/{path} differ");
        return;
    }
    let old = file.old["text"].as_str().unwrap_or_default();
    let new = file.new["text"].as_str().unwrap_or_default();
    if old == new {
        return;
    }
    let _ = writeln!(
        output,
        "{}",
        if file.old.is_null() {
            "--- /dev/null".to_owned()
        } else {
            format!("--- a/{path}")
        }
    );
    let _ = writeln!(
        output,
        "{}",
        if file.new.is_null() {
            "+++ /dev/null".to_owned()
        } else {
            format!("+++ b/{path}")
        }
    );
    use gix_diff::blob::{
        Algorithm, InternedInput, UnifiedDiff, diff_with_slider_heuristics,
        unified_diff::{ConsumeHunk, ContextSize, DiffLineKind, HunkHeader},
    };
    struct Hunks<'a>(&'a mut String);
    impl ConsumeHunk for Hunks<'_> {
        type Out = ();
        fn consume_hunk(
            &mut self,
            header: HunkHeader,
            lines: &[(DiffLineKind, &[u8])],
        ) -> std::io::Result<()> {
            let _ = writeln!(
                self.0,
                "@@ -{} +{} @@",
                hunk_range(header.before_hunk_start, header.before_hunk_len),
                hunk_range(header.after_hunk_start, header.after_hunk_len)
            );
            for (kind, line) in lines {
                self.0.push(match kind {
                    DiffLineKind::Context => ' ',
                    DiffLineKind::Add => '+',
                    DiffLineKind::Remove => '-',
                });
                let line = String::from_utf8_lossy(line);
                self.0.push_str(&line);
                if !line.ends_with('\n') {
                    self.0.push_str("\n\\ No newline at end of file\n");
                }
            }
            Ok(())
        }
        fn finish(self) {}
    }
    let input = InternedInput::new(old, new);
    let diff = diff_with_slider_heuristics(Algorithm::Histogram, &input);
    let _ = UnifiedDiff::new(&diff, &input, Hunks(output), ContextSize::symmetrical(3)).consume();
}

/// One side of a hunk header as Git writes it: an empty range names the
/// line before it and a one-line range omits its count, so patch tools
/// read Crab's diffs exactly like Git's.
fn hunk_range(start: u32, len: u32) -> String {
    match len {
        0 => format!("{},0", start.saturating_sub(1)),
        1 => start.to_string(),
        _ => format!("{start},{len}"),
    }
}

fn line_changes(old: &str, new: &str) -> (u64, u64) {
    use gix_diff::blob::{Algorithm, InternedInput, diff_with_slider_heuristics};
    let input = InternedInput::new(old, new);
    let diff = diff_with_slider_heuristics(Algorithm::Histogram, &input);
    (
        u64::from(diff.count_additions()),
        u64::from(diff.count_removals()),
    )
}

/// GitHub names pull request branches without the `refs/heads/` prefix
/// that native pull requests store.
fn branch_name(reference: &str) -> &str {
    reference.strip_prefix("refs/heads/").unwrap_or(reference)
}

fn review_state(review: &Value) -> &'static str {
    if !review["dismissal"].is_null() {
        return "DISMISSED";
    }
    match review["state"].as_str() {
        Some("approved") => "APPROVED",
        Some("changes_requested") => "CHANGES_REQUESTED",
        _ => "COMMENTED",
    }
}

fn merge_state_status(detail: &Value) -> &'static str {
    if detail["state"] != "open" {
        return "UNKNOWN";
    }
    let requirements = &detail["merge_requirements"];
    if detail["draft"] == true {
        "DRAFT"
    } else if requirements["head_up_to_date"] == false {
        "BEHIND"
    } else if requirements["satisfied"] != true {
        "BLOCKED"
    } else if requirements["checks_satisfied"] == false {
        "UNSTABLE"
    } else {
        "CLEAN"
    }
}

fn review_decision(detail: &Value) -> Value {
    let requirements = &detail["merge_requirements"];
    let required = requirements["required_approvals"].as_u64().unwrap_or(0);
    let approvals = requirements["approvals"].as_u64().unwrap_or(0);
    let changes = requirements["changes_requested"].as_u64().unwrap_or(0);
    Value::from(if changes > 0 {
        "CHANGES_REQUESTED"
    } else if required > 0 && approvals < required {
        "REVIEW_REQUIRED"
    } else if approvals > 0 {
        "APPROVED"
    } else {
        return Value::Null;
    })
}

#[async_trait]
impl Object<Context> for PullRequest {
    fn typename(&self) -> &str {
        "PullRequest"
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        let row = &self.row;
        let node = NodeId::Pull(self.repository.id, self.number);
        let state = text(row, "state");
        Ok(match field.name {
            "id" => R::leaf(node.encode()),
            "databaseId" => R::leaf(database_id(&node)),
            "fullDatabaseId" => R::leaf(rest_id(&node).to_string()),
            "number" => R::leaf(self.number),
            "title" => R::leaf(text(row, "title")),
            "body" => R::leaf(text(self.detail(context).await?, "body")),
            "state" => R::leaf(match state {
                "merged" => "MERGED",
                "closed" => "CLOSED",
                _ => "OPEN",
            }),
            "closed" => R::leaf(state != "open"),
            "merged" => R::leaf(state == "merged"),
            "closedAt" => R::Leaf(timestamp(&row["closed_at"])),
            "mergedAt" => R::Leaf(timestamp(&row["merged_at"])),
            "createdAt" | "publishedAt" => R::Leaf(timestamp(&row["created_at"])),
            "updatedAt" => R::Leaf(timestamp(&row["updated_at"])),
            "lastEditedAt" | "potentialMergeCommit" => R::null(),
            "autoMergeRequest" => self.auto_merge(context).await?,
            "url" | "permalink" => R::leaf(context.url(&self.path())),
            "resourcePath" => R::leaf(self.path()),
            "author" => author(&row["author"]),
            "authorAssociation" => R::leaf("NONE"),
            "includesCreatedEdit" => {
                R::leaf(self.detail(context).await?["has_edit_history"] == true)
            }
            "isDraft" => R::leaf(row["draft"] == true),
            "baseRefName" => R::leaf(branch_name(text(row, "base_ref"))),
            "headRefName" => R::leaf(branch_name(text(row, "head_ref"))),
            "baseRefOid" => R::leaf(text(self.detail(context).await?, "base_oid")),
            // List rows carry the current head in their check rollup.
            "headRefOid" => match row["rollup"]["head_oid"].as_str() {
                Some(oid) => R::leaf(oid),
                None => R::leaf(text(self.detail(context).await?, "head_oid")),
            },
            "baseRef" => {
                let found = self
                    .repository
                    .find_ref(context, text(row, "base_ref"))
                    .await?;
                match found {
                    Some(base) if field.selects(&["branchProtectionRule"]) => {
                        let detail = self.detail(context).await?;
                        R::object(base.based(&detail["merge_requirements"]))
                    }
                    found => R::optional(found),
                }
            }
            "headRef" => match self.head_repository(context).await? {
                Some(repository) => {
                    R::optional(repository.find_ref(context, text(row, "head_ref")).await?)
                }
                None => R::null(),
            },
            "baseRepository" | "repository" => R::object(Arc::clone(&self.repository)),
            "headRepository" => R::optional(self.head_repository(context).await?.map(Arc::clone)),
            "headRepositoryOwner" => match self.head_repository(context).await? {
                Some(repository) => super::actors::repository_owner(repository),
                None => R::null(),
            },
            "isCrossRepository" => R::leaf(self.cross_repository()),
            // Crab grants fork pushes only through the fork's own grants.
            "maintainerCanModify" => R::leaf(false),
            // Crab detects conflicts when a merge runs, not ahead of it.
            "mergeable" => R::leaf("UNKNOWN"),
            "mergeStateStatus" => R::leaf(merge_state_status(self.detail(context).await?)),
            "reviewDecision" => R::Leaf(review_decision(self.detail(context).await?)),
            "mergeCommit" => match self.detail(context).await?["merge"]["commit_oid"].as_str() {
                Some(oid) => R::object(Commit::new(&self.repository, oid.to_owned(), None)),
                None => R::null(),
            },
            "mergedBy" => match &self.detail(context).await?["merge"]["author"] {
                Value::Null => R::null(),
                author => R::optional(User::from_author(author)),
            },
            "additions" => R::leaf(
                self.files(context)
                    .await?
                    .iter()
                    .map(|file| file.additions)
                    .sum::<u64>(),
            ),
            "deletions" => R::leaf(
                self.files(context)
                    .await?
                    .iter()
                    .map(|file| file.deletions)
                    .sum::<u64>(),
            ),
            "changedFiles" => R::leaf(self.files(context).await?.len()),
            "files" => {
                let items = self
                    .files(context)
                    .await?
                    .iter()
                    .map(|file| {
                        Arc::new(JsonObject::new(
                            "PullRequestChangedFile",
                            json!({"path": file.path, "additions": file.additions, "deletions": file.deletions, "changeType": file.change}),
                        )) as Arc<dyn Object<Context>>
                    })
                    .collect();
                R::object(Connection::slice(
                    "PullRequestChangedFileConnection",
                    "PullRequestChangedFileEdge",
                    items,
                    &page(field)?,
                )?)
            }
            "commits" => self.commits(context, field).await?,
            "assignees" => users(&row["assignees"], field)?,
            "labels" => label_connection(context, &self.repository, &row["labels"], field).await?,
            "milestone" => match self.milestone(context).await? {
                Value::Null => R::null(),
                milestone => R::object(Milestone::new(&self.repository, milestone.clone())),
            },
            "comments" => {
                let total = row["comment_count"].as_u64().map(|count| count as usize);
                comments(
                    context,
                    &self.repository,
                    Discussion::Pull,
                    self.number,
                    total,
                    field,
                )
                .await?
            }
            "totalCommentsCount" => R::Leaf(row["comment_count"].clone()),
            "reviews" => self.reviews(context, field, false).await?,
            "latestReviews" => self.reviews(context, field, true).await?,
            "reviewRequests" => R::object(Connection::slice(
                "ReviewRequestConnection",
                "ReviewRequestEdge",
                self.review_requests(context).await?,
                &page(field)?,
            )?),
            "reactionGroups" => reaction_groups(self.detail(context).await?),
            "viewerCanUpdate" => R::leaf(self.detail(context).await?["can_edit"] == true),
            "viewerDidAuthor" => R::leaf(viewer_authored(context, &row["author"]).await),
            _ => R::null(),
        })
    }
}

struct PullCommit {
    pull: String,
    repository: Arc<Repository>,
    oid: String,
    detail: Option<Value>,
}

#[async_trait]
impl Object<Context> for PullCommit {
    fn typename(&self) -> &str {
        "PullRequestCommit"
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        let path = format!("{}/commits/{}", self.pull, self.oid);
        Ok(match field.name {
            "id" => R::leaf(NodeId::Commit(self.repository.id, self.oid.clone()).encode()),
            "commit" => R::object(Commit::new(
                &self.repository,
                self.oid.clone(),
                self.detail.clone(),
            )),
            "url" => R::leaf(context.url(&path)),
            "resourcePath" => R::leaf(path),
            "pullRequest" => {
                return Err(FieldError::new(
                    "Select the pull request from its parent field",
                ));
            }
            _ => R::null(),
        })
    }
}

pub(super) struct Review {
    pull: String,
    repository: Arc<Repository>,
    number: u64,
    value: Value,
}

impl Review {
    /// A native review view on pull request `number`.
    pub(super) fn new(repository: &Arc<Repository>, number: u64, value: Value) -> Self {
        Self {
            pull: format!("{}/pull/{number}", repository.path()),
            repository: Arc::clone(repository),
            number,
            value,
        }
    }
}

#[async_trait]
impl Object<Context> for Review {
    fn typename(&self) -> &str {
        "PullRequestReview"
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        let value = &self.value;
        let review = value["number"].as_u64().unwrap_or(0);
        let node = NodeId::Review(self.repository.id, self.number, review);
        let path = format!("{}#pullrequestreview-{}", self.pull, rest_id(&node));
        Ok(match field.name {
            "id" => R::leaf(node.encode()),
            "databaseId" => R::leaf(database_id(&node)),
            "fullDatabaseId" => R::leaf(rest_id(&node).to_string()),
            "author" => author(&value["author"]),
            "authorAssociation" => R::leaf("NONE"),
            "body" => R::leaf(text(value, "body")),
            "state" => R::leaf(review_state(value)),
            "submittedAt" | "createdAt" => R::Leaf(timestamp(&value["created_at"])),
            "updatedAt" => R::Leaf(timestamp(&value["updated_at"])),
            "includesCreatedEdit" => R::leaf(value["has_edit_history"] == true),
            "commit" => match value["commit_oid"].as_str().or(value["head_oid"].as_str()) {
                Some(oid) => R::object(Commit::new(&self.repository, oid.to_owned(), None)),
                None => R::null(),
            },
            "reactionGroups" => reaction_groups(value),
            "url" => R::leaf(context.url(&path)),
            "resourcePath" => R::leaf(path),
            "pullRequest" => {
                R::optional(PullRequest::find(context, &self.repository, self.number).await?)
            }
            _ => R::null(),
        })
    }
}

enum Reviewer {
    Person(Value),
    Team(Team),
}

struct ReviewRequest {
    pull: u64,
    repository: uuid::Uuid,
    /// The native request's reviewer ID or team UUID.
    key: String,
    code_owner: bool,
    reviewer: Reviewer,
}

#[async_trait]
impl Object<Context> for ReviewRequest {
    fn typename(&self) -> &str {
        "ReviewRequest"
    }

    async fn resolve(&self, _: &Context, field: &Field<'_>) -> Result<R> {
        Ok(match field.name {
            "id" => R::leaf(format!(
                "RR_{}",
                &blake3::hash(
                    format!("{}\0{}\0{}", self.repository, self.pull, self.key).as_bytes()
                )
                .to_hex()[..24]
            )),
            "asCodeOwner" => R::leaf(self.code_owner),
            "requestedReviewer" => match &self.reviewer {
                Reviewer::Person(reviewer) => author(reviewer),
                Reviewer::Team(team) => R::object(team.clone()),
            },
            _ => {
                return Err(FieldError::new(
                    "Select the pull request from its parent field",
                ));
            }
        })
    }
}

/// A commit's combined statuses and check runs, or none when it has neither.
pub(super) async fn status_check_rollup(
    context: &Context,
    repository: &Arc<Repository>,
    oid: &str,
) -> Result<R> {
    let statuses = context
        .native
        .get(&repository.api(&format!("commits/{oid}/status")))
        .await?;
    let mut runs = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let path = match &cursor {
            Some(after) => format!(
                "{}?{}",
                repository.api(&format!("commits/{oid}/check-runs")),
                query(&[("after", after)])
            ),
            None => repository.api(&format!("commits/{oid}/check-runs")),
        };
        let response = context.native.get(&path).await?;
        runs.extend(response["items"].as_array().cloned().unwrap_or_default());
        cursor = response["next"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    let statuses = statuses["statuses"].as_array().cloned().unwrap_or_default();
    if statuses.is_empty() && runs.is_empty() {
        return Ok(R::null());
    }
    Ok(R::object(Rollup {
        repository: Arc::clone(repository),
        oid: oid.to_owned(),
        statuses,
        runs,
    }))
}

struct Rollup {
    repository: Arc<Repository>,
    oid: String,
    statuses: Vec<Value>,
    runs: Vec<Value>,
}

fn status_state(value: &Value) -> &'static str {
    match value.as_str() {
        Some("success") => "SUCCESS",
        Some("failure") => "FAILURE",
        Some("error") => "ERROR",
        Some("expected") => "EXPECTED",
        _ => "PENDING",
    }
}

fn check_status(run: &Value) -> &'static str {
    match run["status"].as_str() {
        Some("completed") => "COMPLETED",
        Some("in_progress") => "IN_PROGRESS",
        Some("queued") => "QUEUED",
        Some("waiting") => "WAITING",
        Some("requested") => "REQUESTED",
        _ => "PENDING",
    }
}

fn check_conclusion(run: &Value) -> Value {
    run["conclusion"]
        .as_str()
        .map_or(Value::Null, |conclusion| {
            Value::String(conclusion.to_ascii_uppercase())
        })
}

impl Rollup {
    fn state(&self) -> &'static str {
        let failing = |conclusion: &str| {
            matches!(
                conclusion,
                "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
            )
        };
        let mut pending = false;
        for status in &self.statuses {
            match status_state(&status["state"]) {
                "FAILURE" => return "FAILURE",
                "ERROR" => return "ERROR",
                "SUCCESS" => {}
                _ => pending = true,
            }
        }
        for run in &self.runs {
            match check_conclusion(run).as_str() {
                Some(conclusion) if failing(conclusion) => return "FAILURE",
                Some(_) => {}
                None => pending = true,
            }
        }
        if pending { "PENDING" } else { "SUCCESS" }
    }
}

#[async_trait]
impl Object<Context> for Rollup {
    fn typename(&self) -> &str {
        "StatusCheckRollup"
    }

    async fn resolve(&self, _: &Context, field: &Field<'_>) -> Result<R> {
        Ok(match field.name {
            "id" => R::leaf(format!(
                "SCR_{}",
                &blake3::hash(format!("{}\0{}", self.repository.id, self.oid).as_bytes()).to_hex()
                    [..24]
            )),
            "state" => R::leaf(self.state()),
            "commit" => R::object(Commit::new(&self.repository, self.oid.clone(), None)),
            "contexts" => {
                let mut items: Vec<Arc<dyn Object<Context>>> = Vec::new();
                for run in &self.runs {
                    items.push(Arc::new(CheckContext::Run(
                        Arc::clone(&self.repository),
                        run.clone(),
                    )));
                }
                for status in &self.statuses {
                    items.push(Arc::new(CheckContext::Status(
                        Arc::clone(&self.repository),
                        status.clone(),
                    )));
                }
                let mut run_counts = std::collections::BTreeMap::new();
                for run in &self.runs {
                    let state = check_conclusion(run)
                        .as_str()
                        .map_or_else(|| check_status(run).to_owned(), str::to_owned);
                    *run_counts.entry(state).or_insert(0_usize) += 1;
                }
                let mut status_counts = std::collections::BTreeMap::new();
                for status in &self.statuses {
                    *status_counts
                        .entry(status_state(&status["state"]))
                        .or_insert(0_usize) += 1;
                }
                R::object(
                    Connection::slice(
                        "StatusCheckRollupContextConnection",
                        "StatusCheckRollupContextEdge",
                        items,
                        &page(field)?,
                    )?
                    .scalar("checkRunCount", self.runs.len())
                    .scalar("statusContextCount", self.statuses.len())
                    .objects(
                        "checkRunCountsByState",
                        "CheckRunStateCount",
                        run_counts
                            .into_iter()
                            .map(|(state, count)| json!({"state": state, "count": count}))
                            .collect(),
                    )
                    .objects(
                        "statusContextCountsByState",
                        "StatusContextStateCount",
                        status_counts
                            .into_iter()
                            .map(|(state, count)| json!({"state": state, "count": count}))
                            .collect(),
                    ),
                )
            }
            _ => R::null(),
        })
    }
}

enum CheckContext {
    Run(Arc<Repository>, Value),
    Status(Arc<Repository>, Value),
}

impl CheckContext {
    /// Whether the named check is required for the pull request argument.
    async fn required(&self, context: &Context, field: &Field<'_>, name: &str) -> Result<bool> {
        let repository = match self {
            Self::Run(repository, _) | Self::Status(repository, _) => repository,
        };
        let number = match (
            field.int("pullRequestNumber"),
            field.string("pullRequestId"),
        ) {
            (Some(number), _) => u64::try_from(number).ok(),
            (None, Some(id)) => match NodeId::decode(id) {
                Some(NodeId::Pull(owner, number)) if owner == repository.id => Some(number),
                _ => None,
            },
            (None, None) => None,
        };
        let Some(number) = number else {
            return Ok(false);
        };
        let Some(pull) = PullRequest::find(context, repository, number).await? else {
            return Ok(false);
        };
        Ok(pull.detail(context).await?["merge_requirements"]["checks"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|check| check["name"] == name || check["context"] == name))
    }
}

#[async_trait]
impl Object<Context> for CheckContext {
    fn typename(&self) -> &str {
        match self {
            Self::Run(..) => "CheckRun",
            Self::Status(..) => "StatusContext",
        }
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        Ok(match self {
            Self::Run(repository, run) => {
                let id = run["id"].as_u64().unwrap_or(0);
                let path = format!("{}/runs/{id}", repository.path());
                match field.name {
                    "id" => R::leaf(format!("CR_{}_{id}", repository.id.simple())),
                    "databaseId" => R::leaf(id),
                    "name" => R::leaf(text(run, "name")),
                    "status" => R::leaf(check_status(run)),
                    "conclusion" => R::Leaf(check_conclusion(run)),
                    "startedAt" => R::Leaf(timestamp(&run["started_at"])),
                    "completedAt" => R::Leaf(timestamp(&run["completed_at"])),
                    "detailsUrl" => R::Leaf(run["details_url"].clone()),
                    "title" => R::Leaf(run["output_title"].clone()),
                    "summary" => R::null(),
                    "url" => R::leaf(context.url(&path)),
                    "resourcePath" => R::leaf(path),
                    "checkSuite" => R::object(JsonObject::new(
                        "CheckSuite",
                        json!({"id": format!("CS_{}_{id}", repository.id.simple()), "workflowRun": null}),
                    )),
                    "isRequired" => {
                        R::leaf(self.required(context, field, text(run, "name")).await?)
                    }
                    _ => R::null(),
                }
            }
            Self::Status(repository, status) => match field.name {
                "id" => R::leaf(format!(
                    "SC_{}",
                    &blake3::hash(
                        format!(
                            "{}\0{}\0{}",
                            repository.id, status["context"], status["created_at"]
                        )
                        .as_bytes()
                    )
                    .to_hex()[..24]
                )),
                "context" => R::leaf(text(status, "context")),
                "state" => R::leaf(status_state(&status["state"])),
                "description" => R::Leaf(status["description"].clone()),
                "targetUrl" => R::Leaf(status["target_url"].clone()),
                "createdAt" => R::Leaf(timestamp(&status["created_at"])),
                "avatarUrl" => R::null(),
                "isRequired" => R::leaf(
                    self.required(context, field, text(status, "context"))
                        .await?,
                ),
                _ => R::null(),
            },
        })
    }
}

/// Builds a native discussion query from GitHub's connection filters.
fn discussion_query(field: &Field<'_>, pull: bool) -> Result<String> {
    let mut terms = vec![if pull { "is:pr" } else { "is:issue" }.to_owned()];
    let filters = field.argument("filterBy").cloned().unwrap_or(Value::Null);
    let states: Vec<String> = field
        .argument("states")
        .or_else(|| filters.get("states").filter(|states| !states.is_null()))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_ascii_lowercase)
        .collect();
    let all: &[&str] = if pull {
        &["open", "closed", "merged"]
    } else {
        &["open", "closed"]
    };
    let selected: Vec<&str> = all
        .iter()
        .copied()
        .filter(|state| states.iter().any(|value| value == state))
        .collect();
    match (selected.len(), all.len()) {
        (0, _) | (2, 2) | (3, 3) => {}
        (1, _) => terms.push(format!("is:{}", selected[0])),
        // Two of three pull states: exclude the third.
        _ => {
            if let Some(excluded) = all.iter().find(|state| !selected.contains(state)) {
                terms.push(format!("-is:{excluded}"));
            }
        }
    }
    let quote = |value: &str| format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""));
    let labels: Vec<String> = field
        .argument("labels")
        .or_else(|| filters.get("labels").filter(|labels| !labels.is_null()))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(quote)
        .collect();
    if !labels.is_empty() {
        terms.push(format!("label:{}", labels.join(",")));
    }
    for (filter, qualifier) in [
        ("assignee", "assignee"),
        ("createdBy", "author"),
        ("milestone", "milestone"),
    ] {
        if let Some(value) = filters.get(filter).and_then(Value::as_str) {
            terms.push(format!("{qualifier}:{}", quote(value)));
        }
    }
    if filters
        .get("mentioned")
        .is_some_and(|value| !value.is_null())
        || filters
            .get("viewerSubscribed")
            .is_some_and(|value| value == true)
        || filters
            .get("milestoneNumber")
            .is_some_and(|value| !value.is_null())
    {
        return Err(FieldError::unprocessable(
            "Crab does not filter by mention, subscription, or milestone number; use a search query",
        ));
    }
    if let Some(since) = filters.get("since").and_then(Value::as_str) {
        terms.push(format!("updated:>={since}"));
    }
    for (argument, qualifier) in [("headRefName", "head"), ("baseRefName", "base")] {
        if let Some(value) = field.string(argument) {
            terms.push(format!("{qualifier}:{}", quote(value)));
        }
    }
    Ok(terms.join(" "))
}

fn ordering(field: &Field<'_>) -> (&'static str, &'static str) {
    let order = field.argument("orderBy").cloned().unwrap_or(Value::Null);
    let sort = match order["field"].as_str() {
        Some("UPDATED_AT") => "updated",
        Some("COMMENTS") => "comments",
        _ => "created",
    };
    let direction = if order["direction"] == "ASC" {
        "asc"
    } else {
        "desc"
    };
    (sort, direction)
}

/// Reads one GitHub page of a native discussion list, joining native pages.
/// One native discussion page: its items, the cursor after it, and the
/// number of discussions the query matches.
async fn native_discussions(
    context: &Context,
    path: &str,
    q: &str,
    sort: (&str, &str),
    cursor: Option<&str>,
    limit: usize,
) -> Result<(Vec<Value>, Option<String>, u64)> {
    let limit = limit.clamp(1, NATIVE_PAGE).to_string();
    let mut pairs = vec![
        ("q", q),
        ("limit", limit.as_str()),
        ("sort", sort.0),
        ("direction", sort.1),
    ];
    if let Some(cursor) = cursor {
        pairs.push(("before", cursor));
    }
    let response = context
        .native
        .get(&format!("{path}?{}", query(&pairs)))
        .await?;
    Ok((
        response["items"].as_array().cloned().unwrap_or_default(),
        response["next"].as_str().map(str::to_owned),
        response["matching"].as_u64().unwrap_or(0),
    ))
}

/// Fills one GitHub page. `head` keeps only that exact head ref: native
/// `head:` matches a branch-name prefix, GitHub's `headRefName` is exact.
async fn discussion_page(
    context: &Context,
    path: &str,
    q: &str,
    sort: (&str, &str),
    page: &Page,
    head: Option<&str>,
) -> Result<(Vec<Value>, Option<String>, u64)> {
    let keep = |item: &Value| head.is_none_or(|head| item["head_ref"] == head);
    if page.size == 0 {
        // `first: 0` still asks for the total.
        let (_, _, total) =
            native_discussions(context, path, q, sort, page.after.as_deref(), 1).await?;
        return Ok((Vec::new(), page.after.clone(), total));
    }
    let mut items = Vec::new();
    let mut cursor = page.after.clone();
    let mut total = 0;
    // Each native page is no larger than the items still needed, so the
    // native cursor always resumes exactly after this GitHub page.
    for _ in 0..MAX_NATIVE_PAGES {
        if items.len() >= page.size {
            break;
        }
        let (batch, next, matching) = native_discussions(
            context,
            path,
            q,
            sort,
            cursor.as_deref(),
            page.size - items.len(),
        )
        .await?;
        total = matching;
        items.extend(batch.into_iter().filter(keep));
        cursor = next;
        if cursor.is_none() {
            break;
        }
    }
    Ok((items, cursor, total))
}

/// Counts pull requests whose head is exactly `head`, which the native
/// prefix total over-counts.
async fn exact_head_total(context: &Context, path: &str, q: &str, head: &str) -> Result<u64> {
    let mut total = 0;
    let mut cursor = None;
    for _ in 0..MAX_NATIVE_PAGES {
        let (batch, next, _) = native_discussions(
            context,
            path,
            q,
            ("created", "desc"),
            cursor.as_deref(),
            NATIVE_PAGE,
        )
        .await?;
        total += batch.iter().filter(|item| item["head_ref"] == head).count() as u64;
        cursor = next;
        if cursor.is_none() {
            return Ok(total);
        }
    }
    Err(FieldError::unprocessable(
        "Too many pull requests share this head branch prefix to count; omit totalCount",
    ))
}

pub(super) async fn issues(
    context: &Context,
    repository: &Arc<Repository>,
    field: &Field<'_>,
) -> Result<R> {
    let q = discussion_query(field, false)?;
    let page = page(field)?;
    let (items, next, total) = discussion_page(
        context,
        &repository.api("issues"),
        &q,
        ordering(field),
        &page,
        None,
    )
    .await?;
    let nodes = items
        .into_iter()
        .map(|item| {
            (
                Arc::new(Issue::new(repository, item, false)) as Arc<dyn Object<Context>>,
                next.clone().unwrap_or_default(),
            )
        })
        .collect();
    Ok(R::object(Connection::new(
        "IssueConnection",
        "IssueEdge",
        nodes,
        usize::try_from(total).unwrap_or(usize::MAX),
        next.is_some(),
    )))
}

pub(super) async fn pull_requests(
    context: &Context,
    repository: &Arc<Repository>,
    field: &Field<'_>,
) -> Result<R> {
    let q = discussion_query(field, true)?;
    let page = page(field)?;
    let path = repository.api("pulls");
    let head = field
        .string("headRefName")
        .map(|branch| format!("refs/heads/{branch}"));
    let (items, next, total) =
        discussion_page(context, &path, &q, ordering(field), &page, head.as_deref()).await?;
    let total = match &head {
        Some(head) if field.selects(&["totalCount"]) => {
            exact_head_total(context, &path, &q, head).await?
        }
        _ => total,
    };
    let nodes = items
        .into_iter()
        .map(|item| {
            (
                Arc::new(PullRequest::new(repository, item, false)) as Arc<dyn Object<Context>>,
                next.clone().unwrap_or_default(),
            )
        })
        .collect();
    Ok(R::object(Connection::new(
        "PullRequestConnection",
        "PullRequestEdge",
        nodes,
        usize::try_from(total).unwrap_or(usize::MAX),
        next.is_some(),
    )))
}

/// GitHub's search, for issues and pull requests in one repository. The
/// `repo:` qualifier selects the repository; Crab's grammar parses the rest.
pub(super) async fn search(context: &Context, field: &Field<'_>) -> Result<R> {
    let page = page(field)?;
    let search = super::required(field, "query")?;
    if field.string("type") != Some("ISSUE") {
        return Err(FieldError::unprocessable(
            "Crab searches issues and pull requests; repository and user search use the REST API",
        ));
    }
    let mut address = None;
    let mut sort = ("created", "desc");
    let mut terms = Vec::new();
    for term in split_terms(search) {
        if let Some(repository) = term.strip_prefix("repo:") {
            address = repository
                .split_once('/')
                .map(|(owner, name)| (owner.to_owned(), name.to_owned()));
        } else if let Some(order) = term.strip_prefix("sort:") {
            let (key, direction) = order.split_once('-').unwrap_or((order, "desc"));
            sort = (
                match key {
                    "updated" => "updated",
                    "comments" => "comments",
                    _ => "created",
                },
                if direction == "asc" { "asc" } else { "desc" },
            );
        } else if term == "type:issue" {
            terms.push("is:issue".to_owned());
        } else if term == "type:pr" {
            terms.push("is:pr".to_owned());
        } else {
            terms.push(term);
        }
    }
    let Some((owner, name)) = address else {
        return Err(FieldError::unprocessable(
            "Crab searches one repository at a time; add a repo:OWNER/NAME qualifier",
        ));
    };
    let repository = Repository::find(context, &owner, &name).await?;
    let q = terms.join(" ");
    let (items, next, total) =
        discussion_page(context, &repository.api("issues"), &q, sort, &page, None).await?;
    let nodes = items
        .into_iter()
        .map(|item| {
            let node: Arc<dyn Object<Context>> = if item["kind"] == "pull" {
                Arc::new(PullRequest::new(&repository, item, false))
            } else {
                Arc::new(Issue::new(&repository, item, false))
            };
            (node, next.clone().unwrap_or_default())
        })
        .collect();
    Ok(R::object(
        Connection::new(
            "SearchResultItemConnection",
            "SearchResultItemEdge",
            nodes,
            usize::try_from(total).unwrap_or(usize::MAX),
            next.is_some(),
        )
        .scalar("issueCount", total)
        .scalar("codeCount", 0)
        .scalar("discussionCount", 0)
        .scalar("repositoryCount", 0)
        .scalar("userCount", 0)
        .scalar("wikiCount", 0),
    ))
}

/// Splits a search on whitespace outside quotes, keeping quoted phrases.
fn split_terms(search: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in search.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                current.push(character);
            }
            character if character.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    terms.push(std::mem::take(&mut current));
                }
            }
            character => current.push(character),
        }
    }
    if !current.is_empty() {
        terms.push(current);
    }
    terms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_diffs_match_git_byte_for_byte() {
        let side =
            |oid: &str, text: &str| json!({"mode": "100644", "oid": oid.repeat(40), "text": text});
        for (change, old, new, expected) in [
            (
                "ADDED",
                Value::Null,
                side("8", "# Guide\n"),
                "diff --git a/f b/f\nnew file mode 100644\nindex 0000000..8888888\n--- /dev/null\n+++ b/f\n@@ -0,0 +1 @@\n+# Guide\n",
            ),
            (
                "MODIFIED",
                side("a", "a\nb\nc\n"),
                side("b", "a\nB\nc\n"),
                "diff --git a/f b/f\nindex aaaaaaa..bbbbbbb 100644\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n",
            ),
        ] {
            let mut output = String::new();
            let file = FileDiff {
                path: "f".into(),
                change,
                old,
                new,
                additions: 0,
                deletions: 0,
            };
            render_file(&mut output, &file);
            assert_eq!(output, expected, "{change}");
        }
    }

    #[test]
    fn line_changes_count_like_git_diff() {
        assert_eq!(line_changes("a\nb\nc\n", "a\nB\nc\nd\n"), (2, 1));
        assert_eq!(line_changes("", "one\ntwo\n"), (2, 0));
        assert_eq!(line_changes("same\n", "same\n"), (0, 0));
    }

    #[test]
    fn search_terms_keep_quoted_phrases() {
        assert_eq!(
            split_terms(r#"repo:team/private is:open label:"needs review" sort:updated-asc"#),
            [
                "repo:team/private",
                "is:open",
                "label:\"needs review\"",
                "sort:updated-asc"
            ]
        );
    }
}
