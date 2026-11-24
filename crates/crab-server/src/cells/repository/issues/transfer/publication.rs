use super::recovery::Work;
use super::*;
use crate::cells::repository::collaboration::milestones;

const MOVED: &str = "collaboration/v1/issue-transfers/moved/";
const RESERVED: &str = "collaboration/v1/issue-transfers/reserved/";
const CANCELLED: &str = "collaboration/v1/issue-transfers/cancelled/";
const PUBLICATION_GRACE_MS: i64 = 5_000;
const CLEANUP_INTERVAL_MS: i64 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Destination {
    pub repository: Uuid,
    pub number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reservation {
    pub request: Uuid,
    pub source: Source,
    pub destination: Destination,
    pub ownership_epoch: u32,
    pub actor: IdentityKey,
    pub commitment: [u8; 32],
}

#[derive(Deserialize, Serialize)]
pub(super) struct Prepared {
    number: u64,
    ownership_epoch: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum DestinationState {
    Prepared(Prepared),
    Published(Prepared),
    Cancelled,
}

impl DestinationState {
    fn prepared(&self) -> Option<&Prepared> {
        match self {
            Self::Prepared(value) | Self::Published(value) => Some(value),
            Self::Cancelled => None,
        }
    }
}

fn job_key(request: Uuid) -> String {
    format!("{PREFIX}{request}")
}

fn move_key(number: u64) -> String {
    format!("{MOVED}{number:016x}")
}

fn reservation_key(request: Uuid) -> String {
    format!("{RESERVED}{request}")
}

fn cancellation_key(request: Uuid) -> String {
    format!("{CANCELLED}{request}")
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PrepareInput {
    pub request: Uuid,
    pub source: Source,
    pub actor: IdentityKey,
    pub commitment: [u8; 32],
    pub ownership_epoch: u32,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PrepareOutcome {
    Reserved { reservation: Reservation },
    Missing,
    Conflict,
    Incomplete,
    Archived,
    Expired,
}

pub(crate) struct Prepare;

impl Mutation for Prepare {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 457;
    type Input = PrepareInput;
    type Output = PrepareOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        input: PrepareInput,
    ) -> Result<MutationResult<PrepareOutcome>> {
        let key = job_key(input.request);
        let Some(mut job) = context.document::<Job>(&key)? else {
            return Ok(MutationResult::Rejected(PrepareOutcome::Missing));
        };
        if job.actor != input.actor || job.source != input.source {
            return Ok(MutationResult::Rejected(PrepareOutcome::Conflict));
        }
        if matches!(job.publication, Some(DestinationState::Cancelled)) {
            return Ok(MutationResult::Rejected(PrepareOutcome::Expired));
        }
        if job.progress.frames == 0 || job.progress.next.is_some() {
            return Ok(MutationResult::Rejected(PrepareOutcome::Incomplete));
        }
        if job.progress.commitment != input.commitment {
            return Ok(MutationResult::Rejected(PrepareOutcome::Conflict));
        }
        let number = if let Some(prepared) = job
            .publication
            .as_ref()
            .and_then(DestinationState::prepared)
        {
            if prepared.ownership_epoch != input.ownership_epoch {
                return Ok(MutationResult::Rejected(PrepareOutcome::Conflict));
            }
            prepared.number
        } else {
            if settings::archived(context)? {
                return Ok(MutationResult::Rejected(PrepareOutcome::Archived));
            }
            lifecycle::ensure_ready(context)?;
            let number = allocate_issue(context)?;
            let (repository, _) = identity(context)?;
            let reservation = Reservation {
                request: input.request,
                source: job.source,
                destination: Destination { repository, number },
                ownership_epoch: input.ownership_epoch,
                actor: job.actor.clone(),
                commitment: job.progress.commitment,
            };
            // Check the same installation and projections before allowing the
            // source to retire. A rejected nested scope discards the preview.
            context.apply(|preview| {
                install(preview, &reservation, job.progress.frames)?;
                search::index::flush(preview)?;
                Ok(MutationResult::Rejected(()))
            })?;
            job.publication = Some(DestinationState::Prepared(Prepared {
                number,
                ownership_epoch: input.ownership_epoch,
            }));
            context.put_document(&key, &job)?;
            // The ownership route checks this fence under the same policy
            // lease the ingress coordinator must hold when it prepares.
            context.put_document(&reservation_key(input.request), &input.request)?;
            recovery::arm(context, Work::Expire(input.request), recovery::IDLE_MS)?;
            number
        };
        let (repository, _) = identity(context)?;
        Ok(MutationResult::Success(PrepareOutcome::Reserved {
            reservation: Reservation {
                request: input.request,
                source: input.source,
                destination: Destination { repository, number },
                ownership_epoch: input.ownership_epoch,
                actor: input.actor,
                commitment: input.commitment,
            },
        }))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Decision {
    reservation: Reservation,
    source_revision: u64,
}

impl Decision {
    pub(crate) fn destination(&self) -> Destination {
        self.reservation.destination
    }

    pub(crate) fn matches_request(
        &self,
        request: Uuid,
        destination: Uuid,
        version: u64,
        actor: &IdentityKey,
    ) -> bool {
        self.reservation.request == request
            && self.reservation.destination.repository == destination
            && self.reservation.source.version == version
            && &self.reservation.actor == actor
    }
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RetireOutcome {
    Committed { decision: Decision },
    Missing,
    Conflict,
    Changed,
    Archived,
    Expired,
}

pub(crate) struct Retire;

fn validate_source_reservation(context: &Transaction<'_>, reservation: &Reservation) -> Result<()> {
    let (repository, _) = identity(context)?;
    validate_number(reservation.source.issue)?;
    validate_number(reservation.destination.number)?;
    reservation.actor.validate()?;
    if reservation.source.repository != repository
        || reservation.destination.repository.is_nil()
        || reservation.destination.repository == repository
        || reservation.request.is_nil()
    {
        return Err(Error::Invalid(
            "Issue transfer source does not match this repository",
        ));
    }
    Ok(())
}

// Ingress orchestration must prove the reservation with the destination owner
// and both current grants before sending this private command.
impl Mutation for Retire {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 458;
    type Input = Reservation;
    type Output = RetireOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        reservation: Reservation,
    ) -> Result<MutationResult<RetireOutcome>> {
        validate_source_reservation(context, &reservation)?;
        if let Some(decision) = moved(context, reservation.source.issue)? {
            return Ok(if decision.reservation == reservation {
                MutationResult::Success(RetireOutcome::Committed { decision })
            } else {
                MutationResult::Rejected(RetireOutcome::Conflict)
            });
        }
        if context
            .document::<Reservation>(&cancellation_key(reservation.request))?
            .is_some()
        {
            return Ok(MutationResult::Rejected(RetireOutcome::Expired));
        }
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(RetireOutcome::Archived));
        }
        let Some(issue) = GetIssue::execute(context, reservation.source.issue)? else {
            return Ok(MutationResult::Rejected(RetireOutcome::Missing));
        };
        if issue.state != 0 || observed(context, &issue)? != reservation.source {
            return Ok(MutationResult::Rejected(RetireOutcome::Changed));
        }
        let subject = Subject {
            kind: SubjectKind::Issue,
            number: issue.number,
        };
        pins::retire(context, issue.number)?;
        milestones::transfer_assignment(context, subject, None)?;
        search::contributions::retire_issue(context, issue.number)?;
        if !context.query(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'repository_issue_parents'",
            &[],
        )?.rows.is_empty() {
            // Removing a parent leaves a revisioned empty relationship row.
            context.query(
                "DELETE FROM repository_issue_parents WHERE child = ? AND parent IS NULL",
                &[integer(issue.number)?],
            )?;
        }
        // Cascades remove ordinary comment reads. The search projections and
        // durable redirect commit in the same source transaction.
        let removed = context.query(
            "DELETE FROM repository_issues WHERE number = ?",
            &[integer(issue.number)?],
        )?;
        if removed.rows_affected == 0 {
            return Err(Error::Invalid("Issue transfer source disappeared"));
        }
        advance_revision(context)?;
        let (_, source_revision) = identity(context)?;
        let decision = Decision {
            reservation,
            source_revision,
        };
        context.put_document(&move_key(issue.number), &decision)?;
        recovery::arm(context, Work::Publish(issue.number), PUBLICATION_GRACE_MS)?;
        Ok(MutationResult::Success(RetireOutcome::Committed {
            decision,
        }))
    }
}

pub(in crate::cells::repository) fn moved(
    context: &Transaction<'_>,
    number: u64,
) -> Result<Option<Decision>> {
    validate_number(number)?;
    context.document(&move_key(number))
}

pub(crate) struct GetMove;

impl Read for GetMove {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 457;
    type Input = u64;
    type Output = Option<Decision>;

    fn execute(context: &mut Transaction<'_>, number: u64) -> Result<Self::Output> {
        moved(context, number)
    }
}

pub(crate) struct HasReservations;

impl Read for HasReservations {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 458;
    type Input = ();
    type Output = bool;

    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<bool> {
        Ok(!context.documents::<Uuid>(RESERVED, None, 1)?.is_empty())
    }
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FenceOutcome {
    Cancelled,
    Committed,
    Conflict,
}

pub(crate) struct Fence;

impl Mutation for Fence {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 460;
    type Input = Reservation;
    type Output = FenceOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        reservation: Reservation,
    ) -> Result<MutationResult<FenceOutcome>> {
        validate_source_reservation(context, &reservation)?;
        if moved(context, reservation.source.issue)?
            .is_some_and(|decision| decision.reservation == reservation)
        {
            return Ok(MutationResult::Success(FenceOutcome::Committed));
        }
        let key = cancellation_key(reservation.request);
        if let Some(previous) = context.document::<Reservation>(&key)? {
            return Ok(if previous == reservation {
                MutationResult::Success(FenceOutcome::Cancelled)
            } else {
                MutationResult::Rejected(FenceOutcome::Conflict)
            });
        }
        // Fence and Retire serialize at the source even if the destination
        // stopped waiting for the original retirement reply.
        context.put_document(&key, &reservation)?;
        Ok(MutationResult::Success(FenceOutcome::Cancelled))
    }
}

pub(in crate::cells) enum Recovery {
    Publish(Decision),
    Fence(Reservation),
}

pub(in crate::cells) fn due(context: &Transaction<'_>) -> Result<Option<Recovery>> {
    match recovery::due(context)? {
        None => Ok(None),
        Some(Work::Publish(number)) => {
            let decision = moved(context, number)?
                .ok_or(Error::Invalid("Issue transfer wake has no source decision"))?;
            Ok(Some(Recovery::Publish(decision)))
        }
        Some(Work::Expire(request)) => {
            let key = job_key(request);
            let Some(job) = context.document::<Job>(&key)? else {
                return Err(Error::Invalid(
                    "Issue transfer expiration has no staging job",
                ));
            };
            match &job.publication {
                Some(DestinationState::Prepared(prepared)) => {
                    let (repository, _) = identity(context)?;
                    Ok(Some(Recovery::Fence(Reservation {
                        request,
                        source: job.source,
                        destination: Destination {
                            repository,
                            number: prepared.number,
                        },
                        ownership_epoch: prepared.ownership_epoch,
                        actor: job.actor,
                        commitment: job.progress.commitment,
                    })))
                }
                None => {
                    cancel(context, request, job)?;
                    Ok(None)
                }
                Some(DestinationState::Cancelled) => {
                    prune_frame(context, request, job.source)?;
                    Ok(None)
                }
                Some(DestinationState::Published(_)) => {
                    recovery::finish(context, Work::Expire(request))?;
                    Ok(None)
                }
            }
        }
    }
}

pub(in crate::cells) fn acknowledged(context: &Transaction<'_>, decision: &Decision) -> Result<()> {
    let number = decision.reservation.source.issue;
    if moved(context, number)?.as_ref() != Some(decision) {
        return Err(Error::Invalid(
            "Issue transfer acknowledgement does not match its decision",
        ));
    }
    recovery::finish(context, Work::Publish(number))
}

pub(in crate::cells) fn fenced(
    context: &Transaction<'_>,
    reservation: &Reservation,
    outcome: FenceOutcome,
) -> Result<()> {
    let request = reservation.request;
    let (repository, _) = identity(context)?;
    let job = context
        .document::<Job>(&job_key(request))?
        .ok_or(Error::Invalid(
            "Issue transfer expiration has no staging job",
        ))?;
    let prepared = job
        .publication
        .as_ref()
        .and_then(DestinationState::prepared)
        .ok_or(Error::Invalid("Issue transfer no longer has a reservation"))?;
    if repository != reservation.destination.repository
        || prepared.number != reservation.destination.number
        || prepared.ownership_epoch != reservation.ownership_epoch
        || job.source != reservation.source
        || job.actor != reservation.actor
        || job.progress.commitment != reservation.commitment
    {
        return Err(Error::Invalid(
            "Issue transfer fence does not match its reservation",
        ));
    }
    match outcome {
        FenceOutcome::Cancelled
            if matches!(job.publication, Some(DestinationState::Prepared(_))) =>
        {
            cancel(context, request, job)
        }
        // The source already owns a durable publication retry. Keep its
        // reservation and let publication close the ownership fence.
        FenceOutcome::Committed => recovery::finish(context, Work::Expire(request)),
        _ => Err(Error::Invalid("Issue transfer could not fence its source")),
    }
}

fn cancel(context: &Transaction<'_>, request: Uuid, mut job: Job) -> Result<()> {
    job.publication = Some(DestinationState::Cancelled);
    context.put_document(&job_key(request), &job)?;
    context.delete_document(&reservation_key(request))?;
    prune_frame(context, request, job.source)
}

fn prune_frame(context: &Transaction<'_>, request: Uuid, source: Source) -> Result<()> {
    let key = SqlValue::Blob(request.as_bytes().to_vec());
    let result = context.query(
        "SELECT digest, payload, ordinal FROM repository_issue_transfer_frames WHERE transfer = ? ORDER BY ordinal LIMIT 1",
        std::slice::from_ref(&key),
    )?;
    let Some(row) = result.rows.first() else {
        return recovery::finish(context, Work::Expire(request));
    };
    for upload in decode_frame(row, source)?.uploads() {
        attachments::discard_transfer(context, request, upload.id)?;
    }
    context.query(
        "DELETE FROM repository_issue_transfer_frames WHERE transfer = ? AND ordinal = ?",
        &[key, integer(result_u64_from_row(row, 2)?)?],
    )?;
    recovery::arm(context, Work::Expire(request), CLEANUP_INTERVAL_MS)
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PublishOutcome {
    Published { destination: Destination },
    Conflict,
}

pub(crate) struct Publish;

impl Mutation for Publish {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 459;
    type Input = Decision;
    type Output = PublishOutcome;

    fn execute(
        context: &mut Transaction<'_>,
        decision: Decision,
    ) -> Result<MutationResult<PublishOutcome>> {
        let reservation = decision.reservation;
        let key = job_key(reservation.request);
        let (repository, _) = identity(context)?;
        let Some(mut job) = context.document::<Job>(&key)? else {
            return Ok(MutationResult::Rejected(PublishOutcome::Conflict));
        };
        let Some(state) = &job.publication else {
            return Ok(MutationResult::Rejected(PublishOutcome::Conflict));
        };
        let Some(prepared) = state.prepared() else {
            return Ok(MutationResult::Rejected(PublishOutcome::Conflict));
        };
        if reservation.destination.repository != repository
            || reservation.destination.number != prepared.number
            || reservation.ownership_epoch != prepared.ownership_epoch
            || reservation.actor != job.actor
            || reservation.source != job.source
            || reservation.commitment != job.progress.commitment
            || decision.source_revision <= reservation.source.revision
        {
            return Ok(MutationResult::Rejected(PublishOutcome::Conflict));
        }
        if matches!(state, DestinationState::Prepared(_)) {
            // A committed source move must recover even if the repository has
            // archived since the reservation was admitted.
            install(context, &reservation, job.progress.frames)?;
            job.publication = Some(DestinationState::Published(Prepared {
                number: reservation.destination.number,
                ownership_epoch: reservation.ownership_epoch,
            }));
            context.put_document(&key, &job)?;
            context.delete_document(&reservation_key(reservation.request))?;
            recovery::finish(context, Work::Expire(reservation.request))?;
            advance_revision(context)?;
        }
        Ok(MutationResult::Success(PublishOutcome::Published {
            destination: reservation.destination,
        }))
    }
}

fn frame(context: &Transaction<'_>, reservation: &Reservation, ordinal: u64) -> Result<Records> {
    let result = context.query(
        "SELECT digest, payload FROM repository_issue_transfer_frames WHERE transfer = ? AND ordinal = ?",
        &[SqlValue::Blob(reservation.request.as_bytes().to_vec()), integer(ordinal)?],
    )?;
    let row = result
        .rows
        .first()
        .ok_or(Error::Invalid("Reserved issue frame is missing"))?;
    decode_frame(row, reservation.source)
}

fn decode_frame(row: &[SqlValue], source: Source) -> Result<Records> {
    let bytes = result_blob(row, 1)?;
    if blake3::hash(bytes).as_bytes() != result_blob(row, 0)? {
        return Err(Error::Invalid("Reserved issue frame digest does not match"));
    }
    let frame: Frame = serde_json::from_slice(bytes)?;
    if frame.source != source {
        return Err(Error::Invalid(
            "Reserved issue frame belongs to another source",
        ));
    }
    Ok(frame.records)
}

fn install(context: &mut Transaction<'_>, reservation: &Reservation, count: u64) -> Result<()> {
    let number = reservation.destination.number;
    let Records::Issue { header } = frame(context, reservation, 1)? else {
        return Err(Error::Invalid("Reserved issue transfer has no header"));
    };
    let mut issue = header.issue;
    issue.number = number;
    attachments::publish_transfer(
        context,
        reservation.request,
        reservation.source.repository,
        reservation.destination.repository,
        Target::Issue { number },
        &mut issue.body,
    )?;
    let names = header
        .labels
        .iter()
        .map(|name| name.to_lowercase())
        .collect::<std::collections::BTreeSet<_>>();
    issue.label_ids = ListLabels::execute(context, ())?
        .labels
        .into_iter()
        .filter(|label| names.contains(&label.name.to_lowercase()))
        .map(|label| label.number)
        .collect();
    issue.label_ids.sort_unstable();
    validate_issue(&issue)?;
    insert_issue(context, &issue)?;
    lifecycle::store(context, number, &header.lifecycle)?;
    let subject = Subject {
        kind: SubjectKind::Issue,
        number,
    };
    if header.lock.version != 0 {
        locks::store(context, subject, header.lock)?;
    }
    if let Some(selection) = header.issue_type {
        issue_type::store(
            context,
            number,
            issue_type::Assignment {
                version: reservation.source.issue_type_version,
                selection: Some(issue_type::Selection {
                    ownership_epoch: reservation.ownership_epoch,
                    ..selection
                }),
            },
        )?;
    }
    if let Some(milestone) = header.milestone {
        let matched = milestones::find_transfer_match(
            context,
            &milestone.title,
            milestone.due_on.as_deref(),
        )?;
        milestones::transfer_assignment(context, subject, matched)?;
    }

    let mut last = 0;
    let mut activity_from = count + 1;
    for ordinal in 2..=count {
        let Records::Comments { items, .. } = frame(context, reservation, ordinal)? else {
            activity_from = ordinal;
            break;
        };
        for item in items {
            let mut comment = item.record;
            comment.issue = number;
            let target = Target::IssueComment {
                issue: number,
                number: comment.number,
            };
            attachments::publish_transfer(
                context,
                reservation.request,
                reservation.source.repository,
                reservation.destination.repository,
                target,
                &mut comment.body,
            )?;
            validate_comment(&comment)?;
            last = last.max(comment.number);
            insert_comment(context, &comment)?;
            if let Some(visibility) = item.visibility {
                minimization::store(context, target, visibility)?;
            }
            search::contributions::project(context, target, &comment.author, &comment.body)?;
        }
        search::contributions::flush(context)?;
    }
    if last != 0 {
        context.query(
            "INSERT INTO repository_comment_sequences(issue_number, last) VALUES (?, ?)",
            &[integer(number)?, integer(last)?],
        )?;
    }

    for ordinal in (activity_from..=count).rev() {
        let Records::Activity { items, .. } = frame(context, reservation, ordinal)? else {
            return Err(Error::Invalid("Reserved issue activity is out of order"));
        };
        for mut event in items.into_iter().rev() {
            if relocate_event(&mut event, reservation.source.issue, number) {
                activity::import(context, event)?;
            }
        }
    }
    Ok(())
}

fn relocate_event(event: &mut activity::Event, source: u64, destination: u64) -> bool {
    // Keep the original frame for relationships that still identify resources
    // in the source repository. A numeric link must not resolve in the target.
    if !matches!(
        event.action,
        Action::Created
            | Action::Updated
            | Action::Closed
            | Action::Reopened
            | Action::Locked { .. }
            | Action::Unlocked
            | Action::CommentHidden { .. }
            | Action::CommentRestored
            | Action::IssueCloseReasonChanged { .. }
            | Action::IssueClosed {
                pull: None,
                commit: None,
                ..
            }
    ) {
        return false;
    }
    match &mut event.target {
        Target::Issue { number } if *number == source => *number = destination,
        Target::IssueComment { issue, .. } if *issue == source => *issue = destination,
        _ => return false,
    }
    true
}
