use uuid::Uuid;

use crate::{
    api::organizations,
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        self, Address,
        organization::teams::{
            List, ListInput, Page,
            repository_access::{ResolveReviewTeams, ReviewTeamQuery},
        },
        repository::{
            SearchInput,
            search::{
                Query,
                completion::{CompletionInput, Suggestions, append_qualified_values},
                reviews::teams::{Bindings, QUALIFIER},
            },
        },
    },
    repository::Repository,
    server::Server,
};

struct Scope {
    names: Vec<String>,
    include_mine: bool,
}

pub(in crate::api) struct Bound {
    scope: Scope,
    bindings: Bindings,
}

pub(super) async fn bind(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
    input: &mut SearchInput,
) -> Result<Option<Bound>> {
    let query = Query::parse(input.query.as_deref()).map_err(Error::Invalid)?;
    let scope = Scope {
        names: query.review_team_names().into_iter().collect(),
        include_mine: query.includes_my_team_requests(),
    };
    if scope.names.is_empty() && !scope.include_mine {
        return Ok(None);
    }
    let bindings = scope.resolve(server, principal, repo).await?;
    input.review_teams = bindings.clone();
    Ok(Some(Bound { scope, bindings }))
}

impl Bound {
    pub(in crate::api) async fn verify(
        &self,
        server: &Server,
        principal: &Principal,
        repo: &Repository,
    ) -> Result<()> {
        // A repository grant can survive a team visibility change. Recheck the
        // exact binding used for filtering, totals, and cursor generation.
        if self.scope.resolve(server, principal, repo).await? != self.bindings {
            return Err(Error::PageChanged);
        }
        app::membership(server, principal, repo).await?;
        Ok(())
    }
}

impl Scope {
    async fn resolve(
        &self,
        server: &Server,
        principal: &Principal,
        repo: &Repository,
    ) -> Result<Bindings> {
        let mut bound = Bindings {
            named: self
                .names
                .iter()
                .cloned()
                .map(|name| (name, None))
                .collect(),
            mine: Vec::new(),
        };
        let mut selected = Vec::new();
        let mut names = Vec::new();
        for (index, name) in self.names.iter().enumerate() {
            if let Some((owner, team)) = name.split_once('/')
                && owner.eq_ignore_ascii_case(&repo.config.owner)
            {
                selected.push(index);
                names.push(team.to_owned());
            }
        }
        if selected.is_empty() && !self.include_mine {
            return Ok(bound);
        }
        let Some((organization, actor)) = authority(server, principal, repo).await? else {
            return Ok(bound);
        };
        let cells = server.cells().ok_or(Error::CellUnavailable)?;
        let response = cells
            .query::<ResolveReviewTeams>(
                Address::Organization(organization),
                None,
                ReviewTeamQuery {
                    actor,
                    names,
                    include_mine: self.include_mine,
                },
            )
            .await;
        let output = match response {
            Ok(receipt) => receipt.output,
            Err(cells::Error::NotInitialized | cells::Error::AccessDenied) => return Ok(bound),
            Err(error) => return Err(Error::Cell(error)),
        };
        if output.named.len() != selected.len() {
            return Err(Error::CellContract(
                "Review team resolution returned an invalid result count",
            ));
        }
        for (index, team) in selected.into_iter().zip(output.named) {
            bound.named[index].1 = team;
        }
        bound.mine = output.mine;
        Ok(bound)
    }
}

async fn authority(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
) -> Result<Option<(Uuid, Uuid)>> {
    let Some(organization) = repo.organization else {
        return Ok(None);
    };
    match organizations::actor(server, principal).await {
        Ok(actor) => Ok(Some((organization, actor))),
        Err(Error::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

async fn visible_choices(
    server: &Server,
    organization: Uuid,
    actor: Uuid,
    prefix: &str,
) -> Result<Vec<(Uuid, String)>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    match cells
        .query::<List>(
            Address::Organization(organization),
            None,
            ListInput {
                actor,
                search: prefix.to_owned(),
                after: None,
                limit: 12,
            },
        )
        .await
    {
        Ok(receipt) => match receipt.output.page {
            Page::Page { items, .. } => Ok(items
                .into_iter()
                .map(|item| (item.view.team.id, item.view.team.name))
                .collect()),
            Page::Changed => Err(Error::PageChanged),
        },
        Err(cells::Error::NotInitialized | cells::Error::AccessDenied) => Ok(Vec::new()),
        Err(error) => Err(Error::Cell(error)),
    }
}

pub(super) async fn complete(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
    input: &CompletionInput,
    suggestions: &mut Suggestions,
    prefix: &str,
) -> Result<()> {
    let prefix = match prefix.split_once('/') {
        Some((owner, prefix)) if owner.eq_ignore_ascii_case(&repo.config.owner) => prefix,
        Some(_) => return Ok(()),
        None => prefix,
    };
    if prefix.chars().count() > 100 {
        return Ok(());
    }
    let Some((organization, actor)) = authority(server, principal, repo).await? else {
        return Ok(());
    };
    let choices = visible_choices(server, organization, actor, prefix).await?;
    app::membership(server, principal, repo).await?;
    if choices != visible_choices(server, organization, actor, prefix).await? {
        return Err(Error::PageChanged);
    }
    let values = choices
        .into_iter()
        .map(|(_, name)| {
            let handle = serde_json::to_string(&format!("{}/{}", repo.config.owner, name))
                .map_err(Error::Json)?;
            Ok((handle, name))
        })
        .collect::<Result<Vec<_>>>()?;
    append_qualified_values(input, suggestions, QUALIFIER, values.into_iter())
        .map_err(Error::Invalid)
}
