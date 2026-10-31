//! Git protocol-v2 upload-pack over the helper's already-authenticated stdio.
//!
//! This module owns wire framing, command semantics, and the product-level
//! captured-repository admission before a fetch starts. Generation pinning, object
//! traversal, and pack production stay in the shared read and remote-git crates.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crab_metadata::git_visibility::GitVisibilityIndex;
use crab_read::plan_upload_pack;
use crab_read::upload_pack_wire::{
    FetchRequest, LsRefsRequest, MAX_PACKET_BYTES, flush_cancellable, parse_fetch, parse_ls_refs,
    read_command_request, write_all_cancellable, write_delimiter, write_flush, write_packet,
    write_response_end,
};
use crab_read::{
    FetchAdmissionPolicy, UPLOAD_PACK_MAX_DURATION, UploadPackFilter, UploadPackRequest,
    plan_upload_pack_tip_bound_with_transitions, tip_bound_transitions_cover_wants,
    upload_pack_repository_options,
};
use crab_remote_git::{RemoteGitRepository, RemoteGitRuntime, RepositoryIdentity, RepositoryRefs};
use futures_util::StreamExt;
use gix_hash::ObjectId;
use rand::Rng as _;
use sha1::{Digest as _, Sha1};
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result};
use crate::storage::retry::{RetryClass, retry_class};

const READ_ADMISSION_WAIT: Duration = UPLOAD_PACK_MAX_DURATION;
const READ_ADMISSION_RETRY_BASE: Duration = Duration::from_millis(50);
const READ_ADMISSION_RETRY_CAP: Duration = Duration::from_secs(2);
const MAX_NEGOTIATED_HAVES: usize = 1_000_000;
const DIRECT_PACK_STREAM_CHUNK: usize = MAX_PACKET_BYTES.saturating_sub(5);
const DIRECT_PACK_STREAM_BUFFER: usize = 1024 * 1024;
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

enum UploadPackVisibilityProof {
    Materialized(GitVisibilityIndex),
    /// An ordinary fetch rooted at exact advertised tips.
    ///
    /// This proof is only admitted for an unfiltered, non-shallow request.
    /// The root and per-ref controls authenticate the advertised tips; the
    /// shared planner then walks the complete closure from those tips.
    TipBound {
        transitions: Arc<crab_read::capsule_protocol::CapsuleTipBoundTransitions>,
    },
}

impl UploadPackVisibilityProof {
    fn object_count_for_refs(&self, refs: &[String]) -> usize {
        match self {
            Self::Materialized(visibility) => {
                visibility.object_count_for_refs(refs.iter().map(String::as_str))
            }

            Self::TipBound { .. } => 0,
        }
    }

