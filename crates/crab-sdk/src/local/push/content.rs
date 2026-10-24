use super::{LocalPushUpdate, local_error};
use crate::{Error, ErrorKind, Result};

pub(super) struct Preparation<'a> {
    pub tools: &'a crab_remote::local::LocalTools,
    pub root: &'a std::path::Path,
    pub common: &'a std::path::Path,
    pub advertised: &'a std::collections::BTreeMap<String, String>,
    pub updates: &'a [LocalPushUpdate],
    pub scratch: &'a std::path::Path,
    pub origin: &'a crab_storage::StoreLayout<crab_storage::Store>,
    pub limits: crate::ReadLimits,
}

pub(super) async fn prepare(
    input: Preparation<'_>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<crab_remote::prepare::PreparedContent>> {
    let Preparation {
        tools,
        root,
        common,
        advertised,
        updates,
        scratch,
        origin,
        limits,
    } = input;
    let max_bytes = limits.max_fetched_bytes;
    let pointers = discover_push_pointers(tools, root, advertised, updates, cancel).await?;
    let lfs = prepare_lfs(
        tools,
        root,
        common,
        &pointers.lfs,
        scratch,
        max_bytes,
        cancel,
    )
    .await?;
    let content = prepare_xet(pointers.crab, common, scratch, origin, limits, cancel).await?;
    match (content, lfs.is_empty()) {
        (content, true) => Ok(content),
        (Some(content), false) => content
            .with_lfs(lfs, max_bytes)
            .map(Some)
            .map_err(preparation_error),
        (None, false) => crab_remote::prepare::PreparedContent::from_lfs(lfs, max_bytes)
            .map(Some)
            .map_err(preparation_error),
    }
}

