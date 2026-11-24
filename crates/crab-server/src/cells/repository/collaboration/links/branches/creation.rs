use super::*;
use crate::cells::directory::IdentityKey;

const PREFIX: &str = "collaboration/v1/development-branch-creation";
const RETRY_MS: u64 = 10_000;
const EXPIRE_MS: u64 = 15 * 60 * 1000;
const CLASS: &str = crate::cells::repository::CLASS;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub id: Uuid,
    pub issue: u64,
    pub branch: Branch,
    pub commit: String,
    pub actor: IdentityKey,
    pub target_epoch: u32,
}

impl Request {
    pub(crate) fn plan(&self, source: Uuid) -> String {
        let mut hash = blake3::Hasher::new();
        hash.update(b"crab-development-branch-creation-v1");
        hash.update(source.as_bytes());
        hash.update(self.id.as_bytes());
        hash.finalize().to_hex().to_string()
    }

    pub(crate) fn oid(&self) -> Result<gix_hash::ObjectId> {
        gix_hash::ObjectId::from_hex(self.commit.as_bytes())
            .ok()
            .filter(|oid| self.commit.len() == 40 && !oid.is_null())
            .ok_or(Error::Invalid("Invalid development branch starting commit"))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Pending,
    Created,
    Expired,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Job {
    pub request: Request,
    pub status: Status,
    due: u64,
    expires_at: u64,
}

fn key(id: Uuid) -> String {
    format!("{PREFIX}/requests/{id}")
}

fn issue_key(issue: u64) -> String {
    format!("{PREFIX}/issues/{issue:016x}")
}

const DUE: &str = "collaboration/v1/development-branch-creation/due/";

fn due_key(id: Uuid, due: u64) -> String {
    format!("{DUE}{due:020}/{id}")
}

pub(in crate::cells::repository) fn pending(
    context: &Transaction<'_>,
    issue: u64,
) -> Result<Option<Uuid>> {
    context.document(&issue_key(issue))
}

fn now(context: &Transaction<'_>) -> Result<u64> {
    crate::cells::repository::timestamp(context.now_ms())
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Decision {
    Saved(Status),
    Archived,
    Missing,
    Conflict,
    Limit,
}

pub(crate) struct Reserve;
impl Mutation for Reserve {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 463;
    type Input = Request;
    type Output = Decision;

    fn execute(context: &mut Transaction<'_>, input: Request) -> Result<MutationResult<Decision>> {
        input.branch.validate()?;
        input.actor.validate()?;
        input.oid()?;
        if input.id.is_nil() {
            return Err(Error::Invalid("Development branch request ID is invalid"));
        }
        if let Some(saved) = context.document::<Job>(&key(input.id))? {
            return Ok(if saved.request == input {
                MutationResult::Success(Decision::Saved(saved.status))
            } else {
                MutationResult::Rejected(Decision::Conflict)
            });
        }
        if settings::archived(context)? {
            return Ok(MutationResult::Rejected(Decision::Archived));
        }
        if !exists(context, input.issue)? {
            return Ok(MutationResult::Rejected(Decision::Missing));
        }
        let state = load(context, input.issue)?;
        if pending(context, input.issue)?.is_some()
            || state.branches.binary_search(&input.branch).is_ok()
        {
            return Ok(MutationResult::Rejected(Decision::Conflict));
        }
        if state.branches.len() >= MAX_BRANCHES {
            return Ok(MutationResult::Rejected(Decision::Limit));
        }
        let now = now(context)?;
        let job = Job {
            request: input,
            status: Status::Pending,
            due: now,
            expires_at: now.saturating_add(EXPIRE_MS),
        };
        context.put_document(&issue_key(job.request.issue), &job.request.id)?;
        save(context, &job)?;
        advance_revision(context)?;
        Ok(MutationResult::Success(Decision::Saved(Status::Pending)))
    }
}

fn save(context: &Transaction<'_>, job: &Job) -> Result<()> {
    if job.status == Status::Pending {
        context.put_document(&due_key(job.request.id, job.due), &job.request.id)?;
    }
    context.put_document(&key(job.request.id), job)
}

pub(crate) struct Get;
impl Read for Get {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 463;
    type Input = Uuid;
    type Output = Option<Job>;

    fn execute(context: &mut Transaction<'_>, id: Uuid) -> Result<Option<Job>> {
        context.document(&key(id))
    }
}

pub(super) fn for_issue(context: &Transaction<'_>, issue: u64) -> Result<Option<Request>> {
    let Some(id) = pending(context, issue)? else {
        return Ok(None);
    };
    let job = context.document::<Job>(&key(id))?.ok_or(Error::Invalid(
        "Pending development branch request is missing",
    ))?;
    Ok(Some(job.request))
}

#[derive(Deserialize, Serialize)]
pub(crate) struct DueJob {
    pub request: Request,
    pub expired: bool,
}
pub(crate) struct Due;
impl Read for Due {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 464;
    type Input = ();
    type Output = Vec<DueJob>;

    fn execute(context: &mut Transaction<'_>, (): ()) -> Result<Vec<DueJob>> {
        let now = now(context)?;
        let mut jobs = Vec::new();
        for (_, id) in context.documents_after::<Uuid>(DUE, None, 8)? {
            let job = context.document::<Job>(&key(id))?.ok_or(Error::Invalid(
                "Scheduled development branch request is missing",
            ))?;
            if job.due > now {
                break;
            }
            jobs.push(DueJob {
                request: job.request,
                expired: job.expires_at <= now,
            });
        }
        Ok(jobs)
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Resolution {
    Committed,
    Abandoned,
    Defer,
}

pub(crate) struct Settle;
impl Mutation for Settle {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 464;
    type Input = (Uuid, Resolution);
    type Output = Option<Status>;

    fn execute(
        context: &mut Transaction<'_>,
        (id, resolution): Self::Input,
    ) -> Result<MutationResult<Self::Output>> {
        let Some(mut job) = context.document::<Job>(&key(id))? else {
            return Ok(MutationResult::Rejected(None));
        };
        if job.status != Status::Pending {
            return Ok(MutationResult::Success(Some(job.status)));
        }
        let issue = job.request.issue;
        context.delete_document(&due_key(id, job.due))?;
        match resolution {
            Resolution::Defer => {
                job.due = now(context)?.saturating_add(RETRY_MS);
            }
            Resolution::Committed => {
                // The reservation owns one slot and prevents the issue moving
                // while Git can still publish its exact branch creation.
                let mut state = load(context, issue)?;
                if let Err(index) = state.branches.binary_search(&job.request.branch) {
                    state.branches.insert(index, job.request.branch.clone());
                    state.version = next_version(state.version)?;
                    install(context, issue, &state)?;
                }
                job.status = Status::Created;
            }
            Resolution::Abandoned => job.status = Status::Expired,
        }
        if job.status != Status::Pending {
            context.delete_document(&issue_key(issue))?;
            advance_revision(context)?;
        }
        save(context, &job)?;
        Ok(MutationResult::Success(Some(job.status)))
    }
}
