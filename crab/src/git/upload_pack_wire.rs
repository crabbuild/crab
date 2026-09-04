//! Git protocol-v2 upload-pack over the helper's already-authenticated stdio.
//!
//! This module composes shared wire framing with helper command semantics and
//! product-level admission repair. Generation pinning, object traversal, and
//! pack production stay in the shared read and remote-git crates.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use crab_metadata::git_visibility::GitVisibilityIndex;
#[cfg(test)]
use crab_read::plan_upload_pack;
use crab_read::upload_pack_wire::{
    FetchRequest, LsRefsRequest, MAX_PACKET_BYTES, flush_cancellable, parse_fetch, parse_ls_refs,
    read_command_request, write_all_cancellable, write_delimiter, write_flush, write_packet,
    write_response_end,
};
use crab_read::{
    FetchAdmissionPolicy, UPLOAD_PACK_MAX_DURATION, UploadPackFilter, UploadPackRequest,
    plan_upload_pack_catalog, upload_pack_repository_options,
};
use crab_remote_git::{
    Error as RemoteGitError, GitCatalogVisibilityIndex, RemoteGitRepository, RemoteGitRuntime,
    RepositoryIdentity,
};
use gix_hash::ObjectId;
use rand::Rng as _;
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result};
use crate::storage::retry::{RetryClass, retry_class};

const LOCATOR_READ_REPAIR_LOCK_TTL: Duration = Duration::from_secs(30);
const LOCATOR_READ_RETRY_LIMIT: usize = 120;
const LOCATOR_READ_RETRY_BASE: Duration = Duration::from_millis(100);
const LOCATOR_READ_RETRY_CAP: Duration = Duration::from_secs(2);
const READ_ADMISSION_WAIT: Duration = UPLOAD_PACK_MAX_DURATION;
const READ_ADMISSION_RETRY_BASE: Duration = Duration::from_millis(50);
const READ_ADMISSION_RETRY_CAP: Duration = Duration::from_secs(2);
#[cfg(test)]
const MIB: u64 = 1024 * 1024;

struct ObjectStoreGeneratedPackLeaseProvider {
    store: Arc<dyn object_store::ObjectStore>,
    prefix: String,
}

struct ObjectStoreGeneratedPackLease {
    lock: crab_coordination::PushLock,
    // Cache waiters release the session lease before polling; the producer
    // reacquires one here so only actual pack generation consumes read capacity.
    read_admission: crab_coordination::ReadAdmissionTicket,
}

struct ObjectStoreGeneratedPackReadPermit {
    read_admission: crab_coordination::ReadAdmissionTicket,
}

impl crab_remote_git::GeneratedPackLease for ObjectStoreGeneratedPackReadPermit {
    fn renew(
        &mut self,
    ) -> futures_util::future::BoxFuture<
        '_,
        std::result::Result<(), crab_remote_git::GeneratedPackLeaseError>,
    > {
        Box::pin(async move {
            if let Err(error) = self.read_admission.renew().await {
                // Admission bounds load but does not protect artifact correctness.
                // A transient renewal failure must not discard a verified pack.
                tracing::warn!(
                    error = %error,
                    "generated-pack cache read admission renewal failed; continuing read"
                );
            }
            Ok(())
        })
    }

    fn release(
        self: Box<Self>,
    ) -> futures_util::future::BoxFuture<
        'static,
        std::result::Result<(), crab_remote_git::GeneratedPackLeaseError>,
    > {
        Box::pin(async move {
            if let Err(error) = self.read_admission.release().await {
                // Slots are crash-reclaimable, so release failure is load leakage
                // until TTL expiry rather than a reason to fail a completed fetch.
                tracing::warn!(
                    error = %error,
                    "generated-pack cache read admission release failed"
                );
            }
            Ok(())
        })
    }
}

enum VisibilityRequirement {
    #[cfg(test)]
    Materialized,
    Catalog,
}

enum UploadPackVisibilityProof {
    #[cfg(test)]
    Materialized(GitVisibilityIndex),
    Catalog(GitCatalogVisibilityIndex),
}

impl UploadPackVisibilityProof {
    fn as_catalog(&self) -> Option<&GitCatalogVisibilityIndex> {
        match self {
            Self::Catalog(visibility) => Some(visibility),
            #[cfg(test)]
            Self::Materialized(_) => None,
        }
    }

    fn into_catalog(self) -> Result<GitCatalogVisibilityIndex> {
        match self {
            Self::Catalog(visibility) => Ok(visibility),
            #[cfg(test)]
            Self::Materialized(_) => Err(CrabError::Internal(
                "catalog upload-pack proof was not returned".to_owned(),
            )),
        }
    }

    fn object_count_for_refs(&self, refs: &[String]) -> usize {
        match self {
            #[cfg(test)]
            Self::Materialized(visibility) => {
                visibility.object_count_for_refs(refs.iter().map(String::as_str))
            }
            Self::Catalog(visibility) => {
                visibility.object_count_for_refs(refs.iter().map(String::as_str))
            }
        }
    }

    fn authorization_digest_for_refs(&self, refs: &[String]) -> [u8; 32] {
        match self {
            #[cfg(test)]
            Self::Materialized(visibility) => {
                visibility.authorization_digest_for_refs(refs.iter().map(String::as_str))
            }
            Self::Catalog(visibility) => {
                visibility.authorization_digest_for_refs(refs.iter().map(String::as_str))
            }
        }
    }
}

impl crab_remote_git::GeneratedPackLeaseProvider for ObjectStoreGeneratedPackLeaseProvider {
    fn try_acquire<'a>(
        &'a self,
        resource: &'a str,
        ttl: Duration,
    ) -> futures_util::future::BoxFuture<
        'a,
        std::result::Result<
            crab_remote_git::GeneratedPackLeaseAttempt,
            crab_remote_git::GeneratedPackLeaseError,
        >,
    > {
        Box::pin(async move {
            let mut context =
                crab_coordination::PushLockAcquireContext::new(Arc::clone(&self.store));
            match context
                .try_acquire_internal(&self.prefix, resource, ttl)
                .await
            {
                Ok(lock) => {
                    let read_admission = acquire_read_admission(
                        &self.store,
                        &self.prefix,
                        &CancellationToken::new(),
                    )
                    .await;
                    match read_admission {
                        Ok(read_admission) => {
                            Ok(crab_remote_git::GeneratedPackLeaseAttempt::Acquired(
                                Box::new(ObjectStoreGeneratedPackLease {
                                    lock,
                                    read_admission,
                                }),
                            ))
                        }
                        Err(error) => {
                            if let Err(release_error) = lock.release().await {
                                tracing::warn!(
                                    error = %release_error,
                                    "generated-pack lease release failed after read admission failure"
                                );
                            }
                            Err(crab_remote_git::GeneratedPackLeaseError::new(error))
                        }
                    }
                }
                Err(crab_coordination::CoordinationError::PushLockHeld {
                    expires_at_unix, ..
                }) => {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let retry_after = expires_at_unix
                        .map(|expires_at| Duration::from_secs(expires_at.saturating_sub(now)))
                        .unwrap_or(ttl)
                        .min(ttl);
                    Ok(crab_remote_git::GeneratedPackLeaseAttempt::Held { retry_after })
                }
                Err(error) => Err(crab_remote_git::GeneratedPackLeaseError::new(error)),
            }
        })
    }

    fn acquire_read<'a>(
        &'a self,
        cancellation: &'a CancellationToken,
        max_wait: Duration,
    ) -> futures_util::future::BoxFuture<
        'a,
        std::result::Result<
            Box<dyn crab_remote_git::GeneratedPackLease>,
            crab_remote_git::GeneratedPackLeaseError,
        >,
    > {
        Box::pin(async move {
            let read_admission =
                acquire_read_admission_with_wait(&self.store, &self.prefix, cancellation, max_wait)
                    .await
                    .map_err(crab_remote_git::GeneratedPackLeaseError::new)?;
            Ok(
                Box::new(ObjectStoreGeneratedPackReadPermit { read_admission })
                    as Box<dyn crab_remote_git::GeneratedPackLease>,
            )
        })
    }
}

