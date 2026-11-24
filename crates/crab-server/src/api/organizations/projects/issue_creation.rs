use super::*;
use crate::cells::repository::projects::organization_issue as source_issue;

async fn writable(
    server: &Server,
    principal: &Principal,
    organization: Uuid,
    repository: Uuid,
) -> Result<Option<Arc<Repository>>> {
    let Some(repo) = server
        .repositories
        .by_id(repository)
        .filter(|repo| repo.organization == Some(organization))
    else {
        return Ok(None);
    };
    match app::membership(server, principal, &repo).await {
        Ok(current) if current.principal.can_write(repo.id) => Ok(Some(repo)),
        Ok(_) | Err(Error::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) async fn submit(
    server: Arc<Server>,
    principal: Principal,
    organization: Uuid,
    actor: Uuid,
    input: Submission,
    cancel: bool,
) -> Result<projects::Outcome> {
    super::super::properties::policy_write(
        server,
        organization,
        move |server, address, shutdown| async move {
            let Command::CreateIssue {
                project: project_id,
                repository,
                ref item,
                type_id,
            } = input.command
            else {
                return Err(Error::Invalid("Expected an issue creation command"));
            };
            let repo = if cancel {
                None
            } else {
                writable(&server, &principal, organization, repository).await?
            };
            let author = crate::cells::repository::RepositoryAuthor::from(&app::actor(&principal)?);
            let actor = authority::viewer(&server, &principal, organization, actor).await?;
            let request = Request {
                actor,
                request_id: input.request_id,
                source_allowed: repo.is_some(),
                attachment: None,
                source_epoch: repo.as_ref().map(|repo| repo.ownership_epoch),
                eligible_outside: None,
                cancel,
                command: input.command.clone(),
            };
            let cells = server.cells().ok_or(Error::CellUnavailable)?;
            let decision = app::command_output(
                cells
                    .command::<projects::Change>(
                        address,
                        app::mutation_identity()?,
                        request.clone(),
                    )
                    .await,
            )?;
            let projects::Outcome::PendingIssue { issue } = decision else {
                return if decision == projects::Outcome::RequestConflict {
                    Err(Error::Conflict)
                } else {
                    Ok(decision)
                };
            };
            let ownership_epoch = issue.ownership_epoch;

            // The reservation is already durable, so recovery may probe the same
            // source Cell after revocation. It may publish fresh work only with
            // current write access to the same repository ownership incarnation.
            let current = if cancel {
                None
            } else {
                writable(&server, &principal, organization, repository)
                    .await?
                    .filter(|repo| repo.ownership_epoch == ownership_epoch)
            };
            let can_manage = if current.is_some() {
                match project(&server, &principal, organization, actor, project_id).await {
                    Ok(view) => view.can_manage,
                    Err(Error::NotFound) => false,
                    Err(error) => return Err(error),
                }
            } else {
                false
            };
            let issue_type = match (&current, type_id) {
                (Some(repo), Some(id)) if can_manage => {
                    issue_types::active_selection(&server, repo, id).await?
                }
                _ => None,
            };
            if shutdown.is_cancelled() {
                return Err(Error::MergeBusy);
            }
            app::account(&principal)?;
            let source = cells.repository(repository).map_err(Error::Cell)?;
            let receipt = app::command_output(
                source
                    .client
                    .command::<source_issue::Publish>(
                        &source.target,
                        app::mutation_identity()?,
                        source_issue::Request {
                            organization,
                            project: project_id,
                            request_id: request.request_id,
                            actor_id: actor.id(),
                            actor: author,
                            title: item.title.clone(),
                            body: item.body.clone(),
                            type_id,
                            ownership_epoch,
                            issue_type,
                            can_write: can_manage && current.is_some(),
                            cancel,
                        },
                    )
                    .await,
            )?;
            app::command_output(
                cells
                    .command::<projects::ResolveIssue>(
                        address,
                        app::mutation_identity()?,
                        projects::ResolveInput {
                            request_id: request.request_id,
                            digest: issue.digest,
                            receipt,
                        },
                    )
                    .await,
            )
        },
    )
    .await
}
