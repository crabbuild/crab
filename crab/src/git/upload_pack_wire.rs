//! Git protocol-v2 upload-pack over the helper's already-authenticated stdio.
//!
//! This module owns wire framing, command semantics, and the product-level
//! admission repair before a session starts. Generation pinning, object
//! traversal, and pack production stay in the shared read and remote-git crates.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;

use crab_metadata::git_visibility::GitVisibilityIndex;
use crab_read::{
    FetchAdmissionPolicy, UploadPackFilter, UploadPackRequest, combine_upload_pack_filters,
    parse_upload_pack_filter, plan_upload_pack,
};
use crab_remote_git::{
    Error as RemoteGitError, RemoteGitRepository, RemoteGitRuntime, RepositoryIdentity,
    RepositoryOptions,
};
use gix_hash::ObjectId;
use gix_packetline::{PacketLineRef, decode::PacketLineOrWantedSize};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
};
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result};

const MAX_PACKET_BYTES: usize = 65_520;
const MAX_REQUEST_PACKETS: usize = 4_096;
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const LOCATOR_READ_REPAIR_LOCK_TTL: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Packet {
    Data(Vec<u8>),
    Flush,
    Delimiter,
    ResponseEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandRequest {
    command: String,
    capabilities: Vec<String>,
    args: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct LsRefsRequest {
    symrefs: bool,
    peel: bool,
    unborn: bool,
    prefixes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct FetchRequest {
    wants: Vec<ObjectId>,
    haves: Vec<ObjectId>,
    shallow: Vec<ObjectId>,
    deepen: Option<u32>,
    deepen_relative: bool,
    include_tags: bool,
    no_progress: bool,
    done: bool,
    thin_pack: bool,
    ofs_delta: bool,
    filter: UploadPackFilter,
}

/// Check whether the repository has the complete proof required to advertise
/// protocol-v2 upload-pack without rebuilding lagging locator coverage.
pub async fn snapshot_available(
    store: &crab_storage::Store,
    prefix: &str,
    cancellation: &CancellationToken,
) -> bool {
    // If exact locator coverage lags, omit v2 and let Git use the
    // already-advertised complete-pack fetch path. Rebuilding it here makes
    // every dependent hot-ref generation pay the full locator publication cost.
    let Ok(repository) = open_repository_snapshot(store, prefix, cancellation).await else {
        return false;
    };
    match repository.visibility_index(cancellation).await {
        Ok(_) => true,
        Err(error) if visibility_index_needs_repair(&error) => {
            let repair_store = crate::storage::Store::from_storage(store.clone());
            let repair_layout =
                crate::storage::StoreLayout::new(repair_store.clone(), prefix.to_owned());
            if matches!(
                super::push::git_generation_owner_is_active(&repair_store, &repair_layout).await,
                Ok(true)
            ) {
                return false;
            }
            match Box::pin(super::push::repair_git_visibility_if_current(
                &repair_store,
                &repair_layout,
                repository.generation(),
                LOCATOR_READ_REPAIR_LOCK_TTL,
                cancellation,
            ))
            .await
            {
                Ok(Some(super::push::GitVisibilityPublication::Published)) => {
                    let Ok(repository) =
                        open_repository_snapshot(store, prefix, cancellation).await
                    else {
                        return false;
                    };
                    repository.visibility_index(cancellation).await.is_ok()
                }
                Ok(Some(super::push::GitVisibilityPublication::CompletePackOnly(_)) | None) => {
                    false
                }
                Err(error) => {
                    tracing::warn!(%error, "current Git visibility repair failed");
                    false
                }
            }
        }
        Err(_) => false,
    }
}

fn visibility_index_needs_repair(error: &RemoteGitError) -> bool {
    matches!(
        error,
        RemoteGitError::Metadata(crab_metadata::error::MetadataError::Storage {
            source: crab_storage::StorageError::NotFound { .. },
        }) | RemoteGitError::RepositoryState {
            reason: crab_remote_git::RepositoryStateError::VisibilityProofMismatch,
        }
    )
}

pub(crate) fn hidden_ref_patterns_are_valid(patterns: &[String]) -> bool {
    compile_hidden_refs(patterns).is_ok()
}

/// Serve one terminal `stateless-connect git-upload-pack` helper session.
pub async fn serve<R, W>(
    reader: &mut R,
    writer: &mut W,
    store: &crab_storage::Store,
    prefix: &str,
    hidden_ref_patterns: &[String],
    fetch_policy: &FetchAdmissionPolicy,
    progress: bool,
    cancellation: &CancellationToken,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let repository = open_repository(store, prefix, cancellation).await?;
    let visibility = repository
        .visibility_index(cancellation)
        .await
        .map_err(remote_error)?;
    let visible_ref_names = visible_ref_names(&repository, hidden_ref_patterns)?;

    // The remote-helper positive response is one raw blank line. Only after
    // this acknowledgement does the stdio stream become protocol-v2 bytes.
    tracing::debug!(
        refs = visible_ref_names.len(),
        "starting protocol-v2 upload-pack session"
    );
    write_all_cancellable(writer, b"\n", cancellation).await?;
    flush_cancellable(writer, cancellation).await?;
    write_capabilities(writer, cancellation).await?;
    tracing::debug!("protocol-v2 capability advertisement sent");

    let mut negotiation_rounds = 0u32;
    loop {
        tracing::debug!("waiting for protocol-v2 command request");
        let request = match read_command_request(reader, cancellation).await {
            Ok(Some(request)) => request,
            Ok(None) => {
                tracing::debug!("protocol-v2 client closed the session");
                return Ok(());
            }
            Err(error) => return reject_protocol_request(writer, error, cancellation).await,
        };
        tracing::debug!(command = %request.command, args = request.args.len(), "protocol-v2 command request received");
        match request.command.as_str() {
            "ls-refs" => {
                let args = match parse_ls_refs(&request.args) {
                    Ok(args) => args,
                    Err(error) => {
                        return reject_protocol_request(writer, error, cancellation).await;
                    }
                };
                write_ls_refs(
                    writer,
                    &repository,
                    &visible_ref_names,
                    hidden_ref_patterns,
                    &args,
                    cancellation,
                )
                .await?;
            }
            "fetch" => {
                negotiation_rounds = negotiation_rounds.saturating_add(1);
                let fetch = match parse_fetch(&request.args) {
                    Ok(fetch) => fetch,
                    Err(error) => {
                        return reject_protocol_request(writer, error, cancellation).await;
                    }
                };
                if let Err(error) = validate_fetch_admission(
                    &repository,
                    &visibility,
                    &visible_ref_names,
                    &fetch,
                    fetch_policy,
                ) {
                    return reject_protocol_request(writer, error, cancellation).await;
                }
                if !fetch.done {
                    let common_haves = common_haves(&fetch, &visibility, &visible_ref_names);
                    if common_haves.is_empty() {
                        write_acknowledgments(writer, cancellation).await?;
                    } else {
                        write_fetch_response(
                            writer,
                            &repository,
                            &visibility,
                            &visible_ref_names,
                            &fetch,
                            negotiation_rounds,
                            progress,
                            Some(&common_haves),
                            cancellation,
                        )
                        .await?;
                    }
                    continue;
                }
                write_fetch_response(
                    writer,
                    &repository,
                    &visibility,
                    &visible_ref_names,
                    &fetch,
                    negotiation_rounds,
                    progress,
                    None,
                    cancellation,
                )
                .await?;
            }
            other => {
                let error =
                    CrabError::Protocol(format!("unsupported protocol-v2 command: {other}"));
                return reject_protocol_request(writer, error, cancellation).await;
            }
        }
    }
}

fn validate_fetch_admission(
    repository: &RemoteGitRepository,
    visibility: &GitVisibilityIndex,
    visible_ref_names: &[String],
    request: &FetchRequest,
    policy: &FetchAdmissionPolicy,
) -> Result<()> {
    let advertised_tips = repository
        .refs()
        .entries
        .iter()
        .filter(|reference| visible_ref_names.contains(&reference.name))
        .flat_map(|reference| [Some(reference.target), reference.peeled])
        .flatten()
        .collect::<HashSet<_>>();
    validate_fetch_wants(
        &advertised_tips,
        visibility,
        visible_ref_names,
        request,
        policy,
    )
}

fn validate_fetch_wants(
    advertised_tips: &HashSet<ObjectId>,
    visibility: &GitVisibilityIndex,
    visible_ref_names: &[String],
    request: &FetchRequest,
    policy: &FetchAdmissionPolicy,
) -> Result<()> {
    for want in &request.wants {
        if policy.allow_any_sha_in_want
            || (policy.allow_tip_sha_in_want && advertised_tips.contains(want))
            || (policy.allow_reachable_sha_in_want
                && visibility.contains_for_refs(
                    visible_ref_names.iter().map(String::as_str),
                    &want.to_hex().to_string(),
                ))
        {
            continue;
        }
        return Err(protocol(format!(
            "want {want} is denied by upload-pack policy"
        )));
    }
    Ok(())
}

pub(crate) async fn open_repository(
    store: &crab_storage::Store,
    prefix: &str,
    cancellation: &CancellationToken,
) -> Result<RemoteGitRepository> {
    let open = open_repository_snapshot(store, prefix, cancellation).await;
    let (observed_generation, required_generation) = match open {
        Ok(repository) => return Ok(repository),
        Err(RemoteGitError::RepositoryIndexing { observed, required }) => (observed, required),
        Err(error) => return Err(remote_error(error)),
    };

    // The manifest generation check distinguishes derived publication lag from
    // an active ref-journal transaction, which must remain unavailable.
    let repair_store = crate::storage::Store::from_storage(store.clone());
    let repair_layout = crate::storage::StoreLayout::new(repair_store.clone(), prefix.to_owned());
    if matches!(
        super::push::git_generation_owner_is_active(&repair_store, &repair_layout).await,
        Ok(true)
    ) {
        return Err(remote_error(RemoteGitError::RepositoryIndexing {
            observed: observed_generation,
            required: required_generation,
        }));
    }
    let repaired = super::push::repair_git_object_locator_if_current(
        &repair_store,
        &repair_layout,
        required_generation,
        LOCATOR_READ_REPAIR_LOCK_TTL,
        cancellation,
    )
    .await?;
    if !repaired {
        return Err(remote_error(RemoteGitError::RepositoryIndexing {
            observed: observed_generation,
            required: required_generation,
        }));
    }
    tracing::info!(
        observed_generation,
        required_generation,
        "repaired current Git locator before upload-pack admission"
    );

    open_repository_snapshot(store, prefix, cancellation)
        .await
        .map_err(remote_error)
}

async fn open_repository_snapshot(
    store: &crab_storage::Store,
    prefix: &str,
    cancellation: &CancellationToken,
) -> crab_remote_git::Result<RemoteGitRepository> {
    let bucket = store.bucket_identity();
    let provider = format!("{:?}:{}:{}", bucket.cloud, bucket.host, bucket.container);
    let identity = RepositoryIdentity::new(provider, prefix.to_owned(), 1)?;
    let layout = crab_storage::StoreLayout::new(store.clone(), prefix.to_owned());
    let runtime = Arc::new(RemoteGitRuntime::default());
    RemoteGitRepository::open(
        store.clone(),
        layout,
        identity,
        runtime,
        RepositoryOptions::default(),
        cancellation,
    )
    .await
}

pub(crate) fn visible_ref_names(
    repository: &RemoteGitRepository,
    hidden_ref_patterns: &[String],
) -> Result<Vec<String>> {
    let hidden = compile_hidden_refs(hidden_ref_patterns)?;
    Ok(repository
        .refs()
        .entries
        .iter()
        .filter(|entry| !hidden.is_match(&entry.name))
        .map(|entry| entry.name.clone())
        .collect())
}

fn compile_hidden_refs(patterns: &[String]) -> Result<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        let glob = globset::Glob::new(pattern)
            .map_err(|error| protocol(format!("invalid transfer.hideRefs pattern: {error}")))?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|error| protocol(format!("invalid transfer.hideRefs patterns: {error}")))
}

async fn write_capabilities<W: AsyncWrite + Unpin>(
    writer: &mut W,
    cancellation: &CancellationToken,
) -> Result<()> {
    write_data(writer, b"version 2\n", cancellation).await?;
    let agent = format!("agent=crab/{}\n", env!("CARGO_PKG_VERSION"));
    write_data(writer, agent.as_bytes(), cancellation).await?;
    write_data(writer, b"ls-refs=unborn\n", cancellation).await?;
    write_data(
        writer,
        b"fetch=shallow deepen deepen-relative filter thin-pack no-progress include-tag ofs-delta\n",
        cancellation,
    )
    .await?;
    write_flush(writer, cancellation).await
}

async fn read_command_request<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    cancellation: &CancellationToken,
) -> Result<Option<CommandRequest>> {
    let closed = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(CrabError::Cancelled),
        result = reader.fill_buf() => result?.is_empty(),
    };
    if closed {
        return Ok(None);
    }
    let first = read_packet(reader, cancellation).await?;
    let Packet::Data(first) = first else {
        return match first {
            Packet::Flush => Ok(None),
            Packet::Delimiter | Packet::ResponseEnd => {
                Err(protocol("request must start with command"))
            }
            Packet::Data(_) => Err(protocol("request packet was consumed twice")),
        };
    };
    let command = text_line(&first)?;
    let command = command
        .strip_prefix("command=")
        .ok_or_else(|| protocol("request is missing command="))?
        .to_owned();
    if command.is_empty()
        || !command
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(protocol("invalid protocol-v2 command name"));
    }

    let mut capabilities = Vec::new();
    let mut args = Vec::new();
    let mut in_args = false;
    let mut packet_count = 1usize;
    let mut byte_count = first.len();
    loop {
        if packet_count >= MAX_REQUEST_PACKETS || byte_count > MAX_REQUEST_BYTES {
            return Err(protocol("protocol-v2 request exceeds bounds"));
        }
        let packet = read_packet(reader, cancellation).await?;
        packet_count += 1;
        match packet {
            Packet::Data(data) => {
                byte_count = byte_count.saturating_add(data.len());
                if byte_count > MAX_REQUEST_BYTES {
                    return Err(protocol("protocol-v2 request exceeds bounds"));
                }
                let line = text_line(&data)?.to_owned();
                if in_args {
                    args.push(line);
                } else {
                    capabilities.push(line);
                }
            }
            Packet::Delimiter => {
                if in_args {
                    return Err(protocol("duplicate protocol-v2 request delimiter"));
                }
                in_args = true;
            }
            Packet::Flush => {
                if !in_args {
                    return Err(protocol("protocol-v2 request is missing its delimiter"));
                }
                validate_request_capabilities(&capabilities)?;
                return Ok(Some(CommandRequest {
                    command,
                    capabilities,
                    args,
                }));
            }
            Packet::ResponseEnd => return Err(protocol("response-end is not valid in a request")),
        }
    }
}