impl crab_remote_git::GeneratedPackLease for ObjectStoreGeneratedPackLease {
    fn renew(
        &mut self,
    ) -> futures_util::future::BoxFuture<
        '_,
        std::result::Result<(), crab_remote_git::GeneratedPackLeaseError>,
    > {
        Box::pin(async move {
            self.read_admission
                .renew()
                .await
                .map_err(crab_remote_git::GeneratedPackLeaseError::new)?;
            self.lock
                .renew()
                .await
                .map_err(crab_remote_git::GeneratedPackLeaseError::new)
        })
    }

    fn release(
        self: Box<Self>,
    ) -> futures_util::future::BoxFuture<
        'static,
        std::result::Result<(), crab_remote_git::GeneratedPackLeaseError>,
    > {
        Box::pin(async move {
            let read_result = self.read_admission.release().await;
            let lock_result = self.lock.release().await;
            match (read_result, lock_result) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(error), _) => Err(crab_remote_git::GeneratedPackLeaseError::new(error)),
                (Ok(()), Err(error)) => Err(crab_remote_git::GeneratedPackLeaseError::new(error)),
            }
        })
    }
}

/// Check whether protocol-v2 admission has usable generation evidence.
pub async fn snapshot_available(
    store: &crab_storage::Store,
    prefix: &str,
    cancellation: &CancellationToken,
) -> bool {
    // A v2 session cannot fall back after the terminal handoff. Only advertise
    // it when admission has current visibility evidence to use or migrate.
    let Ok(stable) = capability_snapshot_is_stable(store, prefix, cancellation).await else {
        return false;
    };
    stable
}

async fn capability_snapshot_is_stable(
    store: &crab_storage::Store,
    prefix: &str,
    cancellation: &CancellationToken,
) -> crab_remote_git::Result<bool> {
    // Capability discovery proves that terminal admission can establish an
    // exact snapshot. `serve_admitted` performs any bounded repair before its
    // positive handoff, so requiring derived catalogs here would strand v2.
    let layout = crab_storage::StoreLayout::new(store.clone(), prefix.to_owned());
    let (manifest, _) = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(RemoteGitError::Cancelled),
        result = crab_metadata::manifest_store::read_manifest(store, &layout) => result.map_err(|source| RemoteGitError::Manifest { source })?,
    };
    let repair_store = crate::storage::Store::from_storage(store.clone());
    let repair_layout = crate::storage::StoreLayout::new(repair_store.clone(), prefix.to_owned());
    let active_marker_present = store
        .list_prefix_bounded(&layout.ref_journal_active_prefix(), 1)
        .await?
        .is_none_or(|objects| !objects.is_empty());
    let owner_active =
        match super::push::git_generation_owner_is_active(&repair_store, &repair_layout).await {
            Ok(active) => active,
            Err(error) => {
                tracing::warn!(%error, "generation owner probe failed during capability discovery");
                return Ok(false);
            }
        };
    if owner_active {
        tracing::debug!(
            active_marker_present,
            "protocol-v2 capability withheld while generation-owner admission is active"
        );
        return Ok(false);
    }
    if manifest.refs.is_empty() {
        return Ok(true);
    }
    match super::push::git_visibility_proof_available_for_manifest(
        &repair_store,
        &repair_layout,
        &manifest,
    )
    .await
    {
        Ok(available) => Ok(available),
        Err(error) => {
            tracing::warn!(%error, "visibility proof probe failed during capability discovery");
            Ok(false)
        }
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
    let started = std::time::Instant::now();
    let read_admission = acquire_read_admission(store.inner(), prefix, cancellation).await?;
    let result = serve_with_read_admission(
        read_admission,
        reader,
        writer,
        store,
        prefix,
        hidden_ref_patterns,
        fetch_policy,
        progress,
        cancellation,
    )
    .await;
    tracing::debug!(
        result = result.is_ok(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "protocol-v2 upload-pack session completed"
    );
    result
}

async fn acquire_read_admission(
    store: &Arc<dyn object_store::ObjectStore>,
    prefix: &str,
    cancellation: &CancellationToken,
) -> Result<crab_coordination::ReadAdmissionTicket> {
    acquire_read_admission_with_wait(store, prefix, cancellation, READ_ADMISSION_WAIT).await
}

async fn acquire_read_admission_with_wait(
    store: &Arc<dyn object_store::ObjectStore>,
    prefix: &str,
    cancellation: &CancellationToken,
    max_wait: Duration,
) -> Result<crab_coordination::ReadAdmissionTicket> {
    let mut ticket = crab_coordination::ReadAdmissionTicket::new(
        store,
        prefix,
        crab_coordination::DEFAULT_READ_ADMISSION_CAPACITY,
        crab_coordination::DEFAULT_READ_ADMISSION_TTL,
    )
    .map_err(CrabError::from)?;
    let deadline = Instant::now() + max_wait;
    let started = Instant::now();
    let mut attempt = 0;
    loop {
        if cancellation.is_cancelled() {
            return Err(CrabError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CrabError::Throttled {
                retry_after: Some(READ_ADMISSION_RETRY_CAP),
            });
        }

        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(CrabError::Cancelled),
            result = tokio::time::timeout(remaining, ticket.try_admit()) => match result {
                Ok(result) => result.map_err(CrabError::from),
                Err(_) => Err(CrabError::Throttled {
                    retry_after: Some(READ_ADMISSION_RETRY_CAP),
                }),
            },
        };
        match result {
            Ok(true) => {
                let waited_ms = started.elapsed().as_millis();
                tracing::debug!(
                    waited_ms,
                    capacity = crab_coordination::DEFAULT_READ_ADMISSION_CAPACITY,
                    "upload-pack read admission acquired"
                );
                return Ok(ticket);
            }
            Ok(false) => {}
            Err(error) => {
                let retry_after = match retry_class(&error) {
                    RetryClass::Transient => None,
                    RetryClass::Throttled { retry_after } => retry_after,
                    _ => return Err(error),
                };
                tracing::debug!(
                    error = %error,
                    attempt,
                    "upload-pack read admission probe will retry"
                );
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(error);
                }
                let delay = read_admission_retry_delay(attempt, retry_after).min(remaining);
                attempt = attempt.saturating_add(1);
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = cancellation.cancelled() => return Err(CrabError::Cancelled),
                }
                continue;
            }
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(CrabError::Throttled {
                retry_after: Some(READ_ADMISSION_RETRY_CAP),
            });
        }
        let delay = read_admission_retry_delay(attempt, None).min(remaining);
        attempt = attempt.saturating_add(1);
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = cancellation.cancelled() => return Err(CrabError::Cancelled),
        }
    }
}

