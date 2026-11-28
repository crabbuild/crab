//! Diskless Git import: protocol-v2 fetch, staged pack, streamed index, streamed run.
//!
//! The fetched pack streams into one staging object while its frames are
//! verified. A second sequential read reconstructs and identifies every object,
//! and the published run streams the staged bytes between small in-memory
//! sections. No local file is written and the source pack keeps its deltas.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bytes::Bytes;
use crab_git::import_graph::{CompleteGraph, GraphLimits, ImportGraph};
use crab_git::pack_stream::{
    IndexedPack, PackResolver, PackScanner, PackTable, StreamLimits, WindowedSource,
};
use crab_git::receive_plan::{RefPolicy, RefUpdate, VerifiedObjects};
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
    CapsuleVisibilityDelta, DetachedGitPack, DetachedPackCapsule, StreamedLeafRun,
};
use crab_metadata::git_visibility::{
    GIT_VISIBILITY_EDIT_VERSION, GitVisibilityDictionary, GitVisibilityEdit, GitVisibilityObjects,
};
use crab_storage::Store;
use futures_util::{StreamExt, TryStreamExt, stream::FuturesUnordered};
use gix_hash::ObjectId;
use gix_object::Kind;
use object_store::path::Path;
use tokio_util::sync::CancellationToken;

use super::{ImportError, remote::Remote};

const MAX_IMPORT_REFS: usize = 100_000;
const PART_BYTES: usize = 16 * 1024 * 1024;
const PARTS_IN_FLIGHT: usize = 4;
const CHANNEL_CHUNKS: usize = 8;
// Delta bases plus re-read windows bound pass-two memory near 1 GiB per import.
const BASE_CACHE_BYTES: usize = 768 * 1024 * 1024;
const WINDOW_BYTES: u64 = 4 * 1024 * 1024;
const WINDOW_SLOTS: usize = 64;
const LEASE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

// Matches native receive validation: every object, entry, and history read counts.
const SYNC_GRAPH: crab_git::receive_plan::GraphLimits = crab_git::receive_plan::GraphLimits {
    max_ref_updates: MAX_IMPORT_REFS,
    max_graph_steps: 100_000_000,
    max_object_bytes: LIMITS.max_object_bytes,
    max_read_bytes: 64 * 1024 * 1024 * 1024,
};

pub(super) const LIMITS: StreamLimits = StreamLimits {
    max_pack_bytes: 64 * 1024 * 1024 * 1024,
    max_objects: 10_000_000,
    max_object_bytes: 256 * 1024 * 1024,
    max_inflated_bytes: 4 * 1024 * 1024 * 1024 * 1024,
    max_delta_depth: 128,
    max_cache_bytes: BASE_CACHE_BYTES,
};

/// Branch and tag tips advertised by the source, with its default branch.
pub(super) struct Discovered {
    pub(super) head: String,
    pub(super) refs: BTreeMap<String, String>,
    pub(super) peeled: BTreeMap<String, ObjectId>,
}

impl Discovered {
    pub(super) fn tips(&self) -> Result<Vec<ObjectId>, ImportError> {
        let unique = self
            .refs
            .values()
            .map(|oid| object_id(oid))
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(unique.into_iter().collect())
    }
}

