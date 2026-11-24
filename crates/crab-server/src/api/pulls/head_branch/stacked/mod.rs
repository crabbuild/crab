use std::collections::{BTreeMap, BTreeSet};

use crab_remote::publication::{LeaseOptions, PublicationLeases, acquire_leases};
use crab_remote_git::{RepositoryOptions, RevisionError};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::cells::repository::{pull_base, pull_base::stacked as cell};

pub(crate) mod native;

#[derive(Serialize)]
pub(crate) struct Preview {
    base_ref: String,
    pulls: Vec<u64>,
    token: String,
}

struct Destination {
    reference: String,
    parent: u64,
    base: String,
    plan: String,
}

pub(crate) struct Stack {
    destination: Destination,
    dependents: Vec<cell::Dependent>,
    token: String,
    forks: BTreeMap<Uuid, (Arc<Repository>, BTreeSet<String>)>,
}

pub(super) fn can_retarget(pull: &PullRequest) -> bool {
    pull.state == PullState::Merged && pull.merge.is_some() && pull.head_repository.is_none()
}

async fn inspect(
    server: &Server,
    source: &Repository,
    reference: &str,
    plan: Option<String>,
) -> Result<cell::View> {
    let routed = app::repository_cell(server, source)?;
    app::query_output(
        routed
            .client
            .query::<cell::Inspect>(
                &routed.target,
                None,
                cell::Query {
                    reference: reference.to_owned(),
                    plan,
                },
            )
            .await,
    )
}

pub(crate) async fn validate_replay(
    server: &Server,
    source: &Repository,
    reference: &str,
    edit: &PullBranchPublication,
) -> Result<()> {
    if let PullBranchBinding::Manual { retarget_token, .. } = &edit.binding {
        let view = inspect(server, source, reference, Some(edit.plan_id.clone())).await?;
        if view.applied != *retarget_token {
            return Err(Error::RequestConflict);
        }
    }
    Ok(())
}

pub(super) async fn preview(
    server: &Server,
    source: &Repository,
    pull: &PullRequest,
) -> Result<Option<Preview>> {
    let view = inspect(server, source, &pull.head_ref, None).await?;
    preview_dependents(source, &pull.base_ref, view).await
}

async fn preview_dependents(
    source: &Repository,
    base: &str,
    view: cell::View,
) -> Result<Option<Preview>> {
    if view.dependents.is_empty() {
        return Ok(None);
    }
    if !cell::eligible(base, &view.dependents) || source.ref_oid(base).await?.is_none() {
        return Err(Error::PullBranchInUse);
    }
    Ok(Some(Preview {
        base_ref: base.to_owned(),
        pulls: view.dependents.iter().map(|child| child.number).collect(),
        token: cell::token(base, &view.dependents).map_err(Error::Cell)?,
    }))
}

pub(crate) async fn plan(
    server: &Server,
    source: &Repository,
    edit: &PullBranchPublication,
) -> Result<Option<Stack>> {
    if source.id != edit.upstream.id {
        return Ok(None);
    }
    let pull = associated_pull(server, &edit.upstream, source, edit.pull).await?;
    if !can_retarget(&pull) {
        return Ok(None);
    }
    let view = inspect(server, source, &pull.head_ref, Some(edit.plan_id.clone())).await?;
    let expected = match &edit.binding {
        PullBranchBinding::Manual { retarget_token, .. } => Some(retarget_token.as_deref()),
        PullBranchBinding::Automatic { .. } => None,
    };
    let destination = Destination {
        reference: pull.head_ref,
        parent: pull.number,
        base: pull.base_ref,
        plan: edit.plan_id.clone(),
    };
    build(server, source, destination, view, expected)
}

fn build(
    server: &Server,
    source: &Repository,
    destination: Destination,
    view: cell::View,
    expected: Option<Option<&str>>,
) -> Result<Option<Stack>> {
    if let Some(applied) = view.applied {
        // The Cell can have committed even when the Git marker or its reply
        // was lost. The original intent may continue after the batch moved.
        if !view.dependents.is_empty()
            || expected.is_some_and(|value| value != Some(applied.as_str()))
        {
            return Err(Error::PullBranchChanged);
        }
        return Ok(Some(Stack {
            destination,
            dependents: Vec::new(),
            token: applied,
            forks: BTreeMap::new(),
        }));
    }
    if view.dependents.is_empty() {
        return if expected.flatten().is_none() {
            Ok(None)
        } else {
            Err(Error::PullBranchChanged)
        };
    }
    if expected == Some(None) || !cell::eligible(&destination.base, &view.dependents) {
        return Err(Error::PullBranchInUse);
    }
    let token = cell::token(&destination.base, &view.dependents).map_err(Error::Cell)?;
    if expected.flatten().is_some_and(|expected| expected != token) {
        return Err(Error::PullBranchChanged);
    }
    let mut forks: BTreeMap<_, (_, BTreeSet<_>)> = BTreeMap::new();
    for child in &view.dependents {
        if let Some(id) = child.fork {
            let fork = super::super::git::fork_repository(server, source, id)?;
            forks
                .entry(id)
                .or_insert_with(|| (fork, BTreeSet::new()))
                .1
                .insert(child.head.clone());
        }
    }
    Ok(Some(Stack {
        destination,
        dependents: view.dependents,
        token,
        forks,
    }))
}