fn validate_request_capabilities(capabilities: &[String]) -> Result<()> {
    let mut seen_agent = false;
    for capability in capabilities {
        if let Some(agent) = capability.strip_prefix("agent=") {
            if seen_agent
                || agent.is_empty()
                || agent.bytes().any(|byte| byte <= b' ' || byte >= 0x7f)
            {
                return Err(protocol(
                    "invalid or duplicate protocol-v2 agent capability",
                ));
            }
            seen_agent = true;
            continue;
        }
        return Err(protocol(format!(
            "protocol-v2 request capability was not advertised: {capability}"
        )));
    }
    Ok(())
}

fn parse_ls_refs(args: &[String]) -> Result<LsRefsRequest> {
    let mut request = LsRefsRequest::default();
    let mut seen = HashSet::new();
    for arg in args {
        if let Some(prefix) = arg.strip_prefix("ref-prefix ") {
            if prefix.is_empty() || prefix.chars().any(char::is_whitespace) {
                return Err(protocol("ref-prefix must contain one non-empty value"));
            }
            request.prefixes.push(prefix.to_owned());
            continue;
        }
        match arg.as_str() {
            "symrefs" => request.symrefs = true,
            "peel" => request.peel = true,
            "unborn" => request.unborn = true,
            "ref-prefix" => return Err(protocol("ref-prefix is missing its value")),
            _ => return Err(protocol(format!("unsupported ls-refs argument: {arg}"))),
        }
        if !seen.insert(arg.clone()) {
            return Err(protocol(format!("duplicate ls-refs argument: {arg}")));
        }
    }
    Ok(request)
}