async fn prepare_xet(
    pointers: std::collections::BTreeMap<[u8; 32], crab_types::pointer::Pointer>,
    common: &std::path::Path,
    scratch: &std::path::Path,
    origin: &crab_storage::StoreLayout<crab_storage::Store>,
    limits: crate::ReadLimits,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<crab_remote::prepare::PreparedContent>> {
    let max_bytes = limits.max_fetched_bytes;
    if pointers.is_empty() {
        return Ok(None);
    }
    let staging_root = common.parent().ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            "Git common directory has no worktree parent",
        )
    })?;
    let staging_root = staging_root.join(".crab").join("staging");
    if !staging_root.exists() {
        return Ok(None);
    }
    let staging = crab_staging::StagingAreaReadOnly::open_blocking_default(staging_root)
        .await
        .map_err(staging_error)?;
    let source_root = scratch.join("hydrated-sources");
    tokio::fs::create_dir(&source_root)
        .await
        .map_err(|source| {
            Error::with_source(ErrorKind::Io, "cannot create push content scratch", source)
        })?;
    let mut sources = Vec::new();
    let mut remote_xorbs = std::collections::BTreeMap::new();
    for (index, pointer) in pointers.values().enumerate() {
        let file_hash = crab_xet::hash::MerkleHash::from(pointer.file_hash);
        let Some(recipe) = staging
            .published_recipe_for_file(&file_hash)
            .map_err(staging_error)?
        else {
            continue;
        };
        if recipe.file_size() != pointer.size {
            continue;
        }
        if recipe.chunk_count() > limits.max_entries {
            return Err(Error::new(
                ErrorKind::LimitExceeded,
                "staged recipe exceeds chunk limit",
            ));
        }
        let mut chunks = Vec::with_capacity(usize::try_from(recipe.chunk_count()).unwrap_or(0));
        let mut occurrence = 0u64;
        while occurrence < recipe.chunk_count() {
            let page = staging
                .recipe_page(&recipe, occurrence)
                .map_err(staging_error)?;
            if page.chunks.is_empty() || page.start_occurrence != occurrence {
                return Err(Error::new(
                    ErrorKind::Corruption,
                    "local staging recipe is incomplete",
                ));
            }
            occurrence = page.next_occurrence();
            chunks.extend(page.chunks.into_iter().map(|chunk| chunk.chunk_hash));
        }
        if let Some(plan) = staging
            .load_file_push_plan(&file_hash)
            .await
            .map_err(staging_error)?
        {
            let planned_chunks = plan
                .prepared_xorbs
                .iter()
                .filter(|xorb| xorb.upload)
                .flat_map(|xorb| xorb.placements.iter())
                .map(|placement| placement.chunk_hash.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            if chunks
                .iter()
                .all(|chunk| planned_chunks.contains(chunk.hex().as_str()))
            {
                sources.push(
                    crate::client::mutation::native_content::NativeSource::Staged(
                        crate::client::mutation::native_content::StagedSource {
                            file_hash,
                            size: pointer.size,
                            chunks,
                            plan,
                            staging_root: staging.root().to_path_buf(),
                        },
                    ),
                );
                continue;
            }
        }
        // Reused chunks can have only remote recipe authority. Reconstruct
        // through verified bytes before preparing this file's new catalog entry;
        // shared chunk hints cannot substitute for that repository authority.
        let remote = staging
            .recipe_remote_chunk_range(&recipe, 0, recipe.chunk_count())
            .map_err(staging_error)?
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut complete = true;
        for chunk in chunks.iter().collect::<std::collections::BTreeSet<_>>() {
            if !staging.has_chunk(chunk).await.map_err(staging_error)?
                && !remote.contains_key(chunk)
            {
                complete = false;
                break;
            }
        }
        if !complete {
            continue;
        }
        let path = source_root.join(index.to_string());
        let mut file = tokio::fs::File::create(&path).await.map_err(|source| {
            Error::with_source(ErrorKind::Io, "cannot create push content source", source)
        })?;
        let mut occurrence = 0u64;
        while occurrence < recipe.chunk_count() {
            let page = staging
                .recipe_page(&recipe, occurrence)
                .map_err(staging_error)?;
            if page.chunks.is_empty() || page.start_occurrence != occurrence {
                return Err(Error::new(
                    ErrorKind::Corruption,
                    "local staging recipe is incomplete",
                ));
            }
            occurrence = page.next_occurrence();
            for chunk in page.chunks {
                if cancel.is_cancelled() {
                    return Err(Error::new(
                        ErrorKind::Cancelled,
                        "local push content preparation cancelled",
                    ));
                }
                let local = staging
                    .get_chunk(&chunk.chunk_hash)
                    .await
                    .map_err(staging_error)?;
                let bytes = if let Some(local) = local {
                    local
                } else if let Some(candidate) = remote.get(&chunk.chunk_hash) {
                    reused_chunk(
                        origin,
                        candidate,
                        chunk.chunk_hash,
                        chunk.len,
                        &mut remote_xorbs,
                        cancel,
                    )
                    .await?
                } else {
                    return Err(Error::new(
                        ErrorKind::Corruption,
                        "local staging lost a committed pointer chunk",
                    ));
                };
                if bytes.len() as u64 != chunk.len {
                    return Err(Error::new(
                        ErrorKind::Corruption,
                        "local staging chunk size changed",
                    ));
                }
                tokio::io::AsyncWriteExt::write_all(&mut file, &bytes)
                    .await
                    .map_err(|source| {
                        Error::with_source(
                            ErrorKind::Io,
                            "cannot materialize staged push content",
                            source,
                        )
                    })?;
            }
        }
        tokio::io::AsyncWriteExt::flush(&mut file)
            .await
            .map_err(|source| {
                Error::with_source(ErrorKind::Io, "cannot flush staged push content", source)
            })?;
        sources.push(
            crate::client::mutation::native_content::NativeSource::Hydrated(
                crate::client::mutation::native_content::HydratedSource {
                    path,
                    descriptor: None,
                    size: pointer.size,
                    expected_hash: Some(pointer.file_hash),
                },
            ),
        );
    }
    drop(staging);
    if sources.is_empty() {
        return Ok(None);
    }
    let (content, _) = crate::client::mutation::native_content::prepare_native_sources(
        &sources, scratch, max_bytes, cancel,
    )
    .await?;
    Ok(Some(content))
}

