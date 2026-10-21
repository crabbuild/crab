use super::super::{
    Mutation, MutationResult, Read, RepositoryAuthor, Transaction, advance_revision, settings,
    timestamp, validate_author, validate_number, validate_title,
};
use super::{Decision, PREFIX, Page, Subject, SubjectSummary, next_version};
use super::{
    SubjectKind, Target,
    activity::{self, Action},
};
use serde::{Deserialize, Serialize};

const MAX_MILESTONES: u64 = 1_000;
const MILESTONES: &str = "collaboration/v1/milestones/";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MilestoneState {
    Open,
    Closed,
}

impl MilestoneState {
    fn key(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Milestone {
    pub number: u64,
    pub title: String,
    pub description: String,
    pub state: MilestoneState,
    pub due_on: Option<String>,
    pub author: RepositoryAuthor,
    pub open_count: u64,
    pub closed_count: u64,
    pub version: u64,
    pub created_at: u64,
    pub updated_at: u64,
    pub closed_at: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateMilestoneInput {
    pub submission_id: [u8; 16],
    pub author: RepositoryAuthor,
    pub title: String,
    pub description: String,
    pub due_on: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditMilestoneInput {
    pub number: u64,
    pub version: u64,
    pub actor: RepositoryAuthor,
    pub title: String,
    pub description: String,
    pub due_on: Option<String>,
    pub state: Option<MilestoneState>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListMilestonesInput {
    pub before: Option<u64>,
    pub limit: u8,
    pub state: Option<MilestoneState>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct StoredAssignment {
    pub position: u64,
    pub subject: Subject,
    pub milestone: Option<u64>,
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssignMilestoneInput {
    pub subject: Subject,
    pub milestone: Option<u64>,
    pub version: u64,
    pub actor: RepositoryAuthor,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Assignment {
    pub subject: Subject,
    pub milestone: Option<Milestone>,
    pub version: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Submission {
    digest: String,
    number: u64,
}

fn key(number: u64) -> String {
    format!("{MILESTONES}{number:016x}")
}
fn state_prefix(state: MilestoneState) -> String {
    format!("{PREFIX}/milestone-states/{}/", state.key())
}
fn state_key(state: MilestoneState, number: u64) -> String {
    format!("{}{number:016x}", state_prefix(state))
}
fn name_key(title: &str) -> String {
    format!(
        "{PREFIX}/milestone-names/{}",
        blake3::hash(title.to_lowercase().as_bytes()).to_hex()
    )
}
fn assignment_key(subject: Subject) -> String {
    format!("{PREFIX}/milestone-assignments/{}", subject.key())
}

pub(crate) fn validate_fields(
    title: &str,
    description: &str,
    due_on: Option<&str>,
) -> crate::cells::Result<()> {
    validate_title(title)?;
    if title.trim() != title
        || description.len() > 8192
        || description
            .chars()
            .any(|value| value.is_control() && !matches!(value, '\n' | '\r' | '\t'))
    {
        return Err(crate::cells::Error::Invalid(
            "milestone title or description is invalid",
        ));
    }
    if let Some(date) = due_on {
        if date.len() != 10
            || !date.bytes().enumerate().all(|(index, value)| {
                if matches!(index, 4 | 7) {
                    value == b'-'
                } else {
                    value.is_ascii_digit()
                }
            })
        {
            return Err(crate::cells::Error::Invalid(
                "milestone due date must be a valid YYYY-MM-DD date",
            ));
        }
        let parsed = (|| {
            let year = date[..4].parse::<i32>().ok()?;
            let month = time::Month::try_from(date[5..7].parse::<u8>().ok()?).ok()?;
            let day = date[8..].parse::<u8>().ok()?;
            (year >= 1).then_some(())?;
            time::Date::from_calendar_date(year, month, day).ok()
        })();
        if parsed.is_none() {
            return Err(crate::cells::Error::Invalid(
                "milestone due date must be a valid YYYY-MM-DD date",
            ));
        }
    }
    Ok(())
}

fn load(context: &Transaction<'_>, number: u64) -> crate::cells::Result<Option<Milestone>> {
    validate_number(number)?;
    let milestone: Option<Milestone> = context.document(&key(number))?;
    if let Some(value) = &milestone {
        validate_fields(&value.title, &value.description, value.due_on.as_deref())?;
        validate_author(&value.author)?;
        validate_number(value.version)?;
        if value.number != number
            || value.updated_at < value.created_at
            || value
                .closed_at
                .is_some_and(|closed| closed < value.created_at || closed > value.updated_at)
            || value
                .open_count
                .checked_add(value.closed_count)
                .is_none_or(|count| count > super::super::MAX_NUMBER)
            || (value.state == MilestoneState::Closed) != value.closed_at.is_some()
        {
            return Err(crate::cells::Error::Invalid("stored milestone is invalid"));
        }
    }
    Ok(milestone)
}

pub(crate) struct CreateMilestone;
impl Mutation for CreateMilestone {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 200;
    type Input = CreateMilestoneInput;
    type Output = Decision<Milestone>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Decision::Archived));
        }
        validate_author(&input.author)?;
        validate_fields(&input.title, &input.description, input.due_on.as_deref())?;
        let submission = format!(
            "{PREFIX}/milestone-submissions/{}",
            uuid::Uuid::from_bytes(input.submission_id)
        );
        // Presentation names can change between HTTP retries. Submission
        // identity depends on stable identity-provider claims and content.
        let digest = blake3::hash(&serde_json::to_vec(&(
            &input.author.issuer,
            &input.author.subject,
            &input.title,
            &input.description,
            &input.due_on,
        ))?)
        .to_hex()
        .to_string();
        if let Some(existing) = context.document::<Submission>(&submission)? {
            if existing.digest != digest {
                return Ok(MutationResult::Rejected(Decision::Conflict));
            }
            return load(context, existing.number)?
                .map(|value| MutationResult::Success(Decision::Applied(value)))
                .ok_or(crate::cells::Error::Invalid(
                    "milestone submission has no milestone",
                ));
        }
        let name = name_key(&input.title);
        if context.document::<u64>(&name)?.is_some() {
            return Ok(MutationResult::Rejected(Decision::NameConflict));
        }
        let sequence = format!("{PREFIX}/sequences/milestones");
        let last = context.document::<u64>(&sequence)?.unwrap_or(0);
        if last >= MAX_MILESTONES {
            return Ok(MutationResult::Rejected(Decision::LimitReached));
        }
        let number = next_version(last)?;
        let now = timestamp(context.now_ms())?;
        let milestone = Milestone {
            number,
            title: input.title,
            description: input.description,
            due_on: input.due_on,
            author: input.author,
            state: MilestoneState::Open,
            open_count: 0,
            closed_count: 0,
            version: 1,
            created_at: now,
            updated_at: now,
            closed_at: None,
        };
        context.put_document(&sequence, &number)?;
        context.put_document(&name, &number)?;
        context.put_document(&state_key(milestone.state, number), &number)?;
        context.put_document(&format!("{PREFIX}/milestone-index/{number:016x}"), &number)?;
        context.put_document(&submission, &Submission { digest, number })?;
        context.put_document(&key(number), &milestone)?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Decision::Applied(milestone)))
    }
}

pub(crate) struct EditMilestone;
impl Mutation for EditMilestone {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 201;
    type Input = EditMilestoneInput;
    type Output = Decision<Milestone>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Decision::Archived));
        }
        validate_author(&input.actor)?;
        validate_fields(&input.title, &input.description, input.due_on.as_deref())?;
        let Some(mut current) = load(context, input.number)? else {
            return Ok(MutationResult::Rejected(Decision::NotFound));
        };
        if current.version != input.version {
            return Ok(MutationResult::Rejected(Decision::Conflict));
        }
        let new_name = name_key(&input.title);
        if context
            .document::<u64>(&new_name)?
            .is_some_and(|number| number != current.number)
        {
            return Ok(MutationResult::Rejected(Decision::NameConflict));
        }
        let state = input.state.unwrap_or(current.state);
        context.delete_document(&name_key(&current.title))?;
        context.delete_document(&state_key(current.state, current.number))?;
        if current.state != state {
            current.closed_at = (state == MilestoneState::Closed)
                .then(|| timestamp(context.now_ms()).map(|now| now.max(current.updated_at)))
                .transpose()?;
        }
        current.title = input.title;
        current.description = input.description;
        current.due_on = input.due_on;
        current.state = state;
        current.version = next_version(current.version)?;
        current.updated_at = timestamp(context.now_ms())?.max(current.updated_at);
        context.put_document(&new_name, &current.number)?;
        context.put_document(&state_key(current.state, current.number), &current.number)?;
        context.put_document(&key(current.number), &current)?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Decision::Applied(current)))
    }
}

