use std::collections::BTreeMap;

use crab_git::receive_plan::RefUpdate;
use crab_read::capsule_protocol::CapsuleRepositoryView;
use crab_remote::prepare::CapsuleArtifacts;

use crate::{
    app,
    auth::Principal,
    cells::repository::{
        GetMergeMethods,
        push_attribution::{Attribution, Authority, Record, RefVersion},
    },
    repository::Repository,
    server::Server,
};

use super::{Result, validate};

pub(super) async fn record(
    server: &Server,
    source: &Repository,
    principal: &Principal,
    artifacts: &mut CapsuleArtifacts,
    plan: &str,
    previous: &CapsuleRepositoryView,
    updates: &[RefUpdate],
) -> Result<()> {
    if server.cells().is_none()
        || !(principal.is_user()
            || (principal.is_git() && principal.automation_credential().is_none()))
    {
        return Ok(());
    }
    let predecessors: BTreeMap<_, _> = updates
        .iter()
        .filter(|update| {
            update.name.starts_with("refs/heads/")
                && update.new.is_some()
                && update.new != update.old
        })
        .filter_map(|update| {
            let old = update.old?.to_string();
            if previous.refs().get(&update.name) != Some(&old) {
                return None;
            }
            previous
                .visible_ref_transactions()
                .get(&update.name)
                .map(|before| (update.name.clone(), before.clone()))
        })
        .collect();
    if predecessors.is_empty() {
        return Ok(());
    }
    let upstream = source.fork_source(server);
    let target = upstream.as_deref().unwrap_or(source);
    let cell = app::repository_cell(server, target)?;
    let settings = app::query_output(
        cell.client
            .query::<GetMergeMethods>(&cell.target, None, ())
            .await,
    )?;
    if !settings.auto_merge {
        return Ok(());
    }
    // A private fork shares the upstream organization policy lease. Use the
    // account's durable upstream grant, independent of the fork Git token.
    let Some(actor) = principal.identity() else {
        return Ok(());
    };
    let writer = Principal::auto_merge(target.id, actor);
    match app::membership(server, &writer, target).await {
        Ok(grant) if grant.principal.can_write(target.id) => (),
        Ok(_) | Err(app::Error::NotFound) => return Ok(()),
        Err(error) => return Err(error.into()),
    }
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
                    authority: Authority {
                        repository: target.id,
                        ownership_epoch: target.ownership_epoch,
                    },
                    predecessors,
                },
            )
            .await,
    )?;
    Ok(())
}
