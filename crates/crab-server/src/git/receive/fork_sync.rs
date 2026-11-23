use std::{io::BufReader, sync::Arc};

use crab_git::receive_plan::{ReceivePlanError, RefUpdate};
use crab_read::{UploadPackRequest, plan_upload_pack, upload_pack_repository_options};
use tokio_util::sync::CancellationToken;

use super::{MAX_BODY, REQUEST_BUDGET, ReceiveError, Result, publish};
use crate::{
    app,
    auth::Principal,
    repository::{Repository, settings},
    server::Server,
};

pub(crate) async fn synchronize_fork(
    server: Arc<Server>,
    principal: Principal,
    fork: Arc<Repository>,
    update: RefUpdate,
) -> Result<()> {
    let source = fork.fork_source(&server).ok_or(ReceiveError::NotFound)?;
    let (Some(old), Some(new)) = (update.old, update.new) else {
        return Err(ReceiveError::Request("Fork sync requires both branch tips"));
    };
    let actor = principal.identity().ok_or(ReceiveError::Forbidden)?;
    let binding = serde_json::to_vec(&(
        fork.id,
        source.id,
        &update.name,
        old.to_string(),
        new.to_string(),
        actor.issuer,
        actor.subject,
    ))
    .map_err(std::io::Error::other)?;
    let plan_id = blake3::derive_key("crab fork synchronization v1", &binding);
    let plan_id = blake3::Hash::from(plan_id).to_hex().to_string();
    if !app::membership(&server, &principal, &fork)
        .await?
        .principal
        .can_write(fork.id)
    {
        return Err(ReceiveError::Forbidden);
    }
    if publish::committed_plan(&fork, std::slice::from_ref(&update), &plan_id).await? {
        return Ok(());
    }
    if settings::load_lifecycle(&server, &fork).await?.archived {
        return Err(ReceiveError::Archived);
    }

    let cancel = server.cancellation.child_token();
    let permit = server.acquire_transfer(&cancel).await?;
    let worker = Arc::clone(&server);
    let (send, result) = tokio::sync::oneshot::channel();
    server.receives.spawn(async move {
        // Upstream repositories cannot be forks here. Take its branch lease
        // first, then let native receive lock the destination and apply policy.
        let work = async {
            let source = &source;
            crab_remote::publication::with_leases(
                &source.store,
                &source.layout,
                [update.name.clone()],
                publish::TTL,
                &cancel,
                |_holders, scoped| async move {
                    sync_locked(&worker, &principal, &fork, source, update, plan_id, &scoped).await
                },
            )
            .await
        };
        tokio::pin!(work);
        let completed = tokio::select! {
            result = &mut work => result,
            () = tokio::time::sleep(REQUEST_BUDGET) => { cancel.cancel(); work.await },
        };
        if let Err(Err(error)) = send.send(completed) {
            tracing::error!(?error, "disconnected fork synchronization failed");
        }
        permit.release().await;
    });
    result
        .await
        .map_err(|_| ReceiveError::Request("Fork sync worker stopped"))?
}

async fn sync_locked(
    server: &Server,
    principal: &Principal,
    fork: &Repository,
    source: &Arc<Repository>,
    update: RefUpdate,
    plan_id: String,
    cancel: &CancellationToken,
) -> Result<()> {
    let (Some(old), Some(new)) = (update.old, update.new) else {
        return Err(ReceiveError::Request("Fork sync requires both branch tips"));
    };
    if publish::committed_plan(fork, std::slice::from_ref(&update), &plan_id).await? {
        return Ok(());
    }
    if fork.ref_oid(&update.name).await?.as_deref() != Some(old.to_string().as_str()) {
        return Err(ReceiveError::BranchChanged);
    }
    if old == new {
        return if source.ref_oid(&update.name).await?.as_deref() == Some(new.to_string().as_str()) {
            Ok(())
        } else {
            Err(ReceiveError::BranchChanged)
        };
    }
    let (directory, reader) =
        stage_fast_forward(server, source, &update.name, old, new, cancel).await?;
    publish::publish_pack(
        server,
        principal,
        &(fork.config.owner.clone(), fork.config.name.clone()),
        directory,
        reader,
        update,
        publish::PackPublication {
            visibility_base: None,
            kind: publish::Publication::ForkSync {
                source: Arc::clone(source),
                plan_id,
            },
        },
        cancel,
    )
    .await
}

async fn stage_fast_forward(
    server: &Server,
    source: &Repository,
    branch: &str,
    old: gix_hash::ObjectId,
    new: gix_hash::ObjectId,
    cancel: &CancellationToken,
) -> Result<(
    crate::local_disk::StagingDirectory,
    BufReader<std::fs::File>,
)> {
    let (view, git) = source
        .open_capsule_repository(server, upload_pack_repository_options()?, cancel)
        .await?;
    if view.refs().get(branch).map(String::as_str) != Some(new.to_string().as_str()) {
        return Err(ReceiveError::BranchChanged);
    }
    let visibility = view.git_visibility_index().await?;
    if !visibility.contains_hex_in_ref(branch, &old.to_string()) {
        return Err(ReceivePlanError::NonFastForward {
            name: branch.into(),
        }
        .into());
    }
    let request = UploadPackRequest {
        wants: vec![new],
        haves: vec![old],
        ..Default::default()
    };
    let plan = plan_upload_pack(&git, &visibility, &[branch.to_owned()], &request, cancel).await?;
    let pack = git.generate_pack(&plan.object_ids, cancel).await?;
    if pack.size() > MAX_BODY {
        return Err(ReceiveError::TooLarge);
    }
    let directory = server.local_staging.create(pack.size(), cancel).await?;
    let reader = BufReader::new(tokio::fs::File::open(pack.path()).await?.into_std().await);
    Ok((directory, reader))
}

pub(crate) async fn merge_fork_head(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
    update: RefUpdate,
    pull: super::PullPublication,
) -> Result<()> {
    let source = pull
        .head_repository
        .as_ref()
        .map(Arc::clone)
        .ok_or(ReceiveError::NotFound)?;
    let cancel = pull.cancel.clone();
    let permit = server.acquire_transfer(&cancel).await?;
    let result = async {
        let old = update.old.ok_or(ReceiveError::BranchChanged)?;
        let (directory, reader) =
            stage_fast_forward(server, &source, &pull.head_ref, old, pull.head_oid, &cancel)
                .await?;
        publish::publish_pack(
            server,
            principal,
            key,
            directory,
            reader,
            update,
            publish::PackPublication {
                visibility_base: None,
                kind: publish::Publication::PullRequest(pull),
            },
            &cancel,
        )
        .await
    }
    .await;
    permit.release().await;
    result
}
