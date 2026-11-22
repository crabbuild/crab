use super::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) mod dispatches;

const ISSUER: &str = "urn:crab:integration:github-actions";
const ACTIVE: &str = "ci/v1/connections";
const MAX_CONNECTIONS: usize = 16;
const DELIVERY_HISTORY_LIMIT: usize = 50;

pub(crate) fn github_connection(issuer: &str, subject: &str) -> Option<Uuid> {
    (issuer == ISSUER)
        .then(|| Uuid::parse_str(subject).ok())
        .flatten()
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Connection {
    pub id: Uuid,
    pub name: String,
    pub github_repository: u64,
    pub workflow: u64,
    pub credential: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch: Option<dispatches::Configuration>,
}

fn connections(context: &Transaction<'_>) -> crate::cells::Result<Vec<Connection>> {
    Ok(context.document(ACTIVE)?.unwrap_or_default())
}

pub(crate) struct List;
impl Read for List {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 405;
    type Input = ();
    type Output = Vec<Connection>;

    fn execute(context: &mut Transaction<'_>, _: ()) -> crate::cells::Result<Self::Output> {
        connections(context)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Change {
    Connect {
        id: Uuid,
        name: String,
        github_repository: u64,
        workflow: u64,
    },
    Dispatch {
        id: Uuid,
        expected: Uuid,
        previous: Option<Uuid>,
        dispatch: Option<dispatches::Configuration>,
    },
    Rotate {
        id: Uuid,
        expected: Uuid,
        credential: Uuid,
    },
    Disconnect {
        id: Uuid,
        expected: Uuid,
    },
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Changed {
    Saved(Connection),
    Disconnected,
    Conflict,
    NotFound,
    AtCapacity,
    Archived,
}

pub(crate) struct Edit;
impl Mutation for Edit {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 405;
    type Input = Change;
    type Output = Changed;

    fn execute(
        context: &mut Transaction<'_>,
        input: Change,
    ) -> crate::cells::Result<MutationResult<Changed>> {
        use MutationResult::{Rejected, Success};
        let mut rows = connections(context)?;
        let saved = match input {
            Change::Connect {
                id,
                name,
                github_repository,
                workflow,
            } => {
                checks::validate_name(&name)?;
                validate_number(github_repository)?;
                validate_number(workflow)?;
                let new = Connection {
                    id,
                    name,
                    github_repository,
                    workflow,
                    credential: id,
                    dispatch: None,
                };
                if let Some(current) = rows.iter().find(|row| row.id == id) {
                    return Ok(if current == &new {
                        Success(Changed::Saved(new))
                    } else {
                        Rejected(Changed::Conflict)
                    });
                }
                if context.document::<bool>(&revoked(id))?.is_some() {
                    return Ok(Rejected(Changed::Conflict));
                }
                if settings::archived(context)? {
                    return Ok(Rejected(Changed::Archived));
                }
                if rows.len() == MAX_CONNECTIONS {
                    return Ok(Rejected(Changed::AtCapacity));
                }
                rows.push(new.clone());
                Changed::Saved(new)
            }
            Change::Dispatch {
                id,
                expected,
                previous,
                dispatch,
            } => {
                if dispatch.as_ref().is_some_and(|config| !config.valid()) {
                    return Err(crate::cells::Error::Invalid(
                        "Invalid GitHub Actions dispatch settings",
                    ));
                }
                let Some(current) = rows.iter_mut().find(|row| row.id == id) else {
                    return Ok(Rejected(Changed::NotFound));
                };
                if current.credential != expected {
                    return Ok(Rejected(Changed::Conflict));
                }
                if current.dispatch == dispatch {
                    return Ok(Success(Changed::Saved(current.clone())));
                }
                if current.dispatch.as_ref().map(|config| config.id) != previous
                    || dispatch
                        .as_ref()
                        .is_some_and(|config| previous == Some(config.id))
                {
                    return Ok(Rejected(Changed::Conflict));
                }
                if settings::archived(context)? {
                    return Ok(Rejected(Changed::Archived));
                }
                // A removed dispatch generation cannot reauthorize an old run.
                if let Some(previous) = previous {
                    context
                        .put_document(&format!("ci/v1/dispatch-retired/{id}/{previous}"), &true)?;
                }
                if let Some(config) = &dispatch
                    && context
                        .document::<bool>(&format!("ci/v1/dispatch-retired/{id}/{}", config.id))?
                        .is_some()
                {
                    return Ok(Rejected(Changed::Conflict));
                }
                current.dispatch = dispatch;
                Changed::Saved(current.clone())
            }
            Change::Rotate {
                id,
                expected,
                credential,
            } => {
                let Some(current) = rows.iter_mut().find(|row| row.id == id) else {
                    return Ok(Rejected(Changed::NotFound));
                };
                if current.credential == credential {
                    return Ok(Success(Changed::Saved(current.clone())));
                }
                if current.credential != expected
                    || context
                        .document::<bool>(&retired(id, credential))?
                        .is_some()
                {
                    return Ok(Rejected(Changed::Conflict));
                }
                context.put_document(&retired(id, expected), &true)?;
                current.credential = credential;
                if let Some(config) = &mut current.dispatch {
                    context.put_document(
                        &format!("ci/v1/dispatch-retired/{id}/{}", config.id),
                        &true,
                    )?;
                    config.id = Uuid::now_v7();
                }
                Changed::Saved(current.clone())
            }
            Change::Disconnect { id, expected } => {
                let Some(index) = rows.iter().position(|row| row.id == id) else {
                    return Ok(if context.document::<bool>(&revoked(id))?.is_some() {
                        Success(Changed::Disconnected)
                    } else {
                        Rejected(Changed::NotFound)
                    });
                };
                if rows[index].credential != expected {
                    return Ok(Rejected(Changed::Conflict));
                }
                context.put_document(&revoked(id), &true)?;
                context.delete_document(&history_key(id))?;
                rows.remove(index);
                Changed::Disconnected
            }
        };
        context.put_document(ACTIVE, &rows)?;
        Ok(Success(saved))
    }
}

fn revoked(id: Uuid) -> String {
    format!("ci/v1/revoked/{id}")
}

fn retired(id: Uuid, credential: Uuid) -> String {
    format!("ci/v1/retired/{id}/{credential}")
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Delivery {
    pub connection: Uuid,
    pub id: Uuid,
    pub digest: [u8; 32],
}

impl Delivery {
    fn key(&self) -> String {
        format!("ci/v1/deliveries/{}/{}", self.connection, self.id)
    }
}

pub(crate) struct Receipt;
impl Read for Receipt {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 406;
    type Input = Delivery;
    type Output = Option<bool>;

    fn execute(
        context: &mut Transaction<'_>,
        input: Delivery,
    ) -> crate::cells::Result<Self::Output> {
        Ok(context
            .document::<[u8; 32]>(&input.key())?
            .map(|digest| digest == input.digest))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeliverySummary {
    pub id: Uuid,
    pub received_at_ms: u64,
    pub oid: String,
    pub run_number: u64,
    pub attempt: u64,
    pub status: u8,
    pub conclusion: Option<u8>,
    pub details_url: Option<String>,
    pub updated_check: bool,
}

fn history_key(connection: Uuid) -> String {
    format!("ci/v1/delivery-history/{connection}")
}

pub(crate) struct DeliveryHistory;
impl Read for DeliveryHistory {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 429;
    type Input = Uuid;
    type Output = Option<Vec<DeliverySummary>>;

    fn execute(context: &mut Transaction<'_>, id: Uuid) -> crate::cells::Result<Self::Output> {
        if !connections(context)?.iter().any(|row| row.id == id) {
            return Ok(None);
        }
        Ok(Some(
            context.document(&history_key(id))?.unwrap_or_default(),
        ))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Report {
    pub delivery: Delivery,
    pub credential: Uuid,
    pub github_repository: u64,
    pub workflow: u64,
    pub run_id: u64,
    pub run_number: u64,
    pub attempt: u64,
    pub oid: String,
    pub report: CheckReportInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch: Option<Uuid>,
}

#[derive(Deserialize, Serialize)]
struct Cursor {
    run_id: u64,
    order: (u64, u64),
    status: u8,
    conclusion: Option<u8>,
    number: u64,
    version: u64,
}

fn run_key(connection: Uuid, oid: &str, generation: Option<Uuid>) -> String {
    if let Some(generation) = generation {
        format!("ci/v1/dispatched-runs/{connection}/{generation}/{oid}")
    } else {
        format!("ci/v1/runs/{connection}/{oid}")
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Requirements {
    pub oid: String,
    pub connections: Vec<Uuid>,
    #[serde(default)]
    pub queued_candidate: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequiredConnection {
    pub id: Uuid,
    pub name: String,
    pub run: Option<CheckRunRecord>,
}

pub(crate) struct ReadRequirements;
impl Read for ReadRequirements {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 407;
    type Input = Requirements;
    type Output = Vec<RequiredConnection>;

    fn execute(
        context: &mut Transaction<'_>,
        input: Requirements,
    ) -> crate::cells::Result<Self::Output> {
        checks::validate_commit(&input.oid)?;
        if input.connections.len() > MAX_CONNECTIONS * (super::rulesets::MAX_RULESETS + 1) {
            return Err(crate::cells::Error::Invalid(
                "Too many required integrations",
            ));
        }
        let active = connections(context)?;
        // An active connection and its selected exact-commit attempt must
        // come from the same repository Cell snapshot at publication.
        active
            .into_iter()
            .filter(|row| input.connections.contains(&row.id))
            .map(|row| {
                let generation = row
                    .dispatch
                    .as_ref()
                    .filter(|_| input.queued_candidate)
                    .map(|config| config.id);
                let run = if let Some(cursor) =
                    context.document::<Cursor>(&run_key(row.id, &input.oid, generation))?
                {
                    let detail = GetCheckRun::execute(
                        context,
                        CheckRunKey {
                            oid: input.oid.clone(),
                            number: cursor.number,
                        },
                    )?
                    .ok_or(crate::cells::Error::Invalid(
                        "Required workflow result is missing",
                    ))?;
                    if detail.run.version != cursor.version
                        || github_connection(&detail.run.author.issuer, &detail.run.author.subject)
                            != Some(row.id)
                    {
                        return Err(crate::cells::Error::Invalid(
                            "Required workflow result changed outside its connection",
                        ));
                    }
                    Some(detail.run)
                } else {
                    None
                };
                Ok(RequiredConnection {
                    id: row.id,
                    name: row.name,
                    run,
                })
            })
            .collect()
    }
}

#[derive(Deserialize, Serialize)]
pub(crate) enum Reported {
    Recorded,
    Conflict,
    Revoked,
    AtCapacity,
    Archived,
}

pub(crate) struct Record;
impl Mutation for Record {
    const CLASS: &'static str = CLASS;
    const ID: u32 = 406;
    type Input = Report;
    type Output = Reported;

    fn execute(
        context: &mut Transaction<'_>,
        input: Report,
    ) -> crate::cells::Result<MutationResult<Reported>> {
        use MutationResult::{Rejected, Success};
        let id = input.delivery.connection;
        let Some(connection) = connections(context)?.into_iter().find(|row| row.id == id) else {
            return Ok(Rejected(Reported::Revoked));
        };
        if connection.credential != input.credential
            || connection.github_repository != input.github_repository
            || connection.workflow != input.workflow
        {
            return Ok(Rejected(Reported::Revoked));
        }
        let receipt = input.delivery.key();
        if let Some(digest) = context.document::<[u8; 32]>(&receipt)? {
            return Ok(if digest == input.delivery.digest {
                Success(Reported::Recorded)
            } else {
                Rejected(Reported::Conflict)
            });
        }
        if settings::archived(context)? {
            return Ok(Rejected(Reported::Archived));
        }
        checks::validate_commit(&input.oid)?;
        for number in [input.run_id, input.run_number, input.attempt] {
            validate_number(number)?;
        }
        let identity = format!("ci/v1/run-identities/{id}/{}", input.run_id);
        let origin = (input.oid.as_str(), input.run_number);
        if let Some(saved) = context.document::<(String, u64)>(&identity)? {
            if (saved.0.as_str(), saved.1) != origin {
                return Ok(Rejected(Reported::Conflict));
            }
        } else {
            context.put_document(&identity, &origin)?;
        }
        let generation = if let Some(request) = input.dispatch {
            let Some(generation) = dispatches::bind(
                context,
                &dispatches::Key {
                    connection: id,
                    oid: input.oid.clone(),
                },
                request,
                input.run_id,
            )?
            else {
                return Ok(Rejected(Reported::Revoked));
            };
            Some(generation)
        } else {
            None
        };
        let key = run_key(id, &input.oid, generation);
        let order = (input.run_number, input.attempt);
        let current = context.document::<Cursor>(&key)?;
        let same_run = current.as_ref().filter(|row| row.order == order);
        if same_run.is_some_and(|row| {
            row.run_id != input.run_id
                || (row.status == 2
                    && input.report.status == 2
                    && row.conclusion != input.report.conclusion)
        }) {
            return Ok(Rejected(Reported::Conflict));
        }
        // GitHub retries and reordered events cannot downgrade a run or allow
        // an older attempt to replace the result used by branch protection.
        let stale = current.as_ref().is_some_and(|row| row.order > order)
            || same_run.is_some_and(|row| row.status >= input.report.status);
        let summary = DeliverySummary {
            id: input.delivery.id,
            received_at_ms: timestamp(context.now_ms())?,
            oid: input.oid.clone(),
            run_number: input.run_number,
            attempt: input.attempt,
            status: input.report.status,
            conclusion: input.report.conclusion,
            details_url: input.report.details_url.clone(),
            updated_check: !stale,
        };
        if !stale {
            let cursor = Cursor {
                run_id: input.run_id,
                order,
                status: input.report.status,
                conclusion: input.report.conclusion,
                number: 0,
                version: 0,
            };
            let actor = RepositoryAuthor {
                issuer: ISSUER.into(),
                subject: id.to_string(),
                name: "GitHub Actions".into(),
            };
            let submission_id = *Uuid::now_v7().as_bytes();
            let detail = if let Some(current) = same_run {
                match UpdateCheckRun::execute(
                    context,
                    UpdateCheckRunInput {
                        submission_id,
                        actor,
                        oid: input.oid,
                        number: current.number,
                        version: current.version,
                        report: input.report,
                    },
                )? {
                    Success(UpdateCheckRunOutcome::Updated(detail)) => detail,
                    _ => {
                        return Err(crate::cells::Error::Invalid(
                            "CI check update did not match its run",
                        ));
                    }
                }
            } else {
                match CreateCheckRun::execute(
                    context,
                    CreateCheckRunInput {
                        submission_id,
                        author: actor,
                        oid: input.oid,
                        name: connection.name,
                        report: input.report,
                    },
                )? {
                    Success(CreateCheckRunOutcome::Created(detail)) => detail,
                    Rejected(CreateCheckRunOutcome::RunLimit) => {
                        return Ok(Rejected(Reported::AtCapacity));
                    }
                    _ => {
                        return Err(crate::cells::Error::Invalid(
                            "CI check creation did not match its run",
                        ));
                    }
                }
            };
            context.put_document(
                &key,
                &Cursor {
                    number: detail.run.number,
                    version: detail.run.version,
                    ..cursor
                },
            )?;
        }
        // History, the check and the deduplication receipt commit together.
        // Trimming history does not drop an old delivery's receipt.
        let history = history_key(id);
        let mut recent: Vec<DeliverySummary> = context.document(&history)?.unwrap_or_default();
        recent.truncate(DELIVERY_HISTORY_LIMIT - 1);
        recent.insert(0, summary);
        context.put_document(&history, &recent)?;
        context.put_document(&receipt, &input.delivery.digest)?;
        Ok(Success(Reported::Recorded))
    }
}
