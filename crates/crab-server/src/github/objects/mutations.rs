//! GitHub mutations, each a native command bound to the target's UUID.
//!
//! Native commands are idempotent per submission UUID and versioned per
//! record. GitHub mutations carry neither, so each one reads the current
//! version first and retries once when a concurrent edit wins the race.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{
    Field, R, Result,
    discussions::{Issue, IssueComment, PullRequest, Review},
    repository::{Ref, Repository},
    text,
};
use crate::github::{
    Context,
    graphql::{FieldError, Object},
    ids::{Discussion, NodeId},
    native::Failure,
};

pub(crate) struct Mutation;

#[async_trait]
impl Object<Context> for Mutation {
    fn typename(&self) -> &str {
        "Mutation"
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        let input = field
            .argument("input")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let client = input
            .get("clientMutationId")
            .cloned()
            .unwrap_or(Value::Null);
        let (typename, key, object): (&str, &str, Arc<dyn Object<Context>>) = match field.name {
            "addComment" => return add_comment(context, &input, client).await,
            "createIssue" => (
                "CreateIssuePayload",
                "issue",
                Arc::new(create_issue(context, &input).await?),
            ),
            "createRepository" => (
                "CreateRepositoryPayload",
                "repository",
                Arc::new(create_repository(context, &input).await?),
            ),
            "closeIssue" | "reopenIssue" => {
                let closing = field.name == "closeIssue";
                let issue = issue_state(context, &input, closing).await?;
                let typename = if closing {
                    "CloseIssuePayload"
                } else {
                    "ReopenIssuePayload"
                };
                (typename, "issue", Arc::new(issue))
            }
            "updateIssue" => (
                "UpdateIssuePayload",
                "issue",
                Arc::new(update_issue(context, &input).await?),
            ),
            "createPullRequest" => (
                "CreatePullRequestPayload",
                "pullRequest",
                Arc::new(create_pull(context, &input).await?),
            ),
            "updatePullRequest" => (
                "UpdatePullRequestPayload",
                "pullRequest",
                Arc::new(update_pull(context, &input).await?),
            ),
            "closePullRequest" | "reopenPullRequest" => {
                let closing = field.name == "closePullRequest";
                let pull = pull_state(context, &input, closing).await?;
                let typename = if closing {
                    "ClosePullRequestPayload"
                } else {
                    "ReopenPullRequestPayload"
                };
                (typename, "pullRequest", Arc::new(pull))
            }
            "mergePullRequest" => (
                "MergePullRequestPayload",
                "pullRequest",
                Arc::new(merge_pull(context, &input).await?),
            ),
            "addPullRequestReview" => (
                "AddPullRequestReviewPayload",
                "pullRequestReview",
                Arc::new(add_review(context, &input).await?),
            ),
            "markPullRequestReadyForReview" | "convertPullRequestToDraft" => {
                let draft = field.name == "convertPullRequestToDraft";
                let pull = readiness(context, &input, draft).await?;
                let typename = if draft {
                    "ConvertPullRequestToDraftPayload"
                } else {
                    "MarkPullRequestReadyForReviewPayload"
                };
                (typename, "pullRequest", Arc::new(pull))
            }
            "addLabelsToLabelable" | "removeLabelsFromLabelable" => {
                let adding = field.name == "addLabelsToLabelable";
                let typename = if adding {
                    "AddLabelsToLabelablePayload"
                } else {
                    "RemoveLabelsFromLabelablePayload"
                };
                (
                    typename,
                    "labelable",
                    change_labels(context, &input, adding).await?,
                )
            }
            _ => return Ok(R::null()),
        };
        Ok(R::object(Payload {
            typename: typename.to_owned(),
            client,
            fields: vec![(key, object)],
        }))
    }
}

struct Payload {
    typename: String,
    client: Value,
    fields: Vec<(&'static str, Arc<dyn Object<Context>>)>,
}

#[async_trait]
impl Object<Context> for Payload {
    fn typename(&self) -> &str {
        &self.typename
    }

