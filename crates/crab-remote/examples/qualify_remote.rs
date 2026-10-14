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
                "usage: qualify_remote <bucket> <prefix> <text-path> [operation|all|build-indexes|checkpoint|ref-point|resolve-oid|blame-history|blame-history-uncached]",
            )
            .into()),
        };
    if selection != "all"
        && selection != "build-indexes"
        && selection != "checkpoint"
        && selection != "graph-pages"
        && selection != "graph-ancestry"
        && selection != "ref-point"
        && selection != "resolve-oid"
        && selection != "blame-history"
        && selection != "blame-history-uncached"
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
    if selection == "ref-point" {
        let root = counters
            .measure("open", "cold", async {
                Ok(crab_metadata::capsule_protocol::load_root(&layout).await?)
            })
            .await?;
        let names = std::collections::BTreeSet::from([root.record().root().head().to_owned()]);
        let mut previous = None;
        for cache in ["cold", "warm"] {
            let refs = counters
                .measure("ref-point", cache, async {
                    Ok(
                        crab_read::capsule_protocol::read_visible_refs_from_root_for_refs(
                            &layout, &root, &names,
                        )
                        .await?,
                    )
                })
                .await?;
            if previous.as_ref().is_some_and(|value| value != &refs) {
                return Err(io::Error::other("selected refs changed during qualification").into());
            }
            previous = Some(refs);
        }
        println!(
            "{}",
            json!({"kind":"result", "operation":"ref-point", "value":previous})
        );
    } else if selection == "checkpoint" {
        counters
            .measure("checkpoint", "maintenance", async {
                let outcome = crab_remote::checkpoint::publish_capsule_checkpoint(
                    &layout,
                    0,
                    2 * 1024 * 1024 * 1024,
                    &CancellationToken::new(),
                )
                .await?;
                if !outcome.published {
                    return Err(io::Error::other("checkpoint publication lost its captured root").into());
                }
                let root = crab_metadata::capsule_protocol::load_root(&layout).await?;
                let pointer = root.record().root().checkpoint().ok_or_else(|| {
                    io::Error::other("published root does not advertise a checkpoint")
                })?;
                let checkpoint = crab_metadata::capsule_protocol::load_layered_checkpoint_control(
                    &layout, pointer,
                )
                .await?;
                println!("{}", json!({"kind":"checkpoint", "hash":pointer.hash(), "bytes":pointer.size(), "control_bytes":pointer.control_size(), "sources":checkpoint.source_count(), "objects":checkpoint.object_count()?, "pack_bytes_read":outcome.pack_bytes_read, "pack_bytes_written":outcome.pack_bytes_written}));
                Ok(())
            })
            .await?;
    } else if selection == "build-indexes" {
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
    } else if matches!(selection, "graph-pages" | "graph-ancestry") {
        qualify_graph_pages(&layout, &path, &counters, selection).await?;
    } else {
        let selected_workload = if selection == "blame-history-uncached" {
            "blame-history"
        } else {
            selection
        };
        let workloads = match selected_workload {
            "resolve-oid" => &["resolve-oid"][..],
            "blame-history" => &["blame-history"][..],
            _ => WORKLOADS,
        };
        for &name in workloads
            .iter()
            .filter(|&&name| selected_workload == "all" || selected_workload == name)
        {
            let runtime = if selection == "blame-history-uncached" {
                RemoteGitRuntime::new(
                    crab_remote_git::RuntimeOptions {
                        max_blame_cache_entries: 0,
                        ..crab_remote_git::RuntimeOptions::default()
                    },
                    Arc::new(crab_remote_git::NoopMetrics),
                )?
            } else {
                RemoteGitRuntime::default()
            };
            let runtime = Arc::new(runtime);
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
        "resolve" | "resolve-oid" => OperationKind::Resolve,
        "commit" => OperationKind::Commit,
        "tree" => OperationKind::Tree,
        "history" => OperationKind::History,
        "path-history" => OperationKind::PathHistory,
        "attribution" => OperationKind::DirectoryAttribution,
        "blame" | "blame-history" => OperationKind::Blame,
        _ => OperationKind::Content,
    };
    let requested_oid = if name == "resolve-oid" {
        Some(gix_hash::ObjectId::from_hex(path.as_bytes())?)
    } else {
        None
    };
    let operation = repository.operation(kind, cancel).await?;
    // Keep every failure inside the semantic result until the locator closes.
    let result = async {
        let snapshot = repository.snapshot(revision, &operation).await?;
        match name {
            "resolve" | "resolve-oid" => {
                // Ref-tip resolution alone bypasses ancestor membership. Explicit
                // commit reads must also exercise the foreground visibility gate.
                let oid = requested_oid.unwrap_or(snapshot.commit_oid());
                let resolved = repository
                    .resolve(&Revision::Commit(oid), &operation)
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
            "blame-history" => {
                let history = snapshot.history(HistoryTraversal::FirstParent, &PageRequest::new(10, None)?, &operation).await?;
                let mut entries = Vec::new();
                for commit in history.items {
                    let version = repository.snapshot(&Revision::Commit(commit.oid), &operation).await?;
                    let blame = version.blame(path, &operation).await?;
                    entries.push(json!({"commit":commit.oid.to_string(), "ranges":blame.ranges.iter().map(|range| json!({
                        "start":range.start, "lines":range.lines, "commit":range.commit.oid.to_string(),
                        "source_path":range.source_path.as_bytes(),
                    })).collect::<Vec<_>>()}));
                }
                Ok(json!({"entries":entries}))
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