/// List the source's branches and tags; other namespaces are never imported.
pub(super) async fn discover(remote: &Remote) -> Result<Discovered, ImportError> {
    remote.capabilities().await?;
    let listed = remote
        .list_refs(&["HEAD", "refs/heads/", "refs/tags/"])
        .await?;
    let mut refs = BTreeMap::new();
    let mut peeled = BTreeMap::new();
    let mut head_target = None;
    for reference in listed {
        if reference.name == "HEAD" {
            head_target = reference.symref_target;
            continue;
        }
        // ref-prefix is advisory; a server may list more than was asked.
        if !reference.name.starts_with("refs/heads/") && !reference.name.starts_with("refs/tags/") {
            continue;
        }
        let Some(oid) = reference.oid else {
            continue;
        };
        crab_git::validate_push_refname(&reference.name).map_err(|_| {
            ImportError::Git("The Git source contains an unsupported ref name".into())
        })?;
        if let Some(target) = reference.peeled {
            peeled.insert(reference.name.clone(), target);
        }
        if refs.insert(reference.name, oid.to_string()).is_some() {
            return Err(ImportError::Git(
                "The Git source advertised a duplicate ref".into(),
            ));
        }
    }
    if refs.len() > MAX_IMPORT_REFS {
        return Err(ImportError::Git(format!(
            "The Git source has more than {MAX_IMPORT_REFS} branches and tags"
        )));
    }
    crab_git::refname::validate_ref_namespace(refs.keys().map(String::as_str)).map_err(|_| {
        ImportError::Git("The Git source has a ref nested beneath another ref".into())
    })?;
    let head = head_target
        .as_deref()
        .and_then(|target| target.strip_prefix("refs/heads/"))
        .filter(|branch| refs.contains_key(&format!("refs/heads/{branch}")))
        .map(str::to_owned)
        .or_else(|| {
            ["main", "master"]
                .into_iter()
                .find(|branch| refs.contains_key(&format!("refs/heads/{branch}")))
                .map(str::to_owned)
        })
        .or_else(|| {
            refs.keys()
                .find_map(|name| name.strip_prefix("refs/heads/"))
                .map(str::to_owned)
        })
        .ok_or_else(|| ImportError::Git("The Git source has no branch".into()))?;
    Ok(Discovered { head, refs, peeled })
}

/// A complete fetched pack held in one staging object.
pub(super) struct Staged {
    pub(super) table: Arc<PackTable>,
    pub(super) blake3: String,
}

/// Fetch a pack for `wants` into `path`, verifying its frames as it streams.
pub(super) async fn stage(
    store: &Store,
    path: &Path,
    remote: &Remote,
    wants: &[ObjectId],
    haves: &[ObjectId],
    cancel: &CancellationToken,
) -> Result<Staged, ImportError> {
    let mut response = std::pin::pin!(remote.fetch(wants, haves).await?);
    let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(CHANNEL_CHUNKS);
    let scanner = tokio::task::spawn_blocking(move || scan(receiver));
    let mut upload = store.create_multipart_upload(path).await?;
    let result = async {
        let mut decoder = crab_git::fetch_wire::FetchResponse::new();
        let mut part = Vec::with_capacity(PART_BYTES);
        let mut pending = FuturesUnordered::new();
        let mut hasher = blake3::Hasher::new();
        let mut size = 0u64;
        loop {
            let chunk = tokio::select! {
                () = cancel.cancelled() => return Err(ImportError::Cancelled),
                chunk = response.next() => chunk,
            };
            let Some(chunk) = chunk else { break };
            let mut pack = Vec::new();
            decoder
                .feed(&chunk?, &mut |bytes: &[u8]| {
                    pack.extend_from_slice(bytes);
                    Ok::<_, std::convert::Infallible>(())
                })
                .map_err(|error| ImportError::Git(error.to_string()))?;
            if pack.is_empty() {
                continue;
            }
            hasher.update(&pack);
            size += pack.len() as u64;
            part.extend_from_slice(&pack);
            if sender.send(Bytes::from(pack)).await.is_err() {
                // The scanner stopped early; its error is reported below.
                break;
            }
            while part.len() >= PART_BYTES {
                let rest = part.split_off(PART_BYTES);
                pending
                    .push(upload.put_part(Bytes::from(std::mem::replace(&mut part, rest)).into()));
                while pending.len() >= PARTS_IN_FLIGHT {
                    if let Some(result) = pending.next().await {
                        result.map_err(storage)?;
                    }
                }
            }
        }
        drop(sender);
        let table = scanner
            .await
            .map_err(|error| ImportError::Io(std::io::Error::other(error)))??;
        crab_git::fetch_wire::FetchResponse::finish(decoder)
            .map_err(|error| ImportError::Git(error.to_string()))?;
        if table.size() != size {
            return Err(ImportError::Git("The Git source pack was truncated".into()));
        }
        if !part.is_empty() {
            pending.push(upload.put_part(Bytes::from(part).into()));
        }
        while let Some(result) = pending.next().await {
            result.map_err(storage)?;
        }
        Ok((table, size, *hasher.finalize().as_bytes()))
    }
    .await;
    match result {
        Ok((table, size, hash)) => {
            store
                .complete_multipart_upload(&mut *upload, path, size, &hash)
                .await?;
            Ok(Staged {
                table: Arc::new(table),
                blake3: blake3::Hash::from(hash).to_hex().to_string(),
            })
        }
        Err(error) => {
            if let Err(abort) = upload.abort().await {
                tracing::warn!(error = %abort, "staged Git pack upload abort failed");
            }
            Err(error)
        }
    }
}