fn parse_fetch(args: &[String]) -> Result<FetchRequest> {
    let mut request = FetchRequest::default();
    let mut seen_single = HashSet::new();
    let mut filter_count = 0usize;
    for arg in args {
        if request.done {
            return Err(protocol("fetch arguments follow done"));
        }
        let (key, value) = arg
            .split_once(' ')
            .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
        match key {
            "want" => request.wants.push(parse_oid(
                value.ok_or_else(|| protocol("want is missing its object ID"))?,
            )?),
            "have" => request.haves.push(parse_oid(
                value.ok_or_else(|| protocol("have is missing its object ID"))?,
            )?),
            "shallow" => request.shallow.push(parse_oid(
                value.ok_or_else(|| protocol("shallow is missing its object ID"))?,
            )?),
            "deepen" => {
                let raw = value.ok_or_else(|| protocol("deepen is missing its depth"))?;
                let depth = raw
                    .parse::<u32>()
                    .map_err(|_| protocol("invalid deepen depth"))?;
                if depth == 0 || request.deepen.replace(depth).is_some() {
                    return Err(protocol("duplicate or zero deepen depth"));
                }
            }
            "deepen-relative" => {
                if value.is_some() || !seen_single.insert(key.to_owned()) {
                    return Err(protocol("duplicate deepen-relative argument"));
                }
                request.deepen_relative = true;
            }
            "thin-pack" => {
                if value.is_some() || !seen_single.insert(key.to_owned()) {
                    return Err(protocol("duplicate thin-pack argument"));
                }
                request.thin_pack = true;
            }
            "no-progress" => {
                if value.is_some() || !seen_single.insert(key.to_owned()) {
                    return Err(protocol("duplicate no-progress argument"));
                }
                request.no_progress = true;
            }
            "include-tag" => {
                if value.is_some() || !seen_single.insert(key.to_owned()) {
                    return Err(protocol("duplicate include-tag argument"));
                }
                request.include_tags = true;
            }
            "ofs-delta" => {
                if value.is_some() || !seen_single.insert(key.to_owned()) {
                    return Err(protocol("duplicate ofs-delta argument"));
                }
                request.ofs_delta = true;
            }
            "sideband-all" => return Err(protocol("sideband-all was not advertised")),
            "done" => {
                if value.is_some() || !seen_single.insert(key.to_owned()) {
                    return Err(protocol("duplicate done argument"));
                }
                request.done = true;
            }
            "filter" => {
                filter_count = filter_count.saturating_add(1);
                if filter_count > 16 {
                    return Err(protocol("fetch request contains too many filters"));
                }
                let value = value.ok_or_else(|| protocol("filter is missing its specification"))?;
                let parsed =
                    parse_upload_pack_filter(value).map_err(|error| protocol(error.to_string()))?;
                let previous = std::mem::take(&mut request.filter);
                request.filter = combine_upload_pack_filters(
                    [previous, parsed]
                        .into_iter()
                        .filter(|filter| !matches!(filter, UploadPackFilter::None))
                        .collect(),
                );
            }
            "deepen-since" | "deepen-not" | "want-ref" | "packfile-uris" | "wait-for-done"
            | "server-option" => {
                return Err(protocol(format!("unsupported fetch argument: {key}")));
            }
            _ => return Err(protocol(format!("unsupported fetch argument: {arg}"))),
        }
    }
    if request.wants.is_empty() {
        return Err(protocol("fetch request contains no wants"));
    }
    if request.deepen_relative && request.deepen.is_none() {
        return Err(protocol("deepen-relative requires deepen"));
    }
    Ok(request)
}

