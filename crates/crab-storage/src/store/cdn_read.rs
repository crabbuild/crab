use std::{
    future::Future,
    ops::Range,
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime},
};

use bytes::Bytes;
use futures_util::{StreamExt, future::try_join_all, stream};
use object_store::path::Path;
use tokio::{io::AsyncWriteExt, time::Instant};
use tokio_util::sync::CancellationToken;

use super::{StorageReadKind, Store};
use crate::{
    Result, StorageError,
    cdn::{
        CdnAccess, CdnGet, CdnReadFamily, CdnRoute, CdnViewer, PayloadObservation,
        PublicTransitionObservation,
    },
    transport_read_admission,
};

#[derive(Debug, thiserror::Error)]
#[error("CDN read stopped: {0}")]
struct TerminalCdnRead(#[source] StorageError);

/// Find a terminal edge failure through storage, cache and reconstruction wrappers.
#[must_use]
pub fn cdn_read_rejection<'a>(
    mut error: &'a (dyn std::error::Error + 'static),
) -> Option<&'a StorageError> {
    loop {
        if let Some(terminal) = error.downcast_ref::<TerminalCdnRead>() {
            return Some(&terminal.0);
        }
        error = error.source()?;
    }
}

fn terminal_cdn_read(source: StorageError) -> StorageError {
    StorageError::ObjectStore {
        source: object_store::Error::Generic {
            store: "CDN",
            source: Box::new(TerminalCdnRead(source)),
        },
    }
}

fn cdn_deadline_exceeded() -> StorageError {
    terminal_cdn_read(StorageError::Io {
        source: std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "immutable read deadline exceeded",
        ),
    })
}

/// Upper limit for one repository-owned immutable object in a diagnostic probe.
pub const CDN_PROBE_MAX_BYTES: u64 = 1024 * 1024;

/// Redacted routing info for an attached CDN binding.
pub struct CdnBindingInfo<'a> {
    pub authority: &'a str,
    pub repository_prefix: &'a str,
    pub private: bool,
}

/// Independent results for an explicit read-only CDN probe.
pub struct CdnProbeResult {
    pub viewer_expires_at: Option<SystemTime>,
    pub whole_object: Result<()>,
    pub range: Result<()>,
    pub mutable_status: Option<Result<u16>>,
}

impl Store {
    /// Attach an authorized CDN route when this build can use its endpoint.
    pub fn with_cdn_route(mut self, mut route: CdnRoute) -> Result<Self> {
        if self.read_prefix_target_identity(&Path::ROOT) != Some(&route.target) {
            return Err(StorageError::Forbidden {
                path: "CDN origin does not match the selected storage target".into(),
            });
        }
        if let Some(scope) = &self.storage_scope
            && (matches!(route.access, CdnAccess::Public)
                || route.layout.repo_prefix() != scope.repo_prefix
                || (route.allow_global && route.layout.global_prefix() != scope.global_prefix))
        {
            return Err(StorageError::Forbidden {
                path: "CDN route does not match the authorized repository view".into(),
            });
        }
        if matches!(route.access, CdnAccess::Public) {
            match &route.publication {
                Some(receipt)
                    if self.native_cdn_origin() != Some(receipt.source_origin.as_str()) =>
                {
                    return Err(StorageError::Forbidden {
                        path: "CDN publication receipt does not match the selected native origin"
                            .into(),
                    });
                }
                None if !route.transport.is_loopback() => return Ok(self.without_cdn_route()),
                _ => {}
            }
        }
        route.provider = self.bucket_identity().cloud;
        if let Some(policy) = route.policy() {
            route.families = policy.families();
        }
        if matches!(route.access, CdnAccess::Public)
            && !route.transport.is_loopback()
            && !route.families.is_empty()
            && !route
                .publication_origin
                .as_ref()
                .zip(self.native_cdn_origin.as_ref())
                .is_some_and(|(approved, selected)| Arc::ptr_eq(approved, selected))
        {
            return Err(StorageError::Forbidden {
                path: "CDN publication requires approval from the selected native authority".into(),
            });
        }
        self.cdn =
            (route.transport.is_loopback() || !route.families.is_empty()).then(|| Arc::new(route));
        Ok(self)
    }

