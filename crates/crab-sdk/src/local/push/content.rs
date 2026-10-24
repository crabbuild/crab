use super::{LocalPushUpdate, git_line, local_error};
use crate::{Error, ErrorKind, Result};

pub(super) async fn prepare(
    tools: &crab_remote::local::LocalTools,
    root: &std::path::Path,
    common: &std::path::Path,
    advertised: &std::collections::BTreeMap<String, String>,
    updates: &[LocalPushUpdate],
    scratch: &std::path::Path,
    max_bytes: u64,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<crab_remote::prepare::PreparedContent>> {
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
    let content = prepare_xet(pointers.crab, common, scratch, max_bytes, cancel).await?;
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
    max_bytes: u64,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<crab_remote::prepare::PreparedContent>> {
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
        if !staging
            .has_complete_segment_authority_for_recipe(&recipe)
            .map_err(staging_error)?
        {
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
                let bytes = staging
                    .get_chunk(&chunk.chunk_hash)
                    .await
                    .map_err(staging_error)?
                    .ok_or_else(|| {
                        Error::new(
                            ErrorKind::Corruption,
                            "local staging lost a committed pointer chunk",
                        )
                    })?;
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
            [
                "rev-list",
                "--objects",
                "--filter=object:type=blob",
                "--stdin",
            ],
            super::revision_input(targets, advertised),
            false,
            cancel,
        )
        .await
        .map_err(local_error)?;
    let mut pointers = PushPointers::default();
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
        let oid = std::str::from_utf8(oid).map_err(|source| {
            Error::with_source(ErrorKind::Corruption, "Git object ID is not UTF-8", source)
        })?;
        if git_line(tools, root, ["cat-file", "-t", oid], cancel).await? != "blob" {
            continue;
        }
        let size = git_line(tools, root, ["cat-file", "-s", oid], cancel)
            .await?
            .parse::<usize>()
            .map_err(|source| {
                Error::with_source(ErrorKind::Corruption, "Git blob size is invalid", source)
            })?;
        if size
            > crab_types::pointer::MAX_POINTER_SIZE.max(crab_git::lfs_pointer::MAX_LFS_POINTER_SIZE)
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
