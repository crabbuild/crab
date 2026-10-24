use super::*;
use crate::cells::repository::collaboration::lifecycle::{
    LifecycleStatus, PrepareLifecycleInput, PrepareLifecycleOutcome,
};

const SCHEMA: &str = "repository/pulls/reviews/dismissals/format";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Dismissal {
    pub author: RepositoryAuthor,
    pub body: String,
    pub version: u64,
    pub created_at_ms: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Detail {
    pub review: PullReviewRecord,
    pub dismissal: Option<Dismissal>,
    pub dismissible: bool,
    pub base_ref: String,
    // None uses configured initial policy. Some(false) is an administrative
    // override and must not restore that seed after protections are cleared.
    pub admin_required: Option<bool>,
}
fn ready(context: &Transaction<'_>) -> crate::cells::Result<LifecycleStatus> {
    match context.document::<u8>(SCHEMA)? {
        None => Ok(LifecycleStatus::Outdated),
        Some(1) => Ok(LifecycleStatus::Ready),
        _ => Err(crate::cells::Error::Invalid(
            "unsupported review lifecycle format",
        )),
    }
}
fn ensure_ready(context: &Transaction<'_>) -> crate::cells::Result<()> {
    if ready(context)? != LifecycleStatus::Ready {
        return Err(crate::cells::Error::SchemaUnavailable);
    }
    Ok(())
}
pub(in crate::cells::repository) fn initialize(
    context: &Transaction<'_>,
) -> crate::cells::Result<()> {
    context.install_schema(include_str!("schema.sql"))?;
    context.put_document(SCHEMA, &1_u8)
}
pub(crate) struct GetReviewState;
impl Read for GetReviewState {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 326;
    type Input = ();
    type Output = LifecycleStatus;
    fn execute(context: &mut Transaction<'_>, (): ()) -> crate::cells::Result<Self::Output> {
        ready(context)
    }
}
pub(crate) struct PrepareReviewState;
impl Mutation for PrepareReviewState {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 326;
    type Input = PrepareLifecycleInput;
    type Output = PrepareLifecycleOutcome;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if input.version != 0 {
            return Ok(MutationResult::Rejected(PrepareLifecycleOutcome::Conflict(
                ready(context)?,
            )));
        }
        if ready(context)? == LifecycleStatus::Outdated {
            initialize(context)?;
            advance_revision(context)?;
        }
        Ok(MutationResult::Success(PrepareLifecycleOutcome::Applied(
            LifecycleStatus::Ready,
        )))
    }
}
fn protected(
    context: &mut Transaction<'_>,
    pull: &PullRecord,
) -> crate::cells::Result<Option<bool>> {
    let settings = GetBranchProtections::execute(context, ())?;
    Ok(settings.map(|settings| {
        settings
            .rules
            .iter()
            .any(|rule| pull.base_ref.strip_prefix("refs/heads/") == Some(rule.branch.as_str()))
    }))
}
pub(in crate::cells::repository) fn load(
    context: &Transaction<'_>,
    key: PullChildKey,
) -> crate::cells::Result<Option<Dismissal>> {
    ensure_ready(context)?;
    let result = context.query("SELECT actor_issuer,actor_subject,actor_name,body,version,created_at_ms FROM repository_pull_review_dismissals WHERE pull_number=? AND review_number=?", &[integer(key.pull)?, integer(key.number)?])?;
    result
        .rows
        .first()
        .map(|row| {
            let value = Dismissal {
                author: author_from_row(row, 0)?,
                body: result_text(row, 3)?,
                version: result_u64_from_row(row, 4)?,
                created_at_ms: result_u64_from_row(row, 5)?,
            };
            validate_author(&value.author)?;
            validate_body(&value.body, true)?;
            validate_number(value.version)?;
            Ok(value)
        })
        .transpose()
}
fn detail(
    context: &Transaction<'_>,
    review: PullReviewRecord,
    admin_required: Option<bool>,
    pull: &PullRecord,
) -> crate::cells::Result<Detail> {
    let dismissal = load(
        context,
        PullChildKey {
            pull: review.pull,
            number: review.number,
        },
    )?;
    if dismissal.as_ref().is_some_and(|value| {
        value.version > review.version
            || value.created_at_ms > review.updated_at_ms
            || review.state == ReviewState::Commented
    }) {
        return Err(crate::cells::Error::Invalid(
            "review dismissal does not match its source",
        ));
    }
    let dismissible = !settings::archived(context)?
        && pull.merge_pending.is_none()
        && review.state != ReviewState::Commented
        && dismissal.is_none();
    Ok(Detail {
        review,
        dismissal,
        dismissible,
        base_ref: pull.base_ref.clone(),
        admin_required,
    })
}
pub(crate) struct GetReviewDetail;
impl Read for GetReviewDetail {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 324;
    type Input = PullChildKey;
    type Output = Option<Detail>;
    fn execute(
        context: &mut Transaction<'_>,
        key: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        ensure_ready(context)?;
        let Some(review) = GetPullReview::execute(context, key.clone())? else {
            return Ok(None);
        };
        let pull = load_pull(context, key.pull)?
            .ok_or(crate::cells::Error::Invalid("review parent is missing"))?;
        let protected = protected(context, &pull)?;
        Ok(Some(detail(context, review, protected, &pull)?))
    }
}
pub(crate) struct ListReviewDetails;
impl Read for ListReviewDetails {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 325;
    type Input = PullChildListInput;
    type Output = Option<crate::cells::repository::collaboration::Page<Detail>>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        ensure_ready(context)?;
        let Some(pull) = load_pull(context, input.pull)? else {
            return Ok(None);
        };
        let protected = protected(context, &pull)?;
        let page = ListPullReviews::execute(context, input)?
            .ok_or(crate::cells::Error::Invalid("review parent is missing"))?;
        let mut items = Vec::<Detail>::new();
        let mut bytes = 4096;
        let mut next = page.next;
        for review in page.items {
            let item = detail(context, review, protected, &pull)?;
            let size = serde_json::to_vec(&item)?.len() + 1;
            if bytes + size > MAX_LIST_OUTPUT_BYTES {
                next = items.last().map(|item| item.review.number);
                break;
            }
            bytes += size;
            items.push(item);
        }
        Ok(Some(crate::cells::repository::collaboration::Page {
            items,
            next,
        }))
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub submission_id: [u8; 16],
    pub key: PullChildKey,
    pub version: u64,
    pub actor: RepositoryAuthor,
    pub body: String,
    pub can_write: bool,
    pub can_admin: bool,
    pub configured_protected: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum Outcome {
    Dismissed,
    NotFound,
    Forbidden,
    Archived,
    Conflict,
    RequestConflict,
    InvalidState,
    MergePending,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Submission {
    digest: String,
}
pub(crate) struct DismissReview;
impl Mutation for DismissReview {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 324;
    type Input = Input;
    type Output = Outcome;
    fn execute(
        context: &mut Transaction<'_>,
        input: Input,
    ) -> crate::cells::Result<MutationResult<Outcome>> {
        validate_number(input.key.pull)?;
        validate_number(input.key.number)?;
        validate_number(input.version)?;
        validate_author(&input.actor)?;
        validate_body(&input.body, true)?;
        let key = format!(
            "repository/pulls/review-dismissal-submissions/v1/{}",
            uuid::Uuid::from_bytes(input.submission_id)
        );
        let digest = blake3::hash(&serde_json::to_vec(&(
            &input.key,
            input.version,
            &input.actor.issuer,
            &input.actor.subject,
            &input.body,
        ))?)
        .to_hex()
        .to_string();
        if let Some(saved) = context.document::<Submission>(&key)? {
            return Ok(if saved.digest == digest {
                MutationResult::Success(Outcome::Dismissed)
            } else {
                MutationResult::Rejected(Outcome::RequestConflict)
            });
        }
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Outcome::Archived));
        }
        if !input.can_write {
            return Ok(MutationResult::Rejected(Outcome::Forbidden));
        }
        let Some(existing) = GetReviewDetail::execute(context, input.key.clone())? else {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        };
        // Configuration seeds policy only until the first administrative save.
        // Persisted rules in this transaction override a stale configured seed.
        if existing
            .admin_required
            .unwrap_or(input.configured_protected)
            && !input.can_admin
        {
            return Ok(MutationResult::Rejected(Outcome::Forbidden));
        }
        if existing.review.version != input.version {
            return Ok(MutationResult::Rejected(Outcome::Conflict));
        }
        let Some(pull) = load_pull(context, input.key.pull)? else {
            return Ok(MutationResult::Rejected(Outcome::NotFound));
        };
        if pull.merge_pending.is_some() {
            return Ok(MutationResult::Rejected(Outcome::MergePending));
        }
        if !existing.dismissible {
            return Ok(MutationResult::Rejected(Outcome::InvalidState));
        }
        let version = next_version(existing.review.version)?;
        let pull_version = next_version(pull.version)?;
        let now = timestamp(context.now_ms())?;
        context.query("INSERT INTO repository_pull_review_dismissals(pull_number,review_number,actor_issuer,actor_subject,actor_name,body,version,created_at_ms) VALUES (?,?,?,?,?,?,?,?)", &[integer(input.key.pull)?,integer(input.key.number)?,SqlValue::Text(input.actor.issuer.clone()),SqlValue::Text(input.actor.subject.clone()),SqlValue::Text(input.actor.name.clone()),SqlValue::Text(input.body.clone()),integer(version)?,integer(now)?])?;
        // Only the exact active decision is removed. Dismissing an older review
        // cannot erase its replacement or revive an earlier approval.
        context.query(
            "DELETE FROM repository_pull_review_decisions WHERE pull_number=? AND review_number=?",
            &[integer(input.key.pull)?, integer(input.key.number)?],
        )?;
        context.query("UPDATE repository_pull_reviews SET version=?,updated_at_ms=? WHERE pull_number=? AND number=? AND version=?", &[integer(version)?,integer(now)?,integer(input.key.pull)?,integer(input.key.number)?,integer(input.version)?])?;
        context.query(
            "UPDATE repository_pulls SET version=?,updated_at_ms=? WHERE number=? AND version=?",
            &[
                integer(pull_version)?,
                integer(now)?,
                integer(pull.number)?,
                integer(pull.version)?,
            ],
        )?;
        activity::append(
            context,
            Target::PullReview {
                pull: input.key.pull,
                number: input.key.number,
            },
            &input.actor,
            Action::ReviewDismissed,
        )?;
        context.put_document(&key, &Submission { digest })?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Outcome::Dismissed))
    }
}
#[cfg(test)]
pub(in crate::cells::repository) mod tests;
