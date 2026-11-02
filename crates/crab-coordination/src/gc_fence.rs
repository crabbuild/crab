//! Bounded shared/exclusive admission for GC-managed object-store domains.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use object_store::path::Path;
use object_store::{ObjectStore, UpdateVersion};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::error::{CoordinationError, Result};
use crate::push_lock::{
    backend_unix_time, create_strict, generate_holder_id, get_with_version, push_locks_prefix,
    store_error, update,
};

#[cfg(test)]
mod renewal_tests;
mod state;
mod sweep;
#[cfg(test)]
mod tests;
mod upgrade;
mod writer;
use state::*;
pub use upgrade::upgrade_gc_fence;

/// Writer history identity within one creation of a GC domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcWriterEpoch {
    pub incarnation: String,
    pub epoch: u64,
}

/// Read-only observation used to register a captured root against destructive GC.
///
/// Observe both physical domains before capture and validate after publishing
/// private retention. GC captures retained claims after exclusive admission.
pub struct GcReadBarrier {
    store: Arc<dyn ObjectStore>,
    domain: String,
    history: Option<(String, u64)>,
}

impl GcReadBarrier {
    /// Observe sweep history without acquiring a writer slot or writing a clock.
    pub async fn observe(store: Arc<dyn ObjectStore>, domain: &str) -> Result<Self> {
        let history = read_sweep_history(&store, domain).await?;
        Ok(Self {
            store,
            domain: domain.to_owned(),
            history,
        })
    }

    /// Reject registration if a sweep crossed capture or remains uncertain.
    pub async fn validate(&self) -> Result<()> {
        if read_sweep_history(&self.store, &self.domain).await? == self.history {
            return Ok(());
        }
        Err(conflict(&gc_fence_path(&self.domain)?))
    }
}

async fn read_sweep_history(
    store: &Arc<dyn ObjectStore>,
    domain: &str,
) -> Result<Option<(String, u64)>> {
    let path = gc_fence_path(domain)?;
    let (body, _) = match get_with_version(store, &Path::from(path.as_str())).await {
        Ok(value) => value,
        Err(object_store::Error::NotFound { .. }) => return Ok(None),
        Err(source) => return Err(store_error(&path, source)),
    };
    let control: Control = decode(&path, &body)?;
    control.validate(&path)?;
    if !control.ready {
        return Err(malformed(&path, "GC fence initialization is incomplete"));
    }
    if let Some(holder) = &control.sweep {
        return Err(held(domain, holder, true, control.epoch));
    }
    Ok(Some((control.incarnation, control.epoch)))
}

/// Default lifetime for one GC fence claim.
pub const DEFAULT_GC_FENCE_TTL: Duration = Duration::from_secs(300);
/// Maximum number of concurrent writer holders in one domain.
pub const GC_FENCE_MAX_WRITERS: usize = 64;
/// Minimum uncertainty retained after an ungraceful holder expiry.
pub const DEFAULT_GC_FENCE_QUARANTINE: Duration = Duration::from_secs(24 * 60 * 60);
pub const GC_FENCE_SCHEMA_VERSION: u32 = 3;
const GC_FENCE_MAX_CAS_ATTEMPTS: usize = 32;

/// One side of the GC fence protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcFenceMode {
    /// Shared admission held through authoritative publication.
    Writer,
    /// Exclusive admission held for a bounded sweep operation.
    Sweep,
}

#[derive(Clone)]
enum Claim {
    Writer {
        path: String,
        admitted: Slot,
        version: UpdateVersion,
    },
    Sweep {
        epoch: u64,
        writer_epoch: u64,
        acquisition: String,
    },
}

struct LeaseInner {
    store: Arc<dyn ObjectStore>,
    path: String,
    domain: String,
    holder: String,
    ttl: Duration,
    incarnation: String,
    claim: Claim,
    operation: Mutex<()>,
    released: AtomicBool,
}

impl LeaseInner {
    fn lost(&self) -> CoordinationError {
        CoordinationError::GcFenceLost {
            domain: self.domain.clone(),
            holder: self.holder.clone(),
        }
    }

    async fn control(&self) -> Result<(Control, UpdateVersion)> {
        let (control, version) = load_control(&self.store, &self.path).await?;
        if !control.ready || control.incarnation != self.incarnation {
            return Err(self.lost());
        }
        Ok((control, version))
    }