pub(crate) struct GetMilestone;
impl Read for GetMilestone {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 200;
    type Input = u64;
    type Output = Option<Milestone>;
    fn execute(context: &mut Transaction<'_>, number: u64) -> crate::cells::Result<Self::Output> {
        load(context, number)
    }
}

pub(crate) struct ListMilestones;
impl Read for ListMilestones {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 201;
    type Input = ListMilestonesInput;
    type Output = Page<Milestone>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        if !(1..=50).contains(&input.limit) {
            return Err(crate::cells::Error::Invalid(
                "milestone page size must be 1–50",
            ));
        }
        if let Some(number) = input.before {
            validate_number(number)?;
        }
        let limit = usize::from(input.limit);
        let mut items = if let Some(state) = input.state {
            let prefix = state_prefix(state);
            let before = input.before.map(|number| state_key(state, number));
            context
                .documents::<u64>(&prefix, before.as_deref(), limit + 1)?
                .into_iter()
                .map(|(stored_key, number)| {
                    if stored_key != state_key(state, number) {
                        return Err(crate::cells::Error::Invalid(
                            "milestone state index key is invalid",
                        ));
                    }
                    let value = load(context, number)?.ok_or(crate::cells::Error::Invalid(
                        "milestone state index has no milestone",
                    ))?;
                    if value.state != state {
                        return Err(crate::cells::Error::Invalid(
                            "milestone state index is invalid",
                        ));
                    }
                    Ok(value)
                })
                .collect::<crate::cells::Result<Vec<_>>>()?
        } else {
            let prefix = format!("{PREFIX}/milestone-index/");
            let before = input.before.map(|number| format!("{prefix}{number:016x}"));
            context
                .documents::<u64>(&prefix, before.as_deref(), limit + 1)?
                .into_iter()
                .map(|(stored_key, number)| {
                    if stored_key != format!("{prefix}{number:016x}") {
                        return Err(crate::cells::Error::Invalid(
                            "milestone index key is invalid",
                        ));
                    }
                    load(context, number)?.ok_or(crate::cells::Error::Invalid(
                        "milestone index has no milestone",
                    ))
                })
                .collect::<crate::cells::Result<Vec<_>>>()?
        };
        let mut has_more = items.len() > limit;
        items.truncate(limit);
        // Records with heavily escaped Markdown can fill the response budget
        // before its item limit. Retain an exclusive cursor for the omitted tail.
        while serde_json::to_vec(&items)?.len() > super::super::MAX_LIST_OUTPUT_BYTES {
            items.pop();
            has_more = true;
        }
        let next = has_more
            .then(|| items.last().map(|value| value.number))
            .flatten();
        Ok(Page { items, next })
    }
}

