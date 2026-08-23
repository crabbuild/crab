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

/// Default lifetime for one GC fence claim.
pub const DEFAULT_GC_FENCE_TTL: Duration = Duration::from_secs(300);
/// Maximum number of concurrent writer holders recorded in one domain.
pub const GC_FENCE_MAX_WRITERS: usize = 64;
/// Minimum quarantine retained after an ungraceful writer expiry.
pub const DEFAULT_GC_FENCE_QUARANTINE: Duration = Duration::from_secs(24 * 60 * 60);
const GC_FENCE_SCHEMA_VERSION: u32 = 2;
const GC_FENCE_MAX_CAS_ATTEMPTS: usize = 32;
const GC_FENCE_MAX_QUARANTINES: usize = 64;

/// One side of the GC fence protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcFenceMode {
    /// Shared admission held by a writer through root publication.
    Writer,
    /// Exclusive admission held only for a bounded sweep operation.
    Sweep,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GcFenceHolder {
    holder: String,
    expires_at_backend: i64,
    lease_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GcFenceQuarantine {
    holder: String,
    mode: GcFenceModeWire,
    expired_at_backend: i64,
    quarantine_until_backend: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum GcFenceModeWire {
    Writer,
    Sweep,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GcFenceState {
    schema_version: u32,
    epoch: u64,
    writers: Vec<GcFenceHolder>,
    sweep: Option<GcFenceHolder>,
    quarantine: Vec<GcFenceQuarantine>,
    /// Fail-closed marker used when the bounded quarantine list is full.
    ///
    /// Without this marker an expired holder could be dropped merely because
    /// the state reached its quarantine bound, allowing a sweep to pass an
    /// uncertain writer. Keeping one deadline preserves safety without
    /// making the fence state grow with crashed writers.
    #[serde(default)]
    quarantine_block_until_backend: Option<i64>,
}

impl GcFenceState {
    fn empty() -> Self {
        Self {
            schema_version: GC_FENCE_SCHEMA_VERSION,
            epoch: 0,
            writers: Vec::new(),
            sweep: None,
            quarantine: Vec::new(),
            quarantine_block_until_backend: None,
        }
    }

    fn validate(&self, path: &str) -> Result<()> {
        if self.schema_version != GC_FENCE_SCHEMA_VERSION {
            return Err(CoordinationError::GcFenceMalformed {
                path: path.to_owned(),
                reason: format!(
                    "unsupported schema version {}, expected {GC_FENCE_SCHEMA_VERSION}",
                    self.schema_version
                ),
            });
        }
        if self.writers.len() > GC_FENCE_MAX_WRITERS {
            return Err(CoordinationError::GcFenceMalformed {
                path: path.to_owned(),
                reason: format!(
                    "writer holder count {} exceeds {GC_FENCE_MAX_WRITERS}",
                    self.writers.len()
                ),
            });
        }
        if self.quarantine.len() > GC_FENCE_MAX_QUARANTINES {
            return Err(CoordinationError::GcFenceMalformed {
                path: path.to_owned(),
                reason: format!(
                    "quarantine holder count {} exceeds {GC_FENCE_MAX_QUARANTINES}",
                    self.quarantine.len()
                ),
            });
        }
        let holders = self
            .writers
            .iter()
            .map(|holder| holder.holder.as_str())
            .collect::<std::collections::HashSet<_>>();
        if holders.len() != self.writers.len() {
            return Err(CoordinationError::GcFenceMalformed {
                path: path.to_owned(),
                reason: "duplicate writer fence holder".to_owned(),
            });
        }
        let mut holders = holders;
        if let Some(sweep) = &self.sweep
            && (sweep.holder.is_empty() || !holders.insert(sweep.holder.as_str()))
        {
            return Err(CoordinationError::GcFenceMalformed {
                path: path.to_owned(),
                reason: "duplicate or empty fence holder".to_owned(),
            });
        }
        for quarantine in &self.quarantine {
            if quarantine.holder.is_empty()
                || !holders.insert(quarantine.holder.as_str())
                || quarantine.expired_at_backend <= 0
                || quarantine.quarantine_until_backend <= quarantine.expired_at_backend
            {
                return Err(CoordinationError::GcFenceMalformed {
                    path: path.to_owned(),
                    reason: "invalid or duplicate fence quarantine".to_owned(),
                });
            }
        }
        if self
            .quarantine_block_until_backend
            .is_some_and(|deadline| deadline <= 0)
        {
            return Err(CoordinationError::GcFenceMalformed {
                path: path.to_owned(),
                reason: "invalid quarantine capacity deadline".to_owned(),
            });
        }
        if self.writers.iter().any(|holder| {
            holder.holder.is_empty() || holder.lease_secs == 0 || holder.expires_at_backend <= 0
        }) || self
            .sweep
            .as_ref()
            .is_some_and(|holder| holder.lease_secs == 0 || holder.expires_at_backend <= 0)
        {
            return Err(CoordinationError::GcFenceMalformed {
                path: path.to_owned(),
                reason: "invalid holder lease fields".to_owned(),
            });
        }
        Ok(())
    }

    fn prune_expired(&mut self, now_backend: i64) -> bool {
        let old_block = self.quarantine_block_until_backend;
        if old_block.is_some_and(|deadline| deadline <= now_backend) {
            self.quarantine_block_until_backend = None;
        }
        let old_quarantine_len = self.quarantine.len();
        self.quarantine
            .retain(|entry| entry.quarantine_until_backend > now_backend);
        let mut changed = old_quarantine_len != self.quarantine.len()
            || old_block != self.quarantine_block_until_backend;
        let mut expired_writers = Vec::new();
        self.writers.retain(|holder| {
            if holder.expires_at_backend > now_backend {
                true
            } else {
                expired_writers.push(holder.clone());
                false
            }
        });
        for holder in expired_writers {
            changed = true;
            self.quarantine_or_extend(&holder, GcFenceModeWire::Writer, now_backend);
        }
        let expired_sweep = self
            .sweep
            .as_ref()
            .filter(|holder| holder.expires_at_backend <= now_backend)
            .cloned();
        if self
            .sweep
            .as_ref()
            .is_some_and(|holder| holder.expires_at_backend <= now_backend)
        {
            self.sweep = None;
            changed = true;
        }
        if let Some(holder) = expired_sweep {
            self.quarantine_or_extend(&holder, GcFenceModeWire::Sweep, now_backend);
        }
        changed
    }

    fn quarantine_or_extend(
        &mut self,
        holder: &GcFenceHolder,
        mode: GcFenceModeWire,
        now_backend: i64,
    ) {
        if self.add_quarantine(holder, mode, now_backend) {
            return;
        }

        // A full quarantine is an availability problem, never permission to
        // forget an expired holder. Keep the holder as a synthetic live claim
        // until the same quarantine deadline so a later GC cannot pass through
        // an uncertain writer/sweep merely because the side list is full.
        let mut retained = holder.clone();
        retained.expires_at_backend = quarantine_until(holder.expires_at_backend);
        retained.lease_secs = retained
            .expires_at_backend
            .saturating_sub(now_backend)
            .try_into()
            .unwrap_or(u64::MAX)
            .max(1);
        self.epoch = self.epoch.saturating_add(1);
        match mode {
            GcFenceModeWire::Writer if self.writers.len() < GC_FENCE_MAX_WRITERS => {
                self.writers.push(retained);
            }
            GcFenceModeWire::Writer => {
                // Retain a bounded fail-closed deadline instead of silently
                // forgetting this holder when both bounded lists are full.
                self.quarantine_block_until_backend = Some(
                    self.quarantine_block_until_backend
                        .unwrap_or_default()
                        .max(retained.expires_at_backend),
                );
            }
            GcFenceModeWire::Sweep => {
                if self.sweep.is_none() {
                    self.sweep = Some(retained);
                } else {
                    self.quarantine_block_until_backend = Some(
                        self.quarantine_block_until_backend
                            .unwrap_or_default()
                            .max(retained.expires_at_backend),
                    );
                }
            }
        }
    }

    fn add_quarantine(
        &mut self,
        holder: &GcFenceHolder,
        mode: GcFenceModeWire,
        now_backend: i64,
    ) -> bool {
        if self
            .quarantine
            .iter()
            .any(|entry| entry.holder == holder.holder)
        {
            return false;
        }
        if self.quarantine.len() >= GC_FENCE_MAX_QUARANTINES {
            return false;
        }
        let quarantine_until_backend = quarantine_until(holder.expires_at_backend);
        self.quarantine.push(GcFenceQuarantine {
            holder: holder.holder.clone(),
            mode,
            expired_at_backend: holder.expires_at_backend.min(now_backend),
            quarantine_until_backend,
        });
        self.epoch = self.epoch.saturating_add(1);
        true
    }
}

struct LeaseInner {
    store: Arc<dyn ObjectStore>,
    path: String,
    domain: String,
    holder: String,
    mode: GcFenceMode,
    ttl: Duration,
    epoch: u64,
    etag: Mutex<Option<UpdateVersion>>,
    released: AtomicBool,
}

/// Renewable shared or exclusive GC fence claim.
pub struct GcFenceLease {
    inner: Arc<LeaseInner>,
}

impl Clone for GcFenceLease {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl GcFenceLease {
    /// Acquires shared writer admission for `domain`.
    pub async fn acquire_writer(
        store: &Arc<dyn ObjectStore>,
        domain: &str,
        ttl: Duration,
    ) -> Result<Self> {
        Self::acquire(store, domain, GcFenceMode::Writer, ttl).await
    }

    /// Acquires exclusive sweep admission for `domain`.
    pub async fn acquire_sweep(
        store: &Arc<dyn ObjectStore>,
        domain: &str,
        ttl: Duration,
    ) -> Result<Self> {
        Self::acquire(store, domain, GcFenceMode::Sweep, ttl).await
    }

    async fn acquire(
        store: &Arc<dyn ObjectStore>,
        domain: &str,
        mode: GcFenceMode,
        ttl: Duration,
    ) -> Result<Self> {
        validate_ttl(ttl)?;
        let path = gc_fence_path(domain)?;
        let holder = generate_holder_id();

        for _ in 0..GC_FENCE_MAX_CAS_ATTEMPTS {
            let now = backend_unix_time(store, &Path::from(path.as_str())).await?;
            let loaded = match get_with_version(store, &Path::from(path.as_str())).await {
                Ok((body, etag)) => {
                    let state = deserialize_state(&path, &body)?;
                    state.validate(&path)?;
                    Some((state, etag))
                }
                Err(object_store::Error::NotFound { .. }) => None,
                Err(source) => return Err(store_error(&path, source)),
            };

            let Some((mut state, etag)) = loaded else {
                let mut state = GcFenceState::empty();
                insert_holder(&mut state, &holder, mode, now, ttl)?;
                let body = serialize_state(&path, &state)?;
                match create_strict(store, &Path::from(path.as_str()), body).await {
                    Ok(new_etag) => {
                        return Ok(Self::new_inner(
                            store,
                            path,
                            domain,
                            holder,
                            mode,
                            ttl,
                            state.epoch,
                            new_etag,
                        ));
                    }
                    Err(object_store::Error::AlreadyExists { .. })
                    | Err(object_store::Error::Precondition { .. }) => continue,
                    Err(source) => return Err(store_error(&path, source)),
                }
            };

            state.prune_expired(now);
            if let Some(blocker) = blocking_holder(&state, mode, now) {
                return Err(CoordinationError::GcFenceHeld {
                    domain: domain.to_owned(),
                    holder: blocker.holder,
                    sweep: blocker.sweep,
                    epoch: state.epoch,
                });
            }
            if mode == GcFenceMode::Writer && state.writers.len() >= GC_FENCE_MAX_WRITERS {
                return Err(CoordinationError::GcFenceHeld {
                    domain: domain.to_owned(),
                    holder: "writer-capacity".to_owned(),
                    sweep: false,
                    epoch: state.epoch,
                });
            }
            if mode == GcFenceMode::Writer
                && state
                    .quarantine
                    .iter()
                    .filter(|entry| entry.mode == GcFenceModeWire::Writer)
                    .count()
                    >= GC_FENCE_MAX_QUARANTINES
            {
                return Err(CoordinationError::GcFenceHeld {
                    domain: domain.to_owned(),
                    holder: "writer-quarantine-capacity".to_owned(),
                    sweep: false,
                    epoch: state.epoch,
                });
            }

            insert_holder(&mut state, &holder, mode, now, ttl)?;
            let body = serialize_state(&path, &state)?;
            match update(store, &Path::from(path.as_str()), body, etag).await {
                Ok(new_etag) => {
                    return Ok(Self::new_inner(
                        store,
                        path,
                        domain,
                        holder,
                        mode,
                        ttl,
                        state.epoch,
                        new_etag,
                    ));
                }
                Err(object_store::Error::AlreadyExists { .. })
                | Err(object_store::Error::Precondition { .. }) => continue,
                Err(source) => return Err(store_error(&path, source)),
            }
        }

        Err(CoordinationError::CasConflict {
            path,
            expected_etag: None,
        })
    }

    fn new_inner(
        store: &Arc<dyn ObjectStore>,
        path: String,
        domain: &str,
        holder: String,
        mode: GcFenceMode,
        ttl: Duration,
        epoch: u64,
        etag: UpdateVersion,
    ) -> Self {
        Self {
            inner: Arc::new(LeaseInner {
                store: Arc::clone(store),
                path,
                domain: domain.to_owned(),
                holder,
                mode,
                ttl,
                epoch,
                etag: Mutex::new(Some(etag)),
                released: AtomicBool::new(false),
            }),
        }
    }

    /// Renews this holder using backend time and a holder-checked CAS.
    pub async fn renew(&self) -> Result<()> {
        if self.inner.released.load(Ordering::Acquire) {
            return Err(CoordinationError::GcFenceLost {
                domain: self.inner.domain.clone(),
                holder: self.inner.holder.clone(),
            });
        }
        let now =
            backend_unix_time(&self.inner.store, &Path::from(self.inner.path.as_str())).await?;
        let mut etag = self.inner.etag.lock().await;
        let Some(current_etag) = etag.clone() else {
            return Err(CoordinationError::GcFenceLost {
                domain: self.inner.domain.clone(),
                holder: self.inner.holder.clone(),
            });
        };
        let (body, observed_etag) =
            get_with_version(&self.inner.store, &Path::from(self.inner.path.as_str()))
                .await
                .map_err(|source| store_error(&self.inner.path, source))?;
        let mut state = deserialize_state(&self.inner.path, &body)?;
        state.validate(&self.inner.path)?;
        if !replace_holder_expiry(
            &mut state,
            &self.inner.holder,
            self.inner.mode,
            now,
            self.inner.ttl,
        ) {
            return Err(CoordinationError::GcFenceLost {
                domain: self.inner.domain.clone(),
                holder: self.inner.holder.clone(),
            });
        }
        let body = serialize_state(&self.inner.path, &state)?;
        let new_etag = match update(
            &self.inner.store,
            &Path::from(self.inner.path.as_str()),
            body,
            observed_etag,
        )
        .await
        {
            Ok(etag) => etag,
            Err(object_store::Error::AlreadyExists { .. })
            | Err(object_store::Error::Precondition { .. }) => {
                return Err(CoordinationError::CasConflict {
                    path: self.inner.path.clone(),
                    expected_etag: current_etag.e_tag,
                });
            }
            Err(source) => return Err(store_error(&self.inner.path, source)),
        };
        *etag = Some(new_etag);
        Ok(())
    }

    /// Releases this holder with a holder-checked CAS. Releasing an already
    /// reclaimed holder is idempotent and succeeds.
    pub async fn release(&self) -> Result<()> {
        if self.inner.released.load(Ordering::Acquire) {
            return Ok(());
        }
        let result = release_holder(
            &self.inner.store,
            &self.inner.path,
            &self.inner.holder,
            self.inner.mode,
        )
        .await;
        if result.is_ok() {
            self.inner.released.store(true, Ordering::Release);
        }
        result
    }

    /// Returns the canonical domain protected by this lease.
    #[must_use]
    pub fn domain(&self) -> &str {
        &self.inner.domain
    }

    /// Returns the backend-monotonic fence epoch observed at acquisition.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.inner.epoch
    }

    /// Returns the holder identity.
    #[must_use]
    pub fn holder(&self) -> &str {
        &self.inner.holder
    }

    /// Returns the configured lease lifetime.
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
        let store = Arc::clone(&self.inner.store);
        let path = self.inner.path.clone();
        let holder = self.inner.holder.clone();
        let mode = self.inner.mode;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(error) = release_holder(&store, &path, &holder, mode).await {
                    warn!(domain = %path, %error, "failed to release GC fence on drop");
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
    /// Starts renewing `lease` and cancels `operation_cancel` on loss.
    pub fn spawn(
        lease: &GcFenceLease,
        operation_cancel: CancellationToken,
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
                    () = operation_cancel.cancelled() => return,
                }
                if self_cancel.is_cancelled() || operation_cancel.is_cancelled() {
                    return;
                }
                if let Err(error) = lease.renew().await {
                    warn!(domain = %lease.domain(), %error, "GC fence renewal failed");
                    operation_cancel.cancel();
                    return;
                }
            }
        });
        Self {
            cancel,
            handle: Some(handle),
        }
    }

    /// Stops renewal and waits for the task to exit.
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

fn validate_ttl(ttl: Duration) -> Result<()> {
    if ttl.as_secs() > 0 {
        return Ok(());
    }
    Err(CoordinationError::Configuration {
        key: ttl.as_secs().to_string(),
        origin: "GC fence TTL must be at least one second".to_owned(),
    })
}

fn serialize_state(path: &str, state: &GcFenceState) -> Result<Bytes> {
    serde_json::to_vec(state)
        .map(Bytes::from)
        .map_err(|source| CoordinationError::Serialize {
            key: path.to_owned(),
            context: "GC fence state",
            source,
        })
}

fn deserialize_state(path: &str, body: &[u8]) -> Result<GcFenceState> {
    serde_json::from_slice(body).map_err(|source| CoordinationError::GcFenceMalformed {
        path: path.to_owned(),
        reason: source.to_string(),
    })
}

fn insert_holder(
    state: &mut GcFenceState,
    holder: &str,
    mode: GcFenceMode,
    now_backend: i64,
    ttl: Duration,
) -> Result<()> {
    let expires_at_backend =
        now_backend.saturating_add(i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX));
    let record = GcFenceHolder {
        holder: holder.to_owned(),
        expires_at_backend,
        lease_secs: ttl.as_secs(),
    };
    state.epoch = state.epoch.saturating_add(1);
    match mode {
        GcFenceMode::Writer => {
            if state.writers.len() >= GC_FENCE_MAX_WRITERS {
                return Err(CoordinationError::GcFenceMalformed {
                    path: "gc-fence".to_owned(),
                    reason: "writer capacity check raced with state mutation".to_owned(),
                });
            }
            state.writers.push(record);
        }
        GcFenceMode::Sweep => state.sweep = Some(record),
    }
    Ok(())
}

