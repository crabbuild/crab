use crate::{
    api::members::roster::Roster,
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::{
        assignments::Assignee,
        collaboration::creation::{Input, Selection},
    },
    repository::Repository,
    server::Server,
};

pub(super) async fn resolve(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
    mut selection: Selection,
) -> Result<Input> {
    selection.label_ids.sort_unstable();
    selection.assignee_ids.sort_unstable();
    selection.validate().map_err(|error| match error {
        crate::cells::Error::Invalid(message) => Error::Invalid(message),
        other => Error::Cell(other),
    })?;
    let (principal, eligible) = if selection.assignee_ids.is_empty() {
        (
            app::membership(server, principal, repo).await?.principal,
            vec![],
        )
    } else {
        let roster = Roster::load(server, principal, repo).await?;
        let mut eligible: Vec<_> = roster
            .items
            .into_iter()
            .filter(|person| selection.assignee_ids.contains(&person.id))
            .map(|person| Assignee {
                id: person.id,
                name: person.name,
            })
            .collect();
        eligible.sort_unstable_by_key(|person| person.id);
        (roster.principal, eligible)
    };
    Ok(Input {
        selection,
        can_triage: principal.can_triage(repo.id),
        eligible,
    })
}

#[cfg(test)]
mod tests;