pub(crate) struct GetAssignment;
impl Read for GetAssignment {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 202;
    type Input = Subject;
    type Output = Option<Assignment>;
    fn execute(
        context: &mut Transaction<'_>,
        subject: Subject,
    ) -> crate::cells::Result<Self::Output> {
        if subject.closed(context)?.is_none() {
            return Ok(None);
        }
        let stored = assignment(context, subject)?;
        Ok(Some(assignment_view(context, stored)?))
    }
}

fn assignment(
    context: &Transaction<'_>,
    subject: Subject,
) -> crate::cells::Result<StoredAssignment> {
    let value: StoredAssignment =
        context
            .document(&assignment_key(subject))?
            .unwrap_or(StoredAssignment {
                position: 0,
                subject,
                milestone: None,
                version: 0,
            });
    if value.subject != subject
        || value.version > super::super::MAX_NUMBER
        || value.position > super::super::MAX_NUMBER
        || value.milestone.is_none() && value.position != 0
        || value.milestone.is_some() && (value.version == 0 || value.position == 0)
    {
        return Err(crate::cells::Error::Invalid(
            "milestone assignment is invalid",
        ));
    }
    Ok(value)
}

fn assignment_view(
    context: &Transaction<'_>,
    value: StoredAssignment,
) -> crate::cells::Result<Assignment> {
    let milestone = value
        .milestone
        .map(|number| {
            load(context, number)?.ok_or(crate::cells::Error::Invalid(
                "assigned milestone is missing",
            ))
        })
        .transpose()?;
    Ok(Assignment {
        subject: value.subject,
        milestone,
        version: value.version,
    })
}

fn adjust(
    context: &Transaction<'_>,
    number: u64,
    closed: bool,
    add: bool,
) -> crate::cells::Result<()> {
    let mut milestone = load(context, number)?.ok_or(crate::cells::Error::Invalid(
        "assigned milestone is missing",
    ))?;
    let count = if closed {
        &mut milestone.closed_count
    } else {
        &mut milestone.open_count
    };
    *count = if add {
        count.checked_add(1)
    } else {
        count.checked_sub(1)
    }
    .filter(|value| *value <= super::super::MAX_NUMBER)
    .ok_or(crate::cells::Error::Invalid(
        "milestone progress count is invalid",
    ))?;
    milestone.version = next_version(milestone.version)?;
    milestone.updated_at = timestamp(context.now_ms())?.max(milestone.updated_at);
    context.put_document(&key(number), &milestone)
}