struct Blocker {
    holder: String,
    sweep: bool,
}

fn blocking_holder(state: &GcFenceState, mode: GcFenceMode, now_backend: i64) -> Option<Blocker> {
    if state
        .quarantine_block_until_backend
        .is_some_and(|deadline| deadline > now_backend)
    {
        return Some(Blocker {
            holder: "quarantine-capacity".to_owned(),
            sweep: false,
        });
    }
    match mode {
        GcFenceMode::Writer => state
            .sweep
            .as_ref()
            .filter(|sweep| sweep.expires_at_backend > now_backend)
            .map(|sweep| Blocker {
                holder: sweep.holder.clone(),
                sweep: true,
            })
            .or_else(|| {
                state
                    .quarantine
                    .iter()
                    .find(|entry| {
                        entry.quarantine_until_backend > now_backend
                            && entry.mode == GcFenceModeWire::Sweep
                    })
                    .map(|entry| Blocker {
                        holder: entry.holder.clone(),
                        sweep: true,
                    })
            }),
        GcFenceMode::Sweep => state
            .writers
            .iter()
            .find(|writer| writer.expires_at_backend > now_backend)
            .map(|writer| Blocker {
                holder: writer.holder.clone(),
                sweep: false,
            })
            .or_else(|| {
                state
                    .quarantine
                    .iter()
                    .find(|entry| {
                        entry.quarantine_until_backend > now_backend
                            && entry.mode == GcFenceModeWire::Writer
                    })
                    .map(|entry| Blocker {
                        holder: entry.holder.clone(),
                        sweep: false,
                    })
            })
            .or_else(|| {
                state
                    .sweep
                    .as_ref()
                    .filter(|sweep| sweep.expires_at_backend > now_backend)
                    .map(|sweep| Blocker {
                        holder: sweep.holder.clone(),
                        sweep: true,
                    })
            })
            .or_else(|| {
                state
                    .quarantine
                    .iter()
                    .find(|entry| entry.quarantine_until_backend > now_backend)
                    .map(|entry| Blocker {
                        holder: entry.holder.clone(),
                        sweep: entry.mode == GcFenceModeWire::Sweep,
                    })
            }),
    }
}