fn read_admission_retry_delay(attempt: usize, retry_after: Option<Duration>) -> Duration {
    let multiplier = 1_u32.checked_shl(attempt.min(6) as u32).unwrap_or(u32::MAX);
    let bound = READ_ADMISSION_RETRY_BASE
        .saturating_mul(multiplier)
        .min(READ_ADMISSION_RETRY_CAP);
    let bound_nanos = u64::try_from(bound.as_nanos()).unwrap_or(u64::MAX);
    retry_after
        .unwrap_or_default()
        .saturating_add(Duration::from_nanos(
            rand::rng().random_range(0..=bound_nanos),
        ))
}

async fn serve_with_read_admission<R, W>(
    read_admission: crab_coordination::ReadAdmissionTicket,
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
    let admission = Arc::new(tokio::sync::Mutex::new(Some(read_admission)));
    let renewal_interval = (admission.lock().await.as_ref().map_or(
        Duration::from_secs(1),
        crab_coordination::ReadAdmissionTicket::ttl,
    ) / 3)
        .max(Duration::from_secs(1));
    let mut ticker = tokio::time::interval(renewal_interval);
    ticker.tick().await;
    let operation = serve_admitted(
        reader,
        writer,
        store,
        prefix,
        hidden_ref_patterns,
        fetch_policy,
        progress,
        &admission,
        cancellation,
    );
    tokio::pin!(operation);
    let mut renewal_error = None;
    let result = loop {
        tokio::select! {
            result = &mut operation => {
                break match result {
                    Err(error) => Err(error),
                    Ok(()) => match renewal_error {
                        Some(error) => Err(CrabError::from(error)),
                        None => Ok(()),
                    },
                };
            }
            _ = ticker.tick(), if renewal_error.is_none() => {
                let renewal = {
                    let mut admission = admission.lock().await;
                    match admission.as_mut() {
                        Some(ticket) => ticket.renew().await,
                        None => Ok(()),
                    }
                };
                if let Err(error) = renewal {
                    cancellation.cancel();
                    renewal_error = Some(error);
                }
            }
        }
    };
    let release_started = std::time::Instant::now();
    let release = release_read_admission(&admission).await;
    tracing::debug!(
        result = release.is_ok(),
        elapsed_ms = release_started.elapsed().as_millis() as u64,
        "upload-pack read admission released"
    );
    match (result, release) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(release_error)) => {
            tracing::warn!(
                error = %release_error,
                "upload-pack read admission release failed after session failure"
            );
            Err(error)
        }
    }
}

async fn release_read_admission(
    admission: &Arc<tokio::sync::Mutex<Option<crab_coordination::ReadAdmissionTicket>>>,
) -> Result<()> {
    let read_admission = admission.lock().await.take();
    match read_admission {
        Some(read_admission) => read_admission.release().await.map_err(CrabError::from),
        None => Ok(()),
    }
}

async fn serve_admitted<R, W>(
    reader: &mut R,
    writer: &mut W,
    store: &crab_storage::Store,
    prefix: &str,
    hidden_ref_patterns: &[String],
    fetch_policy: &FetchAdmissionPolicy,
    progress: bool,
    admission: &Arc<tokio::sync::Mutex<Option<crab_coordination::ReadAdmissionTicket>>>,
    cancellation: &CancellationToken,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (repository, proof) = open_repository_with_visibility_requirement(
        store,
        prefix,
        cancellation,
        VisibilityRequirement::Catalog,
    )
    .await?;
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
        let request = match read_command_request(reader, cancellation)
            .await
            .map_err(CrabError::from)
        {
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
                let args = match parse_ls_refs(&request.args).map_err(CrabError::from) {
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
                let fetch = match parse_fetch(&request.args).map_err(CrabError::from) {
                    Ok(fetch) => fetch,
                    Err(error) => {
                        return reject_protocol_request(writer, error, cancellation).await;
                    }
                };
                if let Err(error) = validate_fetch_admission_catalog(
                    &repository,
                    proof.as_catalog().ok_or_else(|| {
                        CrabError::Internal("upload-pack did not retain catalog proof".to_owned())
                    })?,
                    &visible_ref_names,
                    &fetch,
                    fetch_policy,
                    cancellation,
                )
                .await
                {
                    return reject_protocol_request(writer, error, cancellation).await;
                }
                if !fetch.done {
                    let common_haves = common_haves_catalog(
                        &repository,
                        proof.as_catalog().ok_or_else(|| {
                            CrabError::Internal(
                                "upload-pack did not retain catalog proof".to_owned(),
                            )
                        })?,
                        &fetch,
                        &visible_ref_names,
                        cancellation,
                    )
                    .await?;
                    if common_haves.is_empty() {
                        write_acknowledgments(writer, cancellation).await?;
                    } else {
                        write_fetch_response(
                            writer,
                            &repository,
                            &proof,
                            &visible_ref_names,
                            &fetch,
                            negotiation_rounds,
                            progress,
                            Some(&common_haves),
                            admission,
                            cancellation,
                        )
                        .await?;
                    }
                    continue;
                }
                write_fetch_response(
                    writer,
                    &repository,
                    &proof,
                    &visible_ref_names,
                    &fetch,
                    negotiation_rounds,
                    progress,
                    None,
                    admission,
                    cancellation,
                )
                .await?;
                // A terminal stateless-connect session has no server-side state to preserve
                // after the final fetch response. Closing here lets Git finish processing the
                // pack without waiting for a second empty request on the same pipe.
                if fetch.done {
                    return Ok(());
                }
            }
            other => {
                let error =
                    CrabError::Protocol(format!("unsupported protocol-v2 command: {other}"));
                return reject_protocol_request(writer, error, cancellation).await;
            }
        }
    }
}

#[cfg(test)]
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
            || (visible_reachable_wants_allowed(request, policy)
                && want.as_bytes().try_into().ok().is_some_and(|oid| {
                    visibility.contains_for_refs(visible_ref_names.iter().map(String::as_str), &oid)
                }))
        {
            continue;
        }
        return Err(protocol(format!(
            "want {want} is denied by upload-pack policy"
        )));
    }
    Ok(())
}

async fn validate_fetch_admission_catalog(
    repository: &RemoteGitRepository,
    visibility: &GitCatalogVisibilityIndex,
    visible_ref_names: &[String],
    request: &FetchRequest,
    policy: &FetchAdmissionPolicy,
    cancellation: &CancellationToken,
) -> Result<()> {
    let advertised_tips = repository
        .refs()
        .entries
        .iter()
        .filter(|reference| visible_ref_names.contains(&reference.name))
        .flat_map(|reference| [Some(reference.target), reference.peeled])
        .flatten()
        .collect::<HashSet<_>>();
    let reachable = if visible_reachable_wants_allowed(request, policy) {
        let operation = repository
            .operation(crab_remote_git::OperationKind::UploadPack, cancellation)
            .await
            .map_err(remote_error)?;
        let result = operation
            .catalog_object_ordinals(&request.wants)
            .await
            .map(|ordinals| {
                ordinals
                    .into_iter()
                    .map(|ordinal| {
                        ordinal.is_some_and(|ordinal| {
                            visibility.contains_ordinal_for_refs(
                                visible_ref_names.iter().map(String::as_str),
                                ordinal,
                            )
                        })
                    })
                    .collect::<Vec<_>>()
            });
        operation.finish(result).await.map_err(remote_error)?
    } else {
        vec![false; request.wants.len()]
    };
    for (index, want) in request.wants.iter().enumerate() {
        if policy.allow_any_sha_in_want
            || (policy.allow_tip_sha_in_want && advertised_tips.contains(want))
            || reachable.get(index).copied().unwrap_or(false)
        {
            continue;
        }
        return Err(protocol(format!(
            "want {want} is denied by upload-pack policy"
        )));
    }
    Ok(())
}

