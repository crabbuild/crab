use crab_cell_runtime::{Committed, InvocationError, MutationIdentity, Observed, RequestId};
use serde::{Deserialize, Serialize};
use tokio::time::{Duration, Instant, sleep};
use uuid::Uuid;

use crate::{
    BranchProtection,
    app::{Error, Result},
    auth::Identity,
    cells::{
        RepositoryCell, RepositoryCellRouter,
        repository::{
            BranchProtectionRecord, BranchProtectionSettings, GetBranchProtections,
            GetRepositoryLifecycle, ReplaceBranchProtections, ReplaceBranchProtectionsInput,
            ReplaceBranchProtectionsOutcome, ReplaceRepositoryLifecycle,
            ReplaceRepositoryLifecycleInput, ReplaceRepositoryLifecycleOutcome,
            RepositoryLifecycleRecord,
        },
    },
    config::valid_branch_protections,
    server::{Repository, Server},
};

const MAX_NUMBER: u64 = 9_007_199_254_740_991;
const ROUTE_RETRY_TIMEOUT: Duration = Duration::from_secs(30);
const ROUTE_RETRY_BASE: Duration = Duration::from_millis(25);
const ROUTE_RETRY_MAX_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BranchProtections {
    pub version: u64,
    pub rules: Vec<BranchProtection>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryLifecycle {
    pub version: u64,
    pub archived: bool,
}

impl BranchProtections {
    pub(crate) fn configured(rules: &[BranchProtection]) -> Self {
        Self {
            version: 0,
            rules: rules.to_vec(),
        }
    }

    pub(crate) fn protection(&self, reference: &str) -> Option<&BranchProtection> {
        let branch = reference.strip_prefix("refs/heads/")?;
        self.rules.iter().find(|rule| rule.branch == branch)
    }
}

pub(crate) async fn load(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
) -> Result<BranchProtections> {
    let routed = route(server, repo, actor, "repository.read").await?;
    let stored = query_output(
        routed
            .client
            .query::<GetBranchProtections>(&routed.target, None, ())
            .await,
    )?;
    stored.map_or_else(
        || {
            Ok(BranchProtections::configured(
                &repo.config.protected_branches,
            ))
        },
        branch_protections,
    )
}

pub(crate) async fn replace(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    expected_version: u64,
    rules: Vec<BranchProtection>,
) -> Result<BranchProtections> {
    if expected_version >= MAX_NUMBER || !valid_branch_protections(&rules) {
        return Err(Error::Invalid(
            "Protection rules require at most 100 unique valid branches, 0–20 approvals, and at most 50 unique check names",
        ));
    }
    let routed = route(server, repo, actor, "repository.settings.protections").await?;
    match command_output(
        routed
            .client
            .command::<ReplaceBranchProtections>(
                &routed.target,
                mutation_identity()?,
                ReplaceBranchProtectionsInput {
                    expected_version,
                    rules: rules.into_iter().map(protection_record).collect(),
                },
            )
            .await,
    )? {
        ReplaceBranchProtectionsOutcome::Updated(settings) => branch_protections(settings),
        ReplaceBranchProtectionsOutcome::Conflict => Err(Error::Conflict),
    }
}

pub(crate) async fn load_lifecycle(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
) -> Result<RepositoryLifecycle> {
    let routed = route(server, repo, actor, "repository.read").await?;
    repository_lifecycle(query_output(
        routed
            .client
            .query::<GetRepositoryLifecycle>(&routed.target, None, ())
            .await,
    )?)
}

pub(crate) async fn replace_lifecycle(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    expected_version: u64,
    archived: bool,
) -> Result<RepositoryLifecycle> {
    if expected_version >= MAX_NUMBER {
        return Err(Error::Conflict);
    }
    let routed = route(server, repo, actor, "repository.settings.lifecycle").await?;
    match command_output(
        routed
            .client
            .command::<ReplaceRepositoryLifecycle>(
                &routed.target,
                mutation_identity()?,
                ReplaceRepositoryLifecycleInput {
                    expected_version,
                    archived,
                },
            )
            .await,
    )? {
        ReplaceRepositoryLifecycleOutcome::Updated(lifecycle) => repository_lifecycle(lifecycle),
        ReplaceRepositoryLifecycleOutcome::Conflict => Err(Error::Conflict),
        ReplaceRepositoryLifecycleOutcome::Unchanged => {
            Err(Error::Invalid("Repository lifecycle is unchanged"))
        }
    }
}

async fn route(
    server: &Server,
    repository: &Repository,
    principal: &Identity,
    action: &'static str,
) -> Result<RepositoryCell> {
    let router: &RepositoryCellRouter = server
        .repository_cells
        .as_ref()
        .ok_or(Error::CellUnavailable)?;
    let deadline = Instant::now() + ROUTE_RETRY_TIMEOUT;
    let mut delay = ROUTE_RETRY_BASE;
    loop {
        match router.route(repository.id, principal, action).await {
            Ok(cell) => return Ok(cell),
            Err(crate::Error::Cell(source))
                if retryable_route_error(&source) && Instant::now() < deadline =>
            {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let wait = delay.min(remaining);
                tokio::select! {
                    () = server.cancellation.cancelled() => {
                        return Err(Error::Cell(crab_cell_runtime::Error::RuntimeClosed));
                    }
                    () = sleep(wait) => {}
                }
                delay = delay
                    .checked_mul(2)
                    .unwrap_or(ROUTE_RETRY_MAX_DELAY)
                    .min(ROUTE_RETRY_MAX_DELAY);
            }
            Err(crate::Error::Cell(source)) => return Err(Error::Cell(source)),
            Err(source) => return Err(Error::Repository(source)),
        }
    }
}

fn retryable_route_error(error: &crab_cell_runtime::Error) -> bool {
    matches!(
        error,
        crab_cell_runtime::Error::Capacity(_)
            | crab_cell_runtime::Error::CellNotActive
            | crab_cell_runtime::Error::CellDraining
            | crab_cell_runtime::Error::Deadline
    )
}

fn protection_record(rule: BranchProtection) -> BranchProtectionRecord {
    BranchProtectionRecord {
        branch: rule.branch,
        required_approvals: rule.required_approvals,
        required_checks: rule.required_checks,
    }
}

fn branch_protections(settings: BranchProtectionSettings) -> Result<BranchProtections> {
    let settings = BranchProtections {
        version: settings.version,
        rules: settings
            .rules
            .into_iter()
            .map(|rule| BranchProtection {
                branch: rule.branch,
                required_approvals: rule.required_approvals,
                required_checks: rule.required_checks,
            })
            .collect(),
    };
    if settings.version == 0
        || settings.version >= MAX_NUMBER
        || !valid_branch_protections(&settings.rules)
    {
        return Err(Error::CellContract(
            "Cell returned invalid branch protections",
        ));
    }
    Ok(settings)
}

fn repository_lifecycle(record: RepositoryLifecycleRecord) -> Result<RepositoryLifecycle> {
    if record.version >= MAX_NUMBER {
        return Err(Error::CellContract(
            "Cell returned an invalid repository lifecycle",
        ));
    }
    Ok(RepositoryLifecycle {
        version: record.version,
        archived: record.archived,
    })
}

fn mutation_identity() -> Result<MutationIdentity> {
    let now_ms = crate::cells::unix_now_ms().map_err(Error::Repository)?;
    let expires_at_ms = now_ms
        .checked_add(60_000)
        .ok_or(Error::CellContract("Cell request expiry overflowed"))?;
    Ok(MutationIdentity {
        request_id: RequestId::from_bytes(Uuid::now_v7().into_bytes()),
        issued_at_ms: now_ms,
        expires_at_ms,
    })
}

fn command_output<T>(result: std::result::Result<Committed<T>, InvocationError<T>>) -> Result<T> {
    match result {
        Ok(committed) => Ok(committed.output),
        Err(InvocationError::Rejected(committed)) => Ok(committed.output),
        Err(InvocationError::Pending(_)) => Err(Error::CellPending),
        Err(InvocationError::InvalidPublishedResult { source, .. }) => Err(Error::Cell(*source)),
        Err(InvocationError::NotStarted(source)) => Err(Error::Cell(source)),
    }
}

fn query_output<T>(result: std::result::Result<Observed<T>, InvocationError<T>>) -> Result<T> {
    match result {
        Ok(observed) => Ok(observed.output),
        Err(InvocationError::Rejected(_)) => Err(Error::CellContract(
            "Cell query returned a durable rejection",
        )),
        Err(InvocationError::Pending(_)) => Err(Error::CellContract(
            "Cell query returned pending mutation evidence",
        )),
        Err(InvocationError::InvalidPublishedResult { source, .. }) => Err(Error::Cell(*source)),
        Err(InvocationError::NotStarted(source)) => Err(Error::Cell(source)),
    }
}