/// Re-scan a staged pack left by an interrupted attempt.
pub(super) async fn rescan(store: &Store, path: &Path) -> Result<Option<Staged>, ImportError> {
    let (_, _, mut stream) = match store.get_stream(path, None).await {
        Ok(found) => found,
        Err(crab_storage::StorageError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let (sender, receiver) = tokio::sync::mpsc::channel::<Bytes>(CHANNEL_CHUNKS);
    let scanner = tokio::task::spawn_blocking(move || scan(receiver));
    let mut hasher = blake3::Hasher::new();
    while let Some(chunk) = stream.try_next().await? {
        hasher.update(&chunk);
        if sender.send(chunk).await.is_err() {
            break;
        }
    }
    drop(sender);
    match scanner
        .await
        .map_err(|error| ImportError::Io(std::io::Error::other(error)))?
    {
        Ok(table) => Ok(Some(Staged {
            table: Arc::new(table),
            blake3: hasher.finalize().to_hex().to_string(),
        })),
        // An incomplete or damaged leftover is refetched rather than trusted.
        Err(_) => Ok(None),
    }
}

fn scan(mut receiver: tokio::sync::mpsc::Receiver<Bytes>) -> Result<PackTable, ImportError> {
    let mut scanner = PackScanner::new(LIMITS);
    while let Some(chunk) = receiver.blocking_recv() {
        scanner.feed(&chunk)?;
    }
    Ok(scanner.finish()?)
}

/// Objects, sidecars and object structure of a staged pack, with the run hash prefix absorbed.
pub(super) struct Indexed<G> {
    pub(super) pack: IndexedPack,
    pub(super) graph: G,
    pub(super) hasher: blake3::Hasher,
}

type SinkError = Box<dyn std::error::Error + Send + Sync>;

/// Structure built from every object a staged pack reconstructs.
pub(super) trait Admit: Send + 'static {
    fn admit(&mut self, oid: ObjectId, kind: Kind, data: &[u8]) -> Result<(), SinkError>;
}

impl Admit for ImportGraph {
    fn admit(&mut self, oid: ObjectId, kind: Kind, data: &[u8]) -> Result<(), SinkError> {
        Ok(self.add(oid, kind, data)?)
    }
}

impl Admit for VerifiedObjects {
    fn admit(&mut self, oid: ObjectId, kind: Kind, data: &[u8]) -> Result<(), SinkError> {
        Ok(self.add(oid, kind, data)?)
    }
}

/// Reconstruct every staged object once, building sidecars and `graph`.
///
/// `prefix` is hashed before the pack bytes so the streamed run identity needs
/// no further read of the staged object.
pub(super) async fn index<G: Admit>(
    store: &Store,
    path: &Path,
    table: Arc<PackTable>,
    prefix: Bytes,
    mut graph: G,
    cancel: &CancellationToken,
) -> Result<Indexed<G>, ImportError> {
    let (_, _, mut stream) = store.get_stream(path, None).await?;
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<Bytes>(CHANNEL_CHUNKS);
    let handle = tokio::runtime::Handle::current();
    let (reader, reader_path) = (store.clone(), path.clone());
    let watched = cancel.clone();
    let worker = tokio::task::spawn_blocking(move || {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&prefix);
        let mut source = WindowedSource::new(
            |start, end| {
                handle
                    .block_on(read_range(&reader, &reader_path, start, end))
                    .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
            },
            table.size(),
            WINDOW_BYTES,
            WINDOW_SLOTS,
        );
        let mut sink = |object: crab_git::pack_stream::ResolvedObject<'_>| {
            graph.admit(object.oid, object.kind, object.data)
        };
        let mut resolver = PackResolver::new(&table, LIMITS, move || watched.is_cancelled())?;
        while let Some(chunk) = receiver.blocking_recv() {
            hasher.update(&chunk);
            resolver.feed(&chunk, &mut source, &mut sink)?;
        }
        let pack = resolver.finish(&mut source, &mut sink)?;
        drop(sink);
        Ok::<_, ImportError>(Indexed {
            pack,
            graph,
            hasher,
        })
    });
    while let Some(chunk) = tokio::select! {
        () = cancel.cancelled() => return Err(ImportError::Cancelled),
        chunk = stream.try_next() => chunk?,
    } {
        if sender.send(chunk).await.is_err() {
            break;
        }
    }
    drop(sender);
    worker
        .await
        .map_err(|error| ImportError::Io(std::io::Error::other(error)))?
}

