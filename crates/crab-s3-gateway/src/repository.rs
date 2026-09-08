use std::{sync::Arc, time::Duration};

use crab_remote_git::{RemoteGitRepository, RemoteGitRuntime, RepositoryOptions};
use tokio_util::sync::CancellationToken;

use crate::gateway::Repository;

const MAINTENANCE_TTL: Duration = Duration::from_secs(60);

pub(crate) async fn open_current(
    repository: &Repository,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    cancel: &CancellationToken,
) -> crate::Result<RemoteGitRepository> {
    let open = || {
        RemoteGitRepository::open(
            repository.store.clone(),
            repository.layout.clone(),
            repository.identity.clone(),
            Arc::clone(&runtime),
            options,
            cancel,
        )
    };
    match open().await {
        Ok(remote) if remote.refs().is_empty() || remote.commit_graph_available() => {
            return Ok(remote);
        }
        Ok(_) | Err(crab_remote_git::Error::RepositoryIndexing { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    ensure_readable(repository, Arc::clone(&runtime), options, cancel).await?;
    open().await.map_err(Into::into)
}

pub(crate) async fn ensure_readable(
    repository: &Repository,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    cancel: &CancellationToken,
) -> crate::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let store = repository.store.clone();
        let layout = repository.layout.clone();
        let identity = repository.identity.clone();
        let publication_runtime = Arc::clone(&runtime);
        let publication_cancel = cancel.clone();
        // The publication future is deeply nested. A task boundary prevents its
        // poll stack from accumulating with the request handler's read retry.
        tokio::spawn(async move {
            crab_write::generation::ensure_readable(
                &store,
                &layout,
                &identity,
                publication_runtime,
                options,
                MAINTENANCE_TTL,
                &publication_cancel,
            )
            .await
        })
        .await??;
        let opened = RemoteGitRepository::open(
            repository.store.clone(),
            repository.layout.clone(),
            repository.identity.clone(),
            Arc::clone(&runtime),
            options,
            cancel,
        )
        .await;
        match opened {
            Ok(remote) if remote.refs().is_empty() || remote.commit_graph_available() => {
                return Ok(());
            }
            Ok(_) | Err(crab_remote_git::Error::RepositoryIndexing { .. }) => {}
            Err(error) => return Err(error.into()),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(crate::Error::ReadinessTimeout);
        }
        tokio::select! {
            () = cancel.cancelled() => return Err(crab_remote_git::Error::Cancelled.into()),
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }
}