fn quarantine_until(expired_at_backend: i64) -> i64 {
    expired_at_backend
        .saturating_add(i64::try_from(DEFAULT_GC_FENCE_QUARANTINE.as_secs()).unwrap_or(i64::MAX))
}

fn replace_holder_expiry(
    state: &mut GcFenceState,
    holder: &str,
    mode: GcFenceMode,
    now_backend: i64,
    ttl: Duration,
) -> bool {
    let expires = now_backend.saturating_add(i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX));
    match mode {
        GcFenceMode::Writer => state
            .writers
            .iter_mut()
            .find(|record| record.holder == holder)
            .map(|record| record.expires_at_backend = expires)
            .is_some(),
        GcFenceMode::Sweep => state
            .sweep
            .as_mut()
            .filter(|record| record.holder == holder)
            .map(|record| record.expires_at_backend = expires)
            .is_some(),
    }
}

async fn release_holder(
    store: &Arc<dyn ObjectStore>,
    path: &str,
    holder: &str,
    mode: GcFenceMode,
) -> Result<()> {
    for _ in 0..GC_FENCE_MAX_CAS_ATTEMPTS {
        let (body, etag) = match get_with_version(store, &Path::from(path)).await {
            Ok(value) => value,
            Err(object_store::Error::NotFound { .. }) => return Ok(()),
            Err(source) => return Err(store_error(path, source)),
        };
        let mut state = deserialize_state(path, &body)?;
        state.validate(path)?;
        let removed = match mode {
            GcFenceMode::Writer => {
                let old = state.writers.len();
                state.writers.retain(|record| record.holder != holder);
                old != state.writers.len()
            }
            GcFenceMode::Sweep => state
                .sweep
                .as_ref()
                .is_some_and(|record| record.holder == holder)
                .then(|| state.sweep.take())
                .is_some(),
        };
        if !removed {
            return Ok(());
        }
        state.epoch = state.epoch.saturating_add(1);
        match update(
            store,
            &Path::from(path),
            serialize_state(path, &state)?,
            etag,
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(object_store::Error::AlreadyExists { .. })
            | Err(object_store::Error::Precondition { .. }) => continue,
            Err(source) => return Err(store_error(path, source)),
        }
    }
    Err(CoordinationError::CasConflict {
        path: path.to_owned(),
        expected_etag: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    fn memory_store() -> Arc<dyn ObjectStore> {
        Arc::new(InMemory::new())
    }

    #[tokio::test]
    async fn writer_and_sweep_are_mutually_exclusive() {
        let store = memory_store();
        let writer = GcFenceLease::acquire_writer(&store, "org/repo", Duration::from_secs(30))
            .await
            .unwrap();
        let blocked = GcFenceLease::acquire_sweep(&store, "org/repo", Duration::from_secs(30))
            .await
            .err()
            .unwrap();
        assert!(matches!(
            blocked,
            CoordinationError::GcFenceHeld { sweep: false, .. }
        ));
        writer.release().await.unwrap();
        let sweep = GcFenceLease::acquire_sweep(&store, "org/repo", Duration::from_secs(30))
            .await
            .unwrap();
        let second_sweep = GcFenceLease::acquire_sweep(&store, "org/repo", Duration::from_secs(30))
            .await
            .err()
            .unwrap();
        assert!(matches!(
            second_sweep,
            CoordinationError::GcFenceHeld { sweep: true, .. }
        ));
        let blocked = GcFenceLease::acquire_writer(&store, "org/repo", Duration::from_secs(30))
            .await
            .err()
            .unwrap();
        assert!(matches!(
            blocked,
            CoordinationError::GcFenceHeld { sweep: true, .. }
        ));
        sweep.release().await.unwrap();
    }

    #[tokio::test]
    async fn writers_are_bounded_and_share_admission() {
        let store = memory_store();
        let mut leases = Vec::new();
        for _ in 0..GC_FENCE_MAX_WRITERS {
            leases.push(
                GcFenceLease::acquire_writer(&store, "org/repo", Duration::from_secs(30))
                    .await
                    .unwrap(),
            );
        }
        let blocked = GcFenceLease::acquire_writer(&store, "org/repo", Duration::from_secs(30))
            .await
            .err()
            .unwrap();
        assert!(matches!(blocked, CoordinationError::GcFenceHeld { .. }));
        for lease in leases {
            lease.release().await.unwrap();
        }
    }

    #[tokio::test]
    async fn renewal_and_release_are_holder_checked() {
        let store = memory_store();
        let lease = GcFenceLease::acquire_writer(&store, "org/repo", Duration::from_secs(30))
            .await
            .unwrap();
        let clone = lease.clone();
        lease.renew().await.unwrap();
        lease.release().await.unwrap();
        assert!(clone.renew().await.is_err());
        clone.release().await.unwrap();
        let next = GcFenceLease::acquire_sweep(&store, "org/repo", Duration::from_secs(30))
            .await
            .unwrap();
        next.release().await.unwrap();
    }

    #[test]
    fn full_quarantine_fails_closed_instead_of_dropping_an_expired_holder() {
        let mut state = GcFenceState::empty();
        for index in 0..GC_FENCE_MAX_QUARANTINES {
            state.quarantine.push(GcFenceQuarantine {
                holder: format!("quarantined-{index}"),
                mode: GcFenceModeWire::Writer,
                expired_at_backend: 10,
                quarantine_until_backend: 20,
            });
            state.writers.push(GcFenceHolder {
                holder: format!("synthetic-{index}"),
                expires_at_backend: 100,
                lease_secs: 1,
            });
        }
        let expired = GcFenceHolder {
            holder: "expired".to_owned(),
            expires_at_backend: 10,
            lease_secs: 1,
        };
        state.quarantine_or_extend(&expired, GcFenceModeWire::Writer, 11);
        assert!(state.quarantine_block_until_backend.is_some());
        assert!(blocking_holder(&state, GcFenceMode::Sweep, 11).is_some());
        state.validate("test").unwrap();
    }
}