    /// Preserve a route and its grant when wrapping the same physical target.
    #[must_use]
    pub fn inherit_cdn_route(mut self, origin: &Self) -> Self {
        if self.target_identity.is_some()
            && self.target_identity == origin.target_identity
            && self.storage_scope == origin.storage_scope
        {
            self.cdn = origin.cdn.clone();
            self.read_authorization
                .clone_from(&origin.read_authorization);
            self.direct_read_admissions
                .clone_from(&origin.direct_read_admissions);
        }
        self
    }

    /// Use the canonical provider for strict origin proofs or target selection.
    #[must_use]
    pub fn without_cdn_route(mut self) -> Self {
        self.cdn = None;
        self
    }

    /// Report the selected binding without exposing credentials or the URL path.
    #[must_use]
    pub fn cdn_binding(&self) -> Option<CdnBindingInfo<'_>> {
        self.cdn.as_ref().map(|route| CdnBindingInfo {
            authority: route.transport.authority(),
            repository_prefix: route.layout.repo_prefix(),
            private: matches!(&route.access, CdnAccess::Private(_)),
        })
    }

    /// Fingerprint the attached viewer URL and access policy without exposing grants.
    #[must_use]
    pub fn cdn_binding_sha256(&self) -> Option<String> {
        self.cdn.as_ref().map(|route| {
            route
                .transport
                .binding_sha256(matches!(&route.access, CdnAccess::Private(_)))
        })
    }

    /// Report the verified public export receipt attached to the selected CDN route.
    #[must_use]
    pub fn cdn_public_export_sha256(&self) -> Option<String> {
        self.cdn
            .as_ref()?
            .publication
            .as_ref()
            .map(|receipt| format!("{:x}", receipt.approval_digest))
    }

    /// Report a checksum-verified Git blob returned from an in-process cache.
    pub fn observe_verified_git_cache_hit(&self, oid: &[u8]) {
        let Some(route) = &self.cdn else {
            return;
        };
        let family = CdnReadFamily::Git;
        if (route.transport.is_loopback() || route.families.contains(&family))
            && let Some(policy) = route.policy()
            && let Some(observation) = PayloadObservation::new(policy, family, None)
        {
            observation.record_local_git_blob(oid);
        }
    }

    /// Report a verified xorb cache hit after authorizing the selected physical key.
    pub fn observe_verified_xorb_cache_hit(&self, path: &Path) {
        if let Some((route, CdnReadFamily::Xorb)) = self.cdn_route_for(path)
            && let Some(policy) = route.policy()
            && let Some(observation) = PayloadObservation::new(policy, CdnReadFamily::Xorb, None)
        {
            observation.record_local_xorb(path.as_ref());
        }
    }

    /// Report a SHA-256-verified local LFS object for the active CDN route.
    pub fn observe_verified_lfs_cache_hit(&self, path: &Path, size: u64) {
        if let Some(route) = self.cdn_lfs_cache_route(path, size)
            && let Some(policy) = route.policy()
            && let Some(observation) = PayloadObservation::new(policy, CdnReadFamily::Lfs, None)
        {
            observation.record_local_lfs(path.as_ref());
        }
    }

    /// Derive a cache namespace for an LFS key eligible under the active route.
    ///
    /// The caller must authorize repository access before opening cached bytes.
    #[must_use]
    pub fn cdn_lfs_cache_namespace(&self, path: &Path, size: u64) -> Option<[u8; 32]> {
        let route = self.cdn_lfs_cache_route(path, size)?;
        let mut hash = blake3::Hasher::new_derive_key("crab CDN LFS cache v1");
        hash.update(&route.target);
        hash.update(blake3::hash(route.layout.repo_prefix().as_bytes()).as_bytes());
        let binding = route
            .transport
            .binding_sha256(matches!(&route.access, CdnAccess::Private(_)));
        hash.update(binding.as_bytes());
        if let Some(receipt) = &route.publication {
            hash.update(&receipt.approval_digest);
        }
        Some(*hash.finalize().as_bytes())
    }

    /// Read and verify one immutable unit, optionally using the scoped CDN route.
    ///
    /// Size and range must come from an authenticated descriptor. The verifier
    /// must only perform format and integrity checks. It can run twice when the
    /// first source is corrupt. Publish only the verified value.
    pub fn read_verified<'a, T, E, F, Fut>(
        &'a self,
        path: &'a Path,
        size: u64,
        range: Option<Range<u64>>,
        verify: F,
    ) -> futures_util::future::BoxFuture<'a, std::result::Result<T, E>>
    where
        T: Send + 'a,
        E: From<StorageError> + Send + 'a,
        F: Fn(Bytes) -> Fut + Send + Sync + 'a,
        Fut: Future<Output = std::result::Result<T, E>> + Send + 'a,
    {
        Box::pin(async move {
            let origin = async {
                let bytes = match &range {
                    Some(range) => self.range_get(path, range.clone()).await?,
                    None => self.get_immutable_bounded(path, size).await?,
                };
                verify(bytes).await
            };
            let Some(attempt) = self.cdn_attempt(path) else {
                return origin.await;
            };

            let tail = async {
                match attempt.get(size, range.clone()).await? {
                    Some(bytes) => match verify(bytes).await {
                        Ok(value) => Ok(value),
                        Err(_) => {
                            tracing::warn!(
                                "CDN immutable bytes failed verification; reading from origin"
                            );
                            origin.await
                        }
                    },
                    None => origin.await,
                }
            };
            attempt.within_deadline(tail).await
        })
    }

    fn cdn_route_for(&self, path: &Path) -> Option<(&Arc<CdnRoute>, CdnReadFamily)> {
        let route = self.cdn.as_ref()?;
        let family = route.layout.cdn_read_family(path)?;
        if self.read_prefix_target_identity(path) != Some(&route.target)
            || (family.global() && !route.allow_global)
            || (!route.transport.is_loopback() && !route.families.contains(&family))
        {
            return None;
        }
        Some((route, family))
    }

    fn cdn_lfs_cache_route(&self, path: &Path, size: u64) -> Option<&CdnRoute> {
        let (route, family) = self.cdn_route_for(path)?;
        (family == CdnReadFamily::Lfs
            && route
                .publication
                .as_ref()
                .is_none_or(|receipt| receipt.sizes.get(path.as_ref()) == Some(&size)))
        .then_some(route.as_ref())
    }

    /// Start one edge-only verification attempt for an authorized immutable key.
    #[must_use]
    pub fn cdn_attempt(&self, path: &Path) -> Option<CdnReadAttempt> {
        let (route, family) = self.cdn_route_for(path)?;
        if let Some(receipt) = &route.publication {
            let listed = receipt.sizes.contains_key(path.as_ref());
            if let Some(policy) = route.policy()
                && let Some(observation) =
                    PublicTransitionObservation::new(receipt.approval_digest, path.as_ref())
            {
                observation.record(
                    policy,
                    family,
                    if listed { "listed" } else { "unlisted" },
                    None,
                );
            }
            if !listed {
                return None;
            }
        }
        Some(CdnReadAttempt {
            store: self.clone(),
            route: Arc::clone(route),
            family,
            path: path.clone(),
            deadline: OnceLock::new(),
        })
    }

    /// Observe a verified native blob whose pack key the public export does not list.
    pub fn observe_verified_unlisted_git_blob(&self, path: &Path, oid: &[u8]) {
        if let Some((route, CdnReadFamily::Git)) = self.cdn_route_for(path)
            && let Some(receipt) = &route.publication
            && !receipt.sizes.contains_key(path.as_ref())
            && let Some(policy) = route.policy()
            && let Some(observation) =
                PublicTransitionObservation::new(receipt.approval_digest, path.as_ref())
        {
            observation.record(policy, CdnReadFamily::Git, "verified_native", Some(oid));
        }
    }

    async fn cdn_read(
        &self,
        route: &CdnRoute,
        request: CdnGet<'_>,
        can_restart: bool,
    ) -> Result<Option<Bytes>> {
        if route
            .publication
            .as_ref()
            .is_some_and(|receipt| receipt.sizes.get(request.path.as_ref()) != Some(&request.size))
        {
            // New native objects can become visible before an export updates.
            return Ok(None);
        }
        let cancel = CancellationToken::new();
        let get = |viewer, timeout: Duration| {
            self.record_read_request(if request.range.is_some() {
                StorageReadKind::Range
            } else {
                StorageReadKind::Get
            });
            route
                .transport
                .get(request.clone(), viewer, timeout, &cancel)
        };
        let path = request.path;
        let credential = match &route.access {
            CdnAccess::Public => None,
            CdnAccess::Private(provider) => {
                let Some(viewer) = provider.credential(path, None).await? else {
                    return Ok(None);
                };
                Some((provider, viewer))
            }
        };
        let viewer = credential
            .as_ref()
            .map_or(CdnViewer::Public, |(_, viewer)| viewer.header());
        let bytes = match (get(viewer, route.edge_timeout()).await, credential.as_ref()) {
            (Err(StorageError::Forbidden { .. }), Some((provider, rejected))) => {
                let current = provider
                    .credential(path, Some(rejected.generation()))
                    .await?
                    .ok_or_else(|| StorageError::Forbidden {
                        path: path.to_string(),
                    })?;
                get(current.header(), route.edge_timeout()).await?
            }
            // Only the initial HTTP request can authorize a restart. A
            // credential failure or a failed edge recheck must stay terminal.
            (
                Err(StorageError::NetworkTransient { .. } | StorageError::CorruptObject { .. }),
                _,
            ) if can_restart => {
                return Ok(None);
            }
            (outcome, _) => outcome?,
        };
        self.record_read_bytes(bytes.len() as u64);
        Ok(Some(bytes))
    }
}

