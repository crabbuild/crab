use crate::cells::MutationIdentity;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    BranchProtection,
    app::{Error, Result},
    cells::repository::{
        BranchProtectionRecord, BranchProtectionSettings, GetBranchProtections,
        GetRepositoryLifecycle, ReplaceBranchProtections, ReplaceBranchProtectionsInput,
        ReplaceBranchProtectionsOutcome, ReplaceRepositoryLifecycle,
        ReplaceRepositoryLifecycleInput, ReplaceRepositoryLifecycleOutcome,
        RepositoryLifecycleRecord,
    },
    config::valid_branch_protections,
    server::{Repository, Server},
};

const MAX_NUMBER: u64 = 9_007_199_254_740_991;

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

pub(crate) async fn load(server: &Server, repo: &Repository) -> Result<BranchProtections> {
    let routed = crate::app::repository_cell(server, repo)?;
    let stored = crate::app::query_output(
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
    expected_version: u64,
    rules: Vec<BranchProtection>,
) -> Result<BranchProtections> {
    if expected_version >= MAX_NUMBER || !valid_branch_protections(&rules) {
        return Err(Error::Invalid(
            "Protection rules require at most 100 unique valid branches, 0–20 approvals, and at most 50 unique check names",
        ));
    }
    let routed = crate::app::repository_cell(server, repo)?;
    match crate::app::command_output(
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
) -> Result<RepositoryLifecycle> {
    let routed = crate::app::repository_cell(server, repo)?;
    repository_lifecycle(crate::app::query_output(
        routed
            .client
            .query::<GetRepositoryLifecycle>(&routed.target, None, ())
            .await,
    )?)
}

pub(crate) async fn replace_lifecycle(
    server: &Server,
    repo: &Repository,
    expected_version: u64,
    archived: bool,
) -> Result<RepositoryLifecycle> {
    if expected_version >= MAX_NUMBER {
        return Err(Error::Conflict);
    }
    let routed = crate::app::repository_cell(server, repo)?;
    match crate::app::command_output(
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
    let now_ms = crate::cells::unix_now_ms().map_err(Error::Cell)?;
    let expires_at_ms = now_ms
        .checked_add(60_000)
        .ok_or(Error::CellContract("Cell request expiry overflowed"))?;
    Ok(MutationIdentity {
        request_id: Uuid::now_v7(),
        issued_at_ms: now_ms,
        expires_at_ms,
    })
}
