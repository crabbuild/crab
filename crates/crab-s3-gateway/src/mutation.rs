use std::{
    collections::BTreeMap,
    io::Write as _,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use crab_coordination::{GcFenceHeartbeat, GcFenceLease, PushLock};
use crab_metadata::{git_visibility, manifests::PackManifestEntry, ref_journal::RefJournalEdit};
use crab_remote_git::{EntryMode, OperationKind, RemoteGitRepository, Revision};
use gix_hash::ObjectId;
use gix_object::{Kind, WriteTo as _, bstr::BString, tree};
use md5::Digest as _;
use tokio_util::sync::CancellationToken;

use crate::{attributes, gateway::Repository};

const LOCK_TTL: Duration = Duration::from_secs(300);
const MAX_GENERATED_PACK_BYTES: u64 = 512 * 1024 * 1024;

pub(crate) type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("object path resolves through a non-directory")]
    NotDirectory,
    #[error("object path names a directory")]
    IsDirectory,
    #[error("object write precondition failed")]
    PreconditionFailed,
    #[error("repository mutation was cancelled")]
    Cancelled,
    #[error("system clock is before the Unix epoch")]
    Clock(#[from] std::time::SystemTimeError),
    #[error("repository read failed")]
    Remote(#[from] crab_remote_git::Error),
    #[error("Git object encoding failed")]
    Object(#[from] gix_object::encode::Error),
    #[error("Git object hashing failed")]
    Hash(#[from] gix_hash::hasher::Error),
    #[error("temporary pack I/O failed")]
    Io(#[from] std::io::Error),
    #[error("generated pack validation failed")]
    Pack(#[from] crab_git::incoming_pack::IncomingPackError),
    #[error("generated pack preparation failed")]
    Prepare(#[from] crab_git::incoming_pack::PreparePackError),
    #[error("repository storage failed")]
    Storage(#[from] crab_storage::StorageError),
    #[error("repository metadata failed")]
    Metadata(#[from] crab_metadata::error::MetadataError),
    #[error("repository coordination failed")]
    Coordination(#[from] crab_coordination::CoordinationError),
    #[error("repository publication failed")]
    Write(#[from] crab_write::WriteError),
    #[error("mutation worker failed")]
    Worker(#[from] tokio::task::JoinError),
    #[error("S3 attribute persistence failed")]
    Attributes(#[source] Box<crate::Error>),
}

impl From<crate::Error> for Error {
    fn from(error: crate::Error) -> Self {
        Self::Attributes(Box::new(error))
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Change {
    Put {
        bytes: Bytes,
        attributes: Box<attributes::PutAttributes>,
        condition: PutCondition,
    },
    Delete,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum PutCondition {
    #[default]
    None,
    IfNoneMatchAny,
}

#[derive(Clone, Debug)]
pub(crate) struct Outcome {
    pub(crate) etag: Option<String>,
}

struct RefLease {
    holder: String,
    stop: CancellationToken,
    worker: tokio::task::JoinHandle<std::result::Result<(), crab_coordination::CoordinationError>>,
}

impl RefLease {
    async fn acquire(
        repository: &Repository,
        branch: &str,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        check_cancelled(cancel)?;
        let mut lock = PushLock::acquire_ref(
            repository.store.inner(),
            repository.layout.repo_prefix(),
            branch,
            LOCK_TTL,
        )
        .await?;
        let holder = lock.holder().to_owned();
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let cancel = cancel.clone();
        let worker = tokio::spawn(async move {
            let result = crab_coordination::while_renewing(&mut lock, Some(&cancel), async {
                stopped.cancelled().await;
                Ok::<_, crab_coordination::CoordinationError>(())
            })
            .await;
            result.and(lock.release().await)
        });
        Ok(Self {
            holder,
            stop,
            worker,
        })
    }

    async fn release(self) {
        self.stop.cancel();
        if let result @ (Err(_) | Ok(Err(_))) = self.worker.await {
            tracing::warn!(?result, "S3 ref lease cleanup failed");
        }
    }
}

pub(crate) async fn apply(
    repository: &Repository,
    runtime: Arc<crab_remote_git::RemoteGitRuntime>,
    options: crab_remote_git::RepositoryOptions,
    branch: &str,
    path: &crab_remote_git::GitPath,
    change: Change,
    principal: &str,
    cancel: &CancellationToken,
) -> Result<Outcome> {
    let lease = RefLease::acquire(repository, branch, cancel).await?;
    let maintenance_runtime = Arc::clone(&runtime);
    let mut fences = Vec::new();
    let result = async {
        for domain in [
            repository.layout.global_prefix(),
            repository.layout.repo_prefix(),
        ] {
            check_cancelled(cancel)?;
            let fence =
                GcFenceLease::acquire_writer(repository.store.inner(), domain, LOCK_TTL).await?;
            let heartbeat = GcFenceHeartbeat::spawn(&fence, cancel.clone(), LOCK_TTL / 3);
            fences.push((fence, heartbeat));
        }
        Box::pin(apply_locked(
            repository,
            runtime,
            options,
            branch,
            path,
            change,
            principal,
            &lease.holder,
            cancel,
        ))
        .await
    }
    .await;
    for (fence, heartbeat) in fences.into_iter().rev() {
        heartbeat.stop().await;
        if let Err(error) = fence.release().await {
            tracing::warn!(%error, "S3 GC fence cleanup failed");
        }
    }
    lease.release().await;
    if result.is_ok() {
        crate::repository::ensure_readable(repository, maintenance_runtime, options, cancel)
            .await?;
    }
    result
}

#[expect(clippy::too_many_arguments)]
async fn apply_locked(
    repository: &Repository,
    runtime: Arc<crab_remote_git::RemoteGitRuntime>,
    options: crab_remote_git::RepositoryOptions,
    branch: &str,
    path: &crab_remote_git::GitPath,
    change: Change,
    principal: &str,
    holder: &str,
    cancel: &CancellationToken,
) -> Result<Outcome> {
    check_cancelled(cancel)?;
    let remote = crate::repository::open_current(repository, runtime, options, cancel).await?;
    let old = remote
        .refs()
        .entries
        .iter()
        .find(|reference| reference.name == branch)
        .map(|reference| reference.target);
    let snapshot = crab_metadata::manifest_store::read_repository_snapshot(
        &repository.store,
        &repository.layout,
    )
    .await?;
    if snapshot.manifest.generation != remote.generation()
        || snapshot.journal.refs.get(branch).map(String::as_str)
            != old.as_ref().map(ToString::to_string).as_deref()
    {
        return Err(crab_write::WriteError::RefChanged {
            ref_name: branch.to_owned(),
            path: repository.layout.repo_prefix().to_owned(),
        }
        .into());
    }
    let mut attribute_manifest = match old {
        Some(old) => attributes::load(repository, old)
            .await
            .map_err(|error| Error::Attributes(Box::new(error)))?,
        None => attributes::Manifest::default(),
    };
    let operation = remote.operation(OperationKind::Repository, cancel).await?;
    let built = build_commit(
        &remote,
        &operation,
        old,
        path,
        change,
        principal,
        &attribute_manifest,
    )
    .await;
    let built = match operation.finish(Ok(())).await {
        Ok(()) => built?,
        Err(error) => return Err(error.into()),
    };
    let built = match built {
        Build::Noop(outcome) => return Ok(outcome),
        Build::Commit(built) => *built,
    };
    let (pack_owner, pack) = prepare_pack(built.objects.clone(), cancel).await?;
    check_cancelled(cancel)?;
    let pack_id = pack.content_hash().to_hex().to_string();
    repository
        .store
        .put_multipart_file_retry(
            &repository.layout.pack_path(&pack_id),
            pack.pack_path(),
            pack.size(),
            *pack.content_hash().as_bytes(),
            8 * 1024 * 1024,
            cancel,
            None,
        )
        .await?;
    for (source, target) in [
        (
            pack.index_path(),
            repository.layout.pack_index_path(&pack_id),
        ),
        (
            pack.reverse_path(),
            repository.layout.pack_reverse_index_path(&pack_id),
        ),
        (
            pack.kinds_path(),
            repository.layout.pack_kind_metadata_path(&pack_id),
        ),
    ] {
        check_cancelled(cancel)?;
        repository
            .store
            .put_exact(&target, tokio::fs::read(source).await?.into())
            .await?;
    }
    let visible_objects = built
        .objects
        .iter()
        .map(|(kind, bytes)| object_id(*kind, bytes).map(|oid| oid.to_string()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let evidence = match old {
        Some(old) => git_visibility::GitVisibilityEdit::from_delta_objects(
            Some(old.to_string()),
            built.commit.to_string(),
            visible_objects,
            vec![],
        ),
        None => git_visibility::GitVisibilityEdit::from_replacement_objects(
            None,
            built.commit.to_string(),
            visible_objects,
        ),
    };
    let evidence_hash =
        git_visibility::upload_edit(&repository.store, &repository.layout, &evidence).await?;
    match built.attributes.clone() {
        Some(attributes) => attribute_manifest.put(built.path.clone(), attributes),
        None => attribute_manifest.remove(&built.path),
    }
    attributes::save(repository, built.commit, &attribute_manifest)
        .await
        .map_err(|error| Error::Attributes(Box::new(error)))?;
    check_cancelled(cancel)?;
    crab_write::journal::commit_edits(
        &repository.store,
        &repository.layout,
        &snapshot,
        vec![RefJournalEdit {
            ref_name: branch.to_owned(),
            old_oid: old.map(|oid| oid.to_string()),
            new_oid: Some(built.commit.to_string()),
            peeled_oid: None,
            lock_holder: Some(holder.to_owned()),
            visibility_evidence_hash: Some(evidence_hash),
        }],
        old.is_none().then(|| branch.to_owned()),
        vec![PackManifestEntry {
            pack_id: pack_id.clone(),
            content_hash: pack_id,
            size: pack.size(),
            object_count: pack.object_count().into(),
            ref_tips: vec![built.commit.to_string()],
        }],
        vec![],
        crab_write::journal::CommitOptions::new(LOCK_TTL, cancel),
    )
    .await?;
    drop(pack);
    drop(pack_owner);
    Ok(Outcome { etag: built.etag })
}

struct BuiltCommit {
    commit: ObjectId,
    etag: Option<String>,
    objects: Vec<(Kind, Vec<u8>)>,
    path: String,
    attributes: Option<attributes::ObjectAttributes>,
}

enum Build {
    Noop(Outcome),
    Commit(Box<BuiltCommit>),
}

#[derive(Default)]
struct TreeNode {
    old_oid: Option<ObjectId>,
    files: BTreeMap<Vec<u8>, TreeLeaf>,
    directories: BTreeMap<Vec<u8>, TreeNode>,
}

struct TreeLeaf {
    oid: ObjectId,
    mode: EntryMode,
}

async fn build_commit(
    remote: &RemoteGitRepository,
    operation: &crab_remote_git::OperationContext,
    parent: Option<ObjectId>,
    path: &crab_remote_git::GitPath,
    change: Change,
    principal: &str,
    attribute_manifest: &attributes::Manifest,
) -> Result<Build> {
    let mut root = TreeNode::default();
    if let Some(parent) = parent {
        let snapshot = remote
            .snapshot(&Revision::Commit(parent), operation)
            .await?;
        root.old_oid = Some(snapshot.root_tree_oid());
        for entry in snapshot.list_tree_recursive(operation).await? {
            insert_entry(&mut root, &entry)?;
        }
    }
    let old = root.file(path)?.map(|leaf| (leaf.oid, leaf.mode));
    let path_string = std::str::from_utf8(path.as_bytes())
        .map_err(|_| std::io::Error::other("S3 object path is not UTF-8"))?;
    let (etag, changed, pending_attributes) = match change {
        Change::Put {
            bytes,
            attributes,
            condition,
        } => {
            if matches!(condition, PutCondition::IfNoneMatchAny) && old.is_some() {
                return Err(Error::PreconditionFailed);
            }
            let oid = object_id(Kind::Blob, &bytes)?;
            let digest = md5::Md5::digest(&bytes);
            let logical_size = attributes.logical_size.unwrap_or(bytes.len() as u64);
            let etag = attributes
                .etag_override
                .clone()
                .unwrap_or_else(|| digest.iter().map(|byte| format!("{byte:02x}")).collect());
            if attributes.completion_upload_id.is_some()
                && old.is_some_and(|(old_oid, mode)| old_oid == oid && mode == EntryMode::Regular)
                && attribute_manifest
                    .object(path_string, oid)
                    .is_some_and(|stored| stored.matches_pending(&attributes, &etag, logical_size))
            {
                return Ok(Build::Noop(Outcome { etag: Some(etag) }));
            }
            root.put(path, oid)?;
            let changed = old
                .filter(|(old_oid, _)| *old_oid == oid)
                .map(|_| None)
                .unwrap_or_else(|| Some(bytes.to_vec()));
            (Some(etag), changed, Some((oid, attributes, logical_size)))
        }
        Change::Delete => {
            if old.is_none() {
                return Ok(Build::Noop(Outcome { etag: None }));
            }
            root.delete(path)?;
            (None, None, None)
        }
    };
    let mut objects = Vec::new();
    if let Some(bytes) = changed {
        objects.push((Kind::Blob, bytes));
    }
    let tree = encode_node(&root, &mut objects)?;
    let seconds = now_seconds()?;
    let object_attributes = pending_attributes.map(|(oid, pending, size)| {
        attributes::ObjectAttributes::new(
            oid,
            etag.clone().unwrap_or_default(),
            size,
            seconds,
            *pending,
        )
    });
    let commit_bytes = commit_bytes(tree, parent, principal, seconds);
    let commit = object_id(Kind::Commit, &commit_bytes)?;
    objects.push((Kind::Commit, commit_bytes));
    Ok(Build::Commit(Box::new(BuiltCommit {
        commit,
        etag,
        objects,
        path: path_string.to_owned(),
        attributes: object_attributes,
    })))
}

fn insert_entry(root: &mut TreeNode, entry: &crab_remote_git::TreeEntry) -> Result<()> {
    let components = entry.path.components().collect::<Vec<_>>();
    let (name, parents) = components.split_last().ok_or(Error::NotDirectory)?;
    let mut node = root;
    for component in parents {
        node = node.directories.entry((*component).to_vec()).or_default();
    }
    if entry.mode == EntryMode::Tree {
        node.directories
            .entry((*name).to_vec())
            .or_default()
            .old_oid = Some(entry.oid);
    } else {
        node.files.insert(
            (*name).to_vec(),
            TreeLeaf {
                oid: entry.oid,
                mode: entry.mode,
            },
        );
    }
    Ok(())
}

impl TreeNode {
    fn file(&self, path: &crab_remote_git::GitPath) -> Result<Option<&TreeLeaf>> {
        let components = path.components().collect::<Vec<_>>();
        let (name, parents) = components.split_last().ok_or(Error::IsDirectory)?;
        let mut node = self;
        for component in parents {
            if node.files.contains_key(*component) {
                return Err(Error::NotDirectory);
            }
            let Some(next) = node.directories.get(*component) else {
                return Ok(None);
            };
            node = next;
        }
        if node.directories.contains_key(*name) {
            return Err(Error::IsDirectory);
        }
        Ok(node.files.get(*name))
    }

    fn put(&mut self, path: &crab_remote_git::GitPath, oid: ObjectId) -> Result<()> {
        let components = path.components().collect::<Vec<_>>();
        let (name, parents) = components.split_last().ok_or(Error::IsDirectory)?;
        let mut node = self;
        for component in parents {
            if node.files.contains_key(*component) {
                return Err(Error::NotDirectory);
            }
            node = node.directories.entry((*component).to_vec()).or_default();
        }
        if node.directories.contains_key(*name) {
            return Err(Error::IsDirectory);
        }
        node.files.insert(
            (*name).to_vec(),
            TreeLeaf {
                oid,
                mode: EntryMode::Regular,
            },
        );
        Ok(())
    }

    fn delete(&mut self, path: &crab_remote_git::GitPath) -> Result<()> {
        let components = path.components().collect::<Vec<_>>();
        let (name, parents) = components.split_last().ok_or(Error::IsDirectory)?;
        delete_from(self, parents, name)
    }
}

fn delete_from(node: &mut TreeNode, parents: &[&[u8]], name: &[u8]) -> Result<()> {
    let Some((component, rest)) = parents.split_first() else {
        node.files.remove(name);
        return Ok(());
    };
    if node.files.contains_key(*component) {
        return Err(Error::NotDirectory);
    }
    let Some(child) = node.directories.get_mut(*component) else {
        return Ok(());
    };
    delete_from(child, rest, name)?;
    if child.files.is_empty() && child.directories.is_empty() {
        node.directories.remove(*component);
    }
    Ok(())
}

fn encode_node(node: &TreeNode, objects: &mut Vec<(Kind, Vec<u8>)>) -> Result<ObjectId> {
    let mut entries = Vec::with_capacity(node.files.len() + node.directories.len());
    for (name, child) in &node.directories {
        let oid = encode_node(child, objects)?;
        entries.push(tree::Entry {
            mode: tree::EntryKind::Tree.into(),
            filename: BString::from(name.clone()),
            oid,
        });
    }
    for (name, leaf) in &node.files {
        let mode = match leaf.mode {
            EntryMode::Regular => tree::EntryKind::Blob,
            EntryMode::Executable => tree::EntryKind::BlobExecutable,
            EntryMode::Symlink => tree::EntryKind::Link,
            EntryMode::Submodule => tree::EntryKind::Commit,
            EntryMode::Tree => return Err(Error::IsDirectory),
        };
        entries.push(tree::Entry {
            mode: mode.into(),
            filename: BString::from(name.clone()),
            oid: leaf.oid,
        });
    }
    entries.sort();
    let mut bytes = Vec::new();
    gix_object::Tree { entries }.write_to(&mut bytes)?;
    let oid = object_id(Kind::Tree, &bytes)?;
    if node.old_oid != Some(oid) {
        objects.push((Kind::Tree, bytes));
    }
    Ok(oid)
}

fn commit_bytes(
    tree: ObjectId,
    parent: Option<ObjectId>,
    principal: &str,
    seconds: u64,
) -> Vec<u8> {
    let name: String = principal
        .chars()
        .filter(|character| !matches!(character, '<' | '>' | '\n' | '\r' | '\0'))
        .take(160)
        .collect();
    let name = if name.trim().is_empty() {
        "Crab S3 user"
    } else {
        name.trim()
    };
    let email = blake3::hash(principal.as_bytes()).to_hex();
    let parent = parent
        .map(|oid| format!("parent {oid}\n"))
        .unwrap_or_default();
    format!(
        "tree {tree}\n{parent}author {name} <{email}@users.crab.invalid> {seconds} +0000\ncommitter {name} <{email}@users.crab.invalid> {seconds} +0000\n\nUpdate object through Crab S3 gateway\n"
    )
    .into_bytes()
}

fn object_id(kind: Kind, bytes: &[u8]) -> std::result::Result<ObjectId, gix_hash::hasher::Error> {
    gix_object::compute_hash(gix_hash::Kind::Sha1, kind, bytes)
}

async fn prepare_pack(
    objects: Vec<(Kind, Vec<u8>)>,
    cancel: &CancellationToken,
) -> Result<(tempfile::TempDir, crab_git::incoming_pack::PreparedPack)> {
    let cancelled = Arc::new(AtomicBool::new(cancel.is_cancelled()));
    let watched = cancel.clone();
    let flag = Arc::clone(&cancelled);
    let watcher = tokio::spawn(async move {
        watched.cancelled().await;
        flag.store(true, Ordering::Release);
    });
    let worker_flag = Arc::clone(&cancelled);
    let result = tokio::task::spawn_blocking(move || {
        let owner = tempfile::tempdir()?;
        let input = owner.path().join("generated.pack");
        write_pack(&input, &objects)?;
        let incoming = crab_git::incoming_pack::quarantine(
            std::io::BufReader::new(std::fs::File::open(&input)?),
            owner.path(),
            crab_git::incoming_pack::ReceiveLimits {
                max_pack_bytes: MAX_GENERATED_PACK_BYTES,
                max_objects: 1_000_000,
                max_object_bytes: 256 * 1024 * 1024,
                max_inflated_bytes: MAX_GENERATED_PACK_BYTES,
                max_delta_depth: 1,
            },
            || worker_flag.load(Ordering::Acquire),
            |_| Ok(None),
        )?;
        let prepared = incoming
            .prepare(owner.path(), MAX_GENERATED_PACK_BYTES, &worker_flag)?
            .ok_or_else(|| std::io::Error::other("generated pack contains no objects"))?;
        Ok::<_, Error>((owner, prepared))
    })
    .await?;
    watcher.abort();
    result
}

fn write_pack(path: &std::path::Path, objects: &[(Kind, Vec<u8>)]) -> Result<()> {
    let count = u32::try_from(objects.len())
        .map_err(|_| std::io::Error::other("too many generated Git objects"))?;
    let mut bytes = b"PACK\0\0\0\x02".to_vec();
    bytes.extend_from_slice(&count.to_be_bytes());
    for (kind, data) in objects {
        pack_header(*kind, data.len(), &mut bytes);
        let mut encoder =
            flate2::write::ZlibEncoder::new(&mut bytes, flate2::Compression::default());
        encoder.write_all(data)?;
        encoder.finish()?;
    }
    let mut hasher = gix_hash::hasher(gix_hash::Kind::Sha1);
    hasher.update(&bytes);
    bytes.extend_from_slice(hasher.try_finalize()?.as_bytes());
    std::fs::write(path, bytes)?;
    Ok(())
}

fn pack_header(kind: Kind, size: usize, output: &mut Vec<u8>) {
    let kind = match kind {
        Kind::Commit => 1,
        Kind::Tree => 2,
        Kind::Blob => 3,
        Kind::Tag => 4,
    };
    let mut remaining = size >> 4;
    let mut byte = (kind << 4) | (size as u8 & 0x0f);
    while remaining != 0 {
        output.push(byte | 0x80);
        byte = (remaining as u8) & 0x7f;
        remaining >>= 7;
    }
    output.push(byte);
}

fn now_seconds() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RepositoryAccess, RepositoryConfig};

    async fn fixture() -> (
        Repository,
        Arc<crab_remote_git::RemoteGitRuntime>,
        CancellationToken,
    ) {
        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = crab_storage::StoreLayout::new(store.clone(), "s3-test".to_owned());
        crab_write::initialize::initialize_repository(&store, &layout, "refs/heads/main")
            .await
            .unwrap();
        let repository = Repository::new(
            RepositoryConfig {
                name: "repo".to_owned(),
                provider: crab_storage::StorageProviderKind::Local,
                bucket: "memory".to_owned(),
                prefix: "s3-test".to_owned(),
                default_branch: "main".to_owned(),
                members: vec![crate::RepositoryMember {
                    principal: "user".to_owned(),
                    access: RepositoryAccess::Write,
                }],
                protected_branches: vec![],
            },
            store,
        )
        .unwrap();
        (
            repository,
            Arc::new(crab_remote_git::RemoteGitRuntime::default()),
            CancellationToken::new(),
        )
    }

    async fn read(
        repository: &Repository,
        runtime: Arc<crab_remote_git::RemoteGitRuntime>,
        cancel: &CancellationToken,
        path: &str,
    ) -> Bytes {
        let remote = RemoteGitRepository::open(
            repository.store.clone(),
            repository.layout.clone(),
            repository.identity.clone(),
            runtime,
            crab_remote_git::RepositoryOptions::default(),
            cancel,
        )
        .await
        .unwrap();
        let operation = remote
            .operation(OperationKind::Repository, cancel)
            .await
            .unwrap();
        let snapshot = remote
            .snapshot(&Revision::parse("refs/heads/main").unwrap(), &operation)
            .await
            .unwrap();
        let blob = snapshot
            .read_blob(
                &crab_remote_git::GitPath::new(path.as_bytes().to_vec()).unwrap(),
                &operation,
            )
            .await
            .unwrap();
        operation.finish(Ok(())).await.unwrap();
        blob.bytes
    }

    async fn tip(
        repository: &Repository,
        runtime: Arc<crab_remote_git::RemoteGitRuntime>,
        cancel: &CancellationToken,
    ) -> ObjectId {
        RemoteGitRepository::open(
            repository.store.clone(),
            repository.layout.clone(),
            repository.identity.clone(),
            runtime,
            crab_remote_git::RepositoryOptions::default(),
            cancel,
        )
        .await
        .unwrap()
        .refs()
        .entries
        .iter()
        .find(|reference| reference.name == "refs/heads/main")
        .unwrap()
        .target
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writes_initialize_branch_and_preserve_unrelated_files() {
        let (repository, runtime, cancel) = fixture().await;
        for (path, body) in [("a.txt", "alpha"), ("nested/b.txt", "bravo")] {
            apply(
                &repository,
                Arc::clone(&runtime),
                crab_remote_git::RepositoryOptions::default(),
                "refs/heads/main",
                &crab_remote_git::GitPath::new(path.as_bytes().to_vec()).unwrap(),
                Change::Put {
                    bytes: Bytes::copy_from_slice(body.as_bytes()),
                    attributes: Box::new(attributes::PutAttributes::default()),
                    condition: PutCondition::None,
                },
                "user",
                &cancel,
            )
            .await
            .unwrap();
        }
        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "a.txt").await,
            "alpha"
        );
        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "nested/b.txt").await,
            "bravo"
        );
        apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &crab_remote_git::GitPath::new(b"a.txt".to_vec()).unwrap(),
            Change::Delete,
            "user",
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "nested/b.txt").await,
            "bravo"
        );
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn multipart_completion_token_makes_publication_retry_idempotent() {
        let (repository, runtime, cancel) = fixture().await;
        let path = crab_remote_git::GitPath::new(b"object.bin".to_vec()).unwrap();
        let change = Change::Put {
            bytes: Bytes::from_static(b"multipart content"),
            attributes: Box::new(attributes::PutAttributes {
                etag_override: Some("multipart-etag-1".to_owned()),
                completion_upload_id: Some("upload-id".to_owned()),
                ..Default::default()
            }),
            condition: PutCondition::None,
        };
        apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &path,
            change.clone(),
            "user",
            &cancel,
        )
        .await
        .unwrap();
        let first = tip(&repository, Arc::clone(&runtime), &cancel).await;
        apply(
            &repository,
            Arc::clone(&runtime),
            crab_remote_git::RepositoryOptions::default(),
            "refs/heads/main",
            &path,
            change,
            "user",
            &cancel,
        )
        .await
        .unwrap();
        let second = tip(&repository, Arc::clone(&runtime), &cancel).await;
        assert_eq!(first, second);
        runtime.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn if_none_match_wildcard_preserves_an_existing_object() {
        let (repository, runtime, cancel) = fixture().await;
        let path = crab_remote_git::GitPath::new(b"manifest".to_vec()).unwrap();
        let put = |bytes, condition| {
            apply(
                &repository,
                Arc::clone(&runtime),
                crab_remote_git::RepositoryOptions::default(),
                "refs/heads/main",
                &path,
                Change::Put {
                    bytes,
                    attributes: Box::new(attributes::PutAttributes::default()),
                    condition,
                },
                "user",
                &cancel,
            )
        };
        put(Bytes::from_static(b"first"), PutCondition::IfNoneMatchAny)
            .await
            .unwrap();
        let result = put(
            Bytes::from_static(b"replacement"),
            PutCondition::IfNoneMatchAny,
        )
        .await;

        assert!(matches!(result, Err(Error::PreconditionFailed)));
        assert_eq!(
            read(&repository, Arc::clone(&runtime), &cancel, "manifest").await,
            "first"
        );
        runtime.shutdown().await;
    }
}
