use super::*;

pub(super) struct Current {
    pub version: u64,
    labels: Vec<u64>,
    assignees: Vec<Assignee>,
}
pub(super) fn current(context: &mut Transaction<'_>, subject: Subject) -> Result<Option<Current>> {
    Ok(match subject.kind {
        SubjectKind::Issue => GetIssue::execute(context, subject.number)?.map(|record| Current {
            version: record.version,
            labels: record.label_ids,
            assignees: record.assignees,
        }),
        SubjectKind::Pull => GetPull::execute(context, subject.number)?.map(|record| Current {
            version: record.version,
            labels: record.label_ids,
            assignees: record.assignees,
        }),
    })
}
pub(super) fn apply(context: &mut Transaction<'_>, input: &Input) -> Result<Option<Failure>> {
    let active_labels = if matches!(input.action, Action::Labels { .. }) {
        ListLabels::execute(context, ())?
            .labels
            .into_iter()
            .map(|label| label.number)
            .collect()
    } else {
        BTreeSet::new()
    };
    for selected in &input.items {
        if let Some(failure) = apply_item(context, input, selected, &active_labels)? {
            return Ok(Some(failure));
        }
    }
    Ok(None)
}
fn apply_item(
    context: &mut Transaction<'_>,
    input: &Input,
    selected: &Selected,
    active_labels: &BTreeSet<u64>,
) -> Result<Option<Failure>> {
    let item = &selected.item;
    let Some(mut current) = current(context, item.subject)? else {
        return Ok(Some(Failure::NotFound));
    };
    if current.version != item.version {
        return Ok(Some(Failure::Conflict));
    }
    let mut labels = None;
    let mut assignees = None;
    let mut state = None;
    let mut reason = None;
    match &input.action {
        Action::State {
            state: desired,
            reason: desired_reason,
        } => {
            state = Some(*desired);
            reason = *desired_reason;
        }
        Action::Labels { add, remove } => {
            // Deleted labels remain as historical IDs in source records. Like
            // the individual picker, edits merge only the active selection;
            // otherwise an invisible tombstone would block every future delta.
            current
                .labels
                .retain(|id| active_labels.contains(id) && !remove.contains(id));
            current.labels.extend(add);
            current.labels.sort_unstable();
            current.labels.dedup();
            if current.labels.len() > MAX_LABELS {
                return Ok(Some(Failure::InvalidLabels));
            }
            labels = Some(current.labels);
        }
        Action::Assignees { add, remove } => {
            if add.len() != input.eligible.len()
                || !add
                    .iter()
                    .all(|id| input.eligible.iter().any(|person| person.id == *id))
                || assignments::validate(&input.eligible).is_err()
            {
                return Ok(Some(Failure::InvalidAssignees));
            }
            current
                .assignees
                .retain(|person| !remove.contains(&person.id) && !add.contains(&person.id));
            current.assignees.extend(input.eligible.clone());
            current.assignees.sort_unstable_by_key(|person| person.id);
            if current.assignees.len() > 10 {
                return Ok(Some(Failure::InvalidAssignees));
            }
            assignees = Some(current.assignees);
        }
        Action::Milestone { number } => {
            // Assignment versions are independent of pull edit/merge versions.
            // Preserve canonical progress without changing a reserved merge.
            let assignment = milestones::GetAssignment::execute(context, item.subject)?
                .ok_or(Error::Invalid("bulk milestone assignment is missing"))?;
            if assignment.version != selected.milestone_version {
                return Ok(Some(Failure::Conflict));
            }
            use super::super::Decision;
            return Ok(
                match milestones::AssignMilestone::execute(
                    context,
                    milestones::AssignMilestoneInput {
                        subject: item.subject,
                        milestone: *number,
                        version: selected.milestone_version,
                        actor: input.actor.clone(),
                    },
                )? {
                    MutationResult::Success(Decision::Applied(_)) => None,
                    MutationResult::Rejected(Decision::Archived) => Some(Failure::Archived),
                    MutationResult::Rejected(Decision::NotFound) => Some(Failure::NotFound),
                    MutationResult::Rejected(Decision::Conflict) => Some(Failure::Conflict),
                    _ => return Err(Error::Invalid("bulk milestone decision is inconsistent")),
                },
            );
        }
    }
    Ok(match item.subject.kind {
        SubjectKind::Issue => match UpdateIssue::execute(
            context,
            UpdateIssueInput {
                number: item.subject.number,
                version: item.version,
                actor: input.actor.clone(),
                can_triage: input.can_triage,
                title: None,
                body: None,
                state: state.map(|state| u8::from(state == State::Closed)),
                state_reason: reason,
                label_ids: labels,
                assignees,
            },
        )? {
            MutationResult::Success(UpdateIssueOutcome::Updated(_)) => None,
            MutationResult::Rejected(result) => Some(match result {
                UpdateIssueOutcome::Archived => Failure::Archived,
                UpdateIssueOutcome::NotFound => Failure::NotFound,
                UpdateIssueOutcome::Forbidden
                | UpdateIssueOutcome::LabelForbidden
                | UpdateIssueOutcome::AssigneeForbidden => Failure::Forbidden,
                UpdateIssueOutcome::Conflict => Failure::Conflict,
                UpdateIssueOutcome::LabelInvalid => Failure::InvalidLabels,
                UpdateIssueOutcome::InvalidStateReason => Failure::InvalidState,
                _ => return Err(Error::Invalid("bulk issue decision is inconsistent")),
            }),
            _ => return Err(Error::Invalid("bulk issue outcome is inconsistent")),
        },
        SubjectKind::Pull => match UpdatePull::execute(
            context,
            UpdatePullInput {
                number: item.subject.number,
                version: item.version,
                actor: input.actor.clone(),
                can_triage: input.can_triage,
                title: None,
                body: None,
                state: state.map(|state| {
                    if state == State::Open {
                        PullState::Open
                    } else {
                        PullState::Closed
                    }
                }),
                label_ids: labels,
                assignees,
            },
        )? {
            MutationResult::Success(UpdatePullOutcome::Updated(_)) => None,
            MutationResult::Rejected(result) => Some(match result {
                UpdatePullOutcome::Archived => Failure::Archived,
                UpdatePullOutcome::NotFound => Failure::NotFound,
                UpdatePullOutcome::Forbidden => Failure::Forbidden,
                UpdatePullOutcome::Conflict => Failure::Conflict,
                UpdatePullOutcome::InvalidLabel => Failure::InvalidLabels,
                UpdatePullOutcome::InvalidState => Failure::InvalidState,
                UpdatePullOutcome::MergePending => Failure::MergePending,
                _ => return Err(Error::Invalid("bulk pull decision is inconsistent")),
            }),
            _ => return Err(Error::Invalid("bulk pull outcome is inconsistent")),
        },
    })
}