async fn read_range(
    store: &Store,
    path: &Path,
    start: u64,
    end: u64,
) -> Result<Vec<u8>, ImportError> {
    let (_, _, stream) = store.get_stream(path, Some(start..end)).await?;
    let chunks = stream.try_collect::<Vec<_>>().await?;
    Ok(chunks.concat())
}

/// Ref edits and the verified peeled target of every annotated tag.
pub(super) fn edits(
    graph: &CompleteGraph,
    refs: &BTreeMap<String, String>,
) -> Result<Vec<CapsuleRefEdit>, ImportError> {
    refs.iter()
        .map(|(name, oid)| {
            let target = object_id(oid)?;
            let kind = graph.kind(&target).ok_or_else(|| {
                ImportError::Git(format!("The Git source pack omits the tip of {name}"))
            })?;
            if name.starts_with("refs/heads/") && kind != Kind::Commit {
                return Err(ImportError::Git(format!(
                    "The Git source branch {name} does not point to a commit"
                )));
            }
            let peeled = (kind == Kind::Tag)
                .then(|| graph.peel(target))
                .transpose()?
                .map(|peeled| peeled.to_string());
            Ok(CapsuleRefEdit::new(
                name.clone(),
                None,
                Some(oid.clone()),
                peeled,
            ))
        })
        .collect()
}

/// Shared exact closures for every new ref, as one visibility-delta section.
pub(super) fn visibility(
    graph: CompleteGraph,
    edits: &[CapsuleRefEdit],
    cancel: &CancellationToken,
) -> Result<CapsuleSection, ImportError> {
    let roots = edits
        .iter()
        .map(|edit| object_id(edit.new_oid().unwrap_or_default()))
        .collect::<Result<Vec<_>, _>>()?;
    let shared = graph.visibility(
        &roots,
        crab_git::shared_visibility::Limits {
            max_objects: crab_metadata::git_visibility::MAX_GIT_VISIBILITY_OBJECTS as usize,
            ..Default::default()
        },
        || cancel.is_cancelled(),
    )?;
    let dictionary =
        GitVisibilityDictionary::new(shared.objects().iter().map(ToString::to_string).collect())?;
    let mut evidence = BTreeMap::new();
    for (edit, closure) in edits.iter().zip(shared.closures()) {
        let added = GitVisibilityObjects::from_shared_chunks(
            dictionary.clone(),
            closure.chunks().to_vec(),
        )?;
        let edit_evidence = GitVisibilityEdit {
            version: GIT_VISIBILITY_EDIT_VERSION,
            old_oid: None,
            new_oid: edit.new_oid().unwrap_or_default().to_owned(),
            replaces: true,
            added,
            removed: GitVisibilityObjects::default(),
        };
        edit_evidence.validate()?;
        evidence.insert(edit.ref_name().to_owned(), edit_evidence);
    }
    Ok(CapsuleSection::new(
        CapsuleSectionKind::VisibilityDelta,
        CapsuleVisibilityDelta::new(evidence)?.encode()?,
    ))
}

