use crab_storage::StorageError;
use serde::{Deserialize, Serialize};

use crate::{
    BranchProtection,
    app::{Error, Result},
    app_storage,
    config::valid_branch_protections,
    server::Repository,
};

const BRANCH_PROTECTIONS: &str = "app/v1/settings/branch-protections.json";

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BranchProtections {
    pub version: u64,
    pub rules: Vec<BranchProtection>,
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

pub(crate) async fn load(repo: &Repository) -> Result<BranchProtections> {
    let Some((settings, _)) =
        app_storage::read::<BranchProtections>(repo, BRANCH_PROTECTIONS).await?
    else {
        return Ok(BranchProtections::configured(
            &repo.config.protected_branches,
        ));
    };
    validate_stored(&settings)?;
    Ok(settings)
}

pub(crate) async fn refresh(repo: &Repository) -> Result<BranchProtections> {
    let current = load(repo).await?;
    let mut effective = repo.protections.write().await;
    // A load started before a successful local replacement can finish afterward.
    // Keep that delayed snapshot from rolling policy back to an older version.
    if current.version >= effective.version && *effective != current {
        *effective = current.clone();
    }
    Ok(effective.clone())
}

pub(crate) async fn replace(
    repo: &Repository,
    expected_version: u64,
    rules: Vec<BranchProtection>,
) -> Result<BranchProtections> {
    if !valid_branch_protections(&rules) {
        return Err(Error::Invalid(
            "Protection rules require at most 100 unique valid branches, 0–20 approvals, and at most 50 unique check names",
        ));
    }
    let mut effective = repo.protections.write().await;
    let stored = app_storage::read::<BranchProtections>(repo, BRANCH_PROTECTIONS).await?;
    let current = match &stored {
        Some((settings, _)) => {
            validate_stored(settings)?;
            settings.clone()
        }
        None if effective.version == 0 => effective.clone(),
        None => return Err(Error::Conflict),
    };
    if current != *effective {
        *effective = current.clone();
    }
    if current.version != expected_version || current.version >= app_storage::MAX_NUMBER - 1 {
        return Err(Error::Conflict);
    }
    let proposed = BranchProtections {
        version: current.version + 1,
        rules,
    };
    match stored {
        Some((_, etag)) => {
            match app_storage::update(repo, BRANCH_PROTECTIONS, &proposed, etag).await {
                Ok(()) => {}
                Err(Error::Storage(StorageError::StateConflict { .. })) => {
                    return Err(Error::Conflict);
                }
                Err(error) => return Err(error),
            }
        }
        None => {
            let existing =
                app_storage::create_or_read(repo, BRANCH_PROTECTIONS, proposed.clone()).await?;
            if existing != proposed {
                validate_stored(&existing)?;
                *effective = existing;
                return Err(Error::Conflict);
            }
        }
    }
    *effective = proposed.clone();
    Ok(proposed)
}

fn validate_stored(settings: &BranchProtections) -> Result<()> {
    if settings.version == 0
        || settings.version >= app_storage::MAX_NUMBER
        || !valid_branch_protections(&settings.rules)
    {
        return Err(Error::Invalid(
            "Stored branch protection settings are invalid",
        ));
    }
    Ok(())
}