async fn write_ls_refs<W: AsyncWrite + Unpin>(
    writer: &mut W,
    repository: &RemoteGitRepository,
    visible_ref_names: &[String],
    hidden_ref_patterns: &[String],
    request: &LsRefsRequest,
    cancellation: &CancellationToken,
) -> Result<()> {
    let matches_prefix = |name: &str| {
        request.prefixes.is_empty()
            || request
                .prefixes
                .iter()
                .any(|prefix| name.starts_with(prefix))
    };
    if request.symrefs
        && repository.refs().head.as_ref().is_some_and(|head| {
            visible_ref_names.iter().any(|name| name == &head.name)
                && (matches_prefix("HEAD") || matches_prefix(&head.name))
        })
    {
        let head = repository
            .refs()
            .head
            .as_ref()
            .ok_or_else(|| protocol("missing HEAD"))?;
        let line = format!("{} HEAD symref-target:{}\n", head.target, head.name);
        write_data(writer, line.as_bytes(), cancellation).await?;
    }
    let hidden = compile_hidden_refs(hidden_ref_patterns)?;
    if request.unborn
        && let Some(target) = repository.refs().unborn_head.as_deref()
        && !hidden.is_match(target)
        && (matches_prefix("HEAD") || matches_prefix(target))
    {
        let line = format!("unborn HEAD symref-target:{target}\n");
        write_data(writer, line.as_bytes(), cancellation).await?;
    }
    for reference in &repository.refs().entries {
        if !visible_ref_names.iter().any(|name| name == &reference.name)
            || !matches_prefix(&reference.name)
        {
            continue;
        }
        let mut line = format!("{} {}", reference.target, reference.name);
        if request.peel
            && let Some(peeled) = reference.peeled
        {
            let _ = write!(line, " peeled:{peeled}");
        }
        line.push('\n');
        write_data(writer, line.as_bytes(), cancellation).await?;
    }
    write_flush(writer, cancellation).await?;
    write_response_end(writer, cancellation).await
}

