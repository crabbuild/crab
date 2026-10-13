//! Measure isolated cold/warm capsule reads and emit independent-oracle evidence.
mod support;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use crab_remote_git::{
    GitPath, HistoryTraversal, ObjectLimits, OperationKind, OperationLimits, PageRequest,
    RemoteGitRepository, RemoteGitRuntime, RepositoryIdentity, RepositoryOptions, Revision,
};
use crab_storage::{StorageProviderKind, StoreLayout, build_static_env_store};
use serde_json::{Value, json};
use support::{Measurements, Result, open_repository};
use tokio_util::sync::CancellationToken;

const WORKLOADS: &[&str] = &[
    "resolve",
    "commit",
    "tree",
    "blob",
    "object-point",
    "object-batch",
    "history",
    "path-history",
    "attribution",
    "blame",
];

#[tokio::main]
async fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let (bucket, prefix, path, selection) =
        match args.as_slice() {
            [bucket, prefix, path] => (bucket, prefix, path, "all"),
            [bucket, prefix, path, selection] => (bucket, prefix, path, selection.as_str()),
            _ => return Err(io::Error::other(
                "usage: qualify_remote <bucket> <prefix> <text-path> [operation|all|build-indexes]",
            )
            .into()),
        };
    if selection != "all"
        && selection != "build-indexes"
        && selection != "graph-pages"
        && !WORKLOADS.contains(&selection)
    {
        return Err(io::Error::other("unknown qualification operation").into());
    }
    let path = GitPath::new(Bytes::copy_from_slice(path.as_bytes()))?;
    let options = RepositoryOptions::new(
        ObjectLimits::default(),
        OperationLimits {
            max_duration: Duration::from_secs(3600),
            max_logical_objects: 200_000,
            max_history_commits: 200_000,
            max_storage_requests: 500_000,
            max_fetched_bytes: 4 * 1024 * 1024 * 1024,
            max_inflated_bytes: 4 * 1024 * 1024 * 1024,
            ..OperationLimits::default()
        },
    )?;
    let counters = Arc::new(Measurements::default());
    counters.install()?;
    let store = build_static_env_store(bucket, StorageProviderKind::S3)?
        .with_storage_observer(counters.clone());
    let layout = StoreLayout::new(store, prefix.to_owned());
    let identity = RepositoryIdentity::new(format!("s3:{bucket}"), prefix.as_str(), 1)?;
    if selection == "build-indexes" {
        let runtime = Arc::new(RemoteGitRuntime::default());
        let cancel = CancellationToken::new();
        let result = counters
            .measure("build-indexes", "maintenance", async {
                let complete = crab_remote::browse_indexes::ensure(
                    &layout,
                    &identity,
                    runtime.clone(),
                    options,
                    2 * 1024 * 1024 * 1024,
                    &cancel,
                )
                .await?
                .ok_or_else(|| {
                    io::Error::other("index build was superseded or repository was empty")
                })?;
                if complete.path_state_hash().is_none() {
                    return Err(
                        io::Error::other("path attribution was not completely published").into(),
                    );
                }
                Ok(())
            })
            .await;
        runtime.shutdown().await;
        result?;
    } else if selection == "graph-pages" {
        qualify_graph_pages(&layout, &path, &counters).await?;
    } else {
        for &name in WORKLOADS
            .iter()
            .filter(|&&name| selection == "all" || selection == name)
        {
            let runtime = Arc::new(RemoteGitRuntime::default());
            let result = qualify(
                name,
                &layout,
                &identity,
                runtime.clone(),
                options,
                &path,
                &counters,
            )
            .await;
            runtime.shutdown().await;
            result?;
        }
    }
    println!(
        "{}",
        json!({"kind":"complete", "schema":1, "selection":selection})
    );
    Ok(())
}