    async fn resolve(&self, _: &Context, field: &Field<'_>) -> Result<R> {
        if field.name == "clientMutationId" {
            return Ok(R::Leaf(self.client.clone()));
        }
        Ok(
            match self.fields.iter().find(|(name, _)| *name == field.name) {
                Some((_, object)) => R::Object(Box::new(super::Shared(Arc::clone(object)))),
                None => R::null(),
            },
        )
    }
}

fn string<'a>(input: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    input.get(name).and_then(Value::as_str)
}

fn required<'a>(input: &'a Map<String, Value>, name: &str) -> Result<&'a str> {
    string(input, name)
        .ok_or_else(|| FieldError::unprocessable(format!("Argument '{name}' is required")))
}

fn missing(id: &str) -> FieldError {
    FieldError::not_found(format!(
        "Could not resolve to a node with the global id of '{id}'"
    ))
}

/// The repository and discussion a node ID names, loaded for the caller.
async fn subject(context: &Context, id: &str) -> Result<(Arc<Repository>, Discussion, u64)> {
    let (repository, discussion, number) = match NodeId::decode(id) {
        Some(NodeId::Issue(repository, number)) => (repository, Discussion::Issue, number),
        Some(NodeId::Pull(repository, number)) => (repository, Discussion::Pull, number),
        _ => return Err(missing(id)),
    };
    let repository = Repository::by_id(context, repository)
        .await?
        .ok_or_else(|| missing(id))?;
    Ok((repository, discussion, number))
}

async fn repository(context: &Context, id: &str) -> Result<Arc<Repository>> {
    match NodeId::decode(id) {
        Some(NodeId::Repository(repository)) => Repository::by_id(context, repository)
            .await?
            .ok_or_else(|| missing(id)),
        _ => Err(missing(id)),
    }
}