fn common_haves(
    request: &FetchRequest,
    visibility: &GitVisibilityIndex,
    visible_ref_names: &[String],
) -> Vec<ObjectId> {
    request
        .haves
        .iter()
        .filter(|have| {
            visibility.contains_for_refs(
                visible_ref_names.iter().map(String::as_str),
                &have.to_hex().to_string(),
            )
        })
        .copied()
        .collect()
}

async fn write_acknowledgments<W: AsyncWrite + Unpin>(
    writer: &mut W,
    cancellation: &CancellationToken,
) -> Result<()> {
    write_data(writer, b"acknowledgments\n", cancellation).await?;
    write_data(writer, b"NAK\n", cancellation).await?;
    write_flush(writer, cancellation).await?;
    write_response_end(writer, cancellation).await
}

#[expect(
    clippy::too_many_arguments,
    reason = "the response boundary carries the pinned repository, proof, negotiation, and wire state"
)]
async fn write_fetch_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    repository: &RemoteGitRepository,
    visibility: &GitVisibilityIndex,
    visible_ref_names: &[String],
    request: &FetchRequest,
    negotiation_rounds: u32,
    progress: bool,
    acknowledged_haves: Option<&[ObjectId]>,
    cancellation: &CancellationToken,
) -> Result<()> {
    let started = std::time::Instant::now();
    let request_class = if is_likely_lazy_fetch(repository, request) {
        "lazy"
    } else {
        "fetch"
    };
    let semantic_request = UploadPackRequest {
        wants: request.wants.clone(),
        haves: request.haves.clone(),
        shallow: request.shallow.clone(),
        deepen: request.deepen,
        deepen_relative: request.deepen_relative,
        include_tags: request.include_tags,
        filter: request.filter.clone(),
    };
    let plan = match plan_upload_pack(
        repository,
        visibility,
        visible_ref_names,
        &semantic_request,
        cancellation,
    )
    .await
    {
        Ok(plan) => plan,
        Err(error) => {
            let authorization_rejected = matches!(&error, crab_read::ReadError::UnauthorizedObject);
            tracing::warn!(
                protocol_version = 2,
                request_class,
                negotiation_rounds,
                authorization_rejected,
                failure_code = if authorization_rejected {
                    "authorization"
                } else {
                    "request"
                },
                "protocol-v2 upload-pack request rejected"
            );
            let error = CrabError::Protocol(format!("upload-pack request rejected: {error}"));
            return reject_protocol_request(writer, error, cancellation).await;
        }
    };

    let visible_object_count =
        visibility.object_count_for_refs(visible_ref_names.iter().map(String::as_str));
    let filter = request.filter.canonical_spec();
    tracing::info!(
        protocol_version = 2,
        request_class,
        negotiation_rounds,
        canonical_filter = %filter,
        haves = request.haves.len(),
        common_haves = plan.common_haves.len(),
        shallow = request.shallow.len(),
        deepen = request.deepen,
        deepen_relative = request.deepen_relative,
        include_tags = request.include_tags,
        visible_objects = visible_object_count,
        planned_objects = plan.object_ids.len(),
        omitted_objects = visible_object_count.saturating_sub(plan.object_ids.len()),
        required_bases = plan.required_bases.len(),
        "protocol-v2 upload-pack plan selected"
    );

    if let Some(acknowledged_haves) = acknowledged_haves {
        write_data(writer, b"acknowledgments\n", cancellation).await?;
        for have in acknowledged_haves {
            let line = format!("ACK {have}\n");
            write_data(writer, line.as_bytes(), cancellation).await?;
        }
        write_data(writer, b"ready\n", cancellation).await?;
        write_delimiter(writer, cancellation).await?;
    }
    if !plan.shallow.is_empty() || !plan.unshallow.is_empty() {
        write_data(writer, b"shallow-info\n", cancellation).await?;
        for oid in &plan.shallow {
            let line = format!("shallow {oid}\n");
            write_data(writer, line.as_bytes(), cancellation).await?;
        }
        for oid in &plan.unshallow {
            let line = format!("unshallow {oid}\n");
            write_data(writer, line.as_bytes(), cancellation).await?;
        }
        write_delimiter(writer, cancellation).await?;
    }
    write_data(writer, b"packfile\n", cancellation).await?;
    if progress && !request.no_progress {
        write_packet(writer, b"counting objects\n", Some(2), cancellation).await?;
    }
    let pack = match repository
        .generate_pack(&plan.object_ids, cancellation)
        .await
    {
        Ok(pack) => pack,
        Err(error) => {
            write_packet(writer, error.to_string().as_bytes(), Some(3), cancellation).await?;
            write_flush(writer, cancellation).await?;
            write_response_end(writer, cancellation).await?;
            return Err(CrabError::Protocol(format!(
                "upload-pack generation failed: {error}"
            )));
        }
    };
    tracing::info!(
        protocol_version = 2,
        request_class,
        negotiation_rounds,
        canonical_filter = %filter,
        planned_objects = pack.object_count(),
        reconstructed_objects = pack.object_count(),
        transferred_bytes = pack.size(),
        latency_ms = started.elapsed().as_millis() as u64,
        lazy_fetch_latency_ms =
            (request_class == "lazy").then_some(started.elapsed().as_millis() as u64),
        "protocol-v2 upload-pack pack generated"
    );
    pack.write_sideband(writer, cancellation)
        .await
        .map_err(remote_error)?;
    write_flush(writer, cancellation).await?;
    write_response_end(writer, cancellation).await
}