    async fn validate_expiry(&self, expires_at_backend: i64) -> Result<()> {
        let path = match &self.claim {
            Claim::Writer { path, .. } => path,
            Claim::Sweep { .. } => &self.path,
        };
        // A successful conditional request may have been suspended beyond
        // its lease. Recheck provider time before exposing live ownership.
        let now = backend_unix_time(&self.store, &Path::from(path.as_str())).await?;
        if expires_at_backend <= now {
            return Err(self.lost());
        }
        Ok(())
    }

    async fn release(&self, committed: bool) -> Result<()> {
        let _guard = self.operation.lock().await;
        if self.released.load(Ordering::Acquire) {
            return Ok(());
        }
        match &self.claim {
            Claim::Writer { .. } => {
                writer::mutate(
                    self,
                    if committed {
                        writer::Mutation::ReleaseCommitted
                    } else {
                        writer::Mutation::Release
                    },
                )
                .await?
            }
            Claim::Sweep { .. } if committed => {
                return Err(CoordinationError::Configuration {
                    key: self.domain.clone(),
                    origin: "committed fence release requires a writer lease".to_owned(),
                });
            }
            Claim::Sweep { .. } => sweep::mutate(self, false).await?,
        }
        self.released.store(true, Ordering::Release);
        Ok(())
    }
}

/// Renewable shared or exclusive GC fence claim.
#[derive(Clone)]
pub struct GcFenceLease {
    inner: Arc<LeaseInner>,
}

impl GcFenceLease {
    /// Sample provider time while holding exclusive admission for claim retirement.
    pub async fn read_retention_time(&self) -> Result<i64> {
        let _guard = self.inner.operation.lock().await;
        if self.inner.released.load(Ordering::Acquire) {
            return Err(self.inner.lost());
        }
        sweep::validate(&self.inner).await
    }

    /// Acquire shared writer admission through one private conditional slot.
    pub async fn acquire_writer(
        store: &Arc<dyn ObjectStore>,
        domain: &str,
        ttl: Duration,
    ) -> Result<Self> {
        validate_ttl(ttl)?;
        writer::acquire(store, domain, ttl, generate_holder_id()).await
    }

    /// Close writer admission and verify all slots before admitting a sweep.
    pub async fn acquire_sweep(
        store: &Arc<dyn ObjectStore>,
        domain: &str,
        ttl: Duration,
    ) -> Result<Self> {
        validate_ttl(ttl)?;
        sweep::acquire(store, domain, ttl, generate_holder_id(), false).await
    }

    /// Recover only a journal's own expired exclusive sweep incarnation.
    pub async fn acquire_resumable_sweep(
        store: &Arc<dyn ObjectStore>,
        domain: &str,
        holder: &str,
        ttl: Duration,
    ) -> Result<Self> {
        validate_ttl(ttl)?;
        if !holder.starts_with("gc-run-")
            || holder.len() > 128
            || !holder
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(malformed(
                &gc_fence_path(domain)?,
                "resumable sweep holder is not canonical",
            ));
        }
        sweep::acquire(store, domain, ttl, holder.to_owned(), true).await
    }

    fn new(
        store: &Arc<dyn ObjectStore>,
        path: String,
        domain: &str,
        holder: String,
        ttl: Duration,
        incarnation: String,
        claim: Claim,
    ) -> Self {
        Self {
            inner: Arc::new(LeaseInner {
                store: Arc::clone(store),
                path,
                domain: domain.to_owned(),
                holder,
                ttl,
                incarnation,
                claim,
                operation: Mutex::new(()),
                released: AtomicBool::new(false),
            }),
        }
    }

    /// Renew using backend time and holder-checked conditional writes.
    pub async fn renew(&self) -> Result<()> {
        let _guard = self.inner.operation.lock().await;
        if self.inner.released.load(Ordering::Acquire) {
            return Err(self.inner.lost());
        }
        match self.inner.claim {
            Claim::Writer { .. } => writer::mutate(&self.inner, writer::Mutation::Renew).await,
            Claim::Sweep { .. } => sweep::mutate(&self.inner, true).await,
        }
    }

    /// Release after draining work; expired uncommitted claims remain quarantined.
    pub async fn release(&self) -> Result<()> {
        self.inner.release(false).await
    }

    /// Release a writer whose every uploaded object is durably rooted.
    ///
    /// Error/cancellation paths must use ordinary release. Committed release
    /// can clear its own expired holder without weakening crash quarantine.
    pub async fn release_committed_writer(&self) -> Result<()> {
        self.inner.release(true).await
    }

    /// Return the canonical protected domain.
    #[must_use]
    pub fn domain(&self) -> &str {
        &self.inner.domain
    }

    /// Return this claim's monotonic slot or sweep epoch.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        match &self.inner.claim {
            Claim::Writer { admitted, .. } => admitted.epoch,
            Claim::Sweep { epoch, .. } => *epoch,
        }
    }

    /// Return the complete writer history captured under exclusive admission.
    ///
    /// Writer claims have no complete domain history and return an error.
    pub fn writer_epoch(&self) -> Result<GcWriterEpoch> {
        let Claim::Sweep { writer_epoch, .. } = self.inner.claim else {
            return Err(CoordinationError::Configuration {
                key: self.inner.domain.clone(),
                origin: "writer history requires an exclusive sweep lease".to_owned(),
            });
        };
        Ok(GcWriterEpoch {
            incarnation: self.inner.incarnation.clone(),
            epoch: writer_epoch,
        })
    }

    /// Return the holder identity.
    #[must_use]
    pub fn holder(&self) -> &str {
        &self.inner.holder
    }
    /// Return the configured lease lifetime.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.inner.ttl
    }
}