impl Stack {
    pub(crate) fn local_refs(&self) -> impl Iterator<Item = String> + '_ {
        std::iter::once(self.destination.base.clone()).chain(
            self.dependents
                .iter()
                .filter(|child| child.fork.is_none())
                .map(|child| child.head.clone()),
        )
    }

    pub(super) async fn apply(
        &self,
        server: &Server,
        source: &Repository,
        principal: &Principal,
        pull: &PullRequest,
        edit: &PullBranchPublication,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if !can_retarget(pull)
            || pull.base_ref != self.destination.base
            || source.id != edit.upstream.id
        {
            return Err(Error::PullBranchChanged);
        }
        let batch = self.prepare(server, source, principal, cancel).await?;
        persist(
            server,
            source,
            principal,
            batch.into_iter().collect(),
            cancel,
        )
        .await
    }

    async fn prepare(
        &self,
        server: &Server,
        source: &Repository,
        principal: &Principal,
        cancel: &CancellationToken,
    ) -> Result<Option<cell::Batch>> {
        let destination = &self.destination;
        let current = inspect(
            server,
            source,
            &destination.reference,
            Some(destination.plan.clone()),
        )
        .await?;
        if current.applied.as_deref() == Some(&self.token) && current.dependents.is_empty() {
            return if source.ref_oid(&destination.base).await?.is_some() {
                Ok(None)
            } else {
                Err(Error::PullBranchInUse)
            };
        }
        if current.dependents != self.dependents {
            return Err(Error::PullBranchChanged);
        }
        let _read =
            crate::api::read_admission::acquire_bulk(server).map_err(|_| Error::MergeBusy)?;
        let actor = app::actor(principal)?;
        let base = source
            .open_current(server, RepositoryOptions::default(), cancel)
            .await?;
        let base_oid = branch_tip(&base, &destination.base).ok_or(Error::PullBranchInUse)?;
        let mut forks = BTreeMap::new();
        for (id, (locked, _)) in &self.forks {
            let current = super::super::git::fork_repository(server, source, *id)?;
            if current.ownership_epoch != locked.ownership_epoch
                || current.identity.placement_generation() != locked.identity.placement_generation()
            {
                return Err(Error::PullBranchChanged);
            }
            forks.insert(
                *id,
                current
                    .open_current(server, RepositoryOptions::default(), cancel)
                    .await?,
            );
        }
        let mut changes = Vec::with_capacity(self.dependents.len());
        for child in &self.dependents {
            let fork = child
                .fork
                .map(|id| {
                    forks
                        .get(&id)
                        .cloned()
                        .ok_or(Error::CellContract("Stacked pull fork was not leased"))
                })
                .transpose()?;
            let git = super::super::git::PullGit {
                base: base.clone(),
                fork,
            };
            let head = branch_tip(git.head(), &child.head).ok_or(Error::PullBranchInUse)?;
            let tips = (base_oid.as_str(), head.as_str());
            let comparison = crate::api::query::pull_comparison_base(
                &git.base,
                git.fork.as_ref(),
                tips.0,
                tips.1,
                cancel,
            )
            .await;
            if let Err(error) = comparison {
                if matches!(
                    crate::api::revalidation::shared_cause(&error),
                    crab_remote_git::Error::Revision {
                        reason: RevisionError::NoMergeBase,
                        ..
                    }
                ) {
                    return Err(Error::PullBranchInUse);
                }
                return Err(crate::Error::from(error).into());
            }
            let owners = if child.draft {
                Default::default()
            } else {
                super::super::codeowners::requests(server, source, principal, &git, tips).await?
            };
            let mut request_id = [0; 16];
            let mut digest = blake3::Hasher::new();
            digest.update(b"crab.stacked-base-request.v1\0");
            digest.update(destination.plan.as_bytes());
            digest.update(&child.number.to_be_bytes());
            request_id.copy_from_slice(&digest.finalize().as_bytes()[..16]);
            changes.push(pull_base::Input {
                pull: child.number,
                actor: (&actor).into(),
                can_write: true,
                comparison_current: true,
                change: pull_base::Change {
                    request_id: Uuid::from_bytes(request_id),
                    version: child.version,
                    base_ref: destination.base.clone(),
                    base_oid: base_oid.clone(),
                    head_oid: head,
                },
                code_owner_requests: owners,
            });
        }
        Ok(Some(cell::Batch {
            parent: destination.parent,
            plan: destination.plan.clone(),
            token: self.token.clone(),
            changes,
        }))
    }
}

async fn persist(
    server: &Server,
    source: &Repository,
    principal: &Principal,
    batches: Vec<cell::Batch>,
    cancel: &CancellationToken,
) -> Result<()> {
    if batches.is_empty() {
        return Ok(());
    }
    if cancel.is_cancelled() {
        return Err(Error::MergeBusy);
    }
    if !app::membership(server, principal, source)
        .await?
        .principal
        .can_write(source.id)
    {
        return Err(Error::BranchUpdatePermission);
    }
    let routed = app::repository_cell(server, source)?;
    if !app::command_output(
        routed
            .client
            .command::<cell::Apply>(&routed.target, app::mutation_identity()?, batches)
            .await,
    )? {
        return Err(Error::PullBranchChanged);
    }
    Ok(())
}

pub(crate) async fn lease_forks(
    stacks: &[&Stack],
    options: LeaseOptions,
    cancel: &CancellationToken,
    leases: &mut Vec<PublicationLeases>,
) -> Result<()> {
    let mut forks: BTreeMap<_, (_, BTreeSet<_>)> = BTreeMap::new();
    for stack in stacks {
        for (id, (fork, refs)) in &stack.forks {
            forks
                .entry(*id)
                .or_insert_with(|| (fork, BTreeSet::new()))
                .1
                .extend(refs.iter().cloned());
        }
    }
    // The caller owns the local refs first. Admit each fork once in UUID order.
    for (fork, refs) in forks.into_values() {
        leases.push(acquire_leases(&fork.store, &fork.layout, refs, options, cancel).await?);
    }
    Ok(())
}