/// Encode the streamed run for a verified staged pack.
fn run(
    transaction: &CapsuleTransaction,
    staged: &Staged,
    pack: IndexedPack,
    hasher: blake3::Hasher,
    sections: Vec<CapsuleSection>,
) -> Result<StreamedLeafRun, ImportError> {
    let (members, _) = crab_git::pack_locator::sorted_object_ids_from_index_bytes(&pack.index)
        .map_err(|error| ImportError::Git(format!("Staged pack index is invalid: {error}")))?;
    let members = members
        .into_iter()
        .map(|oid| {
            oid.as_bytes()
                .try_into()
                .map_err(|_| ImportError::Git("Staged pack is not SHA-1".into()))
        })
        .collect::<Result<Vec<[u8; 20]>, _>>()?;
    let capsule = DetachedPackCapsule::build(
        transaction,
        DetachedGitPack {
            length: pack.size,
            blake3: staged.blake3.clone(),
            index: Bytes::from(pack.index),
            reverse_index: Bytes::from(pack.reverse_index),
            locator: Bytes::from(pack.kinds),
            git_checksum: pack.checksum.to_string(),
            object_count: u64::from(pack.object_count),
        },
        sections,
        hasher,
    )?;
    Ok(StreamedLeafRun::build(capsule, vec![members])?)
}

/// Serves a streamed run's wire bytes from memory and the staged pack.
pub(super) struct RunBody {
    pub(super) store: Store,
    pub(super) path: Path,
    pub(super) prefix: Bytes,
    pub(super) pack_length: u64,
    pub(super) suffix: Bytes,
}

#[async_trait::async_trait]
impl crab_storage::MultipartUploadSource for RunBody {
    async fn byte_len(&self) -> crab_storage::Result<u64> {
        Ok(self.prefix.len() as u64 + self.pack_length + self.suffix.len() as u64)
    }

    async fn read_exact(&self, offset: u64, length: usize) -> crab_storage::Result<Bytes> {
        let end = offset + length as u64;
        let prefix = self.prefix.len() as u64;
        let pack_end = prefix + self.pack_length;
        let mut bytes = Vec::with_capacity(length);
        if offset < prefix {
            bytes.extend_from_slice(&self.prefix[offset as usize..end.min(prefix) as usize]);
        }
        let (start, stop) = (offset.max(prefix), end.min(pack_end));
        if start < stop {
            let (_, _, stream) = self
                .store
                .get_stream(&self.path, Some(start - prefix..stop - prefix))
                .await?;
            for chunk in stream.try_collect::<Vec<_>>().await? {
                bytes.extend_from_slice(&chunk);
            }
        }
        if end > pack_end {
            let from = offset.max(pack_end) - pack_end;
            bytes.extend_from_slice(&self.suffix[from as usize..(end - pack_end) as usize]);
        }
        Ok(Bytes::from(bytes))
    }
}

/// Where imported LFS payloads come from.
pub(super) struct LfsSource<'a> {
    pub(super) url: &'a str,
    pub(super) token: Option<&'a str>,
}

