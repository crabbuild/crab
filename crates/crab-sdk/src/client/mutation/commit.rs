use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufWriter, Cursor, Read},
    path::{Path, PathBuf},
};

use gix_object::Kind;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

use super::{
    Failure, PackInput, PreparedMutation, finish_preparation, prepare_batch,
    validate_ref_preparation,
};
use crate::mutation::FileChange;
use crate::{
    CommitOptions, EntryMode, Error, ErrorKind, FileEdit, ObjectId, OperationOptions, RefBatch,
    RefUpdate, Result,
};

struct SpooledEdit {
    path: crate::GitPath,
    mode: Option<EntryMode>,
    body: Option<SpoolBody>,
}

struct SpoolBody {
    path: PathBuf,
    descriptor: File,
    size: u64,
    git_oid: Option<gix_hash::ObjectId>,
    pointer: Option<Vec<u8>>,
}

enum ObjectBody {
    File(PathBuf),
    Bytes(Vec<u8>),
}

struct PackedObject {
    kind: Kind,
    size: u64,
    body: ObjectBody,
}

enum PackReader {
    File(File),
    Bytes(Cursor<Vec<u8>>),
}

impl Read for PackReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::File(file) => file.read(buffer),
            Self::Bytes(bytes) => std::io::Read::read(bytes, buffer),
        }
    }
}

impl super::RemoteRepository {
    /// Prepare one commit and its atomic branch update without a checkout or Git executable.
    ///
    /// Content readers must yield exactly their declared sizes. Git blobs are limited to
    /// 64 MiB; larger logical files use [`FileEdit::hydrated`]. Immutable content and the
    /// normalized recovery pack may be staged, but refs remain unchanged until execution.
    pub async fn prepare_commit(
        &self,
        commit: CommitOptions,
        edits: Vec<FileEdit>,
        scratch: PathBuf,
        options: OperationOptions,
    ) -> Result<PreparedMutation> {
        validate_ref_preparation(&self.client, &self.locator, &scratch)?;
        let limits = options.read_limits();
        commit.validate_size(limits.max_inflated_bytes)?;
        validate_edits(&edits, limits, commit.encoded_size()?)?;
        let client = self.client.clone();
        let locator = self.locator.clone();
        let preparation_root = scratch.clone();
        let caller = client.clone();
        caller
            .0
            .operations
            .run(options.clone(), move |cancel| async move {
                let workspace = tokio::task::spawn_blocking(move || tempfile::tempdir_in(scratch))
                    .await
                    .map_err(Failure::from)?
                    .map_err(Failure::from)?;
                let spooled =
                    spool_edits(edits, workspace.path(), options.read_limits(), &cancel).await?;
                let mut spooled = spooled;
                let native = prepare_hydrated(
                    &mut spooled,
                    workspace.path(),
                    options.read_limits().max_fetched_bytes,
                    &cancel,
                )
                .await?;
                let (commit_id, pack_path) = build_pack(
                    &client,
                    &locator,
                    &commit,
                    &spooled,
                    workspace.path(),
                    &options,
                    &cancel,
                )
                .await?;
                let update = match commit.expected() {
                    Some(expected) => RefUpdate::update(commit.branch(), expected, commit_id)?,
                    None => RefUpdate::create(commit.branch(), commit_id)?,
                };
                let batch =
                    RefBatch::new(vec![update]).map(|batch| batch.with_policy(commit.policy()))?;
                let (mut prepared, directory) = prepare_batch(
                    &client,
                    &locator,
                    &batch,
                    preparation_root,
                    &options,
                    &cancel,
                    PackInput::Local(pack_path),
                    None,
                )
                .await?;
                if let Some(content) = native {
                    prepared = prepared.attach_content(content).map_err(Failure::from)?;
                }
                finish_preparation(
                    super::FinishPreparation {
                        client,
                        locator,
                        batch,
                        prepared,
                        directories: vec![workspace, directory],
                        commit: Some(commit_id),
                        persist_recovery: true,
                    },
                    &options,
                    &cancel,
                )
                .await
            })
            .await
    }
}

