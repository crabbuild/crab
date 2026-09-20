use super::storage;
use crate::{
    app::{Error, Result},
    auth::Identity,
    cells::repository::{
        CreatePullReviewReply, CreatePullReviewReplyInput, CreatePullReviewReplyOutcome,
        CreatePullReviewThread, CreatePullReviewThreadInput, CreatePullReviewThreadOutcome,
        GetPullReviewReply, GetPullReviewReplySubmission, GetPullReviewThread,
        GetPullReviewThreadSubmission, ListPullReviewReplies, ListPullReviewThreads,
        PullReviewReplyKey, PullReviewReplyListInput, PullReviewReplyRecord,
        PullReviewReplySubmissionKey, PullReviewThreadKey, PullReviewThreadListInput,
        PullReviewThreadRecord, PullReviewThreadSide, PullReviewThreadSubmissionKey,
        UpdatePullReviewReply, UpdatePullReviewReplyInput, UpdatePullReviewReplyOutcome,
        UpdatePullReviewThread, UpdatePullReviewThreadInput, UpdatePullReviewThreadOutcome,
    },
    server::{Repository, Server},
};
use crab_cell_runtime::{Committed, InvocationError, Observed};

#[derive(Clone, Debug)]
pub(super) struct PullReviewThread {
    pub number: u64,
    pub author: Identity,
    pub body: String,
    pub suggested_text: Option<String>,
    pub base_oid: String,
    pub head_oid: String,
    pub path: Vec<u8>,
    pub old_blob_oid: Option<String>,
    pub new_blob_oid: Option<String>,
    pub side: PullReviewThreadSide,
    pub start_line: u64,
    pub end_line: u64,
    pub resolved: bool,
    pub resolved_by: Option<Identity>,
    pub resolved_at: Option<u64>,
    pub version: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug)]