pub(crate) struct AssignMilestone;
impl Mutation for AssignMilestone {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 202;
    type Input = AssignMilestoneInput;
    type Output = Decision<Assignment>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<MutationResult<Self::Output>> {
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Decision::Archived));
        }
        validate_author(&input.actor)?;
        let Some(closed) = input.subject.closed(context)? else {
            return Ok(MutationResult::Rejected(Decision::NotFound));
        };
        let key = assignment_key(input.subject);
        let mut assignment = assignment(context, input.subject)?;
        if let Some(number) = input.milestone
            && load(context, number)?.is_none()
        {
            return Ok(MutationResult::Rejected(Decision::NotFound));
        }
        if assignment.milestone == input.milestone {
            return Ok(MutationResult::Success(Decision::Applied(assignment_view(
                context, assignment,
            )?)));
        }
        if assignment.version != input.version {
            return Ok(MutationResult::Rejected(Decision::Conflict));
        }
        let previous = assignment.milestone;
        if let Some(number) = assignment.milestone {
            context.delete_document(&item_key(number, assignment.position))?;
            adjust(context, number, closed, false)?;
        }
        if let Some(number) = input.milestone {
            let sequence_key = format!("{PREFIX}/sequences/milestone-items");
            let last = context.document::<u64>(&sequence_key)?.unwrap_or(0);
            assignment.position = next_version(last)?;
            context.put_document(&sequence_key, &assignment.position)?;
            context.put_document(&item_key(number, assignment.position), &input.subject)?;
            adjust(context, number, closed, true)?;
        } else {
            assignment.position = 0;
        }
        assignment.milestone = input.milestone;
        assignment.version = next_version(assignment.version)?;
        context.put_document(&key, &assignment)?;
        advance_revision(context)?;
        let target = match input.subject.kind {
            SubjectKind::Issue => Target::Issue {
                number: input.subject.number,
            },
            SubjectKind::Pull => Target::Pull {
                number: input.subject.number,
            },
        };
        for (number, assigned) in [(previous, false), (input.milestone, true)] {
            if let Some(number) = number {
                let value = load(context, number)?.ok_or(crate::cells::Error::Invalid(
                    "activity milestone is missing",
                ))?;
                let action = if assigned {
                    Action::MilestoneAssigned {
                        number,
                        title: value.title,
                    }
                } else {
                    Action::MilestoneRemoved {
                        number,
                        title: value.title,
                    }
                };
                activity::append(context, target, &input.actor, action)?;
            }
        }
        Ok(MutationResult::Success(Decision::Applied(assignment_view(
            context, assignment,
        )?)))
    }
}

pub(crate) fn state_changed(
    context: &Transaction<'_>,
    subject: Subject,
    was_closed: bool,
    closed: bool,
) -> crate::cells::Result<()> {
    if was_closed == closed {
        return Ok(());
    }
    if let Some(number) = assignment(context, subject)?.milestone {
        adjust(context, number, was_closed, false)?;
        adjust(context, number, closed, true)?;
    }
    Ok(())
}

fn item_prefix(number: u64) -> String {
    format!("{PREFIX}/milestone-items/{number:016x}/")
}
fn item_key(number: u64, position: u64) -> String {
    format!("{}{position:016x}", item_prefix(number))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListMilestoneItemsInput {
    pub number: u64,
    pub before: Option<u64>,
    pub limit: u8,
}

pub(crate) struct ListMilestoneItems;
impl Read for ListMilestoneItems {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 203;
    type Input = ListMilestoneItemsInput;
    type Output = Option<Page<SubjectSummary>>;
    fn execute(
        context: &mut Transaction<'_>,
        input: Self::Input,
    ) -> crate::cells::Result<Self::Output> {
        if !(1..=50).contains(&input.limit) {
            return Err(crate::cells::Error::Invalid(
                "milestone page size must be 1–50",
            ));
        }
        if let Some(number) = input.before {
            validate_number(number)?;
        }
        if load(context, input.number)?.is_none() {
            return Ok(None);
        }
        let prefix = item_prefix(input.number);
        let before = input
            .before
            .map(|position| item_key(input.number, position));
        let limit = usize::from(input.limit);
        let entries = context.documents::<Subject>(&prefix, before.as_deref(), limit + 1)?;
        let has_more = entries.len() > limit;
        let mut items = Vec::new();
        let mut last = None;
        for (key, subject) in entries.into_iter().take(limit) {
            let assigned = assignment(context, subject)?;
            if assigned.milestone != Some(input.number)
                || key != item_key(input.number, assigned.position)
            {
                return Err(crate::cells::Error::Invalid(
                    "milestone item index is invalid",
                ));
            }
            let summary = subject
                .summary(context)?
                .ok_or(crate::cells::Error::Invalid(
                    "milestone item has no discussion",
                ))?;
            items.push(summary);
            last = Some(assigned.position);
        }
        Ok(Some(Page {
            items,
            next: if has_more { last } else { None },
        }))
    }
}

#[cfg(test)]
mod tests;