fn visible_reachable_wants_allowed(request: &FetchRequest, policy: &FetchAdmissionPolicy) -> bool {
    // Partial-clone clients must request promised interior objects by OID.
    // Filtered requests remain bounded by the generation-pinned visible-ref
    // catalog, so they do not need the general raw-SHA policy opt-in.
    policy.allow_reachable_sha_in_want || !matches!(&request.filter, UploadPackFilter::None)
}

async fn open_repository_with_visibility_requirement(
    store: &crab_storage::Store,
    prefix: &str,
    cancellation: &CancellationToken,
    requirement: VisibilityRequirement,
) -> Result<(RemoteGitRepository, UploadPackVisibilityProof)> {
    let repair_store = crate::storage::Store::from_storage(store.clone());
    let repair_layout = crate::storage::StoreLayout::new(repair_store.clone(), prefix.to_owned());
    let mut last_indexing = None;
    let deadline = Instant::now() + READ_ADMISSION_WAIT;
    for attempt in 0..=LOCATOR_READ_RETRY_LIMIT {
        if Instant::now() >= deadline {
            break;
        }
        let (manifest, _) =
            crate::metadata::manifest::read_manifest(&repair_store, &repair_layout).await?;
        let active_transactions = crate::metadata::manifest::list_active_ref_journal_transactions(
            &repair_store,
            &repair_layout,
        )
        .await?;
        if !active_transactions.is_empty() {
            if super::push::git_generation_owner_is_active(&repair_store, &repair_layout).await? {
                last_indexing = Some((
                    Some(manifest.generation),
                    manifest.generation.saturating_add(1),
                ));
                if !wait_for_locator_read_retry(
                    attempt,
                    deadline,
                    Some(manifest.generation),
                    manifest.generation.saturating_add(1),
                    cancellation,
                )
                .await?
                {
                    break;
                }
                continue;
            }
            if super::push::compact_ref_journal_for_reader(
                &repair_store,
                &repair_layout,
                LOCATOR_READ_REPAIR_LOCK_TTL,
                manifest.pusher.clone(),
                cancellation,
            )
            .await?
            {
                tracing::info!(
                    generation = manifest.generation,
                    "compacted active Git ref journal before upload-pack admission"
                );
                continue;
            }
            last_indexing = Some((
                Some(manifest.generation),
                manifest.generation.saturating_add(1),
            ));
            if !wait_for_locator_read_retry(
                attempt,
                deadline,
                Some(manifest.generation),
                manifest.generation.saturating_add(1),
                cancellation,
            )
            .await?
            {
                break;
            }
            continue;
        }

        let open = open_repository_snapshot(store, prefix, cancellation).await;
        let mut visibility_error = None;
        let (observed_generation, required_generation) = match open {
            Ok(repository) => {
                let visibility = match requirement {
                    #[cfg(test)]
                    VisibilityRequirement::Materialized => repository
                        .visibility_index(cancellation)
                        .await
                        .map(UploadPackVisibilityProof::Materialized),
                    VisibilityRequirement::Catalog => repository
                        .catalog_visibility_index(cancellation)
                        .await
                        .map(UploadPackVisibilityProof::Catalog),
                };
                match visibility {
                    Ok(visibility) => return Ok((repository, visibility)),
                    Err(error) if visibility_index_needs_repair(&error) => {
                        let generation = repository.generation();
                        visibility_error = Some(error);
                        (Some(generation), generation)
                    }
                    Err(error) => return Err(remote_error(error)),
                }
            }
            Err(RemoteGitError::RepositoryIndexing { observed, required }) => (observed, required),
            Err(error) => return Err(remote_error(error)),
        };
        last_indexing = Some((observed_generation, required_generation));

        // Once journal transactions are drained, generation checks distinguish
        // derived publication lag from a concurrent owner update.
        if super::push::git_generation_owner_is_active(&repair_store, &repair_layout).await? {
            if !wait_for_locator_read_retry(
                attempt,
                deadline,
                observed_generation,
                required_generation,
                cancellation,
            )
            .await?
            {
                break;
            }
            continue;
        }
        let repaired = super::push::repair_git_object_locator_if_current_for_reader(
            &repair_store,
            &repair_layout,
            required_generation,
            LOCATOR_READ_REPAIR_LOCK_TTL,
            cancellation,
        )
        .await?;
        if repaired {
            tracing::info!(
                observed_generation,
                required_generation,
                "repaired current Git locator before upload-pack admission"
            );
        }
        if repaired || visibility_error.is_some() {
            let publication =
                super::push::repair_git_visibility_after_locator_if_current_with_limit_for_reader(
                    &repair_store,
                    &repair_layout,
                    required_generation,
                    crab_metadata::git_visibility::MAX_SYNCHRONOUS_GIT_VISIBILITY_OBJECTS,
                    LOCATOR_READ_REPAIR_LOCK_TTL,
                    cancellation,
                )
                .await?;
            if matches!(
                publication,
                Some(
                    super::push::GitVisibilityPublication::Published
                        | super::push::GitVisibilityPublication::CatalogBound
                )
            ) {
                tracing::info!(
                    required_generation,
                    "repaired current catalog-bound Git visibility before upload-pack admission"
                );
                continue;
            }
            if let Some(error) = visibility_error {
                return Err(remote_error(error));
            }
        }
        if !wait_for_locator_read_retry(
            attempt,
            deadline,
            observed_generation,
            required_generation,
            cancellation,
        )
        .await?
        {
            break;
        }
    }

    let Some((observed, required)) = last_indexing else {
        return Err(CrabError::Internal(
            "upload-pack repository admission ended without a result".to_owned(),
        ));
    };
    Err(remote_error(RemoteGitError::RepositoryIndexing {
        observed,
        required,
    }))
}

pub(crate) async fn open_repository_with_catalog_visibility(
    store: &crab_storage::Store,
    prefix: &str,
    cancellation: &CancellationToken,
) -> Result<(RemoteGitRepository, GitCatalogVisibilityIndex)> {
    let (repository, proof) = open_repository_with_visibility_requirement(
        store,
        prefix,
        cancellation,
        VisibilityRequirement::Catalog,
    )
    .await?;
    Ok((repository, proof.into_catalog()?))
}