fn is_likely_lazy_fetch(repository: &RemoteGitRepository, request: &FetchRequest) -> bool {
    request.wants.iter().all(|want| {
        !repository
            .refs()
            .entries
            .iter()
            .any(|reference| reference.target == *want || reference.peeled == Some(*want))
    })
}

async fn read_packet<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    cancellation: &CancellationToken,
) -> Result<Packet> {
    let mut header = [0u8; 4];
    read_exact_cancellable(reader, &mut header, cancellation).await?;
    let length = u16::from_str_radix(
        std::str::from_utf8(&header).map_err(|_| protocol("packet length is not ASCII"))?,
        16,
    )
    .map_err(|_| protocol("packet length is not hexadecimal"))? as usize;
    match length {
        0 => Ok(Packet::Flush),
        1 => Ok(Packet::Delimiter),
        2 => Ok(Packet::ResponseEnd),
        3 => Err(protocol("invalid packet-line length 0003")),
        length if !(4..=MAX_PACKET_BYTES).contains(&length) => {
            Err(protocol("packet-line length exceeds the protocol bound"))
        }
        length => {
            let decoded = gix_packetline::decode::hex_prefix(&header)
                .map_err(|_| protocol("invalid packet-line header"))?;
            let PacketLineOrWantedSize::Wanted(wanted) = decoded else {
                return Err(protocol("packet-line header changed while decoding"));
            };
            if usize::from(wanted) != length - 4 {
                return Err(protocol("packet-line length changed while decoding"));
            }
            let mut data = vec![0u8; length - 4];
            read_exact_cancellable(reader, &mut data, cancellation).await?;
            if !matches!(
                gix_packetline::decode::to_data_line(&data),
                Ok(PacketLineRef::Data(_))
            ) {
                return Err(protocol("packet-line data exceeds the protocol bound"));
            }
            Ok(Packet::Data(data))
        }
    }
}

fn text_line(data: &[u8]) -> Result<&str> {
    let data = data.strip_suffix(b"\n").unwrap_or(data);
    if data.contains(&b'\n') {
        return Err(protocol("protocol-v2 line contains an embedded LF"));
    }
    std::str::from_utf8(data).map_err(|_| protocol("packet-line data is not UTF-8"))
}

fn parse_oid(value: &str) -> Result<ObjectId> {
    if value.len() != 40 {
        return Err(protocol("object ID must contain exactly 40 hex digits"));
    }
    ObjectId::from_hex(value.as_bytes()).map_err(|_| protocol("invalid object ID"))
}

async fn write_data<W: AsyncWrite + Unpin>(
    writer: &mut W,
    data: &[u8],
    cancellation: &CancellationToken,
) -> Result<()> {
    write_packet(writer, data, None, cancellation).await
}

fn protocol_error_payload(message: &str) -> Vec<u8> {
    const PREFIX: &[u8] = b"ERR ";
    const NEWLINE_BYTES: usize = 1;
    let maximum = MAX_PACKET_BYTES - 4;
    let mut payload = Vec::with_capacity(message.len().min(maximum));
    payload.extend_from_slice(PREFIX);
    for character in message.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        let mut encoded = [0; 4];
        let encoded = character.encode_utf8(&mut encoded).as_bytes();
        if payload
            .len()
            .saturating_add(encoded.len())
            .saturating_add(NEWLINE_BYTES)
            > maximum
        {
            break;
        }
        payload.extend_from_slice(encoded);
    }
    payload.push(b'\n');
    payload
}

async fn write_protocol_error<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &str,
    cancellation: &CancellationToken,
) -> Result<()> {
    write_data(writer, &protocol_error_payload(message), cancellation).await?;
    flush_cancellable(writer, cancellation).await
}

async fn reject_protocol_request<W: AsyncWrite + Unpin>(
    writer: &mut W,
    error: CrabError,
    cancellation: &CancellationToken,
) -> Result<()> {
    if let Err(write_error) = write_protocol_error(writer, &error.to_string(), cancellation).await {
        tracing::warn!(error = %write_error, "failed to write protocol-v2 ERR packet");
    }
    Err(error)
}

async fn write_packet<W: AsyncWrite + Unpin>(
    writer: &mut W,
    data: &[u8],
    band: Option<u8>,
    cancellation: &CancellationToken,
) -> Result<()> {
    let payload_len = data.len() + usize::from(band.is_some());
    let length = payload_len.saturating_add(4);
    if length > MAX_PACKET_BYTES {
        return Err(protocol("packet-line payload exceeds the protocol bound"));
    }
    write_all_cancellable(writer, format!("{length:04x}").as_bytes(), cancellation).await?;
    if let Some(band) = band {
        write_all_cancellable(writer, &[band], cancellation).await?;
    }
    write_all_cancellable(writer, data, cancellation).await?;
    Ok(())
}

async fn write_flush<W: AsyncWrite + Unpin>(
    writer: &mut W,
    cancellation: &CancellationToken,
) -> Result<()> {
    write_all_cancellable(writer, b"0000", cancellation).await?;
    flush_cancellable(writer, cancellation).await?;
    Ok(())
}

