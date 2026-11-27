use crab_git::receive_plan::RefUpdate;
use crab_read::capsule_protocol::CapsuleRepositoryView;
use crab_remote::prepare::CapsuleArtifacts;

use crate::{
    app,
    auth::Principal,
    cells::repository::{
        GetMergeMethods,
        push_attribution::{self, Attribution, Authority, Record, RefVersion},
    },
    repository::Repository,
    server::Server,
};

use super::{Result, validate};

pub(super) fn tracks(server: &Server, principal: &Principal, updates: &[RefUpdate]) -> bool {
    server.cells().is_some()
        && principal.import_grant().is_none()
        && !principal.is_mirror()
        && updates.iter().any(branch_update)
}

fn branch_update(update: &RefUpdate) -> bool {
    update.name.starts_with("refs/heads/") && update.new.is_some() && update.new != update.old
}

pub(super) async fn record(
    server: &Server,
    source: &Repository,
    principal: &Principal,
    artifacts: &mut CapsuleArtifacts,
    plan: &str,
    previous: &CapsuleRepositoryView,
    updates: &[RefUpdate],
) -> Result<()> {
    if !tracks(server, principal, updates) {
        return Ok(());
    }
    let Some(actor) = principal.identity() else {
        return Ok(());
    };
    let predecessors: std::collections::BTreeMap<_, _> = updates
        .iter()
        .filter(|update| branch_update(update))
        .map(|update| {
            let before = previous
                .visible_ref_transactions()
                .get(&update.name)
                .filter(|_| {
                    update.old.is_some_and(|old| {
                        previous.refs().get(&update.name) == Some(&old.to_string())
                    })
                })
                .cloned();
            (update.name.clone(), before)
        })
        .collect();
    let identity = push_attribution::actor(&actor.issuer, &actor.subject);
    let authority = if predecessors.values().any(Option::is_some)
        && (principal.is_user()
            || (principal.is_git() && principal.automation_credential().is_none()))
    {
        writer_authority(server, source, actor).await?
    } else {
        None
    };
    // This attempt has its own identity. A failure before Git saves the plan
    // intent cannot leave evidence that authorizes a later retry by the actor.
    let transaction = artifacts
        .bind_plan_attempt(plan)
        .map_err(validate::map_error)?;
    let cell = app::repository_cell(server, source)?;
    app::command_output(
        cell.client
            .command::<Record>(
                &cell.target,
                app::mutation_identity()?,
                Attribution {
                    publication: RefVersion {
                        epoch: previous.root().root().ref_epoch().to_owned(),
                        transaction: transaction.id()?,
                    },
                    authority,
                    actor: identity,
                    predecessors,
                },
            )
            .await,
    )?;
    Ok(())
}

async fn writer_authority(
    server: &Server,
    source: &Repository,
    actor: crate::auth::Identity,
) -> Result<Option<Authority>> {
    let upstream = source.fork_source(server);
    let target = upstream.as_deref().unwrap_or(source);
    let cell = app::repository_cell(server, target)?;
    let settings = app::query_output(
        cell.client
            .query::<GetMergeMethods>(&cell.target, None, ())
            .await,
    )?;
    if !settings.auto_merge {
        return Ok(None);
    }
    // A private fork shares the upstream organization policy lease. Use the
    // account's durable upstream grant, independent of the fork Git token.
    let writer = Principal::auto_merge(target.id, actor);
    match app::membership(server, &writer, target).await {
        Ok(grant) if grant.principal.can_write(target.id) => Ok(Some(Authority {
            repository: target.id,
            ownership_epoch: target.ownership_epoch,
        })),
        Ok(_) | Err(app::Error::NotFound) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
