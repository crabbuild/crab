use std::{io, sync::Arc};

use crab_remote_git::{
    GitPath, HistoryTraversal, OperationKind, PageCursor, PageRequest, RemoteGitRepository,
    RemoteGitRuntime, RepositoryIdentity, RepositoryOptions, Revision,
};
use crab_storage::{Store, StoreLayout};
use gix_hash::ObjectId;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::support::{Measurements, Result, open_repository};

const PAGE_SIZE: usize = 100;
const PAGE_DEPTHS: [usize; 3] = [1, 10, 100];

struct PreparedPage {
    depth: usize,
    cursor: Option<PageCursor>,
    expected: Value,
}

pub async fn qualify(
    layout: &StoreLayout<Store>,
    identity: &RepositoryIdentity,
    options: RepositoryOptions,
    path: &GitPath,
    counters: &Arc<Measurements>,
) -> Result<()> {
    let cancel = CancellationToken::new();
    let preparation_runtime =
        Arc::new(RemoteGitRuntime::new(Default::default(), counters.clone())?);
    // Cursor preparation must not populate the runtime of the measured request.
    let prepared = counters
        .measure("prepare-history-pages", "setup", async {
            let repository = open_repository(
                layout.clone(),
                identity.clone(),
                preparation_runtime.clone(),
                options,
                &cancel,
            )
            .await?;
            prepare(&repository, &cancel).await
        })
        .await;
    preparation_runtime.shutdown().await;
    let (commit, pages) = prepared?;
    for page in pages {
        let runtime = Arc::new(RemoteGitRuntime::new(Default::default(), counters.clone())?);
        let result: Result<()> = async {
            let repository = counters.measure("open", "cold", open_repository(
                layout.clone(), identity.clone(), runtime.clone(), options, &cancel,
            )).await?;
            let name = format!("history-page-{}", page.depth);
            for cache in ["cold", "warm"] {
                let value = counters.measure(&name, cache, query(
                    &repository, commit, page.depth, page.cursor.clone(), &cancel,
                )).await?;
                if value != page.expected {
                    return Err(io::Error::other("resumed history differs from prepared page").into());
                }
            }
            println!("{}", json!({"kind":"result", "operation":name, "path":path.as_bytes(), "value":page.expected}));
            let reopened = counters.measure("open", "warm", open_repository(
                layout.clone(), identity.clone(), runtime.clone(), options, &cancel,
            )).await?;
            if repository.refs() != reopened.refs() {
                return Err(io::Error::other("refs changed during history qualification").into());
            }
            Ok(())
        }.await;
        runtime.shutdown().await;
        result?;
    }
    Ok(())
}

async fn prepare(
    repository: &RemoteGitRepository,
    cancel: &CancellationToken,
) -> Result<(ObjectId, Vec<PreparedPage>)> {
    let head = repository
        .refs()
        .head
        .as_ref()
        .ok_or_else(|| io::Error::other("repository has no HEAD"))?;
    let operation = repository.operation(OperationKind::History, cancel).await?;
    let snapshot = repository
        .snapshot(&Revision::Reference(head.name.clone()), &operation)
        .await;
    let snapshot = operation.finish(snapshot).await?;
    let commit = snapshot.commit_oid();
    let mut cursor = None;
    let mut pages = Vec::new();
    for depth in 1..=PAGE_DEPTHS[2] {
        let operation = repository.operation(OperationKind::History, cancel).await?;
        let result = async {
            let request = PageRequest::new(PAGE_SIZE, cursor.clone())?;
            snapshot
                .history(HistoryTraversal::FirstParent, &request, &operation)
                .await
        }
        .await;
        let page = operation.finish(result).await?;
        if PAGE_DEPTHS.contains(&depth) {
            pages.push(PreparedPage {
                depth,
                cursor,
                expected: value(depth, &page),
            });
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    Ok((commit, pages))
}

async fn query(
    repository: &RemoteGitRepository,
    commit: ObjectId,
    depth: usize,
    cursor: Option<PageCursor>,
    cancel: &CancellationToken,
) -> Result<Value> {
    let operation = repository.operation(OperationKind::History, cancel).await?;
    let result = async {
        let snapshot = repository
            .snapshot(&Revision::Commit(commit), &operation)
            .await?;
        let request = PageRequest::new(PAGE_SIZE, cursor)?;
        let page = snapshot
            .history(HistoryTraversal::FirstParent, &request, &operation)
            .await?;
        Ok(value(depth, &page))
    }
    .await;
    Ok(operation.finish(result).await?)
}

fn value(depth: usize, page: &crab_remote_git::Page<crab_remote_git::Commit>) -> Value {
    json!({"page":depth, "page_size":PAGE_SIZE,
        "commits":page.items.iter().map(|commit| commit.oid.to_string()).collect::<Vec<_>>(),
        "has_more":page.next.is_some()})
}
