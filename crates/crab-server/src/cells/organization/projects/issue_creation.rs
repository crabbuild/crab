use super::*;
use crate::cells::repository::{
    collaboration::{Subject, SubjectKind},
    projects::organization_issue as source,
};
use source::Outcome as SourceOutcome;

// Four validated planning items fit in a bounded Cell document. Source
// titles and bodies are only present in the original browser submission.
const MAX_PENDING: usize = 4;
const WAKE_PREFIX: &str = "organization/projects/v1/issue-recovery/";
#[cfg(not(test))]
const GRACE_MS: i64 = 5 * 60 * 1000;
#[cfg(test)]
const GRACE_MS: i64 = 10 * 1000;
const RETRY_MS: i64 = 30 * 1000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct PendingIssue {
    pub request_id: Uuid,
    pub repository: Uuid,
    pub ownership_epoch: u32,
    pub digest: [u8; 32],
    project: Uuid,
    item: Uuid,
    actor: Uuid,
    wake_at: i64,
}

impl PendingIssue {
    fn wake_key(&self) -> String {
        format!("{WAKE_PREFIX}{:016x}/{}", self.wake_at, self.request_id)
    }

    fn enqueue(&self, context: &Transaction<'_>) -> Result<()> {
        context.put_document(&self.wake_key(), &(self.wake_at, self.request_id))
    }
}

fn next(context: &Transaction<'_>) -> Result<Option<(i64, Uuid)>> {
    let rows = context.documents_after(WAKE_PREFIX, None, 1)?;
    Ok(rows.into_iter().next().map(|(_, wake)| wake))
}

fn schedule(context: &Transaction<'_>) -> Result<()> {
    context.set_alarm(next(context)?.map(|(at, _)| at))
}

pub(in crate::cells) fn due(context: &Transaction<'_>) -> Result<Option<PendingIssue>> {
    let Some((at, id)) = next(context)? else {
        context.set_alarm(None)?;
        return Ok(None);
    };
    if at > context.now_ms() {
        context.set_alarm(Some(at))?;
        return Ok(None);
    }
    let mut saved = commands::Saved::load(context, id)?
        .ok_or(Error::Invalid("Project issue recovery has no submission"))?;
    let Outcome::PendingIssue { issue } = &mut saved.outcome else {
        return Err(Error::Invalid("Project issue recovery is not pending"));
    };
    if issue.wake_at != at || issue.request_id != id {
        return Err(Error::Invalid("Project issue recovery target changed"));
    }
    // Re-arm before calling the source so a lost reply or process exit leaves
    // a durable retry. Move the entry past other due work if its source stalls.
    context.delete_document(&issue.wake_key())?;
    issue.wake_at = context.now_ms().saturating_add(RETRY_MS);
    issue.enqueue(context)?;
    let issue = issue.clone();
    saved.write(context, id)?;
    schedule(context)?;
    Ok(Some(issue))
}

#[derive(Deserialize, Serialize)]
pub(super) struct Reservation {
    pub(super) request_id: Uuid,
    pub(super) repository: Uuid,
    item: planning::Item,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    draft_version: Option<u64>,
}

pub(super) type Reservations = BTreeMap<Uuid, Reservation>;

fn key(project: Uuid) -> String {
    format!("organization/projects/v1/pending-issues/{project}")
}

pub(super) fn reservations(context: &Transaction<'_>, project: Uuid) -> Result<Reservations> {
    Ok(context.document(&key(project))?.unwrap_or_default())
}

pub(super) fn can_edit(
    pending: &Reservations,
    archived: bool,
    definition: &planning::Definition,
) -> bool {
    pending.is_empty() || (!archived && pending.values().all(|entry| entry.item.valid(definition)))
}

pub(super) fn has_room(
    context: &Transaction<'_>,
    project: Uuid,
    pending: &Reservations,
) -> Result<bool> {
    let count = context.query(
        "SELECT COUNT(*) FROM organization_project_items WHERE project=?",
        &[blob(project)],
    )?;
    let insertions = pending
        .values()
        .filter(|entry| entry.draft_version.is_none())
        .count();
    Ok(uint(&count.rows[0][0])?.saturating_add(insertions as u64) < MAX_ITEMS)
}

fn rejected(reason: &str) -> MutationResult<Outcome> {
    MutationResult::Rejected(Outcome::Rejected {
        reason: reason.into(),
    })
}