#[cfg(test)]
pub(crate) async fn open_repository_with_visibility(
    store: &crab_storage::Store,
    prefix: &str,
    cancellation: &CancellationToken,
) -> Result<(RemoteGitRepository, GitVisibilityIndex)> {
    let (repository, proof) = open_repository_with_visibility_requirement(
        store,
        prefix,
        cancellation,
        VisibilityRequirement::Materialized,
    )
    .await?;
    let UploadPackVisibilityProof::Materialized(visibility) = proof else {
        return Err(CrabError::Internal(
            "materialized upload-pack proof was not returned".to_owned(),
        ));
    };
    Ok((repository, visibility))
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
    let repository = RemoteGitRepository::open(
        store.clone(),
        layout,
        identity,
        runtime,
        upload_pack_repository_options()?,
        cancellation,
    )
    .await?;
    let lease_provider = ObjectStoreGeneratedPackLeaseProvider {
        store: Arc::clone(store.inner()),
        prefix: prefix.to_owned(),
    };
    Ok(repository.with_generated_pack_lease_provider(Arc::new(lease_provider)))
}

fn locator_read_retry_delay(attempt: usize) -> Duration {
    let shift = u32::try_from(attempt).unwrap_or(u32::MAX);
    let multiplier = 1u32.checked_shl(shift).unwrap_or(u32::MAX);
    LOCATOR_READ_RETRY_BASE
        .saturating_mul(multiplier)
        .min(LOCATOR_READ_RETRY_CAP)
}

async fn wait_for_locator_read_retry(
    attempt: usize,
    deadline: Instant,
    observed_generation: Option<u64>,
    required_generation: u64,
    cancellation: &CancellationToken,
) -> Result<bool> {
    if attempt >= LOCATOR_READ_RETRY_LIMIT {
        return Ok(false);
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Ok(false);
    }
    let delay = locator_read_retry_delay(attempt).min(remaining);
    tracing::debug!(
        attempt = attempt + 1,
        ?delay,
        ?observed_generation,
        required_generation,
        "waiting for current Git locator publication before upload-pack admission"
    );
    tokio::select! {
        () = tokio::time::sleep(delay) => Ok(true),
        () = cancellation.cancelled() => Err(CrabError::Cancelled),
    }
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
    write_flush(writer, cancellation)
        .await
        .map_err(CrabError::from)
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
    write_response_end(writer, cancellation)
        .await
        .map_err(CrabError::from)
}

async fn common_haves_catalog(
    repository: &RemoteGitRepository,
    visibility: &GitCatalogVisibilityIndex,
    request: &FetchRequest,
    visible_ref_names: &[String],
    cancellation: &CancellationToken,
) -> Result<Vec<ObjectId>> {
    if request.haves.is_empty() {
        return Ok(Vec::new());
    }
    let visible = visible_objects_catalog(
        repository,
        visibility,
        &request.haves,
        visible_ref_names,
        cancellation,
    )
    .await?;
    Ok(request
        .haves
        .iter()
        .copied()
        .zip(visible)
        .filter_map(|(have, visible)| visible.then_some(have))
        .collect())
}

async fn visible_objects_catalog(
    repository: &RemoteGitRepository,
    visibility: &GitCatalogVisibilityIndex,
    object_ids: &[ObjectId],
    visible_ref_names: &[String],
    cancellation: &CancellationToken,
) -> Result<Vec<bool>> {
    let operation = repository
        .operation(crab_remote_git::OperationKind::UploadPack, cancellation)
        .await
        .map_err(remote_error)?;
    let ordinals = operation.catalog_object_ordinals(object_ids).await;
    let result = ordinals.map(|ordinals| {
        ordinals
            .into_iter()
            .map(|ordinal| {
                ordinal.is_some_and(|ordinal| {
                    visibility.contains_ordinal_for_refs(
                        visible_ref_names.iter().map(String::as_str),
                        ordinal,
                    )
                })
            })
            .collect()
    });
    operation.finish(result).await.map_err(remote_error)
}

fn native_shallow_visibility(
    repository: &RemoteGitRepository,
    request: &FetchRequest,
    visible_ref_names: &[String],
    cancellation: &CancellationToken,
) -> Result<(Vec<ObjectId>, bool)> {
    let mut object_ids = Vec::with_capacity(request.haves.len() + request.shallow.len());
    object_ids.extend_from_slice(&request.haves);
    object_ids.extend_from_slice(&request.shallow);
    let visible_ref_names = visible_ref_names.iter().collect::<HashSet<_>>();
    let roots = repository
        .refs()
        .entries
        .iter()
        .filter(|reference| visible_ref_names.contains(&reference.name))
        .flat_map(|reference| [Some(reference.target), reference.peeled])
        .flatten()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let started = Instant::now();
    let Some(visible) = repository
        .commits_reachable_from(&object_ids, &roots, cancellation)
        .map_err(remote_error)?
    else {
        tracing::debug!(
            candidate_commits = object_ids.len(),
            visible_roots = roots.len(),
            "native shallow commit-graph visibility unavailable"
        );
        return Ok((Vec::new(), false));
    };
    tracing::info!(
        telemetry_event = "native_shallow_visibility",
        strategy = "commit_graph",
        candidate_commits = object_ids.len(),
        visible_roots = roots.len(),
        reachable_commits = visible.iter().filter(|visible| **visible).count(),
        visibility_ms = started.elapsed().as_millis() as u64,
        "native shallow visibility resolved from commit graph"
    );
    let (have_visibility, shallow_visibility) = visible.split_at(request.haves.len());
    let common_haves = request
        .haves
        .iter()
        .copied()
        .zip(have_visibility)
        .filter_map(|(have, visible)| visible.then_some(have))
        .collect();
    Ok((
        common_haves,
        shallow_visibility.iter().all(|visible| *visible),
    ))
}

