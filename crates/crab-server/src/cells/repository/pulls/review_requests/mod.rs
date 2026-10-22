use super::*;
use crate::cells::repository::collaboration::{
    Target,
    activity::{self, Action},
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum State {
    Requested,
    Cancelled,
    Fulfilled { review: u64 },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub reviewer: RepositoryAuthor,
    pub requested_by: RepositoryAuthor,
    pub state: State,
    pub requested_at: u64,
    pub updated_at: u64,
}

fn prefix(pull: u64) -> String {
    format!("collaboration/v1/pulls/{pull:016x}/review-requests/")
}

pub(crate) fn reviewer_id(reviewer: &RepositoryAuthor) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(b"crab.pull-reviewer.v1\0");
    hash_text(&mut hash, &reviewer.issuer);
    hash_text(&mut hash, &reviewer.subject);
    hash.finalize().to_hex().to_string()
}

fn validate_id(id: &str) -> crate::cells::Result<()> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(crate::cells::Error::Invalid("review request ID is invalid"));
    }
    Ok(())
}

fn validate(request: &Request, id: &str) -> crate::cells::Result<()> {
    validate_author(&request.reviewer)?;
    validate_author(&request.requested_by)?;
    if reviewer_id(&request.reviewer) != id {
        return Err(crate::cells::Error::Invalid(
            "review request does not match its key",
        ));
    }
    if let State::Fulfilled { review } = request.state {
        validate_number(review)?;
    }
    Ok(())
}

pub(super) fn load(
    context: &Transaction<'_>,
    pull: u64,
    id: &str,
) -> crate::cells::Result<Option<Request>> {
    validate_number(pull)?;
    validate_id(id)?;
    let request = context.document::<Request>(&format!("{}{id}", prefix(pull)))?;
    if let Some(request) = &request {
        validate(request, id)?;
    }
    Ok(request)
}

fn store(context: &Transaction<'_>, pull: u64, request: &Request) -> crate::cells::Result<()> {
    context.put_document(
        &format!("{}{}", prefix(pull), reviewer_id(&request.reviewer)),
        request,
    )
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    pub pull: u64,
    pub version: u64,
    pub actor: RepositoryAuthor,
    pub can_manage: bool,
    pub reviewer: RepositoryAuthor,
    pub requested: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum Outcome {
    Applied(Request),
    Archived,
    NotFound,
    Forbidden,
    OwnReview,
    Closed,
    MergePending,
    Conflict,
}

pub(crate) struct SetReviewRequest;
impl Mutation for SetReviewRequest {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 250;
    type Input = Selection;
    type Output = Outcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: Selection,
    ) -> crate::cells::Result<MutationResult<Outcome>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Outcome::Archived));
        }
        validate_number(input.pull)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        validate_author(&input.reviewer)?;
        let Some(mut pull) = load_pull(context, input.pull)? else {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        };
        if !input.can_manage {
            return Ok(MutationResult::Rejected(Outcome::Forbidden));
        }
        if same_author(&input.reviewer, &pull.author) {
            return Ok(MutationResult::Rejected(Outcome::OwnReview));
        }
        if pull.state != PullState::Open {
            return Ok(MutationResult::Rejected(Outcome::Closed));
        }
        if pull.merge_pending.is_some() {
            return Ok(MutationResult::Rejected(Outcome::MergePending));
        }
        let existing = load(context, pull.number, &reviewer_id(&input.reviewer))?;
        if let Some(request) = &existing {
            if (request.state == State::Requested) == input.requested {
                return Ok(MutationResult::Success(Outcome::Applied(request.clone())));
            }
        } else if !input.requested {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        }
        if pull.version != input.version {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        let now = timestamp(context.now_ms())?;
        let request = if input.requested {
            Request {
                reviewer: input.reviewer,
                requested_by: input.actor.clone(),
                state: State::Requested,
                requested_at: now,
                updated_at: now,
            }
        } else {
            let Some(mut request) = existing else {
                return Ok(MutationResult::Rejected(Outcome::NotFound));
            };
            request.state = State::Cancelled;
            request.updated_at = now;
            request
        };
        super::super::search::index::ensure_ready(context)?;
        store(context, pull.number, &request)?;
        super::super::search::reviews::requested(
            context,
            pull.number,
            &request.reviewer,
            request.state == State::Requested,
        )?;
        let old_version = pull.version;
        pull.version = next_version(pull.version)?;
        pull.updated_at_ms = now;
        update_pull_row(context, &pull, old_version)?;
        let action = if input.requested {
            Action::ReviewRequested {
                reviewer: request.reviewer.name.clone(),
            }
        } else {
            Action::ReviewRequestCancelled {
                reviewer: request.reviewer.name.clone(),
            }
        };
        activity::append(
            context,
            Target::Pull {
                number: pull.number,
            },
            &input.actor,
            action,
        )?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Outcome::Applied(request)))
    }
}

// Called only for a newly created review, after submission replay is resolved.
// Keeping this inside review creation prevents a retry fulfilling a newer request.
pub(super) fn fulfill(
    context: &Transaction<'_>,
    review: &PullReviewRecord,
) -> crate::cells::Result<bool> {
    let Some(mut request) = load(context, review.pull, &reviewer_id(&review.author))? else {
        return Ok(false);
    };
    if request.state != State::Requested {
        return Ok(false);
    }
    request.state = State::Fulfilled {
        review: review.number,
    };
    request.updated_at = review.created_at_ms;
    store(context, review.pull, &request)?;
    Ok(true)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Key {
    pub pull: u64,
    pub id: String,
}

pub(crate) struct GetReviewRequest;
impl Read for GetReviewRequest {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 250;
    type Input = Key;
    type Output = Option<Request>;
    fn execute(context: &mut Transaction<'_>, key: Key) -> crate::cells::Result<Self::Output> {
        load(context, key.pull, &key.id)
    }
}

pub(in crate::cells::repository) struct IndexPage {
    pub items: Vec<(u64, Request)>,
    pub next: Option<String>,
}

// Rebuild advances over at most eight source documents, including unrelated
// pull metadata. Filtering before advancing could turn sparse requests into an
// unbounded walk. The original qualified request remains canonical.
pub(in crate::cells::repository) fn index_page(
    context: &Transaction<'_>,
    before: Option<&str>,
) -> crate::cells::Result<IndexPage> {
    let prefix = "collaboration/v1/pulls/";
    if before.is_some_and(|key| !key.starts_with(prefix)) {
        return Err(crate::cells::Error::Invalid(
            "Review request source cursor is invalid",
        ));
    }
    let rows = context.documents::<serde_json::Value>(prefix, before, 8)?;
    let next = (rows.len() == 8)
        .then(|| rows.last().map(|(key, _)| key.clone()))
        .flatten();
    let mut requests = Vec::new();
    for (key, value) in rows {
        let Some((pull, rest)) = key.strip_prefix(prefix).and_then(|key| key.split_once('/'))
        else {
            continue;
        };
        let Some(id) = rest.strip_prefix("review-requests/") else {
            continue;
        };
        let pull = u64::from_str_radix(pull, 16).map_err(crate::cells::Error::sdk)?;
        validate_number(pull)?;
        let request: Request = serde_json::from_value(value)?;
        validate(&request, id)?;
        requests.push((pull, request));
    }
    Ok(IndexPage {
        items: requests,
        next,
    })
}

#[cfg(test)]
mod tests;