pub(super) struct PullReviewReply {
    pub number: u64,
    pub author: Identity,
    pub body: String,
    pub version: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

pub(super) struct NewPullReviewThread {
    pub author: Identity,
    pub request_id: String,
    pub body: String,
    pub suggested_text: Option<String>,
    pub base_oid: String,
    pub head_oid: String,
    pub path: Vec<u8>,
    pub old_blob_oid: Option<String>,
    pub new_blob_oid: Option<String>,
    pub side: PullReviewThreadSide,
    pub start_line: u64,
    pub end_line: u64,
}

pub(super) struct PullReviewThreadEdit {
    pub actor: Identity,
    pub can_resolve: bool,
    pub version: u64,
    pub body: Option<String>,
    pub suggested_text: Option<Option<String>>,
    pub resolved: Option<bool>,
}

pub(super) struct NewPullReviewReply {
    pub author: Identity,
    pub request_id: String,
    pub body: String,
}

pub(super) async fn thread(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    pull: u64,
    number: u64,
) -> Result<Option<PullReviewThread>> {
    let routed = storage::route(server, repo, actor, "repository.read").await?;
    query_output(
        routed
            .client
            .query::<GetPullReviewThread>(
                &routed.target,
                None,
                PullReviewThreadKey { pull, number },
            )
            .await,
    )
    .map(|value| value.map(from_thread))
}

pub(super) async fn threads(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    input: PullReviewThreadListInput,
) -> Result<Option<(Vec<PullReviewThread>, Option<u64>)>> {
    let routed = storage::route(server, repo, actor, "repository.read").await?;
    let page = query_output(
        routed
            .client
            .query::<ListPullReviewThreads>(&routed.target, None, input)
            .await,
    )?;
    Ok(page.map(|page| (page.items.into_iter().map(from_thread).collect(), page.next)))
}

pub(super) async fn thread_submission(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    pull: u64,
    request_id: &str,
) -> Result<Option<PullReviewThread>> {
    let routed = storage::route(server, repo, actor, "repository.read").await?;
    query_output(
        routed
            .client
            .query::<GetPullReviewThreadSubmission>(
                &routed.target,
                None,
                PullReviewThreadSubmissionKey {
                    pull: Some(pull),
                    submission_id: storage::submission_id(request_id)?,
                },
            )
            .await,
    )
    .map(|value| value.map(from_thread))
}

pub(super) async fn create_thread(
    server: &Server,
    repo: &Repository,
    pull: u64,
    input: NewPullReviewThread,
) -> Result<PullReviewThread> {
    let routed =
        storage::route(server, repo, &input.author, "repository.pull.review.thread").await?;
    match command_output(
        routed
            .client
            .command::<CreatePullReviewThread>(
                &routed.target,
                storage::mutation_identity()?,
                CreatePullReviewThreadInput {
                    pull,
                    submission_id: storage::submission_id(&input.request_id)?,
                    author: storage::to_author(&input.author),
                    body: input.body,
                    suggested_text: input.suggested_text,
                    base_oid: input.base_oid,
                    head_oid: input.head_oid,
                    path: input.path,
                    old_blob_oid: input.old_blob_oid,
                    new_blob_oid: input.new_blob_oid,
                    side: input.side,
                    start_line: input.start_line,
                    end_line: input.end_line,
                },
            )
            .await,
    )? {
        CreatePullReviewThreadOutcome::Created(thread) => Ok(from_thread(*thread)),
        CreatePullReviewThreadOutcome::PullNotFound => Err(Error::NotFound),
        CreatePullReviewThreadOutcome::PullClosed => {
            Err(Error::Invalid("Closed pull requests cannot be reviewed"))
        }
        CreatePullReviewThreadOutcome::RequestConflict => Err(Error::RequestConflict),
    }
}

pub(super) async fn update_thread(
    server: &Server,
    repo: &Repository,
    pull: u64,
    number: u64,
    input: PullReviewThreadEdit,
) -> Result<PullReviewThread> {
    let routed =
        storage::route(server, repo, &input.actor, "repository.pull.review.thread").await?;
    match command_output(
        routed
            .client
            .command::<UpdatePullReviewThread>(
                &routed.target,
                storage::mutation_identity()?,
                UpdatePullReviewThreadInput {
                    key: PullReviewThreadKey { pull, number },
                    actor: storage::to_author(&input.actor),
                    can_resolve: input.can_resolve,
                    version: input.version,
                    body: input.body,
                    suggested_text: input.suggested_text,
                    resolved: input.resolved,
                },
            )
            .await,
    )? {
        UpdatePullReviewThreadOutcome::Updated(thread) => Ok(from_thread(*thread)),
        UpdatePullReviewThreadOutcome::NotFound => Err(Error::NotFound),
        UpdatePullReviewThreadOutcome::Forbidden => Err(Error::Forbidden),
        UpdatePullReviewThreadOutcome::Conflict => Err(Error::Conflict),
    }
}

pub(super) async fn reply(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    pull: u64,
    thread: u64,
    number: u64,
) -> Result<Option<PullReviewReply>> {
    let routed = storage::route(server, repo, actor, "repository.read").await?;
    query_output(
        routed
            .client
            .query::<GetPullReviewReply>(
                &routed.target,
                None,
                PullReviewReplyKey {
                    pull,
                    thread,
                    number,
                },
            )
            .await,
    )
    .map(|value| value.map(from_reply))
}

pub(super) async fn replies(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    input: PullReviewReplyListInput,
) -> Result<Option<(Vec<PullReviewReply>, Option<u64>)>> {
    let routed = storage::route(server, repo, actor, "repository.read").await?;
    let page = query_output(
        routed
            .client
            .query::<ListPullReviewReplies>(&routed.target, None, input)
            .await,
    )?;
    Ok(page.map(|page| (page.items.into_iter().map(from_reply).collect(), page.next)))
}

pub(super) async fn reply_submission(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    pull: u64,
    thread: u64,
    request_id: &str,
) -> Result<Option<PullReviewReply>> {
    let routed = storage::route(server, repo, actor, "repository.read").await?;
    query_output(
        routed
            .client
            .query::<GetPullReviewReplySubmission>(
                &routed.target,
                None,
                PullReviewReplySubmissionKey {
                    pull: Some(pull),
                    thread: Some(thread),
                    submission_id: storage::submission_id(request_id)?,
                },
            )
            .await,
    )
    .map(|value| value.map(from_reply))
}

pub(super) async fn create_reply(
    server: &Server,
    repo: &Repository,
    pull: u64,
    thread: u64,
    input: NewPullReviewReply,
) -> Result<PullReviewReply> {
    let routed =
        storage::route(server, repo, &input.author, "repository.pull.review.thread").await?;
    match command_output(
        routed
            .client
            .command::<CreatePullReviewReply>(
                &routed.target,
                storage::mutation_identity()?,
                CreatePullReviewReplyInput {
                    pull,
                    thread,
                    submission_id: storage::submission_id(&input.request_id)?,
                    author: storage::to_author(&input.author),
                    body: input.body,
                },
            )
            .await,
    )? {
        CreatePullReviewReplyOutcome::Created(reply) => Ok(from_reply(reply)),
        CreatePullReviewReplyOutcome::ThreadNotFound => Err(Error::NotFound),
        CreatePullReviewReplyOutcome::RequestConflict => Err(Error::RequestConflict),
    }
}

pub(super) async fn update_reply(
    server: &Server,
    repo: &Repository,
    pull: u64,
    thread: u64,
    number: u64,
    actor: Identity,
    version: u64,
    body: String,
) -> Result<PullReviewReply> {
    let routed = storage::route(server, repo, &actor, "repository.pull.review.thread").await?;
    match command_output(
        routed
            .client
            .command::<UpdatePullReviewReply>(
                &routed.target,
                storage::mutation_identity()?,
                UpdatePullReviewReplyInput {
                    key: PullReviewReplyKey {
                        pull,
                        thread,
                        number,
                    },
                    actor: storage::to_author(&actor),
                    version,
                    body,
                },
            )
            .await,
    )? {
        UpdatePullReviewReplyOutcome::Updated(reply) => Ok(from_reply(reply)),
        UpdatePullReviewReplyOutcome::NotFound => Err(Error::NotFound),
        UpdatePullReviewReplyOutcome::Forbidden => Err(Error::Forbidden),
        UpdatePullReviewReplyOutcome::Conflict => Err(Error::Conflict),
    }
}

fn from_thread(value: PullReviewThreadRecord) -> PullReviewThread {
    PullReviewThread {
        number: value.number,
        author: storage::from_author(value.author),
        body: value.body,
        suggested_text: value.suggested_text,
        base_oid: value.base_oid,
        head_oid: value.head_oid,
        path: value.path,
        old_blob_oid: value.old_blob_oid,
        new_blob_oid: value.new_blob_oid,
        side: value.side,
        start_line: value.start_line,
        end_line: value.end_line,
        resolved: value.resolved,
        resolved_by: value.resolved_by.map(storage::from_author),
        resolved_at: value.resolved_at_ms,
        version: value.version,
        created_at: value.created_at_ms,
        updated_at: value.updated_at_ms,
    }
}

fn from_reply(value: PullReviewReplyRecord) -> PullReviewReply {
    PullReviewReply {
        number: value.number,
        author: storage::from_author(value.author),
        body: value.body,
        version: value.version,
        created_at: value.created_at_ms,
        updated_at: value.updated_at_ms,
    }
}

fn command_output<T>(result: std::result::Result<Committed<T>, InvocationError<T>>) -> Result<T> {
    storage::command_output(result)
}

fn query_output<T>(result: std::result::Result<Observed<T>, InvocationError<T>>) -> Result<T> {
    storage::query_output(result)
}
