//! Measure isolated cold/warm capsule reads and emit independent-oracle evidence.
mod support;

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
                "usage: qualify_remote <bucket> <prefix> <text-path> [operation|all|build-indexes|visibility-members]",
            )
            .into()),
        };
    if selection != "all"
        && selection != "build-indexes"
        && selection != "graph-pages"
        && selection != "graph-ancestry"
        && selection != "visibility-members"
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
                if complete.path_state_hash().is_none() || complete.object_index_hash().is_none() {
                    return Err(io::Error::other(
                        "physical lookup and path attribution were not completely published",
                    )
                    .into());
                }
                let hash = complete.object_index_hash().ok_or_else(|| io::Error::other("physical index readiness is absent"))?;
                let root = crab_metadata::capsule_protocol::load_root(&layout).await?;
                let view = crab_read::capsule_protocol::open_view_from_root_with_metadata(
                    &layout, root,
                    crab_read::capsule_protocol::CapsuleReadLimits { max_capsule_bytes: 2 * 1024 * 1024 * 1024, max_frontier_bytes: 2 * 1024 * 1024 * 1024 },
                ).await?;
                let snapshot = view.git_snapshot()?;
                let index = crab_metadata::git_object_locator::PagedGitObjectIndex::load(
                    layout.store(), &layout, hash, &snapshot.manifest_etag, &snapshot.manifest,
                    options.object_limits().max_pack_index_bytes,
                ).await?;
                println!("{}", json!({"kind":"physical-index", "descriptor_hash":hash, "body_bytes":layout.store().head(index.body_path()).await?.size}));
                Ok(())
            })
            .await;
        runtime.shutdown().await;
        result?;
    } else if selection == "visibility-members" {
        qualify_visibility_members(&layout, &counters).await?;
    } else if matches!(selection, "graph-pages" | "graph-ancestry") {
        qualify_graph_pages(&layout, &path, &counters, selection).await?;
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

// This elected maintenance mode publishes authoritative metadata. Run only in
// a dedicated qualification namespace; ordinary reads never invoke it.
async fn qualify_visibility_members(
    layout: &StoreLayout<crab_storage::Store>,
    counters: &Measurements,
) -> Result<()> {
    counters
        .measure("visibility-checkpoint", "maintenance", async {
            crab_remote::checkpoint::publish_capsule_checkpoint(
                layout,
                1,
                128 * 1024 * 1024,
                &CancellationToken::new(),
            )
            .await?;
            Ok(())
        })
        .await?;
    let root = crab_metadata::capsule_protocol::load_root(layout).await?;
    let pointer = root
        .record()
        .root()
        .checkpoint()
        .ok_or_else(|| io::Error::other("checkpoint is absent"))?;
    let full = crab_metadata::capsule_protocol::load_layered_checkpoint(layout, pointer).await?;
    let snapshot = full
        .visibility_ordinal_snapshot()?
        .ok_or_else(|| io::Error::other("ordinal visibility is absent"))?;
    let objects = snapshot.objects();
    if objects.is_empty() {
        return Err(io::Error::other("visibility dictionary is empty").into());
    }
    let admission = snapshot
        .member_admission()
        .ok_or_else(|| io::Error::other("member admission is absent"))?;
    let requested = [objects[0], objects[objects.len() - 1], objects[0]];
    let expected = Some(vec![
        Some(admission[0]),
        Some(admission[objects.len() - 1]),
        Some(admission[0]),
    ]);
    for sample in 0..20 {
        let control = counters
            .measure("visibility-open", "cold", async {
                Ok(
                    crab_metadata::capsule_protocol::load_layered_checkpoint_control(
                        layout, pointer,
                    )
                    .await?,
                )
            })
            .await?;
        let path = layout.capsule_checkpoint_path(control.hash());
        let reader = control
            .visibility_reader(path, 128 * 1024)?
            .ok_or_else(|| io::Error::other("visibility pages are absent"))?;
        for cache in ["cold", "warm"] {
            let observed = counters
                .measure("visibility-members", cache, async {
                    Ok(reader.members(layout.store(), &requested).await?)
                })
                .await?;
            if observed != expected {
                return Err(io::Error::other(
                    "paged admission differs from the canonical snapshot",
                )
                .into());
            }
        }
        println!(
            "{}",
            json!({"kind":"visibility-sample", "sample":sample, "oracle":"complete canonical snapshot", "objects":objects.len(), "duplicates_preserved":true})
        );
    }
    Ok(())
}

// This measures the graph paging component, with existing eager control capture
// reported as opening. It makes no foreground visibility or bounded-opening claim.
async fn qualify_graph_pages(
    layout: &StoreLayout<crab_storage::Store>,
    path: &GitPath,
    counters: &Measurements,
    selection: &str,
) -> Result<()> {
    use crab_metadata::split_commit_graph::{
        CommitGraphReader, DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
    };
    let open = async || -> Result<(CommitGraphReader, gix_hash::ObjectId)> {
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
        let tip = view
            .peeled_refs()
            .get(view.head())
            .or_else(|| view.refs().get(view.head()))
            .ok_or_else(|| io::Error::other("captured HEAD is absent"))?;
        let tip = gix_hash::ObjectId::from_hex(tip.as_bytes())?;
        Ok((reader, tip))
    };
    let (reader, tip) = counters.measure("open", "cold", open()).await?;
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
            let membership = if selection == "graph-ancestry" {
                let gix_hash::ObjectId::Sha1(tip_bytes) = tip else {
                    return Err(io::Error::other("unsupported graph tip hash").into());
                };
                let tip_ordinal = reader.ordinals(layout.store(), &[tip_bytes]).await?[0]
                    .ok_or_else(|| io::Error::other("captured HEAD is not a graph commit"))?;
                Some(
                    reader
                        .ancestry_membership(layout.store(), tip_ordinal, &requested)
                        .await?,
                )
            } else {
                None
            };
            Ok(json!({"records": rows, "root": tip.to_string(), "membership": membership}))
        };
    let cold = counters.measure(selection, "cold", query()).await?;
    let warm = counters.measure(selection, "warm", query()).await?;
    if cold != warm {
        return Err(io::Error::other("cold/warm graph queries differ").into());
    }
    println!(
        "{}",
        json!({"kind":"result", "operation":selection, "path":path.as_bytes(), "value":cold})
    );
    let reopened = counters.measure("open", "warm", open()).await?;
    if reopened.0.descriptor() != reader.descriptor() || reopened.1 != tip {
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
            query(name, "cold", &repository, &revision, path, &cancel),
        )
        .await?;
    let warm = counters
        .measure(
            name,
            "warm",
            query(name, "warm", &repository, &revision, path, &cancel),
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
    cache: &str,
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
    let operation = phase(name, cache, "admission", repository.operation(kind, cancel)).await?;
    // Keep every failure inside the semantic result until the locator closes.
    let result = async {
        let snapshot = phase(
            name, cache, "snapshot", repository.snapshot(revision, &operation),
        ).await?;
        match name {
            "resolve" => {
                // Ref resolution alone bypasses ancestor membership. Resolve the
                // same immutable OID through the foreground visibility boundary.
                let resolved = repository
                    .resolve(&Revision::Commit(snapshot.commit_oid()), &operation)
                    .await?;
                Ok(json!({"commit":resolved.commit.to_string()}))
            }
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
                let page = phase(name, cache, "directory", snapshot.list_directory(&GitPath::root(), &PageRequest::new(100, None)?, &operation)).await?;
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
                        let commits = phase(name, cache, "attribution", snapshot.latest_directory_entry_commits(&GitPath::root(), &page.items, &operation)).await?;
                        Ok(json!({"entries":page.items.iter().zip(commits).map(|(entry, commit)| json!({
                            "path":entry.path.as_bytes(), "oid":entry.oid.to_string(), "commit":commit.oid.to_string(),
                        })).collect::<Vec<_>>()}))
                    }
                    _ => Err(crab_remote_git::Error::InternalInvariant { invariant:"unknown qualification operation" }),
                }
            }
        }
    }.await;
    Ok(phase(name, cache, "cleanup", operation.finish(result)).await?)
}

// Phase clocks preserve the enclosing measurement's counters and include
// cleanup. Printing only operation labels keeps paths and content out of timing rows.
async fn phase<T>(
    name: &str,
    cache: &str,
    step: &str,
    future: impl Future<Output = crab_remote_git::Result<T>>,
) -> crab_remote_git::Result<T> {
    let started = Instant::now();
    let result = future.await;
    println!(
        "{}",
        json!({
            "kind":"phase", "schema":1, "operation":name, "cache":cache,
            "phase":step, "elapsed_us":started.elapsed().as_micros(),
            "outcome":if result.is_ok() { "ok" } else { "error" },
        })
    );
    result
}
