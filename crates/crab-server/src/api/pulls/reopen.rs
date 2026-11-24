use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use crab_remote::publication::{LeaseOptions, acquire_ref_leases};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::cells::repository::pull_reopen::{Branches, ForReopen};

pub(in crate::api) enum Access {
    AuthorOrTriage,
    Triage,
}

pub(in crate::api) async fn with_available<T, F>(
    server: &Arc<Server>,
    principal: &Principal,
    repo: &Arc<Repository>,
    pulls: Vec<(u64, u64)>,
    access: Access,
    save: impl FnOnce(BTreeSet<u64>, app::Membership) -> F + Send + 'static,
) -> Result<T>
where
    T: Send + 'static,
    F: Future<Output = Result<T>> + Send + 'static,
{
    let grant = app::membership(server, principal, repo).await?;
    let can_triage = grant.principal.can_triage(repo.id);
    if pulls.is_empty() || matches!(access, Access::Triage) && !can_triage {
        return save(BTreeSet::new(), grant).await;
    }
    let actor = app::actor(principal)?;
    let worker = Arc::clone(server);
    let repo = Arc::clone(repo);
    let principal = principal.clone();
    let (send, response) = tokio::sync::oneshot::channel();
    // Keep the leases and drain the Cell decision if the browser disconnects.
    server.receives.spawn(async move {
        let cancel = worker.cancellation.child_token();
        let mut leases = Vec::new();
        let result = async {
            let cell = app::repository_cell(&worker, &repo)?;
            let mut candidates = app::query_output(
                cell.client
                    .query::<ForReopen>(&cell.target, None, pulls)
                    .await,
            )?;
            candidates.retain(|branch| {
                can_triage || branch.issuer == actor.issuer && branch.subject == actor.subject
            });
            let available = capture(&worker, &repo, candidates, &cancel, &mut leases).await?;
            let grant = app::membership(&worker, &principal, &repo).await?;
            if cancel.is_cancelled() {
                return Err(Error::MergeBusy);
            }
            save(available, grant).await
        }
        .await;
        while let Some(lease) = leases.pop() {
            lease.release().await;
        }
        let _ = send.send(result);
    });
    response.await.map_err(|_| Error::MergeBusy)?
}

async fn capture(
    server: &Server,
    repo: &Arc<Repository>,
    candidates: Vec<Branches>,
    cancel: &CancellationToken,
    leases: &mut Vec<crab_remote::publication::PublicationLeases>,
) -> Result<BTreeSet<u64>> {
    let mut refs = BTreeSet::new();
    let mut forks = BTreeMap::<Uuid, (Arc<Repository>, BTreeSet<String>)>::new();
    for branch in &candidates {
        refs.insert(branch.base.clone());
        if let Some(id) = branch.fork {
            match git::fork_repository(server, repo, id) {
                Ok(fork) => {
                    forks
                        .entry(id)
                        .or_insert_with(|| (fork, BTreeSet::new()))
                        .1
                        .insert(branch.head.clone());
                }
                Err(Error::NotFound) => {}
                Err(error) => return Err(error),
            }
        } else {
            refs.insert(branch.head.clone());
        }
    }
    if refs.is_empty() {
        return Ok(BTreeSet::new());
    }
    // Admission matches comparison and merge publication upstream first.
    // Multiple fork groups use a stable UUID order across bulk requests.
    let opts = LeaseOptions::renewing(Duration::from_secs(300));
    leases.push(
        acquire_ref_leases(
            &repo.store,
            &repo.layout,
            refs.iter().cloned(),
            opts,
            cancel,
        )
        .await?,
    );
    for (fork, names) in forks.values() {
        leases.push(
            acquire_ref_leases(
                &fork.store,
                &fork.layout,
                names.iter().cloned(),
                opts,
                cancel,
            )
            .await?,
        );
    }
    let local = repo.ref_oids(&refs).await?;
    let mut heads = BTreeMap::new();
    for (id, (fork, names)) in forks {
        let current = match git::fork_repository(server, repo, id) {
            Ok(current) => current,
            Err(Error::NotFound) => continue,
            Err(error) => return Err(error),
        };
        if current.ownership_epoch == fork.ownership_epoch
            && current.identity.placement_generation() == fork.identity.placement_generation()
        {
            heads.insert(id, fork.ref_oids(&names).await?);
        }
    }
    Ok(candidates
        .into_iter()
        .filter_map(|branch| {
            let head = match branch.fork {
                Some(id) => heads.get(&id)?,
                None => &local,
            };
            (local.contains_key(&branch.base) && head.contains_key(&branch.head))
                .then_some(branch.number)
        })
        .collect())
}