/// One pinned CDN source with an operation-wide deadline and no origin fallback.
pub struct CdnReadAttempt {
    store: Store,
    route: Arc<CdnRoute>,
    family: CdnReadFamily,
    path: Path,
    deadline: OnceLock<Instant>,
}

impl CdnReadAttempt {
    /// Capture the edge source for a caller-owned payload verification.
    #[must_use]
    pub fn payload_observation(&self) -> Option<PayloadObservation> {
        let public = self.route.publication.as_ref().and_then(|receipt| {
            PublicTransitionObservation::new(receipt.approval_digest, self.path.as_ref())
        });
        PayloadObservation::new(self.route.policy()?, self.family, public)
    }

    /// Report the exact edge LFS object after validating its full SHA-256 and size.
    pub fn observe_verified_lfs(&self) {
        if self.family == CdnReadFamily::Lfs
            && let Some(observation) = self.payload_observation()
        {
            observation.record_verified_lfs(self.path.as_ref());
        }
    }

    /// Record a nonempty xorb payload after verifying the requested chunks.
    pub fn observe_verified_xorb(&self) {
        if self.family == CdnReadFamily::Xorb
            && let Some(observation) = self.payload_observation()
        {
            observation.record_verified_xorb(self.path.as_ref());
        }
    }

    /// Report verified xorb chunks supplied by native storage during a CDN-aware read.
    pub fn observe_verified_origin_xorb(&self) {
        if self.family == CdnReadFamily::Xorb
            && let Some(observation) = self.payload_observation()
        {
            observation.record_origin_xorb(self.path.as_ref());
        }
    }