impl Drop for GcFenceLease {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) != 1 || self.inner.released.load(Ordering::Acquire) {
            return;
        }
        let inner = Arc::clone(&self.inner);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(error) = inner.release(false).await {
                    warn!(domain = %inner.domain, %error, "failed to release GC fence on drop");
                }
            });
        }
    }
}

/// Background renewal for a GC fence lease.
pub struct GcFenceHeartbeat {
    cancel: CancellationToken,
    handle: Option<JoinHandle<()>>,
}

impl GcFenceHeartbeat {
    /// Renew until stopped; operation cancellation keeps renewal alive during drain.
    pub fn spawn(
        lease: &GcFenceLease,
        failure_cancel: CancellationToken,
        interval: Duration,
    ) -> Self {
        let cancel = CancellationToken::new();
        let self_cancel = cancel.clone();
        let lease = lease.clone();
        let interval = interval.max(Duration::from_secs(1));
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = tokio::time::sleep(interval) => {}
                    () = self_cancel.cancelled() => return,
                }
                if self_cancel.is_cancelled() {
                    return;
                }
                if let Err(error) = lease.renew().await {
                    warn!(domain = %lease.domain(), %error, "GC fence renewal failed");
                    failure_cancel.cancel();
                    return;
                }
            }
        });
        Self {
            cancel,
            handle: Some(handle),
        }
    }

    /// Stop renewal and await its completion.
    pub async fn stop(mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for GcFenceHeartbeat {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

fn gc_fence_path(domain: &str) -> Result<String> {
    Ok(format!(
        "{}/internal/gc-fence/state",
        push_locks_prefix(domain)?
    ))
}
fn malformed(path: &str, reason: &str) -> CoordinationError {
    CoordinationError::GcFenceMalformed {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}
fn conflict(path: &str) -> CoordinationError {
    CoordinationError::CasConflict {
        path: path.to_owned(),
        expected_etag: None,
    }
}
fn held(domain: &str, holder: &Holder, sweep: bool, epoch: u64) -> CoordinationError {
    CoordinationError::GcFenceHeld {
        domain: domain.to_owned(),
        holder: holder.holder.clone(),
        sweep,
        epoch,
    }
}
fn next_epoch(path: &str, epoch: u64) -> Result<u64> {
    epoch
        .checked_add(1)
        .ok_or_else(|| malformed(path, "fence epoch exhausted; no further admission is safe"))
}
fn conditional_conflict(error: &object_store::Error) -> bool {
    matches!(
        error,
        object_store::Error::AlreadyExists { .. } | object_store::Error::Precondition { .. }
    )
}
fn validate_ttl(ttl: Duration) -> Result<()> {
    if ttl.as_secs() == 0
        || i64::try_from(ttl.as_secs()).is_err()
        || crate::push_lock::unix_now()
            .checked_add(ttl.as_secs())
            .is_none_or(|expiry| expiry > i64::MAX as u64)
        || std::time::Instant::now().checked_add(ttl).is_none()
    {
        return Err(CoordinationError::Configuration {
            key: ttl.as_secs().to_string(),
            origin: "GC fence TTL is outside the representable range".to_owned(),
        });
    }
    Ok(())
}