/// Publish a staged pack's refs into an empty destination under ref leases and GC fences.
///
/// Ruleset checks and LFS payload copies run inside the same leases, after the
/// staged history is verified and before any ref becomes visible.
pub(super) async fn publish(
    repository: &crate::repository::Repository,
    staged: &Staged,
    path: &Path,
    discovered: &Discovered,
    rules: &super::workflow::ImportRules,
    lfs: LfsSource<'_>,
    cancel: &CancellationToken,
) -> Result<(), ImportError> {
    let names = discovered.refs.keys().cloned().collect::<Vec<_>>();
    let store = repository.store.clone();
    let head = format!("refs/heads/{}", discovered.head);
    crab_remote::publication::with_leases(
        &repository.store,
        &repository.layout,
        names,
        LEASE_TTL,
        cancel,
        move |_holders, cancel| async move {
            let base = crab_write::capsule_protocol::open_root(&repository.layout).await?;
            // Peeled tag targets come from the advertisement and are verified
            // against the reconstructed graph before the transaction is used.
            let advertised = discovered
                .refs
                .iter()
                .map(|(name, oid)| {
                    CapsuleRefEdit::new(
                        name.clone(),
                        None,
                        Some(oid.clone()),
                        discovered.peeled.get(name).map(ToString::to_string),
                    )
                })
                .collect::<Vec<_>>();
            let transaction = CapsuleTransaction::new(base.record().digest(), advertised)?;
            let prefix = DetachedPackCapsule::prefix(&transaction)?;
            let Indexed {
                pack,
                graph,
                hasher,
            } = index(
                &store,
                path,
                Arc::clone(&staged.table),
                prefix.clone(),
                ImportGraph::new(GraphLimits {
                    max_objects: LIMITS.max_objects as usize,
                    max_links: crab_git::shared_visibility::Limits::default().max_links,
                }),
                &cancel,
            )
            .await?;
            let graph = graph.finish()?;
            let verified = edits(&graph, &discovered.refs)?;
            if verified != transaction.edits() {
                return Err(ImportError::Git(
                    "The Git source advertised tag targets that do not match its pack".into(),
                ));
            }
            let creations = verified
                .iter()
                .map(|edit| creation(edit.ref_name(), edit.new_oid()))
                .collect::<Result<Vec<_>, _>>()?;
            rules.check(&creations, &head, &BTreeSet::new(), |reference| {
                discovered
                    .refs
                    .get(reference)
                    .and_then(|oid| object_id(oid).ok())
                    .is_some_and(|tip| !graph.is_linear(tip).unwrap_or(false))
            })?;
            let pointers = graph
                .pointers()
                .iter()
                .map(
                    |(blob, pointer)| crab_git::receive_plan::PointerDependency {
                        blob: *blob,
                        pointer: pointer.clone(),
                    },
                )
                .collect::<Vec<_>>();
            super::lfs::copy(lfs.url, lfs.token, repository, &pointers, &cancel).await?;
            let visibility = visibility(graph, &verified, &cancel)?;
            let run = run(&transaction, staged, pack, hasher, vec![visibility])?;
            let body = Arc::new(RunBody {
                store: store.clone(),
                path: path.clone(),
                prefix,
                pack_length: run.pack_length(),
                suffix: run.suffix().clone(),
            });
            if cancel.is_cancelled() {
                return Err(ImportError::Cancelled);
            }
            crab_write::capsule_protocol::publish_streamed(
                &repository.layout,
                base,
                &transaction,
                &run,
                body,
                &cancel,
            )
            .await?;
            Ok(())
        },
    )
    .await?;
    repository.invalidate().await;
    Ok(())
}

/// A ref creation as the receive model spells it.
fn creation(name: &str, oid: Option<&str>) -> Result<RefUpdate, ImportError> {
    Ok(RefUpdate {
        name: name.to_owned(),
        old: None,
        new: oid.map(object_id).transpose()?,
    })
}