async fn write_acknowledgments<W: AsyncWrite + Unpin>(
    writer: &mut W,
    cancellation: &CancellationToken,
) -> Result<()> {
    write_data(writer, b"acknowledgments\n", cancellation).await?;
    write_data(writer, b"NAK\n", cancellation).await?;
    write_flush(writer, cancellation).await?;
    write_response_end(writer, cancellation)
        .await
        .map_err(CrabError::from)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the preplanning cache boundary carries the pinned proof and protocol response state"
)]
async fn write_preplanned_cached_fetch_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    repository: &RemoteGitRepository,
    proof: &UploadPackVisibilityProof,
    visible_ref_names: &[String],
    request: &FetchRequest,
    semantic_request: &UploadPackRequest,
    negotiation_rounds: u32,
    progress: bool,
    acknowledged_haves: Option<&[ObjectId]>,
    admission: &Arc<tokio::sync::Mutex<Option<crab_coordination::ReadAdmissionTicket>>>,
    cancellation: &CancellationToken,
    started: std::time::Instant,
    request_class: &'static str,
) -> Result<()> {
    let authorization_digest = proof.authorization_digest_for_refs(visible_ref_names);
    let request_digest = preplanned_pack_request_digest(request);
    let cache_key =
        repository.generated_pack_request_cache_key(authorization_digest, request_digest);
    // The generated-pack producer reacquires one read slot under its cross-process lease.
    // Waiters must release the session slot before polling or sixteen identical requests can
    // prevent the sole producer from entering planning.
    release_read_admission(admission).await?;
    tracing::debug!("released upload-pack read admission before request-plan cache wait");

    let producer = async {
        if native_shallow_pack_eligible(request) && proof.as_catalog().is_some() {
            let (common_haves, shallow_visible) =
                native_shallow_visibility(repository, request, visible_ref_names, cancellation)?;
            if !common_haves.is_empty() && shallow_visible {
                let visible_tag_refs = visible_ref_names
                    .iter()
                    .map(String::as_str)
                    .collect::<HashSet<_>>();
                let included_tags = request
                    .include_tags
                    .then(|| {
                        repository
                            .refs()
                            .entries
                            .iter()
                            .filter(|reference| {
                                reference.name.starts_with("refs/tags/")
                                    && visible_tag_refs.contains(reference.name.as_str())
                            })
                            .filter_map(|reference| {
                                reference
                                    .peeled
                                    .map(|peeled| crab_git::repack::ShallowFetchTag {
                                        target: reference.target,
                                        peeled,
                                    })
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                tracing::info!(
                    protocol_version = 2,
                    request_class,
                    negotiation_rounds,
                    haves = request.haves.len(),
                    common_haves = common_haves.len(),
                    shallow = request.shallow.len(),
                    visible_objects = proof.object_count_for_refs(visible_ref_names),
                    "protocol-v2 upload-pack native shallow producer selected"
                );
                return repository
                    .generate_shallow_fetch_pack(
                        &request.wants,
                        &common_haves,
                        &request.shallow,
                        &included_tags,
                        cancellation,
                    )
                    .await
                    .map_err(remote_error);
            }
        }
        let plan = match proof {
            #[cfg(test)]
            UploadPackVisibilityProof::Materialized(visibility) => {
                plan_upload_pack(
                    repository,
                    visibility,
                    visible_ref_names,
                    semantic_request,
                    cancellation,
                )
                .await
            }
            UploadPackVisibilityProof::Catalog(visibility) => {
                plan_upload_pack_catalog(
                    repository,
                    visibility,
                    visible_ref_names,
                    semantic_request,
                    cancellation,
                )
                .await
            }
        }
        .map_err(|error| CrabError::Protocol(format!("upload-pack request rejected: {error}")))?;
        if !plan.shallow.is_empty() || !plan.unshallow.is_empty() {
            return Err(CrabError::Internal(
                "non-deepening shallow fetch unexpectedly changed shallow boundaries".to_owned(),
            ));
        }
        tracing::info!(
            protocol_version = 2,
            request_class,
            negotiation_rounds,
            canonical_filter = %request.filter.canonical_spec(),
            haves = request.haves.len(),
            common_haves = plan.common_haves.len(),
            shallow = request.shallow.len(),
            visible_objects = proof.object_count_for_refs(visible_ref_names),
            planned_objects = plan.object_ids.len(),
            "protocol-v2 upload-pack preplanned cache producer selected"
        );
        repository
            .generate_pack(&plan.object_ids, cancellation)
            .await
            .map_err(remote_error)
    };
    let pack = match repository
        .generate_pack_request_cached(cache_key, producer, cancellation)
        .await
    {
        Ok(pack) => pack,
        Err(crab_remote_git::GeneratedPackRequestCacheError::Producer(error)) => {
            return reject_protocol_request(writer, error, cancellation).await;
        }
        Err(crab_remote_git::GeneratedPackRequestCacheError::Cache(error)) => {
            return reject_protocol_request(writer, remote_error(error), cancellation).await;
        }
    };

    if let Some(acknowledged_haves) = acknowledged_haves {
        write_data(writer, b"acknowledgments\n", cancellation).await?;
        for have in acknowledged_haves {
            let line = format!("ACK {have}\n");
            write_data(writer, line.as_bytes(), cancellation).await?;
        }
        write_data(writer, b"ready\n", cancellation).await?;
        write_delimiter(writer, cancellation).await?;
    }
    write_data(writer, b"packfile\n", cancellation).await?;
    if progress && !request.no_progress {
        write_packet(writer, b"counting objects\n", Some(2), cancellation).await?;
    }
    tracing::info!(
        protocol_version = 2,
        request_class,
        negotiation_rounds,
        canonical_filter = %request.filter.canonical_spec(),
        planned_objects = pack.object_count(),
        reconstructed_objects = pack.object_count(),
        transferred_bytes = pack.size(),
        latency_ms = started.elapsed().as_millis() as u64,
        "protocol-v2 upload-pack preplanned pack generated"
    );
    pack.write_sideband(writer, cancellation)
        .await
        .map_err(remote_error)?;
    write_flush(writer, cancellation).await?;
    write_response_end(writer, cancellation)
        .await
        .map_err(CrabError::from)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the response boundary carries the pinned repository, proof, negotiation, and wire state"
)]
async fn write_fetch_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    repository: &RemoteGitRepository,
    proof: &UploadPackVisibilityProof,
    visible_ref_names: &[String],
    request: &FetchRequest,
    negotiation_rounds: u32,
    progress: bool,
    acknowledged_haves: Option<&[ObjectId]>,
    admission: &Arc<tokio::sync::Mutex<Option<crab_coordination::ReadAdmissionTicket>>>,
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
    if request_pack_preplanning_cache_eligible(request) {
        return write_preplanned_cached_fetch_response(
            writer,
            repository,
            proof,
            visible_ref_names,
            request,
            &semantic_request,
            negotiation_rounds,
            progress,
            acknowledged_haves,
            admission,
            cancellation,
            started,
            request_class,
        )
        .await;
    }
    let plan = match match proof {
        #[cfg(test)]
        UploadPackVisibilityProof::Materialized(visibility) => {
            plan_upload_pack(
                repository,
                visibility,
                visible_ref_names,
                &semantic_request,
                cancellation,
            )
            .await
        }
        UploadPackVisibilityProof::Catalog(visibility) => {
            plan_upload_pack_catalog(
                repository,
                visibility,
                visible_ref_names,
                &semantic_request,
                cancellation,
            )
            .await
        }
    } {
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

    let visible_object_count = proof.object_count_for_refs(visible_ref_names);
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
    let thin_bases = request
        .thin_pack
        .then_some(plan.common_haves.as_slice())
        .unwrap_or_default();
    if request.haves.is_empty() && request.done {
        // Identical cache waiters do not perform repository reads while the
        // producer builds the immutable response artifact.
        release_read_admission(admission).await?;
        tracing::debug!("released upload-pack read admission before generated-pack cache wait");
    }
    let generated = if request.haves.is_empty() {
        let authorization_digest = proof.authorization_digest_for_refs(visible_ref_names);
        let cache_key = repository.generated_pack_cache_key(
            authorization_digest,
            &plan.object_ids,
            !thin_bases.is_empty(),
        );
        if dense_selected_response(&request) {
            repository
                .generate_pack_cached_with_dense_selection(
                    &plan.object_ids,
                    cache_key,
                    cancellation,
                )
                .await
        } else {
            repository
                .generate_pack_cached(&plan.object_ids, cache_key, cancellation)
                .await
        }
    } else {
        repository
            .generate_pack_with_bases(&plan.object_ids, thin_bases, cancellation)
            .await
    };
    let pack = match generated {
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
    write_response_end(writer, cancellation)
        .await
        .map_err(CrabError::from)
}

fn request_pack_preplanning_cache_eligible(request: &FetchRequest) -> bool {
    !request.haves.is_empty()
        && !request.shallow.is_empty()
        && request.deepen.is_none()
        && !request.deepen_relative
        && matches!(request.filter, UploadPackFilter::None)
}

fn native_shallow_pack_eligible(request: &FetchRequest) -> bool {
    request_pack_preplanning_cache_eligible(request)
}

fn preplanned_pack_request_digest(request: &FetchRequest) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"crab.upload-pack.preplanned-request.v1\0");
    let filter = request.filter.canonical_spec();
    hash.update(&(filter.len() as u64).to_be_bytes());
    hash.update(filter.as_bytes());
    hash.update(&[
        u8::from(request.deepen_relative),
        u8::from(request.include_tags),
        u8::from(request.thin_pack),
        u8::from(request.ofs_delta),
    ]);
    match request.deepen {
        Some(depth) => {
            hash.update(&[1]);
            hash.update(&depth.to_be_bytes());
        }
        None => {
            hash.update(&[0]);
        }
    }
    for objects in [
        request.wants.as_slice(),
        request.haves.as_slice(),
        request.shallow.as_slice(),
    ] {
        let mut objects = objects.to_vec();
        objects.sort_unstable();
        hash.update(&(objects.len() as u64).to_be_bytes());
        for oid in objects {
            hash.update(oid.as_bytes());
        }
    }
    *hash.finalize().as_bytes()
}

fn dense_selected_response(request: &FetchRequest) -> bool {
    request.shallow.is_empty()
        && request.deepen.is_none()
        && !request.deepen_relative
        && request.filter.is_catalog_exact()
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

async fn write_data<W: AsyncWrite + Unpin>(
    writer: &mut W,
    data: &[u8],
    cancellation: &CancellationToken,
) -> Result<()> {
    write_packet(writer, data, None, cancellation)
        .await
        .map_err(CrabError::from)
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
    flush_cancellable(writer, cancellation)
        .await
        .map_err(CrabError::from)
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

fn remote_error(error: impl std::fmt::Display) -> CrabError {
    CrabError::Protocol(format!("remote Git upload-pack error: {error}"))
}

fn protocol(message: impl Into<String>) -> CrabError {
    CrabError::Protocol(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(data: &[u8]) -> Vec<u8> {
        let length = data.len() + 4;
        let mut packet = format!("{length:04x}").into_bytes();
        packet.extend_from_slice(data);
        packet
    }

    #[test]
    fn preplanned_pack_request_digest_binds_shallow_incremental_negotiation() {
        let first =
            ObjectId::from_hex(b"1111111111111111111111111111111111111111").expect("object ID");
        let second =
            ObjectId::from_hex(b"2222222222222222222222222222222222222222").expect("object ID");
        let request = FetchRequest {
            wants: vec![second],
            haves: vec![first],
            shallow: vec![first],
            thin_pack: true,
            ..FetchRequest::default()
        };
        assert!(request_pack_preplanning_cache_eligible(&request));
        assert!(native_shallow_pack_eligible(&request));

        let digest = preplanned_pack_request_digest(&request);
        let mut changed = request.clone();
        changed.wants = vec![first];
        assert_ne!(digest, preplanned_pack_request_digest(&changed));
        changed = request.clone();
        changed.haves = vec![second];
        assert_ne!(digest, preplanned_pack_request_digest(&changed));
        changed = request.clone();
        changed.shallow = vec![second];
        assert_ne!(digest, preplanned_pack_request_digest(&changed));
        changed = request.clone();
        changed.deepen = Some(1);
        assert!(!request_pack_preplanning_cache_eligible(&changed));
        changed = request;
        changed.filter = UploadPackFilter::BlobNone;
        assert!(!request_pack_preplanning_cache_eligible(&changed));
        let changed = FetchRequest {
            wants: vec![second],
            haves: vec![first],
            shallow: vec![first],
            include_tags: true,
            ..FetchRequest::default()
        };
        assert!(request_pack_preplanning_cache_eligible(&changed));
        assert!(native_shallow_pack_eligible(&changed));
    }

    #[test]
    fn dense_selected_response_requires_a_catalog_filter_without_shallow_state() {
        let mut request = FetchRequest {
            filter: UploadPackFilter::BlobNone,
            ..FetchRequest::default()
        };
        assert!(dense_selected_response(&request));

        request.filter = UploadPackFilter::ObjectType(crab_read::UploadPackObjectType::Tree);
        assert!(dense_selected_response(&request));

        request.filter = UploadPackFilter::TreeDepth(1);
        assert!(!dense_selected_response(&request));

        request.filter = UploadPackFilter::BlobNone;
        request.deepen = Some(100);
        assert!(!dense_selected_response(&request));

        request.filter = UploadPackFilter::None;
        assert!(!dense_selected_response(&request));
    }

    #[tokio::test]
    async fn generated_pack_lease_provider_serializes_one_repository_resource() {
        let store: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let provider = ObjectStoreGeneratedPackLeaseProvider {
            store,
            prefix: "org/repo".to_owned(),
        };
        let first = match crab_remote_git::GeneratedPackLeaseProvider::try_acquire(
            &provider,
            "generated-pack-request",
            Duration::from_secs(5),
        )
        .await
        .expect("first lease acquisition")
        {
            crab_remote_git::GeneratedPackLeaseAttempt::Acquired(lease) => lease,
            crab_remote_git::GeneratedPackLeaseAttempt::Held { .. } => {
                panic!("first lease must be available")
            }
        };
        let blocked = crab_remote_git::GeneratedPackLeaseProvider::try_acquire(
            &provider,
            "generated-pack-request",
            Duration::from_secs(5),
        )
        .await
        .expect("contending lease acquisition");
        let retry_after = match blocked {
            crab_remote_git::GeneratedPackLeaseAttempt::Held { retry_after } => retry_after,
            crab_remote_git::GeneratedPackLeaseAttempt::Acquired(_) => {
                panic!("contending lease must remain held")
            }
        };
        assert!(!retry_after.is_zero());
        assert!(retry_after <= Duration::from_secs(5));

        first.release().await.expect("release first lease");
        let replacement = match crab_remote_git::GeneratedPackLeaseProvider::try_acquire(
            &provider,
            "generated-pack-request",
            Duration::from_secs(5),
        )
        .await
        .expect("replacement lease acquisition")
        {
            crab_remote_git::GeneratedPackLeaseAttempt::Acquired(lease) => lease,
            crab_remote_git::GeneratedPackLeaseAttempt::Held { .. } => {
                panic!("released lease must be acquirable")
            }
        };
        replacement.release().await.expect("release replacement");
    }

    #[tokio::test]
    async fn generated_pack_coordination_reserves_and_releases_read_admission() {
        let store: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let provider = ObjectStoreGeneratedPackLeaseProvider {
            store: Arc::clone(&store),
            prefix: "org/repo".to_owned(),
        };
        let lease = match crab_remote_git::GeneratedPackLeaseProvider::try_acquire(
            &provider,
            "generated-pack-request",
            Duration::from_secs(5),
        )
        .await
        .expect("lease acquisition")
        {
            crab_remote_git::GeneratedPackLeaseAttempt::Acquired(lease) => lease,
            crab_remote_git::GeneratedPackLeaseAttempt::Held { .. } => {
                panic!("lease must be available")
            }
        };

        let mut admitted = Vec::new();
        for _ in 0..crab_coordination::DEFAULT_READ_ADMISSION_CAPACITY {
            let mut ticket = crab_coordination::ReadAdmissionTicket::new(
                &store,
                "org/repo",
                crab_coordination::DEFAULT_READ_ADMISSION_CAPACITY,
                Duration::from_secs(60),
            )
            .expect("read admission ticket");
            for _ in 0..crab_coordination::DEFAULT_READ_ADMISSION_CAPACITY {
                if ticket.try_admit().await.expect("read admission probe") {
                    admitted.push(ticket);
                    break;
                }
            }
        }
        assert_eq!(
            admitted.len(),
            crab_coordination::DEFAULT_READ_ADMISSION_CAPACITY - 1
        );

        lease.release().await.expect("release generated-pack lease");
        let permit = crab_remote_git::GeneratedPackLeaseProvider::acquire_read(
            &provider,
            &CancellationToken::new(),
            READ_ADMISSION_WAIT,
        )
        .await
        .expect("generated-pack cache read admission");
        permit
            .release()
            .await
            .expect("release cache read admission");
        let mut replacement = crab_coordination::ReadAdmissionTicket::new(
            &store,
            "org/repo",
            crab_coordination::DEFAULT_READ_ADMISSION_CAPACITY,
            Duration::from_secs(60),
        )
        .expect("replacement read admission ticket");
        let mut replacement_admitted = false;
        for _ in 0..crab_coordination::DEFAULT_READ_ADMISSION_CAPACITY {
            if replacement
                .try_admit()
                .await
                .expect("replacement admission")
            {
                replacement_admitted = true;
                break;
            }
        }
        assert!(replacement_admitted);

        replacement.release().await.expect("release replacement");
        for ticket in admitted {
            ticket.release().await.expect("release read admission");
        }
    }

    #[test]
    fn upload_pack_profile_covers_the_supported_visibility_generation() {
        let options = upload_pack_repository_options().expect("valid upload-pack limits");

        assert_eq!(options.object_limits().max_object_bytes, 128 * MIB);
        assert_eq!(
            options.operation_limits().max_logical_objects,
            crab_metadata::git_visibility::MAX_GIT_VISIBILITY_OBJECTS
        );
        assert_eq!(options.operation_limits().max_duration, READ_ADMISSION_WAIT);
        assert!(
            options.operation_limits().max_response_bytes
                < options.operation_limits().max_inflated_bytes
        );
    }

    #[test]
    fn read_admission_retry_delay_honors_provider_hint() {
        let hint = Duration::from_secs(2);

        let delay = read_admission_retry_delay(0, Some(hint));

        assert!(delay >= hint);
        assert!(delay <= hint.saturating_add(READ_ADMISSION_RETRY_BASE));
    }

    #[test]
    fn read_admission_retry_delay_is_capped_without_provider_hint() {
        let delay = read_admission_retry_delay(usize::MAX, None);

        assert!(delay <= READ_ADMISSION_RETRY_CAP);
    }

    #[tokio::test]
    async fn capability_snapshot_short_circuits_empty_repositories() {
        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = crab_storage::StoreLayout::new(store.clone(), "org/repo".to_owned());
        let manifest = crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main");
        crab_metadata::manifest_store::create_manifest(&store, &layout, &manifest)
            .await
            .expect("create empty manifest");

        assert_eq!(
            capability_snapshot_is_stable(&store, "org/repo", &CancellationToken::new())
                .await
                .expect("read empty capability snapshot"),
            true
        );
    }

    #[tokio::test]
    async fn snapshot_capability_rejects_refs_without_a_visibility_proof() {
        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = crab_storage::StoreLayout::new(store.clone(), "org/repo".to_owned());
        let mut manifest = crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main");
        manifest.refs.insert(
            "refs/heads/main".to_owned(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        );
        manifest.seal_git_validation();
        crab_metadata::manifest_store::create_manifest(&store, &layout, &manifest)
            .await
            .expect("create incomplete manifest");

        assert!(
            !snapshot_available(&store, "org/repo", &CancellationToken::new()).await,
            "terminal protocol v2 must not be advertised without repairable visibility evidence"
        );
    }

    #[test]
    fn visibility_mismatch_enters_the_bounded_repair_path() {
        let error = RemoteGitError::RepositoryState {
            reason: crab_remote_git::RepositoryStateError::VisibilityProofMismatch,
        };

        assert!(visibility_index_needs_repair(&error));
    }

    #[test]
    fn locator_read_retry_delay_is_bounded() {
        assert_eq!(locator_read_retry_delay(0), Duration::from_millis(100));
        assert_eq!(locator_read_retry_delay(3), Duration::from_millis(800));
        assert_eq!(locator_read_retry_delay(4), Duration::from_millis(1600));
        assert_eq!(locator_read_retry_delay(5), Duration::from_secs(2));
        assert_eq!(locator_read_retry_delay(usize::MAX), Duration::from_secs(2));
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
        let ancestor = ObjectId::from_hex("a".repeat(40).as_bytes()).expect("ancestor oid");
        let tip = ObjectId::from_hex("b".repeat(40).as_bytes()).expect("tip oid");
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
        )
        .expect("valid visibility proof");

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
    fn filtered_reachable_non_tip_want_is_accepted_by_default() {
        let blob = ObjectId::from_hex("a".repeat(40).as_bytes()).expect("blob oid");
        let tip = ObjectId::from_hex("b".repeat(40).as_bytes()).expect("tip oid");
        let request = FetchRequest {
            wants: vec![blob],
            filter: UploadPackFilter::BlobNone,
            ..FetchRequest::default()
        };
        let visible_refs = vec!["refs/heads/main".to_owned()];
        let visibility = GitVisibilityIndex::new(
            1,
            "c".repeat(64),
            "d".repeat(64),
            std::collections::BTreeMap::from([(
                visible_refs[0].clone(),
                vec![blob.to_string(), tip.to_string()],
            )]),
        )
        .expect("valid visibility proof");

        let result = validate_fetch_wants(
            &HashSet::from([tip]),
            &visibility,
            &visible_refs,
            &request,
            &FetchAdmissionPolicy::default(),
        );

        assert!(result.is_ok());
    }

    #[test]
    fn filtered_reachable_non_tip_want_outside_visible_ref_closure_is_denied() {
        let hidden_blob = ObjectId::from_hex("a".repeat(40).as_bytes()).expect("hidden blob oid");
        let visible_blob = ObjectId::from_hex("b".repeat(40).as_bytes()).expect("visible blob oid");
        let tip = ObjectId::from_hex("c".repeat(40).as_bytes()).expect("tip oid");
        let request = FetchRequest {
            wants: vec![hidden_blob],
            filter: UploadPackFilter::BlobNone,
            ..FetchRequest::default()
        };
        let visible_refs = vec!["refs/heads/main".to_owned()];
        let visibility = GitVisibilityIndex::new(
            1,
            "d".repeat(64),
            "e".repeat(64),
            std::collections::BTreeMap::from([(
                visible_refs[0].clone(),
                vec![visible_blob.to_string(), tip.to_string()],
            )]),
        )
        .expect("valid visibility proof");

        let error = validate_fetch_wants(
            &HashSet::from([tip]),
            &visibility,
            &visible_refs,
            &request,
            &FetchAdmissionPolicy::default(),
        )
        .expect_err("objects outside visible ref closure must remain denied");

        assert!(error.to_string().contains("denied by upload-pack policy"));
    }

    #[test]
    fn reachable_non_tip_want_is_accepted_when_enabled() {
        let ancestor = ObjectId::from_hex("a".repeat(40).as_bytes()).expect("ancestor oid");
        let tip = ObjectId::from_hex("b".repeat(40).as_bytes()).expect("tip oid");
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
        )
        .expect("valid visibility proof");
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