    fn authorization_digest_for_refs(&self, refs: &[String]) -> [u8; 32] {
        match self {
            Self::Materialized(visibility) => {
                visibility.authorization_digest_for_refs(refs.iter().map(String::as_str))
            }

            Self::TipBound { .. } => {
                let mut names = refs.iter().map(String::as_str).collect::<Vec<_>>();
                names.sort_unstable();
                let mut digest = blake3::Hasher::new();
                digest.update(b"crab.upload-pack.tip-bound.v1\0");
                for name in names {
                    digest.update(&(name.len() as u64).to_be_bytes());
                    digest.update(name.as_bytes());
                }
                *digest.finalize().as_bytes()
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

/// Serve one terminal `stateless-connect git-upload-pack` helper session.
pub async fn serve<R, W>(
    reader: &mut R,
    writer: &mut W,
    store: &crab_storage::Store,
    prefix: &str,
    hidden_ref_patterns: &[String],
    fetch_policy: &FetchAdmissionPolicy,
    progress: bool,
    capsule_root: crab_metadata::capsule_protocol::RootSnapshot,
    runtime: &Arc<RemoteGitRuntime>,
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
        capsule_root,
        runtime,
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

/// Run one non-terminal upload-pack operation under the shared reader limit.
pub(crate) async fn with_read_admission<T>(
    store: &crab_storage::Store,
    prefix: &str,
    cancellation: &CancellationToken,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    let mut admission = acquire_read_admission(store.inner(), prefix, cancellation).await?;
    let renewal_interval = (admission.ttl() / 3).max(Duration::from_secs(1));
    let mut ticker = tokio::time::interval(renewal_interval);
    ticker.tick().await;
    tokio::pin!(operation);
    let mut renewal_error = None;
    let result = loop {
        tokio::select! {
            result = &mut operation => {
                break match result {
                    Err(error) => Err(error),
                    Ok(value) => match renewal_error {
                        Some(error) => Err(CrabError::from(error)),
                        None => Ok(value),
                    },
                };
            }
            _ = ticker.tick(), if renewal_error.is_none() => {
                if let Err(error) = admission.renew().await {
                    cancellation.cancel();
                    renewal_error = Some(error);
                }
            }
        }
    };
    let release = admission.release().await.map_err(CrabError::from);
    match (result, release) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(release_error)) => {
            tracing::warn!(
                error = %release_error,
                "upload-pack read admission release failed after operation failure"
            );
            Err(error)
        }
    }
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
                source: None,
            });
        }

        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(CrabError::Cancelled),
            result = tokio::time::timeout(remaining, ticket.try_admit()) => match result {
                Ok(result) => result.map_err(CrabError::from),
                Err(_) => Err(CrabError::Throttled {
                    retry_after: Some(READ_ADMISSION_RETRY_CAP),
                    source: None,
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
                source: None,
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
    capsule_root: crab_metadata::capsule_protocol::RootSnapshot,
    runtime: &Arc<RemoteGitRuntime>,
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
        capsule_root,
        &admission,
        runtime,
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
    capsule_root: crab_metadata::capsule_protocol::RootSnapshot,
    admission: &Arc<tokio::sync::Mutex<Option<crab_coordination::ReadAdmissionTicket>>>,
    runtime: &Arc<RemoteGitRuntime>,
    cancellation: &CancellationToken,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let layered = capsule_root.record().root().checkpoint().is_some();
    let capture = crab_read::capsule_protocol::CapsuleRepositoryCapture::from_root(
        crab_storage::StoreLayout::new(store.clone(), prefix.to_owned()),
        capsule_root,
    )
    .await?;
    let refs = {
        let view = capture.ref_view()?;
        RepositoryRefs::from_maps(view.head(), view.refs(), view.peeled_refs())
            .map_err(CrabError::from)?
    };
    let mut cold_clone_pack = None;
    let mut fetch_snapshot: Option<(RemoteGitRepository, UploadPackVisibilityProof)> = None;
    let visible_ref_names = visible_ref_names(&refs, hidden_ref_patterns)?;
    // Discovery reads canonical metadata, even if payloads or derived indexes
    // are damaged. Fetch still requires its complete verified admission path.
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
    let mut negotiated_haves = Vec::new();
    let mut negotiated_have_set = HashSet::new();
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
                    &refs,
                    &visible_ref_names,
                    hidden_ref_patterns,
                    &args,
                    cancellation,
                )
                .await?;
            }
            "fetch" => {
                negotiation_rounds = negotiation_rounds.saturating_add(1);
                let mut fetch = match parse_fetch(&request.args).map_err(CrabError::from) {
                    Ok(fetch) => fetch,
                    Err(error) => {
                        return reject_protocol_request(writer, error, cancellation).await;
                    }
                };
                // Protocol-v2 sends haves in one or more negotiation rounds
                // and omits them from the terminal `done` request. Retain
                // that complete client have set before deciding whether the
                // request is a cold clone or changing the visibility proof;
                // otherwise a complete-view promotion can regenerate the
                // entire repository for an ordinary incremental fetch.
                merge_negotiated_haves(
                    &mut negotiated_haves,
                    &mut negotiated_have_set,
                    &fetch.haves,
                )?;
                if refs.is_empty() {
                    return reject_protocol_request(
                        writer,
                        protocol("fetch requested an object from an empty repository"),
                        cancellation,
                    )
                    .await;
                }
                if fetch_snapshot.is_none() {
                    let footer_only = layered
                        && tip_bound_fetch_eligible(&fetch, fetch_policy, &visible_ref_names);
                    let admitted = match open_capsule_repository_from_capture(
                        &capture,
                        footer_only,
                        fetch.done && negotiated_haves.is_empty(),
                        runtime,
                        cancellation,
                    )
                    .await
                    {
                        Ok(admitted) => admitted,
                        Err(error) => {
                            return reject_protocol_request(writer, error, cancellation).await;
                        }
                    };
                    cold_clone_pack = admitted.2;
                    fetch_snapshot = Some((admitted.0, admitted.1));
                }
                let tip_bound = fetch_snapshot.as_ref().is_some_and(|(_, proof)| {
                    matches!(proof, UploadPackVisibilityProof::TipBound { .. })
                });
                let cold_clone_needs_complete_view =
                    fetch.done && negotiated_haves.is_empty() && cold_clone_pack.is_none();
                if tip_bound
                    && (cold_clone_needs_complete_view
                        || !tip_bound_fetch_eligible(&fetch, fetch_policy, &visible_ref_names))
                {
                    // Ref heads are mutable even while the root digest stays fixed.
                    // Promotion must reuse this session's capture or a concurrent
                    // push can invalidate an object already advertised to Git.
                    // The capture pins that authority independently of the prior
                    // repository's large lookup and visibility allocations.
                    drop(fetch_snapshot.take());
                    let admitted = match open_capsule_repository_from_capture(
                        &capture,
                        false,
                        false,
                        runtime,
                        cancellation,
                    )
                    .await
                    {
                        Ok(admitted) => admitted,
                        Err(error) => {
                            return reject_protocol_request(writer, error, cancellation).await;
                        }
                    };
                    cold_clone_pack = None;
                    fetch_snapshot = Some((admitted.0, admitted.1));
                }
                if fetch.done {
                    fetch.haves = negotiated_haves.clone();
                }
                let (repository, proof) = fetch_snapshot.as_ref().ok_or_else(|| {
                    CrabError::Internal("upload-pack did not retain fetch admission".to_owned())
                })?;
                if let Err(error) = validate_fetch_admission(
                    repository,
                    proof,
                    &visible_ref_names,
                    &fetch,
                    fetch_policy,
                ) {
                    return reject_protocol_request(writer, error, cancellation).await;
                }
                if !fetch.done {
                    let common_haves = common_haves(proof, &fetch, &visible_ref_names);
                    let transition_ready = match proof {
                        UploadPackVisibilityProof::TipBound { transitions } => {
                            tip_bound_transitions_cover_wants(
                                &repository.refs().entries,
                                &visible_ref_names,
                                transitions,
                                &fetch.wants,
                                &negotiated_haves,
                            )
                        }
                        UploadPackVisibilityProof::Materialized(_) => false,
                    };
                    if common_haves.is_empty() && !transition_ready {
                        write_acknowledgments(writer, cancellation).await?;
                    } else {
                        if transition_ready {
                            // Reuse the proven cut point without ACKing historical haves
                            // as currently visible. The response still admits its full plan.
                            fetch.haves.clone_from(&negotiated_haves);
                        }
                        write_fetch_response(
                            writer,
                            repository,
                            proof,
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
                if let Some(source) = cold_clone_pack.as_ref()
                    && cold_clone_fetch_eligible(repository, &visible_ref_names, &fetch, proof)
                {
                    write_layered_cold_clone_response(
                        writer,
                        store,
                        source,
                        progress,
                        cancellation,
                    )
                    .await?;
                } else {
                    write_fetch_response(
                        writer,
                        repository,
                        proof,
                        &visible_ref_names,
                        &fetch,
                        negotiation_rounds,
                        progress,
                        None,
                        admission,
                        cancellation,
                    )
                    .await?;
                }
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

fn validate_fetch_admission(
    repository: &RemoteGitRepository,
    proof: &UploadPackVisibilityProof,
    visible_ref_names: &[String],
    request: &FetchRequest,
    policy: &FetchAdmissionPolicy,
) -> Result<()> {
    match proof {
        UploadPackVisibilityProof::Materialized(visibility) => {
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

        UploadPackVisibilityProof::TipBound { .. } => {
            if !tip_bound_fetch_eligible(request, policy, visible_ref_names) {
                return Err(protocol(
                    "tip-bound upload-pack proof cannot authorize this fetch shape",
                ));
            }
            let advertised_tips = repository
                .refs()
                .entries
                .iter()
                .filter(|reference| visible_ref_names.contains(&reference.name))
                .flat_map(|reference| [Some(reference.target), reference.peeled])
                .flatten()
                .collect::<HashSet<_>>();
            for want in &request.wants {
                if !advertised_tips.contains(want) {
                    return Err(protocol(format!(
                        "want {want} is denied by upload-pack policy"
                    )));
                }
            }
            Ok(())
        }
    }
}

fn visible_reachable_wants_allowed(request: &FetchRequest, policy: &FetchAdmissionPolicy) -> bool {
    // Partial-clone clients must request promised interior objects by OID.
    // Filtered requests remain bounded by the generation-pinned visible-ref
    // catalog, so they do not need the general raw-SHA policy opt-in.
    policy.allow_reachable_sha_in_want || !matches!(&request.filter, UploadPackFilter::None)
}

fn tip_bound_fetch_eligible(
    request: &FetchRequest,
    policy: &FetchAdmissionPolicy,
    visible_ref_names: &[String],
) -> bool {
    // Git sends include-tag for ordinary fetches even when the advertised view
    // has no tags. It is a no-op in that case; only a visible tag requires the
    // complete visibility proof needed to authorize tag-object closure.
    let include_tags_requires_complete_view = request.include_tags
        && visible_ref_names
            .iter()
            .any(|name| name.starts_with("refs/tags/"));
    policy.allow_tip_sha_in_want
        && !request.wants.is_empty()
        && !include_tags_requires_complete_view
        && request.shallow.is_empty()
        && request.deepen.is_none()
        && request.deepen_since.is_none()
        && request.deepen_not.is_empty()
        && !request.deepen_relative
        && matches!(request.filter, UploadPackFilter::None)
}

fn merge_negotiated_haves(
    accumulated: &mut Vec<ObjectId>,
    seen: &mut HashSet<ObjectId>,
    haves: &[ObjectId],
) -> Result<()> {
    for have in haves {
        if seen.contains(have) {
            continue;
        }
        if accumulated.len() >= MAX_NEGOTIATED_HAVES {
            return Err(protocol("fetch negotiation contains too many haves"));
        }
        seen.insert(*have);
        accumulated.push(*have);
    }
    Ok(())
}

/// Open one authenticated capsule root for protocol-v2 upload-pack.
///
/// Layered ordinary fetches use the checkpoint/run controls and a tip-bound
/// proof only when the exact one-pack stream can be sent directly. A
/// multi-member clone uses complete captured metadata so protocol-v2 can
/// consolidate authenticated source packs without retaining their payloads
/// before cache lookup or the bounded pack downloader. Choose that view before
/// opening the repository so cold clones do not build two lookup states.
async fn open_capsule_repository_from_capture(
    capture: &crab_read::capsule_protocol::CapsuleRepositoryCapture,
    footer_only: bool,
    cold_clone: bool,
    runtime: &Arc<RemoteGitRuntime>,
    cancellation: &CancellationToken,
) -> Result<(
    RemoteGitRepository,
    UploadPackVisibilityProof,
    Option<crab_read::capsule_protocol::LayeredColdClonePack>,
)> {
    let started = std::time::Instant::now();
    let layout = capture.layout();
    let store = layout.store();
    let prefix = layout.repo_prefix();
    let options = upload_pack_repository_options().map_err(CrabError::from)?;
    let maximum = options.operation_limits().max_fetched_bytes;
    let limits = crab_read::capsule_protocol::CapsuleReadLimits {
        max_capsule_bytes: maximum,
        max_frontier_bytes: maximum,
    };
    let (mut view, direct_pack, tip_bound) =
        if footer_only && (!cold_clone || capture.may_have_layered_cold_clone_packs()) {
            let footer_view = capture.layered_control_view(limits).await?;
            let direct_pack = footer_view
                .layered_cold_clone_pack(layout, options.operation_limits().max_response_bytes)
                .map_err(CrabError::from)?;
            if cold_clone && direct_pack.is_none() {
                drop(footer_view);
                (capture.metadata_view(limits).await?, None, false)
            } else {
                (footer_view, direct_pack, true)
            }
        } else {
            (capture.metadata_view(limits).await?, None, false)
        };
    tracing::debug!(
        footer_only,
        cold_clone,
        tip_bound,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "upload-pack captured view opened"
    );
    let bucket = store.bucket_identity();
    let provider = format!("{:?}:{}:{}", bucket.cloud, bucket.host, bucket.container);
    let identity =
        RepositoryIdentity::new(provider, prefix.to_owned(), 1).map_err(CrabError::from)?;
    let proof = if tip_bound {
        UploadPackVisibilityProof::TipBound {
            transitions: Arc::new(view.tip_bound_transitions().await?),
        }
    } else {
        UploadPackVisibilityProof::Materialized(view.take_git_visibility_index().await?)
    };
    tracing::debug!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "upload-pack visibility proof opened"
    );
    let repository = view
        .git_repository_from_store(
            layout.clone(),
            identity,
            Arc::clone(runtime),
            options,
            maximum,
            cancellation,
            None,
        )
        .await?;
    tracing::debug!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        "upload-pack repository opened"
    );
    let lease_provider = ObjectStoreGeneratedPackLeaseProvider {
        store: Arc::clone(store.inner()),
        prefix: prefix.to_owned(),
    };
    Ok((
        repository.with_generated_pack_lease_provider(Arc::new(lease_provider)),
        proof,
        direct_pack,
    ))
}

pub(crate) fn visible_ref_names(
    refs: &RepositoryRefs,
    hidden_ref_patterns: &[String],
) -> Result<Vec<String>> {
    let hidden = compile_hidden_refs(hidden_ref_patterns)?;
    Ok(refs
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
        b"fetch=shallow deepen deepen-relative deepen-since deepen-not filter thin-pack no-progress include-tag ofs-delta\n",
        cancellation,
    )
    .await?;
    write_flush(writer, cancellation)
        .await
        .map_err(CrabError::from)
}

async fn write_ls_refs<W: AsyncWrite + Unpin>(
    writer: &mut W,
    refs: &RepositoryRefs,
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
        && refs.head.as_ref().is_some_and(|head| {
            visible_ref_names.iter().any(|name| name == &head.name)
                && (matches_prefix("HEAD") || matches_prefix(&head.name))
        })
    {
        let head = refs.head.as_ref().ok_or_else(|| protocol("missing HEAD"))?;
        let line = format!("{} HEAD symref-target:{}\n", head.target, head.name);
        write_data(writer, line.as_bytes(), cancellation).await?;
    }
    let hidden = compile_hidden_refs(hidden_ref_patterns)?;
    if request.unborn
        && let Some(target) = refs.unborn_head.as_deref()
        && !hidden.is_match(target)
        && (matches_prefix("HEAD") || matches_prefix(target))
    {
        let line = format!("unborn HEAD symref-target:{target}\n");
        write_data(writer, line.as_bytes(), cancellation).await?;
    }
    for reference in &refs.entries {
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

fn common_haves(
    proof: &UploadPackVisibilityProof,
    request: &FetchRequest,
    visible_ref_names: &[String],
) -> Vec<ObjectId> {
    match proof {
        UploadPackVisibilityProof::Materialized(visibility) => request
            .haves
            .iter()
            .copied()
            .filter(|have| {
                have.as_bytes().try_into().ok().is_some_and(|oid| {
                    visibility.contains_for_refs(visible_ref_names.iter().map(String::as_str), &oid)
                })
            })
            .collect(),
        // Historical transition bases can finish negotiation without an ACK.
        UploadPackVisibilityProof::TipBound { .. } => Vec::new(),
    }
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

fn cold_clone_fetch_eligible(
    repository: &RemoteGitRepository,
    visible_ref_names: &[String],
    request: &FetchRequest,
    proof: &UploadPackVisibilityProof,
) -> bool {
    if !matches!(proof, UploadPackVisibilityProof::TipBound { .. })
        || !request.done
        || !request.haves.is_empty()
        || !request.shallow.is_empty()
        || request.deepen.is_some()
        || request.deepen_since.is_some()
        || !request.deepen_not.is_empty()
        || request.deepen_relative
        || !matches!(request.filter, UploadPackFilter::None)
    {
        return false;
    }
    let visible = repository
        .refs()
        .entries
        .iter()
        .filter(|reference| visible_ref_names.iter().any(|name| name == &reference.name))
        .flat_map(|reference| [Some(reference.target), reference.peeled])
        .flatten()
        .collect::<HashSet<_>>();
    visible_ref_names.len() == repository.refs().entries.len()
        && !visible.is_empty()
        && visible.iter().all(|tip| request.wants.contains(tip))
}

async fn write_layered_cold_clone_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    store: &crab_storage::Store,
    source: &crab_read::capsule_protocol::LayeredColdClonePack,
    progress: bool,
    cancellation: &CancellationToken,
) -> Result<()> {
    let (metadata, returned_range, mut stream) = store
        .get_parallel_range_stream(
            &source.source_path,
            source.pack_range.clone(),
            None,
            cancellation,
        )
        .await
        .map_err(CrabError::from)?;
    if returned_range != source.pack_range || metadata.size < source.pack_range.end {
        return Err(CrabError::CorruptObject {
            path: source.source_path.to_string(),
            reason: "layered cold-clone pack range changed while reading".to_owned(),
        });
    }
    write_data(writer, b"packfile\n", cancellation).await?;
    if progress {
        write_packet(writer, b"counting objects\n", Some(2), cancellation).await?;
    }

    let expected_size = source
        .pack_range
        .end
        .checked_sub(source.pack_range.start)
        .ok_or_else(|| CrabError::CorruptObject {
            path: source.source_path.to_string(),
            reason: "layered cold-clone pack range underflowed".to_owned(),
        })?;
    let mut written = 0_u64;
    let mut header = Vec::with_capacity(12);
    let mut trailer = [0_u8; 20];
    let mut trailer_len = 0_usize;
    let mut git_hasher = Sha1::new();
    let mut content_hasher = blake3::Hasher::new();
    let mut sideband_buffer = Vec::with_capacity(DIRECT_PACK_STREAM_BUFFER);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(CrabError::from)?;
        written =
            written
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| CrabError::CorruptObject {
                    path: source.source_path.to_string(),
                    reason: "layered cold-clone pack size overflowed".to_owned(),
                })?;
        if written > expected_size {
            return Err(CrabError::CorruptObject {
                path: source.source_path.to_string(),
                reason: "layered cold-clone pack stream exceeded its descriptor".to_owned(),
            });
        }
        content_hasher.update(&chunk);
        if header.len() < 12 {
            let remaining = 12 - header.len();
            header.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        }
        update_pack_body_hash(&mut git_hasher, &mut trailer, &mut trailer_len, &chunk);
        for packet in chunk.chunks(DIRECT_PACK_STREAM_CHUNK) {
            let length = packet
                .len()
                .checked_add(5)
                .ok_or_else(|| protocol("layered cold-clone packet length overflowed"))?;
            if length > MAX_PACKET_BYTES {
                return Err(protocol(
                    "layered cold-clone packet exceeds the protocol bound",
                ));
            }
            sideband_buffer.extend_from_slice(&packet_line_prefix(length));
            sideband_buffer.push(1);
            sideband_buffer.extend_from_slice(packet);
            if sideband_buffer.len() >= DIRECT_PACK_STREAM_BUFFER {
                write_all_cancellable(writer, &sideband_buffer, cancellation).await?;
                sideband_buffer.clear();
            }
        }
    }
    if !sideband_buffer.is_empty() {
        write_all_cancellable(writer, &sideband_buffer, cancellation).await?;
    }
    if written != expected_size || header.len() != 12 || trailer_len != 20 {
        return Err(CrabError::CorruptObject {
            path: source.source_path.to_string(),
            reason: "layered cold-clone pack stream was truncated".to_owned(),
        });
    }
    let expected_objects =
        u32::try_from(source.object_count).map_err(|_| CrabError::CorruptObject {
            path: source.source_path.to_string(),
            reason: "layered cold-clone object count exceeds Git's pack limit".to_owned(),
        })?;
    let version = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let object_count = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    if &header[..4] != b"PACK" || !matches!(version, 2 | 3) || object_count != expected_objects {
        return Err(CrabError::CorruptObject {
            path: source.source_path.to_string(),
            reason: "layered cold-clone pack header does not match its descriptor".to_owned(),
        });
    }
    let content_hash = content_hasher.finalize().to_hex().to_string();
    if content_hash != source.content_hash {
        return Err(CrabError::CorruptObject {
            path: source.source_path.to_string(),
            reason: "layered cold-clone pack content hash does not match its descriptor".to_owned(),
        });
    }
    let git_checksum = git_hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let trailer_checksum = trailer
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if git_checksum != source.git_checksum || trailer_checksum != source.git_checksum {
        return Err(CrabError::CorruptObject {
            path: source.source_path.to_string(),
            reason: "layered cold-clone Git checksum does not match its descriptor".to_owned(),
        });
    }
    write_flush(writer, cancellation).await?;
    write_response_end(writer, cancellation)
        .await
        .map_err(CrabError::from)
}

fn packet_line_prefix(length: usize) -> [u8; 4] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    [
        HEX[(length >> 12) & 0xf],
        HEX[(length >> 8) & 0xf],
        HEX[(length >> 4) & 0xf],
        HEX[length & 0xf],
    ]
}

fn update_pack_body_hash(
    hasher: &mut Sha1,
    trailer: &mut [u8; 20],
    trailer_len: &mut usize,
    chunk: &[u8],
) {
    if chunk.is_empty() {
        return;
    }
    let total = trailer_len.saturating_add(chunk.len());
    if total <= trailer.len() {
        trailer[*trailer_len..total].copy_from_slice(chunk);
        *trailer_len = total;
        return;
    }

    let body_len = total - trailer.len();
    if body_len < *trailer_len {
        hasher.update(&trailer[..body_len]);
        let retained_len = *trailer_len - body_len;
        trailer.copy_within(body_len..*trailer_len, 0);
        trailer[retained_len..].copy_from_slice(chunk);
    } else {
        hasher.update(&trailer[..*trailer_len]);
        let chunk_body_len = body_len - *trailer_len;
        hasher.update(&chunk[..chunk_body_len]);
        trailer.copy_from_slice(&chunk[chunk_body_len..]);
    }
    *trailer_len = trailer.len();
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

    let producer = |producer_cancellation: CancellationToken| async move {
        let cancellation = &producer_cancellation;
        let plan = match proof {
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

            UploadPackVisibilityProof::TipBound { transitions } => {
                plan_upload_pack_tip_bound_with_transitions(
                    repository,
                    visible_ref_names,
                    semantic_request,
                    Some(transitions.as_ref()),
                    cancellation,
                )
                .await
            }
        }
        .map_err(CrabError::from)?;
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
            .map_err(CrabError::from)
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
            return reject_protocol_request(writer, CrabError::from(error), cancellation).await;
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
        .map_err(CrabError::from)?;
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
        deepen_since: request.deepen_since,
        deepen_not: request.deepen_not.clone(),
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

        UploadPackVisibilityProof::TipBound { transitions } => {
            plan_upload_pack_tip_bound_with_transitions(
                repository,
                visible_ref_names,
                &semantic_request,
                Some(transitions.as_ref()),
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
            let error = CrabError::from(error);
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
    let allow_external_bases = external_thin_pack_eligible(request, &plan);
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
        if allow_external_bases {
            repository
                .generate_pack_with_external_bases(&plan.object_ids, thin_bases, cancellation)
                .await
        } else {
            repository
                .generate_pack_with_bases(&plan.object_ids, thin_bases, cancellation)
                .await
        }
    };
    let pack = match generated {
        Ok(pack) => pack,
        Err(error) => {
            return reject_pack_response(writer, CrabError::from(error), cancellation).await;
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
        .map_err(CrabError::from)?;
    write_flush(writer, cancellation).await?;
    write_response_end(writer, cancellation)
        .await
        .map_err(CrabError::from)
}

fn request_pack_preplanning_cache_eligible(request: &FetchRequest) -> bool {
    !request.haves.is_empty()
        && !request.shallow.is_empty()
        && request.deepen.is_none()
        && request.deepen_since.is_none()
        && request.deepen_not.is_empty()
        && !request.deepen_relative
        && matches!(request.filter, UploadPackFilter::None)
}

fn external_thin_pack_eligible(request: &FetchRequest, plan: &crab_read::PackPlan) -> bool {
    request.thin_pack
        && request.shallow.is_empty()
        && request.deepen.is_none()
        && request.deepen_since.is_none()
        && request.deepen_not.is_empty()
        && !request.deepen_relative
        && matches!(request.filter, UploadPackFilter::None)
        && !plan.common_haves.is_empty()
        && plan.common_haves.len() == request.haves.len()
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
    match request.deepen_since {
        Some(timestamp) => {
            hash.update(&[1]);
            hash.update(&timestamp.to_be_bytes());
        }
        None => {
            hash.update(&[0]);
        }
    }
    let mut deepen_not = request.deepen_not.clone();
    deepen_not.sort_unstable();
    hash.update(&(deepen_not.len() as u64).to_be_bytes());
    for reference in deepen_not {
        hash.update(&(reference.len() as u64).to_be_bytes());
        hash.update(reference.as_bytes());
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
        && request.deepen_since.is_none()
        && request.deepen_not.is_empty()
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

async fn reject_pack_response<W: AsyncWrite + Unpin>(
    writer: &mut W,
    error: CrabError,
    cancellation: &CancellationToken,
) -> Result<()> {
    // Git may disconnect after a fatal sideband. Reporting failure cannot
    // replace the integrity or origin failure that stopped pack generation.
    let report = async {
        write_packet(writer, error.to_string().as_bytes(), Some(3), cancellation).await?;
        write_flush(writer, cancellation).await?;
        write_response_end(writer, cancellation).await
    }
    .await;
    if let Err(write_error) = report {
        tracing::warn!(error = %write_error, "failed to write upload-pack fatal sideband");
    }
    Err(error)
}

fn protocol(message: impl Into<String>) -> CrabError {
    CrabError::Protocol(message.into())
}

#[cfg(test)]
#[path = "upload_pack_wire_tests.rs"]
mod tests;