/// Publish one mirror sync's ref updates under ref leases and GC fences.
///
/// The staged pack is parsed once while it is reread; native receive rules
/// then validate those objects against the mirror's visible history and plan
/// exact visibility. Rulesets and LFS payloads are checked before any ref
/// moves, and the run streams from the staged object, so no local file is
/// written. `updates` must be computed from refs the leases still protect.
pub(super) async fn publish_updates(
    server: &crate::server::Server,
    repository: &crate::repository::Repository,
    staged: Option<(&Staged, &Path)>,
    discovered: &Discovered,
    updates: Vec<RefUpdate>,
    rules: &super::workflow::ImportRules,
    lfs: LfsSource<'_>,
    cancel: &CancellationToken,
) -> Result<(), ImportError> {
    let names = updates
        .iter()
        .map(|update| update.name.clone())
        .collect::<Vec<_>>();
    let store = repository.store.clone();
    crab_remote::publication::with_leases(
        &repository.store,
        &repository.layout,
        names,
        LEASE_TTL,
        cancel,
        move |_holders, cancel| async move {
            let options =
                crate::git::receive::repository_options(server).map_err(ImportError::Setup)?;
            let (mut view, mut reader) = repository
                .open_capsule_repository(server, options, &cancel)
                .await
                .map_err(ImportError::Setup)?;
            // Bound each ref's capsule frontier before adding another capsule.
            if updates.iter().any(|update| {
                view.ref_capsule_count(&update.name)
                    >= crate::repository::maintenance::FOREGROUND_CAPSULE_THRESHOLD
            }) {
                repository
                    .checkpoint_now(server, &cancel)
                    .await
                    .map_err(ImportError::Setup)?;
                (view, reader) = repository
                    .open_capsule_repository(server, options, &cancel)
                    .await
                    .map_err(ImportError::Setup)?;
            }
            if updates.iter().any(|update| {
                view.refs().get(&update.name).map(String::as_str)
                    != update.old.map(|oid| oid.to_string()).as_deref()
            }) {
                return Err(ImportError::DestinationChanged);
            }
            let base = view.root_snapshot().clone();
            // Peeled tag targets come from the advertisement; validation proves
            // them against the received graph before publication.
            let transaction = CapsuleTransaction::new(
                base.record().digest(),
                updates
                    .iter()
                    .map(|update| {
                        CapsuleRefEdit::new(
                            update.name.clone(),
                            update.old.map(|oid| oid.to_string()),
                            update.new.map(|oid| oid.to_string()),
                            update
                                .new
                                .and_then(|_| discovered.peeled.get(&update.name))
                                .map(ToString::to_string),
                        )
                    })
                    .collect(),
            )?;
            let (received, indexed) = match staged {
                Some((staged, path)) => {
                    let prefix = DetachedPackCapsule::prefix(&transaction)?;
                    let Indexed {
                        pack,
                        graph,
                        hasher,
                    } = index(
                        &store,
                        path,
                        Arc::clone(&staged.table),
                        prefix.clone(),
                        VerifiedObjects::new(SYNC_GRAPH.max_graph_steps),
                        &cancel,
                    )
                    .await?;
                    (graph, Some((pack, hasher, prefix, staged, path)))
                }
                None => (VerifiedObjects::new(0), None),
            };
            let linear = updates
                .iter()
                .filter(|update| {
                    update.new.is_some() && rules.requires_linear(&update.name, view.head())
                })
                .map(|update| update.name.clone())
                .collect::<BTreeSet<_>>();
            let visibility_index = view
                .git_visibility_index()
                .await
                .map_err(|error| ImportError::Publication(Box::new(error)))?;
            let (plan, evidence) = crab_remote::prepare::plan_verified(
                reader,
                visibility_index,
                received,
                updates.clone(),
                &cancel,
                crab_remote::prepare::Options {
                    layout: repository.layout.clone(),
                    graph: SYNC_GRAPH,
                    pack: crab_git::incoming_pack::ReceiveLimits {
                        max_pack_bytes: LIMITS.max_pack_bytes,
                        max_objects: LIMITS.max_objects,
                        max_object_bytes: LIMITS.max_object_bytes,
                        max_inflated_bytes: LIMITS.max_inflated_bytes,
                        max_delta_depth: LIMITS.max_delta_depth,
                    },
                    // A mirror follows its upstream through rewinds and deletions.
                    policy: move |name: &str| RefPolicy {
                        allow_delete: true,
                        allow_non_fast_forward: true,
                        inspect_linear_history: linear.contains(name),
                    },
                    inspect_visible_tip: None,
                },
            )
            .await
            .map_err(|error| ImportError::Validation(Box::new(error)))?;
            if updates
                .iter()
                .filter(|update| update.new.is_some())
                .any(|update| {
                    plan.peeled().get(&update.name).map(ToString::to_string)
                        != discovered.peeled.get(&update.name).map(ToString::to_string)
                })
            {
                return Err(ImportError::Git(
                    "The Git source advertised tag targets that do not match its pack".into(),
                ));
            }
            rules.check(&updates, view.head(), plan.rewritten_refs(), |reference| {
                plan.linear_history().get(reference) == Some(&false)
            })?;
            super::lfs::copy(lfs.url, lfs.token, repository, plan.pointers(), &cancel).await?;
            let sections = if evidence.is_empty() {
                Vec::new()
            } else {
                vec![CapsuleSection::new(
                    CapsuleSectionKind::VisibilityDelta,
                    CapsuleVisibilityDelta::new(evidence)?.encode()?,
                )]
            };
            if cancel.is_cancelled() {
                return Err(ImportError::Cancelled);
            }
            match indexed {
                Some((pack, hasher, prefix, staged, path)) => {
                    let run = run(&transaction, staged, pack, hasher, sections)?;
                    let body = Arc::new(RunBody {
                        store: store.clone(),
                        path: path.clone(),
                        prefix,
                        pack_length: run.pack_length(),
                        suffix: run.suffix().clone(),
                    });
                    crab_write::capsule_protocol::publish_streamed(
                        &repository.layout,
                        base,
                        &transaction,
                        &run,
                        body,
                        &cancel,
                    )
                    .await?;
                }
                None => {
                    let capsule = Capsule::build(&transaction, Vec::new(), sections)?;
                    crab_write::capsule_protocol::publish(
                        &repository.layout,
                        base,
                        &transaction,
                        capsule,
                    )
                    .await?;
                }
            }
            Ok(())
        },
    )
    .await?;
    repository.invalidate().await;
    Ok(())
}

