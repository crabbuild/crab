use std::collections::BTreeMap;

use super::*;
use crate::cells::{
    Result,
    repository::{
        GetRepositoryLifecycle,
        issues::{
            self,
            transfer::publication::{self, Destination},
        },
    },
};
use uuid::Uuid;

const PENDING: &str = "collaboration/v1/links/commit-publications/due/";
const RETRY_MS: u64 = 10_000;
const RESERVATION_MS: u64 = 24 * 60 * 60 * 1000;
pub(crate) const MAX_CLOSURES: usize = 100;

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Publication {
    pub plan: String,
    pub reference: String,
    pub before: Option<String>,
    pub after: String,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Reservation {
    pub publication: Publication,
    pub actor: RepositoryAuthor,
    pub issues: BTreeMap<u64, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cross: Option<CrossClosure>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct CrossClosure {
    pub organization: Uuid,
    pub issues: Vec<CapturedIssue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<PublishedDescription>,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct PublishedDescription {
    pub pull: u64,
    pub issues: Vec<Destination>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(crate) struct CapturedIssue {
    pub origin: Destination,
    pub state_version: u64,
}

#[derive(Deserialize, Serialize)]
struct Job {
    due: u64,
    created_at: u64,
    reservation: Reservation,
}

fn job_key(plan: &str) -> String {
    format!("{ROOT}/commit-publications/plans/{plan}")
}

fn due_key(plan: &str, due: u64) -> String {
    format!("{PENDING}{due:020}/{plan}")
}

fn is_oid(oid: &str) -> bool {
    oid.len() == 40 && gix_hash::ObjectId::from_hex(oid.as_bytes()).is_ok()
}

pub(crate) struct Reserve;
impl Mutation for Reserve {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 441;
    type Input = Reservation;
    type Output = bool;

    fn execute(
        context: &mut Transaction<'_>,
        mut input: Reservation,
    ) -> Result<MutationResult<bool>> {
        validate_author(&input.actor)?;
        let publication = &input.publication;
        if blake3::Hash::from_hex(&publication.plan).is_err()
            || !publication.reference.starts_with("refs/heads/")
            || !is_oid(&publication.after)
            || publication
                .before
                .as_deref()
                .is_some_and(|oid| !is_oid(oid) || oid == publication.after)
            || input.issues.len() > MAX_CLOSURES
        {
            return Err(crate::cells::Error::Invalid(
                "invalid issue closure publication",
            ));
        }
        if let Some(cross) = &input.cross {
            if cross.organization.is_nil()
                || (cross.issues.is_empty() && cross.description.is_none())
                || cross.issues.len() > 2 * MAX_CLOSING_ISSUES
            {
                return Err(crate::cells::Error::Invalid(
                    "invalid cross-repository issue closure",
                ));
            }
            if let Some(description) = &cross.description {
                validate_number(description.pull)?;
                if description.issues.len() > MAX_CLOSING_ISSUES {
                    return Err(crate::cells::Error::Invalid("Too many described issues"));
                }
                for issue in &description.issues {
                    validate_number(issue.number)?;
                    if issue.repository.is_nil() {
                        return Err(crate::cells::Error::Invalid(
                            "Invalid described issue repository",
                        ));
                    }
                }
            }
            for issue in &cross.issues {
                if issue.origin.repository.is_nil() {
                    return Err(crate::cells::Error::Invalid(
                        "invalid closing issue repository",
                    ));
                }
                validate_number(issue.origin.number)?;
                if issue.state_version != 0 {
                    validate_number(issue.state_version)?;
                }
            }
        }
        let key = job_key(&publication.plan);
        if let Some(existing) = context.document::<Option<Job>>(&key)? {
            return Ok(MutationResult::Rejected(existing.is_some()));
        }
        let mut open = BTreeMap::new();
        for (number, oid) in input.issues {
            validate_number(number)?;
            if !is_oid(&oid) {
                return Err(crate::cells::Error::Invalid("invalid issue closing commit"));
            }
            if issues::GetIssue::execute(context, number)?.is_some_and(|issue| issue.state == 0) {
                open.insert(number, oid);
            }
        }
        if open.is_empty() && input.cross.is_none() {
            return Ok(MutationResult::Rejected(false));
        }
        issues::lifecycle::ensure_ready(context)?;
        let now = crate::cells::repository::timestamp(context.now_ms())?;
        let due = due_key(&publication.plan, now);
        context.put_document(&due, &publication.plan)?;
        input.issues = open;
        context.put_document(
            &key,
            &Some(Job {
                due: now,
                created_at: now,
                reservation: input,
            }),
        )?;
        Ok(MutationResult::Success(true))
    }
}

pub(crate) struct Cross;
impl Read for Cross {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 468;
    type Input = String;
    type Output = Option<(CrossClosure, RepositoryAuthor)>;

    fn execute(context: &mut Transaction<'_>, plan: String) -> Result<Self::Output> {
        Ok(context
            .document::<Option<Job>>(&job_key(&plan))?
            .flatten()
            .and_then(|job| {
                job.reservation
                    .cross
                    .map(|cross| (cross, job.reservation.actor))
            }))
    }
}

pub(crate) struct OpenStateVersion;
impl Read for OpenStateVersion {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 469;
    type Input = u64;
    type Output = Option<u64>;

    fn execute(context: &mut Transaction<'_>, issue: u64) -> Result<Self::Output> {
        let subject = Subject {
            kind: SubjectKind::Issue,
            number: issue,
        };
        if subject.closed(context)? != Some(false) {
            return Ok(None);
        }
        Ok(Some(
            issues::lifecycle::state_version(context, issue)?.unwrap_or_default(),
        ))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) struct DeliverCross {
    pub target: Destination,
    pub captured: CapturedIssue,
    pub actor: RepositoryAuthor,
}

pub(crate) struct CloseCross;
impl Mutation for CloseCross {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 468;
    type Input = DeliverCross;
    type Output = bool;

    fn execute(context: &mut Transaction<'_>, input: DeliverCross) -> Result<MutationResult<bool>> {
        let number = input.target.number;
        let Some(issue) = issues::GetIssue::execute(context, number)? else {
            return Ok(MutationResult::Rejected(false));
        };
        if publication::origin(context, number)?.unwrap_or(input.target) != input.captured.origin {
            return Err(crate::cells::Error::Invalid(
                "closing issue has a different origin",
            ));
        }
        // The issue carries its state revision through transfers. A later
        // reopen wins over a delayed publication, including a lost reply.
        if issue.state != 0
            || issues::lifecycle::state_version(context, number)?.unwrap_or_default()
                != input.captured.state_version
            || GetRepositoryLifecycle::execute(context, ())?.archived
        {
            return Ok(MutationResult::Rejected(true));
        }
        issues::close_linked_issue(
            context,
            number,
            issues::ClosingSource::ExternalPull,
            &input.actor,
        )?;
        Ok(MutationResult::Success(true))
    }
}

pub(crate) struct Next;
impl Read for Next {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 441;
    type Input = ();
    type Output = Option<(u64, Publication)>;

    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<Self::Output> {
        let now = crate::cells::repository::timestamp(context.now_ms())?;
        let Some((_, plan)) = context
            .documents_after::<String>(PENDING, None, 1)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        Ok(context
            .document::<Option<Job>>(&job_key(&plan))?
            .flatten()
            .filter(|job| job.due <= now)
            .map(|job| (job.due, job.reservation.publication)))
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Resolution {
    Committed(String),
    Uncommitted { plan: String, due: u64 },
}

pub(crate) struct Resolve;
impl Mutation for Resolve {
    const CLASS: &'static str = crate::cells::repository::CLASS;
    const ID: u32 = 442;
    type Input = Resolution;
    type Output = ();

    fn execute(context: &mut Transaction<'_>, input: Resolution) -> Result<MutationResult<()>> {
        let plan = match &input {
            Resolution::Committed(plan) | Resolution::Uncommitted { plan, .. } => plan,
        };
        let key = job_key(plan);
        let Some(mut job) = context.document::<Option<Job>>(&key)?.flatten() else {
            return Ok(MutationResult::Rejected(()));
        };
        match input {
            Resolution::Committed(_) => {
                context.delete_document(&due_key(plan, job.due))?;
                if let Some(description) = job.reservation.cross.and_then(|cross| cross.description)
                {
                    keywords::projection::record_publication(
                        context,
                        description.pull,
                        &description.issues,
                    )?;
                }
                // Receipt recovery and live completion share this transaction.
                // Removing the job with the transitions prevents a retry closing a reopened issue.
                for (number, oid) in job.reservation.issues {
                    issues::close_linked_issue(
                        context,
                        number,
                        issues::ClosingSource::Commit(&oid),
                        &job.reservation.actor,
                    )?;
                }
                context.put_document(&key, &Option::<Job>::None)?;
            }
            Resolution::Uncommitted { due, .. } if due == job.due => {
                context.delete_document(&due_key(plan, job.due))?;
                let now = crate::cells::repository::timestamp(context.now_ms())?;
                if now.saturating_sub(job.created_at) > RESERVATION_MS {
                    context.delete_document(&key)?;
                } else {
                    job.due = now.saturating_add(RETRY_MS);
                    context.put_document(&due_key(plan, job.due), plan)?;
                    context.put_document(&key, &Some(job))?;
                }
            }
            Resolution::Uncommitted { .. } => return Ok(MutationResult::Rejected(())),
        }
        Ok(MutationResult::Success(()))
    }
}