#[derive(Default)]
struct PushPointers {
    crab: std::collections::BTreeMap<[u8; 32], crab_types::pointer::Pointer>,
    lfs: std::collections::BTreeMap<[u8; 32], crab_git::LfsPointer>,
}

async fn reused_chunk(
    origin: &crab_storage::StoreLayout<crab_storage::Store>,
    candidate: &crab_staging::push_plan::ExistingChunkCandidate,
    expected_hash: crab_xet::hash::MerkleHash,
    expected_size: u64,
    xorbs: &mut std::collections::BTreeMap<
        crab_xet::hash::MerkleHash,
        std::sync::Arc<crab_xet::xorb::parser::XorbParser>,
    >,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<bytes::Bytes> {
    use crab_xet::xorb::parser::XorbParser;
    let reference = candidate.xorb_ref;
    if u64::from(reference.uncompressed_size) != expected_size {
        return Err(Error::new(
            ErrorKind::Corruption,
            "reused chunk size differs from staged recipe",
        ));
    }
    let parser = if let Some(parser) = xorbs.get(&reference.xorb_hash) {
        parser.clone()
    } else {
        let path = origin.xorb_path(&reference.xorb_hash);
        // This store retains the capture's admission: reused origin reads share
        // its aggregate request/byte budget rather than opening a new scope.
        let bytes = origin
            .store()
            .get_immutable_bounded(&path, crab_xet::xorb::format::MAX_XORB_SIZE as u64)
            .await
            .map_err(|source| {
                Error::with_source(
                    crate::remote_error::storage_kind(&source),
                    "cannot read reused staged xorb",
                    source,
                )
            })?;
        // Placements locate bytes; neither a hint nor a receipt ID proves them.
        // Verify origin framing/identity here and selected chunk bytes below;
        // native preparation then checks the complete committed file hash.
        let parsed = tokio::task::spawn_blocking(move || {
            let parsed = XorbParser::parse(bytes)?;
            parsed.verify_payload_digest()?;
            Ok::<_, crab_xet::error::XetError>(parsed)
        })
        .await
        .map_err(|source| {
            Error::with_source(
                ErrorKind::Io,
                "cannot join reused xorb verification",
                source,
            )
        })?
        .map_err(|source| {
            Error::with_source(ErrorKind::Corruption, "cannot verify reused xorb", source)
        })?;
        if parsed.hash() != reference.xorb_hash {
            return Err(Error::new(
                ErrorKind::Corruption,
                "reused xorb identity differs from staged recipe",
            ));
        }
        let parsed = std::sync::Arc::new(parsed);
        xorbs.insert(reference.xorb_hash, parsed.clone());
        parsed
    };
    if cancel.is_cancelled() {
        return Err(Error::new(
            ErrorKind::Cancelled,
            "local push content preparation cancelled",
        ));
    }
    let chunk = tokio::task::spawn_blocking(move || parser.get_chunk(reference.chunk_index))
        .await
        .map_err(|source| {
            Error::with_source(
                ErrorKind::Io,
                "cannot join reused chunk verification",
                source,
            )
        })?
        .map_err(|source| {
            Error::with_source(ErrorKind::Corruption, "cannot verify reused chunk", source)
        })?;
    if chunk.hash != expected_hash || chunk.data.len() as u64 != expected_size {
        return Err(Error::new(
            ErrorKind::Corruption,
            "reused chunk differs from staged recipe",
        ));
    }
    Ok(chunk.data)
}

async fn discover_push_pointers(
    tools: &crab_remote::local::LocalTools,
    root: &std::path::Path,
    advertised: &std::collections::BTreeMap<String, String>,
    updates: &[LocalPushUpdate],
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<PushPointers> {
    let targets = updates
        .iter()
        .filter_map(|update| update.target.as_deref())
        .collect::<std::collections::BTreeSet<_>>();
    if targets.is_empty() {
        return Ok(PushPointers::default());
    }
    let objects = tools
        .run_git_with_input(
            Some(root),
            ["rev-list", "--objects", "--no-object-names", "--stdin"],
            super::revision_input(targets, advertised),
            false,
            cancel,
        )
        .await
        .map_err(local_error)?;
    let mut oids = Vec::new();
    for line in objects.stdout.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Some(oid) = line.split(|byte| *byte == b' ').next() else {
            continue;
        };
        if oid.len() != 40 || !oid.iter().all(u8::is_ascii_hexdigit) {
            return Err(Error::new(
                ErrorKind::Corruption,
                "Git returned an invalid object during push discovery",
            ));
        }
        oids.push(oid);
    }
    let mut pointers = PushPointers::default();
    // Batch type/size inspection works on the minimum Git version and avoids
    // two subprocesses per blob. Bounded batches retain the tool output ceiling.
    for batch in oids.chunks(1024) {
        let input = batch
            .iter()
            .flat_map(|oid| oid.iter().copied().chain(std::iter::once(b'\n')))
            .collect();
        let metadata = tools
            .run_git_with_input(
                Some(root),
                ["cat-file", "--batch-check", "--buffer"],
                input,
                false,
                cancel,
            )
            .await
            .map_err(local_error)?;
        let rows = metadata
            .stdout
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        if rows.len() != batch.len() {
            return Err(Error::new(
                ErrorKind::Corruption,
                "Git returned incomplete object metadata",
            ));
        }
        for (expected, row) in batch.iter().zip(rows) {
            let fields = row.split(|byte| *byte == b' ').collect::<Vec<_>>();
            let [oid, kind, size] = fields.as_slice() else {
                return Err(Error::new(
                    ErrorKind::Corruption,
                    "Git returned invalid object metadata",
                ));
            };
            if oid != expected {
                return Err(Error::new(
                    ErrorKind::Corruption,
                    "Git object metadata changed identity",
                ));
            }
            if *kind != b"blob" {
                continue;
            }
            let size = std::str::from_utf8(size)
                .map_err(|source| {
                    Error::with_source(ErrorKind::Corruption, "Git blob size is not UTF-8", source)
                })?
                .parse::<usize>()
                .map_err(|source| {
                    Error::with_source(ErrorKind::Corruption, "Git blob size is invalid", source)
                })?;
            let oid = std::str::from_utf8(oid).map_err(|source| {
                Error::with_source(ErrorKind::Corruption, "Git object ID is not UTF-8", source)
            })?;
            if size
                > crab_types::pointer::MAX_POINTER_SIZE
                    .max(crab_git::lfs_pointer::MAX_LFS_POINTER_SIZE)
            {
                continue;
            }
            let body = tools
                .run_git(Some(root), ["cat-file", "blob", oid], false, cancel)
                .await
                .map_err(local_error)?
                .stdout;
            let pointer = match crab_git::classify(&body) {
                crab_git::pointer_detect::PointerKind::Lfs(pointer) => {
                    if let Some(existing) = pointers.lfs.insert(pointer.oid, pointer.clone())
                        && existing != pointer
                    {
                        return Err(Error::new(
                            ErrorKind::Corruption,
                            "one LFS identity has inconsistent pointer metadata",
                        ));
                    }
                    continue;
                }
                crab_git::pointer_detect::PointerKind::Crab(pointer) => pointer,
                crab_git::pointer_detect::PointerKind::NotAPointer => continue,
            };
            if let Some(existing) = pointers.crab.insert(pointer.file_hash, pointer.clone())
                && existing != pointer
            {
                return Err(Error::new(
                    ErrorKind::Corruption,
                    "one Crab file identity has inconsistent pointer metadata",
                ));
            }
        }
    }
    Ok(pointers)
}

async fn prepare_lfs(
    tools: &crab_remote::local::LocalTools,
    root: &std::path::Path,
    common: &std::path::Path,
    pointers: &std::collections::BTreeMap<[u8; 32], crab_git::LfsPointer>,
    scratch: &std::path::Path,
    max_bytes: u64,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<crab_remote::prepare::ContentArtifact>> {
    if pointers.is_empty() {
        return Ok(Vec::new());
    }
    let mut values = Vec::new();
    for key in ["lfs.storage", "lfs.lfsdir"] {
        let value = match tools
            .run_git(
                Some(root),
                ["config", "--includes", "--null", "--get", key],
                false,
                cancel,
            )
            .await
        {
            Ok(output) => Some(
                String::from_utf8(output.stdout)
                    .map_err(|source| {
                        Error::with_source(
                            ErrorKind::Corruption,
                            "LFS storage setting is not UTF-8",
                            source,
                        )
                    })?
                    .strip_suffix('\0')
                    .ok_or_else(|| {
                        Error::new(ErrorKind::Corruption, "Git LFS setting has invalid framing")
                    })?
                    .to_owned(),
            ),
            Err(crab_remote::local::LocalError::Exit(output))
                if output.status.code() == Some(1) =>
            {
                None
            }
            Err(source) => return Err(local_error(source)),
        };
        values.push(value);
    }
    let environment = std::env::var("GIT_LFS_DIR").ok();
    let configured = crab_lfs::local::storage_override(
        values[0].as_deref(),
        values[1].as_deref(),
        environment.as_deref(),
    );
    let storage = crab_lfs::local::storage_dir(common, configured.as_deref());
    let mut bytes = 0u64;
    let mut objects = Vec::new();
    for pointer in pointers.values() {
        let source = crab_lfs::local::object_path(&storage, &pointer.oid);
        match tokio::fs::metadata(&source).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(Error::with_source(
                    ErrorKind::Io,
                    "cannot inspect local LFS payload",
                    source,
                ));
            }
        }
        bytes = bytes
            .checked_add(pointer.size)
            .filter(|bytes| *bytes <= max_bytes)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::LimitExceeded,
                    "prepared LFS payload exceeds byte limit",
                )
            })?;
        let oid = crab_git::lfs_pointer::hex_encode(&pointer.oid);
        let destination = scratch.join(format!("lfs-{oid}"));
        let digest =
            crab_lfs::local::snapshot(&source, &destination, &pointer.oid, pointer.size, cancel)
                .await
                .map_err(|source| {
                    let kind = match source {
                        crab_lfs::local::SnapshotError::Cancelled => ErrorKind::Cancelled,
                        crab_lfs::local::SnapshotError::Corrupt(_) => ErrorKind::Corruption,
                        crab_lfs::local::SnapshotError::Io(_) => ErrorKind::Io,
                    };
                    Error::with_source(kind, "cannot prepare local LFS payload", source)
                })?;
        objects.push(crab_remote::prepare::ContentArtifact::new(
            pointer.oid,
            digest,
            pointer.size,
            destination,
        ));
    }
    Ok(objects)
}