/// Point the mirror's HEAD at `head` once that branch exists.
pub(super) async fn follow_head(
    server: &crate::server::Server,
    repository: &crate::repository::Repository,
    head: &str,
    cancel: &CancellationToken,
) -> Result<(), ImportError> {
    let view = repository
        .open_ref_view()
        .await
        .map_err(ImportError::Setup)?;
    if view.head() == head || !view.refs().contains_key(head) {
        return Ok(());
    }
    crab_remote::publication::with_leases(
        &repository.store,
        &repository.layout,
        [head.to_owned()],
        LEASE_TTL,
        cancel,
        |_holders, cancel| async move {
            // Default-branch rulesets match HEAD; serialize with their edits.
            crate::repository::settings::with_ruleset_lease(
                server,
                crate::repository::settings::RulesetScope::Repository(repository),
                &cancel,
                |_| async move {
                    let view = repository
                        .open_ref_view()
                        .await
                        .map_err(ImportError::Setup)?;
                    if view.head() == head || !view.refs().contains_key(head) {
                        return Ok::<_, ImportError>(());
                    }
                    crab_write::capsule_protocol::retarget_head(
                        &repository.layout,
                        view.root_snapshot().clone(),
                        view.head(),
                        head,
                    )
                    .await?;
                    Ok(())
                },
            )
            .await
        },
    )
    .await?;
    repository.invalidate().await;
    Ok(())
}

fn object_id(hex: &str) -> Result<ObjectId, ImportError> {
    ObjectId::from_hex(hex.as_bytes())
        .map_err(|_| ImportError::Git("The Git source advertised an invalid object ID".into()))
}

fn storage(error: object_store::Error) -> ImportError {
    ImportError::Stage(crab_storage::StorageError::Internal(format!(
        "staged Git pack upload failed: {error}"
    )))
}
