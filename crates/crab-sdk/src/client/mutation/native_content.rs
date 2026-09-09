use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::{Error, ErrorKind, Result};

pub(crate) struct HydratedSource {
    pub path: PathBuf,
    pub descriptor: Option<std::fs::File>,
    pub size: u64,
    pub expected_hash: Option<[u8; 32]>,
}

#[cfg(feature = "local")]
pub(crate) struct StagedSource {
    pub file_hash: crab_xet::hash::MerkleHash,
    pub size: u64,
    pub chunks: Vec<crab_xet::hash::MerkleHash>,
    pub plan: crab_staging::push_plan::FilePushPlan,
    pub staging_root: PathBuf,
}

pub(crate) enum NativeSource {
    Hydrated(HydratedSource),
    #[cfg(feature = "local")]
    Staged(StagedSource),
}

struct StagedFile {
    file_hash: crab_xet::hash::MerkleHash,
    size: u64,
    chunks: Vec<crab_xet::hash::MerkleHash>,
    placements: crab_xet::reconstruction::ChunkPlacementMap,
    dependencies: Vec<Arc<crab_xet::shard::MDBXorbInfo>>,
}

struct NativeArtifact {
    body_hash: [u8; 32],
    size: u64,
    path: PathBuf,
}

pub(crate) async fn prepare_sources(
    sources: &[HydratedSource],
    directory: &Path,
    max_bytes: u64,
    cancel: &CancellationToken,
) -> Result<(crab_remote::prepare::PreparedContent, Vec<Vec<u8>>)> {
    let sources = sources
        .iter()
        .map(|source| -> Result<NativeSource> {
            Ok(NativeSource::Hydrated(HydratedSource {
                path: source.path.clone(),
                descriptor: source
                    .descriptor
                    .as_ref()
                    .map(std::fs::File::try_clone)
                    .transpose()
                    .map_err(|source| {
                        Error::with_source(
                            ErrorKind::Io,
                            "cannot clone hydrated source descriptor",
                            source,
                        )
                    })?,
                size: source.size,
                expected_hash: source.expected_hash,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    prepare_native_sources(&sources, directory, max_bytes, cancel).await
}

pub(crate) async fn prepare_native_sources(
    sources: &[NativeSource],
    directory: &Path,
    max_bytes: u64,
    cancel: &CancellationToken,
) -> Result<(crab_remote::prepare::PreparedContent, Vec<Vec<u8>>)> {
    let staging = crab_staging::StagingArea::open(directory.join("native-staging"))
        .await
        .map_err(staging_error)?;
    let builder = crab_staging::stream::StreamStageXorbBuilder::new(
        1,
        crab_xet::xorb::builder::XorbBuilder::new,
    );
    let mut staged_files = Vec::with_capacity(sources.len());
    let mut xorbs = BTreeMap::<[u8; 32], NativeArtifact>::new();
    for (index, source) in sources.iter().enumerate() {
        #[cfg(feature = "local")]
        let source = match source {
            NativeSource::Hydrated(source) => source,
            NativeSource::Staged(_) => continue,
        };
        #[cfg(not(feature = "local"))]
        let NativeSource::Hydrated(source) = source;
        let repository_path = format!("hydrated-{index}");
        let progress = crab_staging::stream::StreamStageProgress {
            xorb_builder: Some(builder.clone()),
            ..Default::default()
        };
        let result = match &source.descriptor {
            Some(descriptor) => {
                let descriptor = descriptor.try_clone().map_err(|source| {
                    Error::with_source(
                        ErrorKind::Io,
                        "cannot clone hydrated source descriptor",
                        source,
                    )
                })?;
                crab_staging::stream::stage_file_streaming_from_descriptor_as(
                    descriptor,
                    &source.path,
                    directory,
                    Path::new(&repository_path),
                    &staging,
                    progress,
                    cancel,
                )
                .await
            }
            None => {
                crab_staging::stream::stage_file_streaming_as(
                    &source.path,
                    directory,
                    Path::new(&repository_path),
                    &staging,
                    progress,
                    cancel,
                )
                .await
            }
        }
        .map_err(staging_error)?;
        let file_hash = crab_xet::hash::MerkleHash::from(result.file_hash);
        if result.size != source.size
            || source
                .expected_hash
                .is_some_and(|expected| expected != result.file_hash)
        {
            return Err(Error::new(
                ErrorKind::Corruption,
                "hydrated content differs from its committed Crab pointer",
            ));
        }
        let mut chunks =
            Vec::with_capacity(usize::try_from(result.recipe.chunk_count()).unwrap_or(0));
        let mut occurrence = 0u64;
        while occurrence < result.recipe.chunk_count() {
            let page = staging
                .recipe_page(&result.recipe, occurrence)
                .map_err(staging_error)?;
            if page.chunks.is_empty() || page.start_occurrence != occurrence {
                return Err(Error::new(
                    ErrorKind::Corruption,
                    "staged hydrated recipe is incomplete",
                ));
            }
            occurrence = page.next_occurrence();
            chunks.extend(page.chunks.into_iter().map(|chunk| chunk.chunk_hash));
        }
        let mut placements = HashMap::new();
        let mut dependencies = Vec::new();
        for xorb in result.prepared_xorbs {
            let info = crab_xet::shard::xorb_info_from_placements(xorb.hash, &xorb.placements)
                .map_err(xet_error)?;
            for placement in &xorb.placements {
                placements
                    .entry(placement.chunk_hash)
                    .or_insert_with(|| placement.clone());
            }
            dependencies.push(Arc::new(info));
            let protocol_hash: [u8; 32] = xorb.hash.into();
            let body_hash = *blake3::Hash::from_hex(&xorb.payload_hash)
                .map_err(|source| {
                    Error::with_source(
                        ErrorKind::Corruption,
                        "prepared xorb body hash is invalid",
                        source,
                    )
                })?
                .as_bytes();
            let artifact = NativeArtifact {
                body_hash,
                size: xorb.bytes,
                path: crab_staging::push_plan::prepared_xorb_path(staging.root(), &xorb.hash),
            };
            if let Some(existing) = xorbs.insert(protocol_hash, artifact)
                && (existing.body_hash != body_hash || existing.size != xorb.bytes)
            {
                return Err(Error::new(
                    ErrorKind::Corruption,
                    "prepared xorb identity collision",
                ));
            }
        }
        staged_files.push(StagedFile {
            file_hash,
            size: result.size,
            chunks,
            placements,
            dependencies,
        });
    }
    staging.close().await.map_err(staging_error)?;

    #[cfg(feature = "local")]
    for source in sources {
        let NativeSource::Staged(source) = source else {
            continue;
        };
        let file_hash = source.plan.file_hash().map_err(staging_error)?;
        if file_hash != source.file_hash
            || source.plan.file_size != source.size
            || source.plan.chunk_count != source.chunks.len() as u64
            || !source.plan.staged_chunk_sequence_verified
        {
            return Err(Error::new(
                ErrorKind::Corruption,
                "prepared staging plan differs from its committed Crab pointer",
            ));
        }
        let mut placements = HashMap::new();
        let mut dependencies = Vec::new();
        for planned in &source.plan.prepared_xorbs {
            if !planned.upload {
                continue;
            }
            let xorb_hash = planned.hash().map_err(staging_error)?;
            let planned_placements = planned
                .placements
                .iter()
                .map(crab_staging::push_plan::PlannedPlacement::to_placement)
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(staging_error)?;
            let info = crab_xet::shard::xorb_info_from_placements(xorb_hash, &planned_placements)
                .map_err(xet_error)?;
            for placement in &planned_placements {
                placements
                    .entry(placement.chunk_hash)
                    .or_insert_with(|| placement.clone());
            }
            dependencies.push(Arc::new(info));
            let protocol_hash: [u8; 32] = xorb_hash.into();
            let body_hash = *blake3::Hash::from_hex(&planned.payload_hash)
                .map_err(|source| {
                    Error::with_source(
                        ErrorKind::Corruption,
                        "prepared xorb body hash is invalid",
                        source,
                    )
                })?
                .as_bytes();
            let artifact = NativeArtifact {
                body_hash,
                size: planned.bytes,
                path: crab_staging::push_plan::prepared_xorb_path(&source.staging_root, &xorb_hash),
            };
            if let Some(existing) = xorbs.insert(protocol_hash, artifact)
                && (existing.body_hash != body_hash || existing.size != planned.bytes)
            {
                return Err(Error::new(
                    ErrorKind::Corruption,
                    "prepared xorb identity collision",
                ));
            }
        }
        if source
            .chunks
            .iter()
            .any(|chunk| !placements.contains_key(chunk))
        {
            return Err(Error::new(
                ErrorKind::Corruption,
                "prepared staging plan does not cover every file chunk",
            ));
        }
        staged_files.push(StagedFile {
            file_hash: source.file_hash,
            size: source.size,
            chunks: source.chunks.clone(),
            placements,
            dependencies,
        });
    }

    let mut session = crab_xet::shard::PushShardSession::new();
    let mut shard_indices = Vec::with_capacity(staged_files.len());
    let mut referenced_xorbs = std::collections::BTreeSet::new();
    for file in &staged_files {
        let info = crab_xet::shard::file_info_from_placements(
            file.file_hash,
            &file.chunks,
            &file.placements,
        )
        .map_err(xet_error)?;
        let required = info
            .segments
            .iter()
            .map(|segment| segment.xorb_hash)
            .collect::<std::collections::BTreeSet<_>>();
        referenced_xorbs.extend(required.iter().map(|hash| <[u8; 32]>::from(*hash)));
        let dependencies = file
            .dependencies
            .iter()
            .filter(|dependency| required.contains(&dependency.metadata.xorb_hash))
            .cloned()
            .collect::<Vec<_>>();
        shard_indices.push(
            session
                .add_file_bundle(info, &dependencies)
                .map_err(xet_error)?,
        );
    }
    let finalized = session.finalize().map_err(xet_error)?;
    let mut shards = Vec::with_capacity(finalized.len());
    let mut shard_hashes = Vec::with_capacity(finalized.len());
    for (index, (bytes, protocol_hash)) in finalized.into_iter().enumerate() {
        let path = directory.join(format!("native-shard-{index}"));
        tokio::fs::write(&path, &bytes).await.map_err(|source| {
            Error::with_source(ErrorKind::Io, "cannot write prepared shard", source)
        })?;
        let protocol: [u8; 32] = protocol_hash.into();
        shard_hashes.push(protocol);
        shards.push(crab_remote::prepare::ContentArtifact::new(
            protocol,
            *blake3::hash(&bytes).as_bytes(),
            bytes.len() as u64,
            path,
        ));
    }
    let mut files = Vec::with_capacity(staged_files.len());
    let mut pointers = Vec::with_capacity(staged_files.len());
    for (file, shard_index) in staged_files.into_iter().zip(shard_indices) {
        let shard_hash = *shard_hashes
            .get(shard_index)
            .ok_or_else(|| Error::new(ErrorKind::Corruption, "prepared shard index is missing"))?;
        pointers.push(
            crab_types::pointer::Pointer {
                file_hash: file.file_hash.into(),
                size: file.size,
                shard_hint: Some(shard_hash),
            }
            .serialize(),
        );
        files.push(crab_remote::prepare::ContentFile::new(
            file.file_hash.into(),
            file.size,
            shard_hash,
        ));
    }
    let xorbs = xorbs
        .into_iter()
        .filter_map(|(protocol_hash, artifact)| {
            referenced_xorbs.contains(&protocol_hash).then(|| {
                crab_remote::prepare::ContentArtifact::new(
                    protocol_hash,
                    artifact.body_hash,
                    artifact.size,
                    artifact.path,
                )
            })
        })
        .collect();
    let content =
        crab_remote::prepare::PreparedContent::new(xorbs, shards, files, max_bytes, cancel)
            .await
            .map_err(|source| {
                Error::with_source(
                    ErrorKind::Corruption,
                    "cannot validate prepared hydrated content",
                    source,
                )
            })?;
    Ok((content, pointers))
}

fn staging_error(source: crab_staging::StagingError) -> Error {
    let kind = if matches!(source, crab_staging::StagingError::Cancelled) {
        ErrorKind::Cancelled
    } else {
        ErrorKind::Io
    };
    Error::with_source(kind, "cannot prepare hydrated content", source)
}

fn xet_error(source: crab_xet::error::XetError) -> Error {
    Error::with_source(
        ErrorKind::Corruption,
        "cannot prepare hydrated content",
        source,
    )
}