fn validate_edits(edits: &[FileEdit], limits: crate::ReadLimits, commit_bytes: u64) -> Result<()> {
    if edits.is_empty() || edits.len() as u64 > limits.max_entries {
        return Err(Error::new(
            ErrorKind::LimitExceeded,
            "remote commit file count exceeds operation limits",
        ));
    }
    let mut components = 0u64;
    let mut request_bytes = commit_bytes;
    for edit in edits {
        edit.path.owner()?;
        let depth = edit.path.as_bytes().split(|byte| *byte == b'/').count() as u64;
        if depth > limits.max_depth {
            return Err(Error::new(
                ErrorKind::LimitExceeded,
                "remote commit path depth exceeds operation limits",
            ));
        }
        components = components.checked_add(depth).ok_or_else(|| {
            Error::new(ErrorKind::LimitExceeded, "remote edit path count overflow")
        })?;
        request_bytes = request_bytes
            .checked_add(edit.path.as_bytes().len() as u64)
            .ok_or_else(|| {
                Error::new(ErrorKind::LimitExceeded, "remote edit path bytes overflow")
            })?;
        if let FileChange::Content { size, .. } = &edit.change {
            request_bytes = request_bytes.checked_add(*size).ok_or_else(|| {
                Error::new(ErrorKind::LimitExceeded, "remote edit byte count overflow")
            })?;
        }
    }
    if components > limits.max_entries || request_bytes > limits.max_response_bytes {
        return Err(Error::new(
            ErrorKind::LimitExceeded,
            "remote commit paths exceed operation limits",
        ));
    }
    let mut paths = edits
        .iter()
        .map(|edit| edit.path.as_bytes())
        .collect::<Vec<_>>();
    paths.sort_unstable();
    if paths.windows(2).any(|pair| {
        pair[0] == pair[1]
            || pair[1]
                .strip_prefix(pair[0])
                .is_some_and(|suffix| suffix.starts_with(b"/"))
    }) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "remote commit file paths overlap",
        ));
    }
    Ok(())
}

async fn spool_edits(
    edits: Vec<FileEdit>,
    directory: &Path,
    limits: crate::ReadLimits,
    cancel: &CancellationToken,
) -> Result<Vec<SpooledEdit>> {
    let mut total = 0u64;
    let mut output = Vec::with_capacity(edits.len());
    for (index, edit) in edits.into_iter().enumerate() {
        let (mode, body) = match edit.change {
            FileChange::Delete => (None, None),
            FileChange::Content {
                mode,
                size,
                hydrated,
                reader,
            } => {
                if !hydrated && size > crate::mutation::MAX_GIT_OBJECT_BYTES {
                    return Err(Error::new(
                        ErrorKind::LimitExceeded,
                        "Git blob exceeds the 64 MiB remote edit limit",
                    ));
                }
                total = total.checked_add(size).ok_or_else(|| {
                    Error::new(ErrorKind::LimitExceeded, "remote edit byte count overflow")
                })?;
                if total > limits.max_response_bytes {
                    return Err(Error::new(
                        ErrorKind::LimitExceeded,
                        "remote edit bytes exceed operation limits",
                    ));
                }
                let body = Some(
                    spool_body(
                        reader,
                        size,
                        hydrated,
                        directory.join(format!("content-{index}")),
                        cancel,
                    )
                    .await?,
                );
                (Some(mode), body)
            }
        };
        output.push(SpooledEdit {
            path: edit.path,
            mode,
            body,
        });
    }
    Ok(output)
}

async fn spool_body(
    mut reader: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send + 'static>>,
    size: u64,
    hydrated: bool,
    path: PathBuf,
    cancel: &CancellationToken,
) -> Result<SpoolBody> {
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .await
        .map_err(|source| Error::with_source(ErrorKind::Io, "cannot create edit spool", source))?;
    let mut git = (!hydrated).then(|| {
        let mut hash = gix_hash::hasher(gix_hash::Kind::Sha1);
        hash.update(format!("blob {size}\0").as_bytes());
        hash
    });
    let result = async {
        let mut remaining = size;
        let mut buffer = vec![0; 64 * 1024];
        while remaining != 0 {
            let wanted = remaining.min(buffer.len() as u64) as usize;
            let mut pinned = reader.as_mut();
            let read = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(Error::new(ErrorKind::Cancelled, "remote edit preparation cancelled")),
                result = pinned.read(&mut buffer[..wanted]) => result
                    .map_err(|source| Error::with_source(ErrorKind::Io, "cannot read remote edit content", source))?,
            };
            if read == 0 {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "remote edit content ended before its declared size",
                ));
            }
            if let Some(hash) = &mut git {
                hash.update(&buffer[..read]);
            }
            file.write_all(&buffer[..read]).await.map_err(|source| {
                Error::with_source(ErrorKind::Io, "cannot write edit spool", source)
            })?;
            remaining -= read as u64;
        }
        let mut pinned = reader.as_mut();
        let extra = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(Error::new(ErrorKind::Cancelled, "remote edit preparation cancelled")),
            result = pinned.read(&mut buffer[..1]) => result
                .map_err(|source| Error::with_source(ErrorKind::Io, "cannot finish remote edit content", source))?,
        };
        if extra != 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "remote edit content exceeds its declared size",
            ));
        }
        file.flush().await.map_err(|source| {
            Error::with_source(ErrorKind::Io, "cannot flush edit spool", source)
        })?;
        match git {
            Some(hash) => hash
                .try_finalize()
                .map(Some)
                .map_err(|source| Error::with_source(ErrorKind::Io, "cannot hash Git blob", source)),
            None => Ok(None),
        }
    }
    .await;
    // Retain the exact read/write handle. Windows security scanners may deny an
    // immediate path reopen even though this process still owns the spool.
    let descriptor = file.into_std().await;
    let git_oid = result?;
    Ok(SpoolBody {
        path,
        descriptor,
        size,
        git_oid,
        pointer: None,
    })
}