// This measures the graph paging component, with existing eager control capture
// reported as opening. It makes no foreground visibility or bounded-opening claim.
async fn qualify_graph_pages(
    layout: &StoreLayout<crab_storage::Store>,
    path: &GitPath,
    counters: &Measurements,
) -> Result<()> {
    use crab_metadata::split_commit_graph::{
        CommitGraphReader, DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
    };
    let open = async || -> Result<CommitGraphReader> {
        let root = crab_metadata::capsule_protocol::load_root(layout).await?;
        let indexes = crab_metadata::capsule_protocol::load_browse_indexes(layout)
            .await?
            .ok_or_else(|| io::Error::other("browse indexes are absent"))?;
        let view = crab_read::capsule_protocol::open_view_from_root_with_control(
            layout,
            root,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: 2 * 1024 * 1024 * 1024,
                max_frontier_bytes: 2 * 1024 * 1024 * 1024,
            },
        )
        .await?;
        if indexes.state_digest() != view.state_digest() {
            return Err(io::Error::other("browse index does not match captured authority").into());
        }
        let snapshot = view.git_snapshot()?;
        let reader = CommitGraphReader::load(
            layout.store(),
            layout,
            indexes.commit_graph_hash(),
            DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
        )
        .await?;
        let descriptor = reader.descriptor();
        if descriptor.generation != snapshot.manifest.generation
            || descriptor.pack_index_hash != snapshot.manifest.pack_index_hash
            || descriptor.git_validation_digest != snapshot.manifest.git_validation_digest
        {
            return Err(io::Error::other("graph does not match captured Git identity").into());
        }
        Ok(reader)
    };
    let reader = counters.measure("open", "cold", open()).await?;
    let count = reader.descriptor().commit_count;
    if count == 0 {
        return Err(io::Error::other("graph is empty").into());
    }
    let requested = [0, count / 2, count - 1, count / 2];
    let query =
        async || -> Result<Value> {
            let records = reader
                .records(layout.store(), &requested)
                .await?
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| io::Error::other("required graph record is absent"))?;
            let oids = records.iter().map(|record| record.oid).collect::<Vec<_>>();
            if reader.ordinals(layout.store(), &oids).await?
                != requested.iter().copied().map(Some).collect::<Vec<_>>()
            {
                return Err(io::Error::other("graph OID/ordinal batch differs").into());
            }
            let parents = records
                .iter()
                .flat_map(|record| record.parents.iter().copied())
                .collect::<Vec<_>>();
            let parents = reader
                .records(layout.store(), &parents)
                .await?
                .into_iter()
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| io::Error::other("graph parent is absent"))?;
            let mut parents = parents.into_iter();
            let rows = records.iter().zip(requested).map(|(record, ordinal)| {
            let parent_oids = parents.by_ref().take(record.parents.len())
                .map(|record| gix_hash::ObjectId::Sha1(record.oid).to_string()).collect::<Vec<_>>();
            json!({"ordinal": ordinal, "oid": gix_hash::ObjectId::Sha1(record.oid).to_string(),
                "tree": gix_hash::ObjectId::Sha1(record.tree_oid).to_string(),
                "parents": parent_oids, "committer_seconds": record.commit_time})
        }).collect::<Vec<_>>();
            Ok(json!({"records": rows}))
        };
    let cold = counters.measure("graph-pages", "cold", query()).await?;
    let warm = counters.measure("graph-pages", "warm", query()).await?;
    if cold != warm {
        return Err(io::Error::other("cold/warm graph queries differ").into());
    }
    println!(
        "{}",
        json!({"kind":"result", "operation":"graph-pages", "path":path.as_bytes(), "value":cold})
    );
    let reopened = counters.measure("open", "warm", open()).await?;
    if reopened.descriptor() != reader.descriptor() {
        return Err(io::Error::other("graph changed during qualification").into());
    }
    Ok(())
}

async fn qualify(
    name: &str,
    layout: &StoreLayout<crab_storage::Store>,
    identity: &RepositoryIdentity,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    path: &GitPath,
    counters: &Measurements,
) -> Result<()> {
    let cancel = CancellationToken::new();
    let repository = counters
        .measure(
            "open",
            "cold",
            open_repository(
                layout.clone(),
                identity.clone(),
                runtime.clone(),
                options,
                &cancel,
            ),
        )
        .await?;
    let head = repository
        .refs()
        .head
        .as_ref()
        .ok_or_else(|| io::Error::other("repository has no HEAD"))?;
    let revision = Revision::Reference(head.name.clone());
    let cold = counters
        .measure(
            name,
            "cold",
            query(name, &repository, &revision, path, &cancel),
        )
        .await?;
    let warm = counters
        .measure(
            name,
            "warm",
            query(name, &repository, &revision, path, &cancel),
        )
        .await?;
    if cold != warm {
        return Err(io::Error::other("cold/warm query results differ").into());
    }
    println!(
        "{}",
        json!({"kind":"result", "operation":name, "path":path.as_bytes(), "value":cold})
    );
    let reopened = counters
        .measure(
            "open",
            "warm",
            open_repository(layout.clone(), identity.clone(), runtime, options, &cancel),
        )
        .await?;
    if repository.refs() != reopened.refs() {
        return Err(io::Error::other("refs changed during qualification").into());
    }
    Ok(())
}