    /// Check direct edge delivery against caller-verified bytes from the native target.
    ///
    /// For an operator-authorized diagnostic deployment only. The mutable root
    /// probe uses the same viewer as the immutable reads and inspects HTTP status.
    pub async fn probe(
        &self,
        expected: &Bytes,
        cancel: &CancellationToken,
    ) -> Result<CdnProbeResult> {
        let size = expected.len() as u64;
        let mutable = self.route.layout.capsule_root_path();
        if !(2..=CDN_PROBE_MAX_BYTES).contains(&size)
            || self.family.global()
            || self.store.read_prefix_target_identity(&mutable) != Some(&self.route.target)
            || self
                .route
                .publication
                .as_ref()
                .is_some_and(|receipt| receipt.sizes.get(self.path.as_ref()) != Some(&size))
        {
            return Err(StorageError::Forbidden {
                path: "CDN probe requires a bounded immutable object in the selected repository"
                    .into(),
            });
        }
        let work =
            transport_read_admission::scope_direct(&self.store.direct_read_admissions, async {
                let _permit = self
                    .store
                    .parallel_reads
                    .acquire()
                    .await
                    .map_err(|_| StorageError::Cancelled)?;
                let credential = match &self.route.access {
                    CdnAccess::Public => None,
                    CdnAccess::Private(provider) => Some(
                        provider
                            .credential(&self.path, None)
                            .await?
                            .ok_or_else(|| StorageError::Forbidden {
                                path: "CDN viewer grant excludes the diagnostic object".into(),
                            })?,
                    ),
                };
                let viewer = || {
                    credential
                        .as_ref()
                        .map_or(CdnViewer::Public, |value| value.header())
                };
                let timeout = self.route.edge_timeout();
                let verify = |result: Result<Bytes>, expected: &[u8]| {
                    let body = result?;
                    self.store.record_read_bytes(body.len() as u64);
                    if body.as_ref() != expected {
                        return Err(StorageError::CorruptObject {
                            path: "CDN diagnostic object".into(),
                            reason: "edge bytes disagree with the verified origin".into(),
                        });
                    }
                    Ok(())
                };
                self.store.record_read_request(StorageReadKind::Get);
                let whole_object = verify(
                    self.route
                        .transport
                        .get(
                            CdnGet::diagnostic(&self.path, size, None),
                            viewer(),
                            timeout,
                            cancel,
                        )
                        .await,
                    expected,
                );
                self.store.record_read_request(StorageReadKind::Range);
                let range = verify(
                    self.route
                        .transport
                        .get(
                            CdnGet::diagnostic(&self.path, size, Some(0..1)),
                            viewer(),
                            timeout,
                            cancel,
                        )
                        .await,
                    &expected[..1],
                );
                // A denied mutable request only proves the route boundary
                // after the same viewer has demonstrated authorized reads.
                let mutable_status = if whole_object.is_ok() && range.is_ok() {
                    self.store.record_read_request(StorageReadKind::Range);
                    Some(
                        self.route
                            .transport
                            .probe_mutable_status(&mutable, viewer(), timeout)
                            .await,
                    )
                } else {
                    None
                };
                Ok(CdnProbeResult {
                    viewer_expires_at: credential.and_then(|value| value.expires_at),
                    whole_object,
                    range,
                    mutable_status,
                })
            });
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(StorageError::Cancelled),
            outcome = self.within_deadline(work) => outcome,
        }
    }

    fn deadline(&self) -> Instant {
        *self
            .deadline
            .get_or_init(|| Instant::now() + self.route.timeout)
    }

    /// Check the shared deadline before returning or persisting verified bytes.
    pub fn check_deadline(&self) -> Result<()> {
        if Instant::now() >= self.deadline() {
            return Err(cdn_deadline_exceeded());
        }
        Ok(())
    }

    /// Bound interruptible I/O or verification to this attempt's shared deadline.
    pub async fn within_deadline<T, E: From<StorageError>>(
        &self,
        work: impl Future<Output = std::result::Result<T, E>>,
    ) -> std::result::Result<T, E> {
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(self.deadline()) => Err(cdn_deadline_exceeded().into()),
            result = work => {
                let value = result?;
                self.check_deadline()?;
                Ok(value)
            }
        }
    }

    /// Signal deadline expiry to a file transfer and wait for it to drain.
    ///
    /// The transfer must observe cancellation during remote I/O and finish any
    /// in-flight writes before returning. Its destination must be caller-private.
    pub async fn with_file_deadline<T, E: From<StorageError>>(
        &self,
        cancel: &CancellationToken,
        transfer: impl AsyncFnOnce(&CancellationToken) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        let transfer_cancel = cancel.child_token();
        let work = transfer(&transfer_cancel);
        tokio::pin!(work);
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(self.deadline()) => {
                transfer_cancel.cancel();
                let _ = work.await;
                Err(cdn_deadline_exceeded().into())
            }
            result = &mut work => {
                let value = result?;
                self.check_deadline()?;
                Ok(value)
            }
        }
    }

    /// Fetch exact bytes for caller-owned verification. None means the entire
    /// attempt may be restarted at the selected origin.
    pub fn get(
        &self,
        size: u64,
        range: Option<Range<u64>>,
    ) -> futures_util::future::BoxFuture<'_, Result<Option<Bytes>>> {
        self.fetch(size, range, true)
    }

    /// Fetch subsequent stream bytes after committing to the CDN source.
    pub async fn get_after_delivery(&self, size: u64, range: Range<u64>) -> Result<Bytes> {
        self.fetch(size, Some(range), false).await?.ok_or_else(|| {
            terminal_cdn_read(StorageError::Forbidden {
                path: "CDN viewer no longer authorizes the object".into(),
            })
        })
    }

    fn fetch(
        &self,
        size: u64,
        range: Option<Range<u64>>,
        can_restart: bool,
    ) -> futures_util::future::BoxFuture<'_, Result<Option<Bytes>>> {
        Box::pin(async move {
            let request =
                transport_read_admission::scope_direct(&self.store.direct_read_admissions, async {
                    let _permit = self
                        .store
                        .parallel_reads
                        .acquire()
                        .await
                        .map_err(|_| StorageError::Cancelled)?;
                    self.store
                        .cdn_read(
                            &self.route,
                            CdnGet {
                                path: &self.path,
                                size,
                                range,
                                context: self.route.policy().map(|policy| (policy, self.family)),
                            },
                            can_restart,
                        )
                        .await
                });
            self.within_deadline(async { request.await.map_err(terminal_cdn_read) })
                .await
        })
    }

    /// Fetch ordered, bounded batches from one authenticated object range.
    /// A recoverable failure in a batch returns `None`. Any terminal denial in
    /// that batch wins over restart. Stop consuming on either outcome.
    pub fn range_batches(
        &self,
        size: u64,
        range: Range<u64>,
    ) -> Result<impl futures_util::Stream<Item = Result<Option<Vec<Bytes>>>> + '_> {
        const RANGE_BYTES: u64 = 8 * 1024 * 1024;
        if range.start > range.end || range.end > size {
            return Err(StorageError::CorruptObject {
                path: self.path.to_string(),
                reason: "file range exceeds the authenticated object bounds".into(),
            });
        }
        let ranges = (range.start..range.end)
            .step_by(RANGE_BYTES as usize)
            .map(move |start| Some(start..start.saturating_add(RANGE_BYTES).min(range.end)));
        // For an empty object, still read the authorized zero-byte body.
        let empty = range.is_empty().then_some((size != 0).then_some(range));
        Ok(stream::iter(ranges.chain(empty))
            .chunks(super::parallel_read::CONCURRENCY)
            .then(move |batch| async move {
                let results =
                    try_join_all(batch.into_iter().map(|range| self.get(size, range))).await?;
                Ok(results.into_iter().collect())
            }))
    }

    /// Write one immutable range to a caller-private staging destination.
    /// The caller must verify the final digest before publishing the file.
    /// A recoverable edge failure returns false and clears any written bytes.
    /// Cancellation drains writes. Await completion before reusing the path.
    pub async fn download(
        &self,
        size: u64,
        range: Range<u64>,
        destination: &std::path::Path,
        cancel: &CancellationToken,
        mut update_digest: impl FnMut(&[u8]),
    ) -> Result<bool> {
        let batches = self.range_batches(size, range)?;
        tokio::pin!(batches);
        let mut file = None::<tokio::fs::File>;
        let result = async {
            loop {
                let next = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(StorageError::Cancelled),
                    next = batches.next() => next,
                };
                let Some(batch) = next else { break };
                let Some(chunks) = batch? else {
                    return Ok(false);
                };
                if file.is_none() {
                    file = Some(tokio::fs::File::create(destination).await?);
                }
                if let Some(output) = &mut file {
                    for bytes in chunks {
                        if cancel.is_cancelled() {
                            return Err(StorageError::Cancelled);
                        }
                        output.write_all(&bytes).await?;
                        update_digest(&bytes);
                    }
                }
            }
            if cancel.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            Ok(true)
        }
        .await;
        if let Some(output) = &mut file {
            let cleanup = async {
                // Tokio file writes can be in flight when the attempt is stopped.
                output.flush().await?;
                if !matches!(result, Ok(true)) {
                    output.set_len(0).await?;
                }
                Ok::<_, std::io::Error>(())
            }
            .await;
            if let Err(error) = cleanup {
                if result.is_ok() {
                    return Err(error.into());
                }
                tracing::warn!(%error, "failed to clear a rejected CDN file attempt");
            }
        }
        result
    }
}