fn preparation_error(source: crab_remote::prepare::Error) -> Error {
    crate::client::mutation::Failure::from(source).into()
}

fn staging_error(source: crab_staging::StagingError) -> Error {
    let kind = if matches!(source, crab_staging::StagingError::Cancelled) {
        ErrorKind::Cancelled
    } else {
        ErrorKind::Io
    };
    Error::with_source(kind, "cannot prepare local staged push content", source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use crab_storage::{Store, StoreLayout};
    use crab_xet::hash::{MerkleHash, compute_data_hash};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct Fixture {
        origin: StoreLayout<Store>,
        candidate: crab_staging::push_plan::ExistingChunkCandidate,
        hash: MerkleHash,
        body: Bytes,
        encoded: Bytes,
    }

    async fn fixture() -> Fixture {
        let body = Bytes::from_static(b"verified reused chunk bytes");
        let hash = compute_data_hash(&body);
        let mut builder = crab_xet::xorb::builder::XorbBuilder::new();
        builder
            .push(
                &crab_xet::xorb::format::Chunk {
                    hash,
                    data: body.clone(),
                },
                crab_xet::xorb::builder::RunId(0),
            )
            .unwrap();
        let xorb = builder.finalize().unwrap().pop().unwrap();
        let origin = StoreLayout::new(
            Store::new(Arc::new(object_store::memory::InMemory::new())),
            "repository".to_owned(),
        );
        origin
            .store()
            .put(&origin.xorb_path(&xorb.hash), xorb.bytes.clone())
            .await
            .unwrap();
        Fixture {
            origin,
            candidate: crab_staging::push_plan::ExistingChunkCandidate {
                xorb_ref: crab_xet::xorb::format::XorbRef {
                    xorb_hash: xorb.hash,
                    chunk_index: 0,
                    uncompressed_size: body.len() as u32,
                },
                placement_id: [0x11; 32],
                origin_proof_id: [0x22; 32],
            },
            hash,
            body,
            encoded: xorb.bytes,
        }
    }

    #[tokio::test]
    async fn reused_chunks_share_verified_origin_bytes_within_preparation() {
        let fixture = fixture().await;
        let mut cache = std::collections::BTreeMap::new();
        let cancel = CancellationToken::new();
        let first = reused_chunk(
            &fixture.origin,
            &fixture.candidate,
            fixture.hash,
            fixture.body.len() as u64,
            &mut cache,
            &cancel,
        )
        .await
        .unwrap();
        fixture
            .origin
            .store()
            .delete(
                &fixture
                    .origin
                    .xorb_path(&fixture.candidate.xorb_ref.xorb_hash),
            )
            .await
            .unwrap();
        let second = reused_chunk(
            &fixture.origin,
            &fixture.candidate,
            fixture.hash,
            fixture.body.len() as u64,
            &mut cache,
            &cancel,
        )
        .await
        .unwrap();
        assert!(first == fixture.body && second == fixture.body);
    }

    #[tokio::test]
    async fn corrupted_reused_payload_is_rejected() {
        use object_store::ObjectStoreExt as _;
        let fixture = fixture().await;
        let mut bytes = fixture.encoded.to_vec();
        bytes[0] ^= 1;
        fixture
            .origin
            .store()
            .inner()
            .put(
                &fixture
                    .origin
                    .xorb_path(&fixture.candidate.xorb_ref.xorb_hash),
                Bytes::from(bytes).into(),
            )
            .await
            .unwrap();
        let result = reused_chunk(
            &fixture.origin,
            &fixture.candidate,
            fixture.hash,
            fixture.body.len() as u64,
            &mut std::collections::BTreeMap::new(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result.unwrap_err().kind(), ErrorKind::Corruption);
    }

    #[tokio::test]
    async fn reused_payload_does_not_reset_preparation_admission() {
        let fixture = fixture().await;
        let directory = tempfile::tempdir().unwrap();
        let client = crate::Client::builder()
            .direct_store(crate::DirectStoreOptions::filesystem(directory.path()).unwrap())
            .build()
            .unwrap();
        let options = crate::OperationOptions::default()
            .with_limits(crate::ReadLimits {
                max_storage_requests: 1,
                ..Default::default()
            })
            .unwrap();
        let cancel = CancellationToken::new();
        let store = client
            .0
            .admitted_store(fixture.origin.store(), &options, &cancel)
            .unwrap();
        let origin = StoreLayout::new(store, fixture.origin.repo_prefix().to_owned());
        origin
            .store()
            .head(&origin.xorb_path(&fixture.candidate.xorb_ref.xorb_hash))
            .await
            .unwrap();
        let result = reused_chunk(
            &origin,
            &fixture.candidate,
            fixture.hash,
            fixture.body.len() as u64,
            &mut std::collections::BTreeMap::new(),
            &cancel,
        )
        .await;
        client.close().await.unwrap();
        assert_eq!(result.unwrap_err().kind(), ErrorKind::LimitExceeded);
    }
}