async fn prepare_hydrated(
    edits: &mut [SpooledEdit],
    directory: &Path,
    max_bytes: u64,
    cancel: &CancellationToken,
) -> Result<Option<crab_remote::prepare::PreparedContent>> {
    let mut edit_indices = Vec::new();
    let mut sources = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        let Some(body) = edit.body.as_ref() else {
            continue;
        };
        if body.git_oid.is_some() {
            continue;
        }
        edit_indices.push(index);
        sources.push(super::native_content::HydratedSource {
            path: body.path.clone(),
            descriptor: Some(body.descriptor.try_clone().map_err(|source| {
                Error::with_source(ErrorKind::Io, "cannot clone edit spool descriptor", source)
            })?),
            size: body.size,
            expected_hash: None,
        });
    }
    if sources.is_empty() {
        return Ok(None);
    }
    let (content, pointers) =
        super::native_content::prepare_sources(&sources, directory, max_bytes, cancel).await?;
    for (edit_index, pointer) in edit_indices.into_iter().zip(pointers) {
        let git_oid = crab_remote::objects::object_id(Kind::Blob, &pointer).map_err(|source| {
            Error::with_source(ErrorKind::Io, "cannot hash Crab pointer", source)
        })?;
        let body = edits[edit_index]
            .body
            .as_mut()
            .ok_or_else(|| Error::new(ErrorKind::Corruption, "hydrated edit spool disappeared"))?;
        body.git_oid = Some(git_oid);
        body.pointer = Some(pointer);
    }
    Ok(Some(content))
}