async fn write_delimiter<W: AsyncWrite + Unpin>(
    writer: &mut W,
    cancellation: &CancellationToken,
) -> Result<()> {
    write_all_cancellable(writer, b"0001", cancellation).await?;
    Ok(())
}

async fn write_response_end<W: AsyncWrite + Unpin>(
    writer: &mut W,
    cancellation: &CancellationToken,
) -> Result<()> {
    write_all_cancellable(writer, b"0002", cancellation).await?;
    flush_cancellable(writer, cancellation).await?;
    Ok(())
}

async fn read_exact_cancellable<R: AsyncRead + Unpin>(
    reader: &mut R,
    bytes: &mut [u8],
    cancellation: &CancellationToken,
) -> Result<()> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(CrabError::Cancelled),
        result = reader.read_exact(bytes) => {
            result.map(|_| ()).map_err(Into::into)
        }
    }
}

async fn write_all_cancellable<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
    cancellation: &CancellationToken,
) -> Result<()> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(CrabError::Cancelled),
        result = writer.write_all(bytes) => result.map_err(Into::into),
    }
}

async fn flush_cancellable<W: AsyncWrite + Unpin>(
    writer: &mut W,
    cancellation: &CancellationToken,
) -> Result<()> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(CrabError::Cancelled),
        result = writer.flush() => result.map_err(Into::into),
    }
}

fn remote_error(error: impl std::fmt::Display) -> CrabError {
    CrabError::Protocol(format!("remote Git upload-pack error: {error}"))
}