async fn query(
    name: &str,
    repository: &RemoteGitRepository,
    revision: &Revision,
    path: &GitPath,
    cancel: &CancellationToken,
) -> Result<Value> {
    let kind = match name {
        "resolve" => OperationKind::Resolve,
        "commit" => OperationKind::Commit,
        "tree" => OperationKind::Tree,
        "history" => OperationKind::History,
        "path-history" => OperationKind::PathHistory,
        "attribution" => OperationKind::DirectoryAttribution,
        "blame" => OperationKind::Blame,
        _ => OperationKind::Content,
    };
    let operation = repository.operation(kind, cancel).await?;
    // Keep every failure inside the semantic result until the locator closes.
    let result = async {
        let snapshot = repository.snapshot(revision, &operation).await?;
        match name {
            "resolve" => Ok(json!({"commit":snapshot.commit_oid().to_string()})),
            "commit" => {
                let commit = snapshot.commit(&operation).await?;
                Ok(json!({"oid":commit.oid.to_string(), "tree":commit.tree.to_string(),
                    "parents":commit.parents.iter().map(ToString::to_string).collect::<Vec<_>>()}))
            }
            "blob" => {
                let blob = snapshot.read_blob(path, &operation).await?;
                Ok(json!({"oid":blob.metadata.oid.to_string(), "kind":"blob", "bytes":blob.bytes.len(),
                    "computed_oid":support::object_oid("blob", &blob.bytes)}))
            }
            "object-point" => {
                let entry = snapshot.entry(path, &operation).await?.ok_or(crab_remote_git::Error::PathNotFound)?;
                let object = operation.read_object(entry.oid).await?;
                Ok(json!({"oid":object.oid.to_string(), "kind":&object.kind.to_string(),
                    "bytes":object.data.len(), "computed_oid":support::object_oid(&object.kind.to_string(), &object.data)}))
            }
            "history" => {
                let page = snapshot.history(HistoryTraversal::FirstParent, &PageRequest::new(1_000, None)?, &operation).await?;
                Ok(json!({"commits":page.items.iter().map(|commit| commit.oid.to_string()).collect::<Vec<_>>(),
                    "has_more":page.next.is_some()}))
            }
            "path-history" => {
                let page = snapshot.path_history(path, HistoryTraversal::FirstParent,
                    &PageRequest::new(1_000, None)?, &operation).await?;
                Ok(json!({"commits":page.items.iter().map(|entry| entry.commit.oid.to_string()).collect::<Vec<_>>(),
                    "has_more":page.next.is_some()}))
            }
            "blame" => {
                let blame = snapshot.blame(path, &operation).await?;
                Ok(json!({"ranges":blame.ranges.iter().map(|range| json!({
                    "start":range.start, "lines":range.lines, "commit":range.commit.oid.to_string(),
                    "source_path":range.source_path.as_bytes(),
                })).collect::<Vec<_>>()}))
            }
            _ => {
                let page = snapshot.list_directory(&GitPath::root(), &PageRequest::new(100, None)?, &operation).await?;
                match name {
                    "tree" => Ok(json!({"entries":page.items.iter().map(|entry| json!({
                        "path":entry.path.as_bytes(), "oid":entry.oid.to_string(), "mode":entry.mode.raw(),
                    })).collect::<Vec<_>>(), "has_more":page.next.is_some()})),
                    "object-batch" => {
                        let oids = page.items.iter().map(|entry| entry.oid).collect::<Vec<_>>();
                        let objects = operation.read_objects(&oids).await?;
                        Ok(json!({"objects":objects.iter().map(|object| json!({
                            "oid":object.oid.to_string(), "kind":&object.kind.to_string(), "bytes":object.data.len(),
                            "computed_oid":support::object_oid(&object.kind.to_string(), &object.data),
                        })).collect::<Vec<_>>()}))
                    }
                    "attribution" => {
                        let commits = snapshot.latest_directory_entry_commits(&GitPath::root(), &page.items, &operation).await?;
                        Ok(json!({"entries":page.items.iter().zip(commits).map(|(entry, commit)| json!({
                            "path":entry.path.as_bytes(), "oid":entry.oid.to_string(), "commit":commit.oid.to_string(),
                        })).collect::<Vec<_>>()}))
                    }
                    _ => Err(crab_remote_git::Error::InternalInvariant { invariant:"unknown qualification operation" }),
                }
            }
        }
    }.await;
    Ok(operation.finish(result).await?)
}