/// Labels must belong to the target repository; IDs from another one are
/// rejected instead of silently matching a same-numbered label.
fn label_numbers(
    input: &Map<String, Value>,
    name: &str,
    repository: Uuid,
) -> Result<Option<Vec<u64>>> {
    let Some(ids) = input.get(name).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    ids.as_array()
        .into_iter()
        .flatten()
        .map(|id| match id.as_str().and_then(NodeId::decode) {
            Some(NodeId::Label(owner, number)) if owner == repository => Ok(number),
            _ => Err(missing(id.as_str().unwrap_or_default())),
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

fn actor_ids(input: &Map<String, Value>) -> Result<Option<Vec<Uuid>>> {
    let Some(ids) = input.get("assigneeIds").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    ids.as_array()
        .into_iter()
        .flatten()
        .map(|id| match id.as_str().and_then(NodeId::decode) {
            Some(NodeId::User(actor)) => Ok(actor),
            _ => Err(missing(id.as_str().unwrap_or_default())),
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

fn milestone_number(input: &Map<String, Value>, repository: Uuid) -> Result<Option<Option<u64>>> {
    match input.get("milestoneId") {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(id) => match id.as_str().and_then(NodeId::decode) {
            Some(NodeId::Milestone(owner, number)) if owner == repository => Ok(Some(Some(number))),
            _ => Err(missing(id.as_str().unwrap_or_default())),
        },
    }
}

fn reject_projects(input: &Map<String, Value>) -> Result<()> {
    if input
        .get("projectIds")
        .and_then(Value::as_array)
        .is_some_and(|ids| !ids.is_empty())
    {
        return Err(FieldError::unprocessable(
            "Crab projects are managed in Crab; project IDs are not accepted here",
        ));
    }
    Ok(())
}

async fn send(
    context: &Context,
    method: Method,
    repository: &Repository,
    tail: String,
    body: Value,
) -> std::result::Result<Value, Failure> {
    context
        .native
        .send(
            method,
            &repository.api(&tail),
            Some(repository.id),
            Some(&body),
        )
        .await
}

/// Runs a versioned edit, rereading the version once if a concurrent edit
/// committed first. Other failures report immediately.
async fn versioned<F, Fut>(
    context: &Context,
    repository: &Repository,
    read: &str,
    mut edit: F,
) -> Result<Value>
where
    F: FnMut(u64) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<Value, Failure>>,
{
    for attempt in 0..2 {
        let current = context.native.get(&repository.api(read)).await?;
        let version = current["version"]
            .as_u64()
            .ok_or_else(|| FieldError::new("The record has no version"))?;
        match edit(version).await {
            Err(failure) if failure.status == StatusCode::CONFLICT && attempt == 0 => continue,
            result => return Ok(result?),
        }
    }
    Err(FieldError::unprocessable(
        "The record changed concurrently; retry",
    ))
}

async fn add_comment(context: &Context, input: &Map<String, Value>, client: Value) -> Result<R> {
    let id = required(input, "subjectId")?;
    let body = required(input, "body")?;
    let (repository, discussion, number) = subject(context, id).await?;
    let segment = match discussion {
        Discussion::Issue => "issues",
        Discussion::Pull => "pulls",
    };
    let value = send(
        context,
        Method::POST,
        &repository,
        format!("{segment}/{number}/comments"),
        json!({"request_id": Uuid::now_v7(), "body": body}),
    )
    .await?;
    let comment: Arc<dyn Object<Context>> = Arc::new(IssueComment {
        repository: Arc::clone(&repository),
        discussion,
        number,
        value,
    });
    let subject: Arc<dyn Object<Context>> = match discussion {
        Discussion::Issue => Arc::new(
            Issue::find(context, &repository, number)
                .await?
                .ok_or_else(|| missing(id))?,
        ),
        Discussion::Pull => Arc::new(
            PullRequest::find(context, &repository, number)
                .await?
                .ok_or_else(|| missing(id))?,
        ),
    };
    let edge: Arc<dyn Object<Context>> = Arc::new(Payload {
        typename: "IssueCommentEdge".into(),
        client: Value::Null,
        fields: vec![("node", comment)],
    });
    Ok(R::object(EdgePayload {
        client,
        edge,
        subject,
    }))
}

/// `addComment` returns an edge whose cursor is a leaf, unlike other payloads.
struct EdgePayload {
    client: Value,
    edge: Arc<dyn Object<Context>>,
    subject: Arc<dyn Object<Context>>,
}

#[async_trait]
impl Object<Context> for EdgePayload {
    fn typename(&self) -> &str {
        "AddCommentPayload"
    }

    async fn resolve(&self, _: &Context, field: &Field<'_>) -> Result<R> {
        Ok(match field.name {
            "clientMutationId" => R::Leaf(self.client.clone()),
            "commentEdge" => R::Object(Box::new(CommentEdge(Arc::clone(&self.edge)))),
            "subject" => R::Object(Box::new(super::Shared(Arc::clone(&self.subject)))),
            _ => R::null(),
        })
    }
}

struct CommentEdge(Arc<dyn Object<Context>>);

#[async_trait]
impl Object<Context> for CommentEdge {
    fn typename(&self) -> &str {
        "IssueCommentEdge"
    }

    async fn resolve(&self, context: &Context, field: &Field<'_>) -> Result<R> {
        match field.name {
            "cursor" => Ok(R::leaf(super::offset_cursor(0))),
            _ => self.0.resolve(context, field).await,
        }
    }
}

async fn create_issue(context: &Context, input: &Map<String, Value>) -> Result<Issue> {
    let repository = repository(context, required(input, "repositoryId")?).await?;
    reject_projects(input)?;
    let labels = label_numbers(input, "labelIds", repository.id)?.unwrap_or_default();
    let assignees = actor_ids(input)?.unwrap_or_default();
    let milestone = milestone_number(input, repository.id)?.flatten();
    let value = send(
        context,
        Method::POST,
        &repository,
        "issues".to_owned(),
        json!({
            "request_id": Uuid::now_v7(),
            "title": required(input, "title")?,
            "body": string(input, "body").unwrap_or_default(),
            "metadata": {"label_ids": labels, "assignee_ids": assignees, "milestone": milestone},
        }),
    )
    .await?;
    Ok(Issue::new(&repository, value, true))
}

fn state_reason(input: &Map<String, Value>) -> Result<Option<&'static str>> {
    match string(input, "stateReason") {
        None => Ok(None),
        Some("COMPLETED") => Ok(Some("completed")),
        Some("NOT_PLANNED") => Ok(Some("not_planned")),
        Some(_) => Err(FieldError::unprocessable(
            "Crab marks duplicates separately; close as completed or not planned",
        )),
    }
}

async fn issue_state(
    context: &Context,
    input: &Map<String, Value>,
    closing: bool,
) -> Result<Issue> {
    let id = required(input, "issueId")?;
    let (repository, discussion, number) = subject(context, id).await?;
    if discussion != Discussion::Issue {
        return Err(missing(id));
    }
    let reason = if closing { state_reason(input)? } else { None };
    let state = if closing { "closed" } else { "open" };
    versioned(context, &repository, &format!("issues/{number}"), |version| {
        send(
            context,
            Method::POST,
            &repository,
            format!("issues/{number}/state"),
            json!({"request_id": Uuid::now_v7(), "version": version, "state": state, "reason": reason}),
        )
    })
    .await?;
    Issue::find(context, &repository, number)
        .await?
        .ok_or_else(|| missing(id))
}

async fn assign_milestone(
    context: &Context,
    repository: &Repository,
    segment: &str,
    number: u64,
    milestone: Option<u64>,
) -> Result<()> {
    versioned(
        context,
        repository,
        &format!("{segment}/{number}/milestone"),
        |version| {
            send(
                context,
                Method::PUT,
                repository,
                format!("{segment}/{number}/milestone"),
                json!({"version": version, "milestone": milestone}),
            )
        },
    )
    .await
    .map(drop)
}

async fn update_issue(context: &Context, input: &Map<String, Value>) -> Result<Issue> {
    let id = required(input, "id")?;
    let (repository, discussion, number) = subject(context, id).await?;
    if discussion != Discussion::Issue {
        return Err(missing(id));
    }
    reject_projects(input)?;
    let mut edit = Map::new();
    for (from, to) in [("title", "title"), ("body", "body")] {
        if let Some(value) = string(input, from) {
            edit.insert(to.into(), json!(value));
        }
    }
    if let Some(state) = string(input, "state") {
        edit.insert("state".into(), json!(state.to_ascii_lowercase()));
    }
    if let Some(labels) = label_numbers(input, "labelIds", repository.id)? {
        edit.insert("label_ids".into(), json!(labels));
    }
    if let Some(assignees) = actor_ids(input)? {
        edit.insert("assignee_ids".into(), json!(assignees));
    }
    if !edit.is_empty() {
        versioned(
            context,
            &repository,
            &format!("issues/{number}"),
            |version| {
                let mut body = edit.clone();
                body.insert("version".into(), json!(version));
                send(
                    context,
                    Method::PATCH,
                    &repository,
                    format!("issues/{number}"),
                    Value::Object(body),
                )
            },
        )
        .await?;
    }
    if let Some(milestone) = milestone_number(input, repository.id)? {
        assign_milestone(context, &repository, "issues", number, milestone).await?;
    }
    Issue::find(context, &repository, number)
        .await?
        .ok_or_else(|| missing(id))
}

/// Resolves GitHub's head reference: a branch here, `owner:branch` in a fork
/// owned by `owner`, or a branch of an explicit head repository.
async fn head(
    context: &Context,
    base: &Arc<Repository>,
    input: &Map<String, Value>,
) -> Result<(String, Option<Arc<Repository>>)> {
    let reference = required(input, "headRefName")?;
    if let Some(id) = string(input, "headRepositoryId") {
        let fork = repository(context, id).await?;
        let branch = reference
            .rsplit_once(':')
            .map_or(reference, |(_, branch)| branch);
        return Ok((branch.to_owned(), (fork.id != base.id).then_some(fork)));
    }
    let Some((owner, branch)) = reference.split_once(':') else {
        return Ok((reference.to_owned(), None));
    };
    if owner == base.owner {
        return Ok((branch.to_owned(), None));
    }
    let forks = context.native.get(&base.api("forks")).await?;
    let no_fork = || {
        FieldError::unprocessable(format!(
            "No fork of {}/{} is owned by {owner}",
            base.owner, base.name
        ))
    };
    let id = forks["repositories"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|fork| fork["owner"] == owner)
        .and_then(|fork| fork["id"].as_str())
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(no_fork)?;
    let fork = Repository::by_id(context, id).await?.ok_or_else(no_fork)?;
    Ok((branch.to_owned(), Some(fork)))
}

async fn create_pull(context: &Context, input: &Map<String, Value>) -> Result<PullRequest> {
    let repository = repository(context, required(input, "repositoryId")?).await?;
    let (branch, fork) = head(context, &repository, input).await?;
    let base_ref = format!("refs/heads/{}", required(input, "baseRefName")?);
    let head_ref = format!("refs/heads/{branch}");
    // Native fork pulls open only for the comparison their author reviewed.
    // The API author's view is the tips at request time, so bind those.
    let expected = match &fork {
        Some(fork) => {
            let tip = |found: Option<Ref>, name: &str| {
                found
                    .as_ref()
                    .and_then(Ref::oid)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        FieldError::unprocessable(format!("Branch {name} does not exist"))
                    })
            };
            Some(json!({
                "base_oid": tip(repository.find_ref(context, &base_ref).await?, &base_ref)?,
                "head_oid": tip(fork.find_ref(context, &head_ref).await?, &head_ref)?,
            }))
        }
        None => None,
    };
    let value = send(
        context,
        Method::POST,
        &repository,
        "pulls".to_owned(),
        json!({
            "request_id": Uuid::now_v7(),
            "title": required(input, "title")?,
            "body": string(input, "body").unwrap_or_default(),
            "base_ref": base_ref,
            "head_ref": head_ref,
            "head_repository": fork.as_ref().map(|fork| fork.id),
            "expected": expected,
            "draft": input.get("draft").and_then(Value::as_bool).unwrap_or(false),
        }),
    )
    .await?;
    let number = value["number"]
        .as_u64()
        .ok_or_else(|| FieldError::new("Created pull request has no number"))?;
    PullRequest::find(context, &repository, number)
        .await?
        .ok_or_else(|| FieldError::new("The created pull request is not readable"))
}

async fn pull_subject(
    context: &Context,
    input: &Map<String, Value>,
) -> Result<(Arc<Repository>, u64, String)> {
    let id = required(input, "pullRequestId")?.to_owned();
    let (repository, discussion, number) = subject(context, &id).await?;
    if discussion != Discussion::Pull {
        return Err(missing(&id));
    }
    Ok((repository, number, id))
}

async fn update_pull(context: &Context, input: &Map<String, Value>) -> Result<PullRequest> {
    let (repository, number, id) = pull_subject(context, input).await?;
    reject_projects(input)?;
    if input
        .get("baseRefName")
        .is_some_and(|value| !value.is_null())
    {
        return Err(FieldError::unprocessable(
            "Change the base branch in Crab; it binds the exact comparison commits",
        ));
    }
    let mut edit = Map::new();
    for name in ["title", "body"] {
        if let Some(value) = string(input, name) {
            edit.insert(name.into(), json!(value));
        }
    }
    if let Some(state) = string(input, "state") {
        edit.insert("state".into(), json!(state.to_ascii_lowercase()));
    }
    if let Some(labels) = label_numbers(input, "labelIds", repository.id)? {
        edit.insert("label_ids".into(), json!(labels));
    }
    if let Some(assignees) = actor_ids(input)? {
        edit.insert("assignee_ids".into(), json!(assignees));
    }
    if !edit.is_empty() {
        versioned(
            context,
            &repository,
            &format!("pulls/{number}"),
            |version| {
                let mut body = edit.clone();
                body.insert("version".into(), json!(version));
                send(
                    context,
                    Method::PATCH,
                    &repository,
                    format!("pulls/{number}"),
                    Value::Object(body),
                )
            },
        )
        .await?;
    }
    if let Some(milestone) = milestone_number(input, repository.id)? {
        assign_milestone(context, &repository, "pulls", number, milestone).await?;
    }
    PullRequest::find(context, &repository, number)
        .await?
        .ok_or_else(|| missing(&id))
}

async fn pull_state(
    context: &Context,
    input: &Map<String, Value>,
    closing: bool,
) -> Result<PullRequest> {
    let (repository, number, id) = pull_subject(context, input).await?;
    let state = if closing { "closed" } else { "open" };
    versioned(
        context,
        &repository,
        &format!("pulls/{number}"),
        |version| {
            send(
                context,
                Method::POST,
                &repository,
                format!("pulls/{number}/state"),
                json!({"request_id": Uuid::now_v7(), "version": version, "state": state}),
            )
        },
    )
    .await?;
    PullRequest::find(context, &repository, number)
        .await?
        .ok_or_else(|| missing(&id))
}

async fn merge_pull(context: &Context, input: &Map<String, Value>) -> Result<PullRequest> {
    let (repository, number, id) = pull_subject(context, input).await?;
    let method = match string(input, "mergeMethod").unwrap_or("MERGE") {
        "SQUASH" => "squash",
        "REBASE" => "rebase",
        "FAST_FORWARD" => "fast_forward",
        _ => "merge_commit",
    };
    let message = match (string(input, "commitHeadline"), string(input, "commitBody")) {
        (Some(headline), Some(body)) if !body.is_empty() => format!("{headline}\n\n{body}"),
        (Some(headline), _) => headline.to_owned(),
        (None, Some(body)) => body.to_owned(),
        (None, None) => String::new(),
    };
    let expected = string(input, "expectedHeadOid");
    let detail = context
        .native
        .get(&repository.api(&format!("pulls/{number}")))
        .await?;
    let head = text(&detail, "head_oid");
    // GitHub refuses a merge whose head moved since the client looked.
    if expected.is_some_and(|expected| expected != head) {
        return Err(FieldError::unprocessable(format!(
            "Head branch was modified. Review and try the merge again. Expected {}",
            expected.unwrap_or_default()
        )));
    }
    send(
        context,
        Method::POST,
        &repository,
        format!("pulls/{number}/merge"),
        json!({
            "request_id": Uuid::now_v7(),
            "version": detail["version"],
            "method": method,
            "base_oid": detail["base_oid"],
            "head_oid": head,
            "message": message,
        }),
    )
    .await?;
    PullRequest::find(context, &repository, number)
        .await?
        .ok_or_else(|| missing(&id))
}

async fn add_review(context: &Context, input: &Map<String, Value>) -> Result<Review> {
    let (repository, number, _) = pull_subject(context, input).await?;
    let state = match string(input, "event") {
        Some("APPROVE") => "approved",
        Some("REQUEST_CHANGES") => "changes_requested",
        Some("COMMENT") => "commented",
        _ => {
            return Err(FieldError::unprocessable(
                "Crab submits reviews at once; choose APPROVE, COMMENT, or REQUEST_CHANGES",
            ));
        }
    };
    // A native review covers exactly the commit its author inspected. GitHub
    // reviews the current head when no commit is named, so bind that head.
    let commit = match string(input, "commitOID") {
        Some(oid) => oid.to_owned(),
        None => {
            let detail = context
                .native
                .get(&repository.api(&format!("pulls/{number}")))
                .await?;
            text(&detail, "head_oid").to_owned()
        }
    };
    let review = send(
        context,
        Method::POST,
        &repository,
        format!("pulls/{number}/reviews"),
        json!({
            "request_id": Uuid::now_v7(),
            "body": string(input, "body").unwrap_or_default(),
            "state": state,
            "commit_oid": commit,
        }),
    )
    .await?;
    Ok(Review::new(&repository, number, review))
}

async fn readiness(
    context: &Context,
    input: &Map<String, Value>,
    draft: bool,
) -> Result<PullRequest> {
    let (repository, number, id) = pull_subject(context, input).await?;
    versioned(
        context,
        &repository,
        &format!("pulls/{number}"),
        |version| {
            send(
                context,
                Method::PUT,
                &repository,
                format!("pulls/{number}/readiness"),
                json!({"version": version, "draft": draft}),
            )
        },
    )
    .await?;
    PullRequest::find(context, &repository, number)
        .await?
        .ok_or_else(|| missing(&id))
}

async fn change_labels(
    context: &Context,
    input: &Map<String, Value>,
    adding: bool,
) -> Result<Arc<dyn Object<Context>>> {
    let id = required(input, "labelableId")?;
    let (repository, discussion, number) = subject(context, id).await?;
    let requested = label_numbers(input, "labelIds", repository.id)?.unwrap_or_default();
    let segment = match discussion {
        Discussion::Issue => "issues",
        Discussion::Pull => "pulls",
    };
    let path = format!("{segment}/{number}");
    let current = context.native.get(&repository.api(&path)).await?;
    let mut selected: Vec<u64> = current["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|label| label["id"].as_u64())
        .collect();
    if adding {
        selected.extend(
            requested
                .iter()
                .copied()
                .filter(|id| !selected.contains(id))
                .collect::<Vec<_>>(),
        );
    } else {
        selected.retain(|id| !requested.contains(id));
    }
    versioned(context, &repository, &path, |version| {
        send(
            context,
            Method::PATCH,
            &repository,
            path.clone(),
            json!({"version": version, "label_ids": selected}),
        )
    })
    .await?;
    Ok(match discussion {
        Discussion::Issue => Arc::new(
            Issue::find(context, &repository, number)
                .await?
                .ok_or_else(|| missing(id))?,
        ),
        Discussion::Pull => Arc::new(
            PullRequest::find(context, &repository, number)
                .await?
                .ok_or_else(|| missing(id))?,
        ),
    })
}

/// Creates a repository under the viewer or an organization, through the
/// same native creation routes as the browser, including their policy.
async fn create_repository(
    context: &Context,
    input: &Map<String, Value>,
) -> Result<Arc<Repository>> {
    let name = required(input, "name")?;
    let visibility = match required(input, "visibility")? {
        "PUBLIC" => "public",
        "PRIVATE" => "private",
        _ => {
            return Err(FieldError::unprocessable(
                "Crab repositories are public or private",
            ));
        }
    };
    if input.get("teamId").is_some_and(|value| !value.is_null())
        || input.get("template").is_some_and(|value| value == true)
        || string(input, "homepageUrl").is_some_and(|url| !url.is_empty())
    {
        return Err(FieldError::unprocessable(
            "Set a repository's teams, template flag, and homepage in Crab after creating it",
        ));
    }
    let organization = match string(input, "ownerId").map(|id| (id, NodeId::decode(id))) {
        None => None,
        Some((_, Some(NodeId::Organization(id)))) => Some(id),
        // The viewer's own node creates a personal repository.
        Some((id, Some(NodeId::User(actor)))) => {
            if context.viewer().await?.actor != Some(actor) {
                return Err(missing(id));
            }
            None
        }
        Some((id, _)) => return Err(missing(id)),
    };
    let path = match organization {
        Some(id) => format!("/api/organizations/{id}/repositories/creation"),
        None => "/api/repos/creation".to_owned(),
    };
    let body = json!({
        "request_id": Uuid::now_v7(),
        "name": name,
        "description": string(input, "description").unwrap_or_default(),
        "visibility": visibility,
    });
    let created = context
        .native
        .send(Method::POST, &path, None, Some(&body))
        .await?;
    Repository::find(context, text(&created, "owner"), text(&created, "name")).await
}