pub(super) fn reserve(
    context: &Transaction<'_>,
    request: &Request,
    project: &Project,
    mut pending: Reservations,
) -> Result<MutationResult<Outcome>> {
    let Command::CreateIssue {
        repository,
        item: draft,
        type_id,
        draft_version,
        ..
    } = &request.command
    else {
        return Err(Error::Invalid("Expected an issue creation command"));
    };
    let Some(ownership_epoch) = request.source_epoch.filter(|_| request.source_allowed) else {
        return Ok(rejected(
            "Choose a linked organization repository where you have write access",
        ));
    };
    if !repositories(context, project.id)?.contains(repository) {
        return Ok(rejected("Choose a linked organization repository"));
    }
    if draft.subject.is_some()
        || !draft.valid(&project.definition)
        || type_id.is_some_and(|id| id.is_nil())
    {
        return Ok(rejected("Check the new issue and its Project fields"));
    }
    if pending.contains_key(&draft.id) {
        return Ok(rejected("This item has a pending issue submission"));
    }
    let version = ledger::item_version(context, project.id, draft.id)?;
    let matches_item = match draft_version {
        None => version == 0,
        Some(expected) => {
            version > 0
                && version == *expected
                && item(context, project.id, draft.id)?
                    .is_some_and(|saved| saved.repository.is_none() && saved.item.subject.is_none())
        }
    };
    if !matches_item {
        return Ok(MutationResult::Rejected(Outcome::ItemConflict {
            project: project.id,
            item: draft.id,
            version,
        }));
    }
    if pending.len() >= MAX_PENDING
        || draft_version.is_none() && !has_room(context, project.id, &pending)?
    {
        return Ok(rejected(
            "This Project has no available issue creation slots",
        ));
    }
    let issue = PendingIssue {
        request_id: request.request_id,
        project: project.id,
        repository: *repository,
        item: draft.id,
        actor: request.actor.id(),
        ownership_epoch,
        digest: source::digest(
            project.id,
            request.actor.id(),
            &draft.title,
            &draft.body,
            *type_id,
            ownership_epoch,
        )?,
        wake_at: context.now_ms().saturating_add(GRACE_MS),
    };
    let mut item = draft.clone();
    item.title.clear();
    item.body.clear();
    // A placeholder validates fields as linked work without publishing a row.
    item.subject = Some(Subject {
        kind: SubjectKind::Issue,
        number: 1,
    });
    pending.insert(
        item.id,
        Reservation {
            request_id: request.request_id,
            repository: *repository,
            item,
            draft_version: *draft_version,
        },
    );
    context.put_document(&key(project.id), &pending)?;
    issue.enqueue(context)?;
    schedule(context)?;
    Ok(MutationResult::Success(Outcome::PendingIssue { issue }))
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolveInput {
    pub request_id: Uuid,
    pub digest: [u8; 32],
    pub receipt: SourceOutcome,
}

pub(crate) struct ResolveIssue;
impl Mutation for ResolveIssue {
    const CLASS: &'static str = super::super::CLASS;
    const ID: u32 = 32;
    type Input = ResolveInput;
    type Output = Outcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: ResolveInput,
    ) -> Result<MutationResult<Outcome>> {
        let Some(mut saved) = commands::Saved::load(context, input.request_id)? else {
            return Err(Error::AccessDenied);
        };
        let Outcome::PendingIssue { issue } = &saved.outcome else {
            return Ok(MutationResult::Success(saved.outcome));
        };
        if issue.digest != input.digest {
            return Err(Error::RequestConflict);
        }
        let PendingIssue {
            project,
            repository,
            item,
            actor,
            ..
        } = *issue;
        let mut pending = reservations(context, project)?;
        let reserved = pending
            .remove(&item)
            .filter(|entry| entry.request_id == input.request_id && entry.repository == repository)
            .ok_or(Error::Invalid("Project issue reservation is missing"))?;
        if pending.is_empty() {
            context.delete_document(&key(project))?;
        } else {
            context.put_document(&key(project), &pending)?;
        }
        context.delete_document(&issue.wake_key())?;
        schedule(context)?;
        let outcome = match input.receipt {
            SourceOutcome::Cancelled => Outcome::Cancelled,
            SourceOutcome::Rejected { reason } => Outcome::Rejected { reason },
            SourceOutcome::RequestConflict => {
                return Err(Error::Invalid("Project issue receipt conflicts"));
            }
            SourceOutcome::Created { number } => {
                let existing = context.query("SELECT 1 FROM organization_project_items WHERE project=? AND repository=? AND kind=0 AND number=?", &[blob(project), blob(repository), integer(number)?])?;
                if !existing.rows.is_empty() {
                    let version = load(context, project)?
                        .ok_or(Error::Invalid("Project issue target is missing"))?
                        .version;
                    Outcome::IssueAlreadyLinked { project, version }
                } else {
                    link(context, project, actor, reserved, number)?
                }
            }
        };
        saved.outcome = outcome.clone();
        saved.write(context, input.request_id)?;
        Ok(MutationResult::Success(outcome))
    }
}

fn link(
    context: &mut Transaction<'_>,
    project: Uuid,
    actor: Uuid,
    reservation: Reservation,
    number: u64,
) -> Result<Outcome> {
    let Reservation {
        request_id,
        repository,
        mut item,
        draft_version,
    } = reservation;
    item.subject = Some(Subject {
        kind: SubjectKind::Issue,
        number,
    });
    let applied = commands::apply_published_issue(
        context,
        &Request {
            command: Command::Item {
                project,
                version: draft_version.unwrap_or_default(),
                repository: Some(repository),
                item,
            },
            actor: actor.into(),
            request_id,
            source_allowed: true,
            attachment: None,
            source_epoch: None,
            eligible_outside: None,
            cancel: false,
        },
    )?;
    let MutationResult::Success(outcome) = applied else {
        return Err(Error::Invalid("Reserved Project item could not be linked"));
    };
    Ok(outcome)
}
