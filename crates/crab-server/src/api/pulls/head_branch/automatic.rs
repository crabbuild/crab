use super::*;
use crate::cells::repository::pull_branch::{
    CleanupJob, CleanupPlan, FinishCleanup, PendingCleanup,
};

pub(in crate::api::pulls) async fn capture(
    server: &Server,
    source: &Repository,
    publisher: &Principal,
    reference: &str,
    head: &str,
) -> Result<Option<CleanupPlan>> {
    let policy = merge_methods::load(server, source).await?;
    if !policy.delete_head_branches {
        return Ok(None);
    }
    let actor = app::actor(publisher)?;
    if principal(server, source, &actor).await?.is_none() {
        return Ok(None);
    }
    let names = [reference.to_owned()].into();
    let view = source.ref_view_for_refs(&names).await?;
    if view.head() == reference || view.refs().get(reference).map(String::as_str) != Some(head) {
        return Ok(None);
    }
    // The merge holds the source ref lease through publication. Its visible
    // transaction distinguishes a recreated branch even at the same commit.
    Ok(view
        .visible_ref_transactions()
        .get(reference)
        .map(|transaction| CleanupPlan {
            source: source.id,
            ownership_epoch: source.ownership_epoch,
            setting_generation: policy.delete_head_branches_generation,
            ref_transaction: transaction.clone(),
            actor: (&actor).into(),
        }))
}

async fn principal(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
) -> Result<Option<Principal>> {
    let local = Principal::local();
    let principal = if server.auth.is_none() && local.identity().as_ref() == Some(actor) {
        local.scoped_to_repository(Some(repo.id))
    } else {
        Principal::auto_merge(repo.id, actor.clone())
    };
    match app::membership(server, &principal, repo).await {
        Ok(grant) => Ok(grant
            .principal
            .can_write(repo.id)
            .then_some(grant.principal)),
        Err(Error::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

pub(in crate::api::pulls) async fn pending(
    server: &Server,
    repo: &Repository,
    before: Option<u64>,
) -> Result<Vec<CleanupJob>> {
    let cell = app::repository_cell(server, repo)?;
    app::query_output(
        cell.client
            .query::<PendingCleanup>(&cell.target, None, before)
            .await,
    )
}

pub(in crate::api::pulls) async fn reconcile(
    server: &Arc<Server>,
    upstream: &Arc<Repository>,
    job: CleanupJob,
) -> Result<()> {
    match apply(server, upstream, &job).await {
        Ok(())
        | Err(
            Error::NotFound
            | Error::BranchUpdatePermission
            | Error::PullBranchChanged
            | Error::PullBranchInUse
            | Error::Archived
            | Error::RefRule(_),
        ) => (),
        Err(error) => return Err(error),
    }
    let cell = app::repository_cell(server, upstream)?;
    app::command_output(
        cell.client
            .command::<FinishCleanup>(
                &cell.target,
                app::mutation_identity()?,
                (job.pull, job.merge),
            )
            .await,
    )
}

async fn apply(server: &Arc<Server>, upstream: &Arc<Repository>, job: &CleanupJob) -> Result<()> {
    let cleanup = &job.plan;
    let source = if cleanup.source == upstream.id {
        Arc::clone(upstream)
    } else {
        super::super::git::fork_repository(server, upstream, cleanup.source)?
    };
    if source.ownership_epoch != cleanup.ownership_epoch {
        return Ok(());
    }
    let actor = storage::from_author(cleanup.actor.clone());
    let Some(principal) = principal(server, &source, &actor).await? else {
        return Ok(());
    };
    let pull = associated_pull(server, upstream, &source, job.pull).await?;
    let Some(merge) = pull
        .merge
        .filter(|record| Uuid::parse_str(&record.request_id) == Ok(job.merge))
    else {
        return Ok(());
    };
    let plan_id = blake3::hash(&serde_json::to_vec(&(
        "automatic-pull-branch-cleanup/v1",
        upstream.id,
        source.id,
        cleanup.ownership_epoch,
        job.pull,
        job.merge,
    ))?)
    .to_hex()
    .to_string();
    receive::edit_pull_branch(
        Arc::clone(server),
        principal,
        RefUpdate {
            name: pull.head_ref,
            old: Some(oid(&merge.head_oid)?),
            new: None,
        },
        PullBranchPublication {
            upstream: Arc::clone(upstream),
            source,
            pull: job.pull,
            binding: PullBranchBinding::Automatic {
                merge: job.merge,
                setting_generation: cleanup.setting_generation,
                ref_transaction: cleanup.ref_transaction.clone(),
            },
            plan_id,
        },
    )
    .await
    .map_err(edit_error)
}