async fn build_pack(
    client: &super::Client,
    locator: &crate::RepositoryLocator,
    commit: &CommitOptions,
    edits: &[SpooledEdit],
    directory: &Path,
    options: &OperationOptions,
    cancel: &CancellationToken,
) -> Result<(ObjectId, PathBuf)> {
    let resolved = client.0.resolve_repository(locator, cancel).await?;
    let layout = crab_storage::StoreLayout::new(resolved.store.clone(), resolved.prefix.clone());
    let identity = crab_remote_git::RepositoryIdentity::new(
        resolved.namespace,
        resolved.prefix,
        resolved.placement,
    )
    .map_err(crate::remote_error::remote_error)?;
    let repository_options =
        crab_remote_git::RepositoryOptions::new(Default::default(), options.owner_limits())
            .map_err(crate::remote_error::remote_error)?;
    let repository = crab_remote_git::RemoteGitRepository::open(
        resolved.store,
        layout,
        identity,
        client.0.git.clone(),
        repository_options,
        cancel,
    )
    .await
    .map_err(crate::remote_error::remote_error)?;
    let operation = repository
        .operation(crab_remote_git::OperationKind::Repository, cancel)
        .await
        .map_err(crate::remote_error::remote_error)?;
    let built = async {
        let mut tree_edits = crab_remote::objects::TreeEdits::default();
        let mut packed = BTreeMap::new();
        for edit in edits {
            let replacement = match (edit.mode, edit.body.as_ref()) {
                (Some(mode), Some(body)) => {
                    let oid = body.git_oid.ok_or_else(|| {
                        Error::new(ErrorKind::Corruption, "edit object identity is missing")
                    })?;
                    let object = match &body.pointer {
                        Some(pointer) => PackedObject {
                            kind: Kind::Blob,
                            size: pointer.len() as u64,
                            body: ObjectBody::Bytes(pointer.clone()),
                        },
                        None => PackedObject {
                            kind: Kind::Blob,
                            size: body.size,
                            body: ObjectBody::File(body.path.clone()),
                        },
                    };
                    packed.entry(oid).or_insert(object);
                    Some((mode.tree_mode()?, oid))
                }
                (None, None) => None,
                _ => {
                    return Err(Error::new(
                        ErrorKind::Io,
                        "remote edit spool is internally inconsistent",
                    ));
                }
            };
            tree_edits
                .insert(&edit.path.owner()?, replacement)
                .map_err(object_error)?;
        }
        let mut encoded = Vec::new();
        let tree = match commit.base() {
            Some(base) => {
                let snapshot = repository
                    .snapshot(&crab_remote_git::Revision::Commit(base.owner()), &operation)
                    .await
                    .map_err(crate::remote_error::remote_error)?;
                tree_edits
                    .apply(&operation, snapshot.root_tree_oid(), &mut encoded)
                    .await
                    .map_err(object_error)?
            }
            None => tree_edits
                .apply_to_empty(&mut encoded)
                .await
                .map_err(object_error)?,
        };
        for (kind, bytes) in encoded {
            let oid = crab_remote::objects::object_id(kind, &bytes).map_err(|source| {
                Error::with_source(ErrorKind::Io, "cannot hash generated Git object", source)
            })?;
            packed.entry(oid).or_insert(PackedObject {
                kind,
                size: bytes.len() as u64,
                body: ObjectBody::Bytes(bytes),
            });
        }
        let commit_bytes = crab_remote::objects::encode_commit(
            tree,
            &commit
                .base()
                .map(|base| vec![base.owner()])
                .unwrap_or_default(),
            commit.author().owner(),
            commit.committer().owner(),
            commit.message(),
        )
        .map_err(object_error)?;
        let commit_oid =
            crab_remote::objects::object_id(Kind::Commit, &commit_bytes).map_err(|source| {
                Error::with_source(ErrorKind::Io, "cannot hash generated commit", source)
            })?;
        packed.entry(commit_oid).or_insert(PackedObject {
            kind: Kind::Commit,
            size: commit_bytes.len() as u64,
            body: ObjectBody::Bytes(commit_bytes),
        });
        Ok::<_, Error>((ObjectId::from_owner(commit_oid)?, packed))
    }
    .await;
    let close = operation
        .finish(Ok(()))
        .await
        .map_err(crate::remote_error::remote_error);
    let (commit_id, packed) = match (built, close) {
        (Ok(value), Ok(())) => value,
        (Err(error), Ok(())) => return Err(error),
        (Ok(_), Err(error)) => return Err(error),
        (Err(error), Err(cleanup)) => return Err(error.with_cleanup(cleanup)),
    };
    let pack_path = directory.join("generated.pack");
    let maximum = options.read_limits().max_fetched_bytes;
    let packing_cancel = cancel.clone();
    let worker_path = pack_path.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let output = BufWriter::new(File::create_new(&worker_path).map_err(|source| {
            Error::with_source(ErrorKind::Io, "cannot create generated pack", source)
        })?);
        let mut inputs = Vec::with_capacity(packed.len());
        for object in packed.into_values() {
            let reader = match object.body {
                ObjectBody::File(path) => PackReader::File(File::open(path).map_err(|source| {
                    Error::with_source(ErrorKind::Io, "cannot reopen edit spool", source)
                })?),
                ObjectBody::Bytes(bytes) => PackReader::Bytes(Cursor::new(bytes)),
            };
            inputs.push(Ok((object.kind, object.size, reader)));
        }
        crab_git::pack_writer::write_pack(output, inputs.into_iter(), maximum, || {
            packing_cancel.is_cancelled()
        })
        .map_err(|source| {
            let kind = match source {
                crab_git::pack_writer::Error::Limit => ErrorKind::LimitExceeded,
                crab_git::pack_writer::Error::Cancelled => ErrorKind::Cancelled,
                _ => ErrorKind::Io,
            };
            Error::with_source(kind, "cannot build generated pack", source)
        })
    })
    .await
    .map_err(Failure::from)??;
    Ok((commit_id, pack_path))
}

fn object_error(source: crab_remote::objects::Error) -> Error {
    match source {
        crab_remote::objects::Error::Remote(source) => crate::remote_error::remote_error(source),
        source => Error::with_source(
            ErrorKind::InvalidInput,
            "cannot build remote commit",
            source,
        ),
    }
}
