use super::*;
use crate::cells::{Error, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const MAX_COMMENTS: usize = 20;
const MAX_DRAFT_BYTES: usize = 96 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Key {
    pub pull: u64,
    pub actor: RepositoryAuthor,
}

impl Key {
    fn path(&self) -> Result<String> {
        validate_number(self.pull)?;
        validate_author(&self.actor)?;
        let mut account = blake3::Hasher::new_derive_key("crab pending review account v1");
        hash_text(&mut account, &self.actor.issuer);
        hash_text(&mut account, &self.actor.subject);
        Ok(format!(
            "collaboration/v1/pulls/{:016x}/pending-reviews/{}",
            self.pull,
            account.finalize().to_hex()
        ))
    }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Draft {
    pub version: Option<Uuid>,
    pub comments: Vec<review_threads::CreatePullReviewThreadInput>,
    digest: Option<String>,
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 402;
    type Input = Key;
    type Output = Option<Draft>;

    fn execute(context: &mut Transaction<'_>, key: Key) -> Result<Self::Output> {
        let path = key.path()?;
        if load_pull_query(context, key.pull)?.is_none() {
            return Ok(None);
        }
        Ok(Some(context.document(&path)?.unwrap_or_default()))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Edit {
    Save {
        comment: Box<review_threads::CreatePullReviewThreadInput>,
        replace: Option<Uuid>,
    },
    Remove(Uuid),
    Discard,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditInput {
    pub key: Key,
    pub expected: Option<Uuid>,
    pub request_id: Uuid,
    pub edit: Edit,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Edited {
    Saved(Draft),
    NotFound,
    Archived,
    Conflict,
    AtCapacity,
    Closed,
    MergePending,
}

pub(crate) struct Change;
impl Mutation for Change {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 402;
    type Input = EditInput;
    type Output = Edited;

    fn execute(context: &mut Transaction<'_>, input: EditInput) -> Result<MutationResult<Edited>> {
        use Edited as Outcome;
        let path = input.key.path()?;
        if input.request_id.is_nil() || input.expected == Some(input.request_id) {
            return Err(Error::Invalid("invalid pending review submission ID"));
        }
        let digest = blake3::hash(&serde_json::to_vec(&(&input.expected, &input.edit))?)
            .to_hex()
            .to_string();
        let mut draft: Draft = context.document(&path)?.unwrap_or_default();
        if draft.version == Some(input.request_id) && draft.digest.as_deref() == Some(&digest) {
            return Ok(MutationResult::Success(Outcome::Saved(draft)));
        }
        if draft.version != input.expected {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Outcome::Archived));
        }
        let Some(pull) = load_pull(context, input.key.pull)? else {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        };
        match input.edit {
            Edit::Save {
                mut comment,
                replace,
            } => {
                if pull.state != PullState::Open {
                    return Ok(MutationResult::Rejected(Outcome::Closed));
                }
                if pull.merge_pending.is_some() {
                    return Ok(MutationResult::Rejected(Outcome::MergePending));
                }
                if comment.pull != input.key.pull || !same_author(&comment.author, &input.key.actor)
                {
                    return Err(Error::Invalid(
                        "pending comment target disagrees with the actor or pull",
                    ));
                }
                if draft.comments.first().is_some_and(|first| {
                    first.base_oid != comment.base_oid || first.head_oid != comment.head_oid
                }) {
                    return Ok(MutationResult::Rejected(Outcome::Conflict));
                }
                if let Some(id) = replace {
                    let Some(previous) = draft
                        .comments
                        .iter_mut()
                        .find(|record| record.submission_id == *id.as_bytes())
                    else {
                        return Ok(MutationResult::Rejected(Outcome::Conflict));
                    };
                    comment.submission_id = previous.submission_id;
                    *previous = *comment;
                } else {
                    if draft.comments.len() >= MAX_COMMENTS {
                        return Ok(MutationResult::Rejected(Outcome::AtCapacity));
                    }
                    if draft
                        .comments
                        .iter()
                        .any(|record| record.submission_id == *input.request_id.as_bytes())
                    {
                        return Ok(MutationResult::Rejected(Outcome::Conflict));
                    }
                    comment.submission_id = *input.request_id.as_bytes();
                    draft.comments.push(*comment);
                }
            }
            Edit::Remove(id) => draft
                .comments
                .retain(|comment| comment.submission_id != *id.as_bytes()),
            Edit::Discard => draft.comments.clear(),
        }
        draft.version = Some(input.request_id);
        draft.digest = Some(digest);
        if serde_json::to_vec(&draft)?.len() > MAX_DRAFT_BYTES {
            return Ok(MutationResult::Rejected(Outcome::AtCapacity));
        }
        // Retain the version after removing the last comment. A delayed retry
        // from before a discard must never recreate that draft.
        context.put_document(&path, &draft)?;
        Ok(MutationResult::Success(Outcome::Saved(draft)))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmitInput {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub can_post_locked: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_teams: Vec<Uuid>,
    pub key: Key,
    pub expected: Uuid,
    pub request_id: Uuid,
    pub state: ReviewState,
    pub body: String,
    pub comparison: Option<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_comparison: Option<ReviewComparison>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<u64>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Published {
    pub review: u64,
    pub threads: Vec<u64>,
}

#[derive(Deserialize, Serialize)]
struct Receipt {
    digest: String,
    result: Published,
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Submitted {
    Published(Published),
    Conflict,
    ComparisonUnavailable,
    ReviewRejected(CreatePullReviewOutcome),
    RequestConflict,
}

pub(crate) struct Submit;
impl Mutation for Submit {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 403;
    type Input = SubmitInput;
    type Output = Submitted;

    fn execute(
        context: &mut Transaction<'_>,
        input: SubmitInput,
    ) -> Result<MutationResult<Submitted>> {
        use Submitted as Outcome;
        let path = input.key.path()?;
        if input.request_id.is_nil() || input.request_id == input.expected {
            return Err(Error::Invalid("invalid pending review submission ID"));
        }
        let receipt = format!("{path}/submissions/{}", input.request_id);
        let fields = (input.expected, input.state, &input.body);
        let content = if let Some(revision) = input.base_revision {
            serde_json::to_vec(&(fields, revision))?
        } else {
            serde_json::to_vec(&fields)?
        };
        let digest = blake3::hash(&content).to_hex().to_string();
        if let Some(saved) = context.document::<Receipt>(&receipt)? {
            return Ok(if saved.digest == digest {
                MutationResult::Success(Outcome::Published(saved.result))
            } else {
                MutationResult::Rejected(Outcome::RequestConflict)
            });
        }
        let draft: Draft = context.document(&path)?.unwrap_or_default();
        let Some(first) = draft
            .comments
            .first()
            .filter(|_| draft.version == Some(input.expected))
        else {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        };
        let Some((base, head)) = input.comparison else {
            return Ok(MutationResult::Rejected(Outcome::ComparisonUnavailable));
        };
        if base != first.base_oid || head != first.head_oid {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        let review = CreatePullReview::write(
            context,
            CreatePullReviewInput {
                can_post_locked: input.can_post_locked,
                review_teams: input.review_teams,
                pull: input.key.pull,
                submission_id: child_id(input.request_id, input.expected),
                author: input.key.actor.clone(),
                body: input.body,
                state: input.state,
                commit_oid: head,
                comparison: input.approved_comparison,
                base_revision: input.base_revision,
            },
            true,
        )?;
        let review = match review {
            MutationResult::Success(CreatePullReviewOutcome::Created(review)) => review.number,
            MutationResult::Rejected(reason) => {
                return Ok(MutationResult::Rejected(Outcome::ReviewRejected(reason)));
            }
            _ => return Err(Error::Invalid("unexpected pending review result")),
        };
        let mut threads = Vec::with_capacity(draft.comments.len());
        for mut comment in draft.comments {
            comment.can_post_locked = input.can_post_locked;
            comment.submission_id =
                child_id(input.request_id, Uuid::from_bytes(comment.submission_id));
            comment.author = input.key.actor.clone();
            match review_threads::CreatePullReviewThread::execute(context, comment)? {
                MutationResult::Success(
                    review_threads::CreatePullReviewThreadOutcome::Created(thread),
                ) => threads.push(thread.number),
                _ => {
                    return Err(Error::Invalid(
                        "one of the pending review threads could not be published",
                    ));
                }
            }
        }
        // The review, threads, activities and receipt share this transaction.
        // A failed thread rolls back the review and any earlier threads.
        let saved = Receipt {
            digest,
            result: Published { review, threads },
        };
        context.put_document(&receipt, &saved)?;
        context.put_document(
            &path,
            &Draft {
                version: Some(input.request_id),
                ..Draft::default()
            },
        )?;
        Ok(MutationResult::Success(Outcome::Published(saved.result)))
    }
}

fn child_id(batch: Uuid, child: Uuid) -> [u8; 16] {
    let mut digest = blake3::Hasher::new_derive_key("crab review batch child v1");
    digest.update(batch.as_bytes());
    digest.update(child.as_bytes());
    let mut id = [0; 16];
    id.copy_from_slice(&digest.finalize().as_bytes()[..16]);
    id
}