fn protocol(message: impl Into<String>) -> CrabError {
    CrabError::Protocol(message.into())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use tokio::io::{AsyncReadExt, BufReader};

    use super::*;

    fn packet(data: &[u8]) -> Vec<u8> {
        let length = data.len() + 4;
        let mut packet = format!("{length:04x}").into_bytes();
        packet.extend_from_slice(data);
        packet
    }

    #[test]
    fn parses_object_id_strictly() {
        assert!(parse_oid(&"a".repeat(40)).is_ok());
        assert!(parse_oid(&"a".repeat(39)).is_err());
        assert!(parse_oid(&format!("{}z", "a".repeat(39))).is_err());
    }

    #[test]
    fn visibility_mismatch_enters_the_bounded_repair_path() {
        let error = RemoteGitError::RepositoryState {
            reason: crab_remote_git::RepositoryStateError::VisibilityProofMismatch,
        };

        assert!(visibility_index_needs_repair(&error));
    }

    #[test]
    fn accepts_optional_terminal_line_feed_and_rejects_embedded_lf() {
        assert_eq!(
            text_line(b"want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .expect("a packet line without a terminal line feed should be accepted"),
            "want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert!(text_line(b"want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nextra\n").is_err());
        assert_eq!(
            text_line(b"want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n")
                .expect("a terminal line feed should be accepted"),
            "want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[test]
    fn protocol_error_payload_uses_err_framing_and_sanitizes_controls() {
        assert_eq!(
            protocol_error_payload("request\nfailed\tcleanly"),
            b"ERR request failed cleanly\n"
        );
    }

    #[test]
    fn protocol_error_payload_respects_the_packet_line_bound() {
        let payload = protocol_error_payload(&"x".repeat(MAX_PACKET_BYTES));

        assert_eq!(payload.len() + 4, MAX_PACKET_BYTES);
        assert!(payload.starts_with(b"ERR "));
        assert!(payload.ends_with(b"\n"));
    }

    #[tokio::test]
    async fn rejected_request_writes_one_terminal_err_packet() {
        let mut writer = Vec::new();
        let cancellation = CancellationToken::new();
        let error = protocol("request rejected");
        let expected = packet(&protocol_error_payload(&error.to_string()));

        reject_protocol_request(&mut writer, error, &cancellation)
            .await
            .expect_err("the semantic request error must remain terminal");

        assert_eq!(writer, expected);
    }

    #[test]
    fn combines_repeated_fetch_filters() {
        let request = parse_fetch(&[
            "want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            "filter blob:none".to_owned(),
            "filter tree:1".to_owned(),
        ])
        .expect("repeated filters should use intersection semantics");
        assert_eq!(request.filter.canonical_spec(), "combine:blob:none+tree:1");
    }

    #[test]
    fn accepts_supported_filter_grammar_before_planning() {
        for filter in [
            "blob:limit=1m",
            "tree:1",
            "object:type=blob",
            "sparse:oid=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "combine:blob%3Anone+tree%3A1",
        ] {
            parse_fetch(&[
                format!("want {}", "a".repeat(40)),
                format!("filter {filter}"),
            ])
            .expect("supported filters must parse before planning");
        }
    }

    #[test]
    fn rejects_unsupported_filter_before_planning() {
        let error = parse_fetch(&[
            format!("want {}", "a".repeat(40)),
            "filter blob:depth=1".to_owned(),
        ])
        .expect_err("unsupported filters must fail in the wire parser");
        assert!(error.to_string().contains("unsupported filter"));
    }

    #[test]
    fn parses_ref_prefix_arguments_with_inline_values() {
        let request = parse_ls_refs(&[
            "symrefs".to_owned(),
            "ref-prefix refs/heads/".to_owned(),
            "ref-prefix refs/tags/".to_owned(),
        ])
        .expect("inline ref-prefix values should parse");
        assert!(request.symrefs);
        assert_eq!(request.prefixes, ["refs/heads/", "refs/tags/"]);
    }

    #[tokio::test]
    async fn command_request_requires_capability_delimiter() {
        let mut bytes = packet(b"command=ls-refs\n");
        bytes.extend_from_slice(b"0000");
        let mut reader = BufReader::new(Cursor::new(bytes));
        let cancellation = CancellationToken::new();

        let error = read_command_request(&mut reader, &cancellation)
            .await
            .expect_err("missing delimiter must be rejected");
        assert!(error.to_string().contains("missing its delimiter"));
    }

    #[tokio::test]
    async fn closed_v2_session_is_not_an_early_eof_error() {
        let mut reader = BufReader::new(Cursor::new(Vec::<u8>::new()));
        let cancellation = CancellationToken::new();
        assert!(
            read_command_request(&mut reader, &cancellation)
                .await
                .expect("clean close should be accepted")
                .is_none()
        );
    }

    #[tokio::test]
    async fn command_request_keeps_pkt_line_bytes_after_terminal_request() {
        let mut bytes = packet(b"command=ls-refs\n");
        bytes.extend_from_slice(b"0001");
        bytes.extend_from_slice(&packet(b"symrefs\n"));
        bytes.extend_from_slice(b"0000tail");
        let mut reader = BufReader::new(Cursor::new(bytes));
        let cancellation = CancellationToken::new();

        let request = read_command_request(&mut reader, &cancellation)
            .await
            .expect("request should parse")
            .expect("request should be present");
        assert_eq!(request.command, "ls-refs");
        assert!(request.capabilities.is_empty());
        assert_eq!(request.args, ["symrefs"]);

        let mut tail = Vec::new();
        reader
            .read_to_end(&mut tail)
            .await
            .expect("tail should remain readable");
        assert_eq!(tail, b"tail");
    }

    #[tokio::test]
    async fn request_byte_limit_is_enforced_before_flush() {
        let mut bytes = packet(b"command=ls-refs\n");
        bytes.extend_from_slice(b"0001");
        let oversized = vec![b'a'; MAX_PACKET_BYTES - 4];
        for _ in 0..65 {
            bytes.extend_from_slice(&packet(&oversized));
        }
        bytes.extend_from_slice(b"0000");
        let mut reader = BufReader::new(Cursor::new(bytes));
        let cancellation = CancellationToken::new();

        let error = read_command_request(&mut reader, &cancellation)
            .await
            .expect_err("oversized requests must fail before the flush packet");
        assert!(error.to_string().contains("exceeds bounds"));
    }

    #[test]
    fn fetch_done_must_be_the_final_argument() {
        let error = parse_fetch(&[
            format!("want {}", "a".repeat(40)),
            "done".to_owned(),
            "no-progress".to_owned(),
        ])
        .expect_err("arguments after done must be rejected");
        assert!(error.to_string().contains("follow done"));
    }

    #[test]
    fn relative_deepen_requires_a_depth() {
        let error = parse_fetch(&[
            format!("want {}", "a".repeat(40)),
            "deepen-relative".to_owned(),
        ])
        .expect_err("relative deepen without depth must be rejected");
        assert!(error.to_string().contains("requires deepen"));
    }

    #[test]
    fn invalid_hidden_ref_glob_fails_closed() {
        let error = compile_hidden_refs(&["[".to_owned()])
            .expect_err("invalid hidden-ref patterns must reject the session");
        assert!(
            error
                .to_string()
                .contains("invalid transfer.hideRefs pattern")
        );
    }

    #[test]
    fn reachable_non_tip_want_is_denied_by_default() {
        let ancestor = parse_oid(&"a".repeat(40)).expect("ancestor oid");
        let tip = parse_oid(&"b".repeat(40)).expect("tip oid");
        let request = FetchRequest {
            wants: vec![ancestor],
            ..FetchRequest::default()
        };
        let visible_refs = vec!["refs/heads/main".to_owned()];
        let visibility = GitVisibilityIndex::new(
            1,
            "c".repeat(64),
            "d".repeat(64),
            std::collections::BTreeMap::from([(
                visible_refs[0].clone(),
                vec![ancestor.to_string(), tip.to_string()],
            )]),
        );

        let error = validate_fetch_wants(
            &HashSet::from([tip]),
            &visibility,
            &visible_refs,
            &request,
            &FetchAdmissionPolicy::default(),
        )
        .expect_err("a reachable non-tip want must be denied without opt-in");

        assert!(error.to_string().contains("denied by upload-pack policy"));
    }

    #[test]
    fn reachable_non_tip_want_is_accepted_when_enabled() {
        let ancestor = parse_oid(&"a".repeat(40)).expect("ancestor oid");
        let tip = parse_oid(&"b".repeat(40)).expect("tip oid");
        let request = FetchRequest {
            wants: vec![ancestor],
            ..FetchRequest::default()
        };
        let visible_refs = vec!["refs/heads/main".to_owned()];
        let visibility = GitVisibilityIndex::new(
            1,
            "c".repeat(64),
            "d".repeat(64),
            std::collections::BTreeMap::from([(
                visible_refs[0].clone(),
                vec![ancestor.to_string(), tip.to_string()],
            )]),
        );
        let policy = FetchAdmissionPolicy {
            allow_reachable_sha_in_want: true,
            ..FetchAdmissionPolicy::default()
        };

        let result = validate_fetch_wants(
            &HashSet::from([tip]),
            &visibility,
            &visible_refs,
            &request,
            &policy,
        );

        assert!(result.is_ok());
    }
}
