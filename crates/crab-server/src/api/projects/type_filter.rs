use crate::{
    app::{Error, Result},
    cells::{
        organization::issue_types::Schema,
        repository::issues::issue_type::search::{Filter, Scope},
    },
    repository::Repository,
};

pub(in crate::api) fn scope(repo: &Repository, filter: Filter, types: &Schema) -> Result<Scope> {
    Ok(Scope {
        organization: repo.organization.ok_or(Error::Invalid(
            "Issue type filters require an organization repository",
        ))?,
        ownership_epoch: repo.ownership_epoch,
        filter,
        types: types
            .definitions
            .iter()
            .map(|definition| definition.id)
            .collect(),
    })
}

pub(in crate::api) fn stale() -> Error {
    Error::Cell(crate::cells::Error::StaleSearch)
}

pub(in crate::api) fn cursor(facts: impl serde::Serialize) -> Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(&facts)?)
        .to_hex()
        .to_string())
}

pub(in crate::api) fn validate_cursor(
    continuing: bool,
    expected: Option<&str>,
    actual: Option<&str>,
) -> Result<()> {
    if continuing && expected != actual {
        return Err(stale());
    }
    Ok(())
}
