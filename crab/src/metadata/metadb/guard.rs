//! Session-scoped guard that ensures every opened SlateDB instance is
//! closed on every exit path.
//!
//! Callers build a [`MetaDbGuard`] once per operation (push, hydrate,
//! mount, gc, …) and drive the owned [`MetaDb`] through it. The happy
//! path consumes the guard via [`MetaDbGuard::close`], which runs the
//! parallel close fan-out and surfaces any failure as a
//! [`CrabError::MetaDb`]. If a caller drops the guard without
//! calling `close`, [`Drop`] runs a best-effort close on a detached
//! helper thread and bumps the `metadb_close_on_drop_count` metric.

use std::sync::Arc;

use crate::core::error::{CrabError, Result};
use crate::core::metrics::Metrics;

use super::{
    CacheDriftOutcome, ChunkIndexStore, FileIndexStore, MetaDb, PushWriteReceipt,
    SystemKeySnapshot, Transaction,
};

/// Close-on-exit guard for a [`MetaDb`] session.
///
/// The guard wraps the owned [`MetaDb`] in an [`Option`] so the happy
/// path ([`close`](Self::close)) can `take` it out and consume it,
/// leaving nothing for [`Drop`] to do. When the `Option` is still
/// populated at drop time, the fallback path fires: a detached helper
/// thread drives `MetaDb::close_all` to completion and the
/// `metadb_close_on_drop_count` counter is bumped if a metrics handle
/// was provided.
///
/// Not [`Clone`]: there is exactly one owner of the underlying
/// SlateDB handles per operation.
pub struct MetaDbGuard {
    metadb: Option<MetaDb>,
    metrics: Option<Arc<Metrics>>,
}

impl MetaDbGuard {
    /// Wrap a [`MetaDb`] without metrics instrumentation.
    ///
    /// Tests and one-off diagnostics typically use this variant; any
    /// production session should prefer
    /// [`new_with_metrics`](Self::new_with_metrics) so a forgotten
    /// close surfaces in the operator-visible counter.
    #[must_use]
    pub fn new(metadb: MetaDb) -> Self {
        Self {
            metadb: Some(metadb),
            metrics: None,
        }
    }

    /// Wrap a [`MetaDb`] and attach a shared [`Metrics`] handle.
    ///
    /// If the `MetaDb` was constructed via [`MetaDb::new`] (no metrics
    /// sink), this guard still wires the metrics so the drop-path
    /// counter works. Callers that need per-DB counters (`metadb_*`)
    /// populated should build the `MetaDb` through
    /// [`MetaDb::new_with_metrics`] directly.
    #[must_use]
    pub fn new_with_metrics(metadb: MetaDb, metrics: Arc<Metrics>) -> Self {
        Self {
            metadb: Some(metadb),
            metrics: Some(metrics),
        }
    }

    /// Borrow the wrapped `MetaDb` while preserving the no-panic contract.
    fn metadb(&self) -> Result<&MetaDb> {
        self.metadb
            .as_ref()
            .ok_or_else(|| CrabError::Internal("MetaDbGuard accessed after close".to_owned()))
    }

    /// Typed accessor that forwards to the inner session.
    pub async fn file_index(&self) -> Result<FileIndexStore> {
        self.metadb()?.file_index().await
    }

    /// Typed accessor that forwards to the inner session.
    pub async fn chunk_index(&self) -> Result<ChunkIndexStore> {
        self.metadb()?.chunk_index().await
    }

    /// Whether the underlying session is read-only (non-fencing).
    pub fn is_read_only(&self) -> Result<bool> {
        Ok(self.metadb()?.is_read_only())
    }

    /// Build a fresh transaction via the inner session.
    pub fn new_transaction(&self) -> Result<Transaction> {
        Ok(self.metadb()?.new_transaction())
    }

    /// Commit a transaction through the inner session.
    pub async fn commit(&self, txn: Transaction) -> Result<PushWriteReceipt> {
        Box::pin(self.metadb()?.commit(txn)).await
    }

    /// Flush every opened writer memtable without closing the session.
    pub async fn flush_memtables(&self) -> Result<()> {
        self.metadb()?.flush_memtables().await
    }

    /// Check GC-drift through the inner session.
    pub async fn check_cache_gc_drift(&self) -> Result<CacheDriftOutcome> {
        self.metadb()?.check_cache_gc_drift().await
    }

    /// Read the `sys:*` snapshot from the per-repo `file_index_db`.
    pub async fn file_index_system_keys(&self) -> Result<SystemKeySnapshot> {
        self.metadb()?.file_index_system_keys().await
    }

    /// Read the `sys:*` snapshot from the global `chunk_index_db`.
    pub async fn chunk_index_system_keys(&self) -> Result<SystemKeySnapshot> {
        self.metadb()?.chunk_index_system_keys().await
    }

    /// Expose the raw `file_index_db` handle for diagnostic scans.
    pub async fn file_index_db_handle(&self) -> Result<std::sync::Arc<super::Db>> {
        self.metadb()?.file_index_db_handle().await
    }

    /// Expose the raw `chunk_index_db` handle for diagnostic scans.
    pub async fn chunk_index_db_handle(&self) -> Result<std::sync::Arc<super::Db>> {
        self.metadb()?.chunk_index_db_handle().await
    }

    /// Bump the global `sys:gc_generation` cursor through the inner
    /// session. Intended for the `crab gc` sweep.
    pub async fn bump_gc_generation(&self) -> Result<u64> {
        self.metadb()?.bump_gc_generation().await
    }

    /// Install an already-opened `PersistentChunkIndex` handle into
    /// the inner session's warm-tier slot, but only if the slot is
    /// still empty.
    ///
    /// Returns `true` when the handle was installed and `false` when
    /// the session had already opened its own handle (the passed
    /// value is then dropped). The push pipeline uses this to share
    /// the warm-tier handle opened by step 3's shard-sync with
    /// step 9b's `warm_local_shard`, so both phases share the same
    /// process-local writer queue.
    pub fn install_persistent_tier(
        &self,
        persistent: std::sync::Arc<crab_metadata::persistent_chunk_index::PersistentChunkIndex>,
    ) -> Result<bool> {
        Ok(self.metadb()?.install_persistent_tier(persistent))
    }

    /// Close every SlateDB instance the session opened.
    ///
    /// Consumes the guard so a second close is impossible at the
    /// type level. Any failure from [`MetaDb::close_all`] bubbles up
    /// as a [`CrabError::MetaDb`]; callers should surface the
    /// error rather than swallow it.
    pub async fn close(mut self) -> Result<()> {
        let Some(metadb) = self.metadb.take() else {
            // Defensive: the only way `metadb` is `None` here is if a
            // future refactor introduces a second consumer. Return Ok
            // rather than panic; a double-close attempt is harmless.
            return Ok(());
        };
        metadb.close_all().await
    }
}

impl Drop for MetaDbGuard {
    fn drop(&mut self) {
        let Some(metadb) = self.metadb.take() else {
            return;
        };

        tracing::warn!(
            "MetaDbGuard dropped without explicit close; falling back to best-effort close"
        );

        if let Some(metrics) = self.metrics.as_ref() {
            metrics.inc_metadb_close_on_drop();
        }

        // Running `close_all` (async) from inside `Drop` needs care:
        // we cannot block the current runtime thread. In particular, joining
        // a helper that calls `Handle::block_on` deadlocks a current-thread
        // runtime because the blocked caller is the only scheduler driver.
        // Strategy:
        //   - if a tokio runtime is active, detach an OS helper and let the
        //     current runtime continue driving the SlateDB close tasks;
        //   - if no runtime is active (test teardown, signal
        //     handler), spin up a throw-away current-thread runtime.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                if let Err(error) = std::thread::Builder::new()
                    .name("crab-metadb-close".to_owned())
                    .spawn(move || {
                        handle.block_on(async move {
                            if let Err(e) = metadb.close_all().await {
                                tracing::error!(
                                    error = %e,
                                    "MetaDbGuard drop: close_all failed"
                                );
                            }
                        });
                    })
                {
                    tracing::error!(
                        error = %error,
                        "MetaDbGuard drop: unable to spawn close helper"
                    );
                }
            }
            Err(_) => match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => {
                    rt.block_on(async move {
                        if let Err(e) = metadb.close_all().await {
                            tracing::error!(
                                error = %e,
                                "MetaDbGuard drop: close_all failed"
                            );
                        }
                    });
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "MetaDbGuard drop: unable to build fallback runtime; skipping close"
                    );
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::ObjectStore;
    use object_store::memory::InMemory;

    use super::*;
    use crate::metadata::metadb::{MetaDb, MetaDbConfig};

    /// Compile-time guarantee that the guard is `Send`. If this
    /// assertion fires a compile error the push pipeline's
    /// `tokio::sync::Mutex<Option<MetaDbGuard>>` field will no longer
    /// let the pipeline's `execute` future be `Send`, and
    /// `tokio::spawn` from the remote helper will fall over with a
    /// confusing higher-ranked lifetime error. Keep this here.
    const _: fn() = || {
        fn assert_send<T: Send>() {}
        assert_send::<MetaDbGuard>();
    };

    fn stub_store() -> Arc<dyn ObjectStore> {
        Arc::new(InMemory::new())
    }

    fn fresh_metadb() -> MetaDb {
        MetaDb::new(
            stub_store(),
            String::from("org/my-repo"),
            MetaDbConfig::for_repo("org/my-repo"),
        )
    }

    #[tokio::test]
    async fn guard_close_succeeds_on_fresh_session() {
        let guard = MetaDbGuard::new(fresh_metadb());
        guard
            .close()
            .await
            .expect("close on a guard that opened nothing must succeed");
    }

    #[tokio::test]
    async fn guard_close_with_metrics_does_not_touch_drop_counter() {
        let metrics = Arc::new(Metrics::new());
        let guard = MetaDbGuard::new_with_metrics(fresh_metadb(), Arc::clone(&metrics));

        guard.close().await.expect("happy-path close");

        assert_eq!(
            metrics.snapshot().metadb_close_on_drop_count,
            0,
            "explicit close must not bump the drop counter"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guard_drop_increments_counter_when_not_closed() {
        let metrics = Arc::new(Metrics::new());

        {
            let _guard = MetaDbGuard::new_with_metrics(fresh_metadb(), Arc::clone(&metrics));
            // Intentionally drop without calling `close()`.
        }

        assert_eq!(
            metrics.snapshot().metadb_close_on_drop_count,
            1,
            "drop path must bump metadb_close_on_drop_count exactly once"
        );
    }

    #[tokio::test]
    async fn guard_drop_does_not_deadlock_current_thread_runtime() {
        let guard = MetaDbGuard::new(fresh_metadb());
        let file = guard.file_index().await.expect("open file index");
        assert!(
            file.get_legacy(&crab_xet::hash::MerkleHash::from([9u64, 8, 7, 6]))
                .await
                .expect("lookup")
                .is_none()
        );

        drop(guard);
        tokio::task::yield_now().await;
    }

    #[tokio::test]
    async fn guard_forwards_accessors_to_inner_metadb() {
        let guard = MetaDbGuard::new(fresh_metadb());

        // file_index forwarder lazy-opens the DB and returns a usable
        // store that answers a miss correctly.
        let file = guard.file_index().await.expect("file_index forwarder");
        let missing = crab_xet::hash::MerkleHash::from([1u64, 2, 3, 4]);
        assert!(file.get_legacy(&missing).await.expect("get").is_none());

        // new_transaction forwarder returns a fresh empty txn.
        let txn = guard.new_transaction().expect("transaction");
        assert!(txn.is_empty());

        // commit forwarder swallows the empty transaction.
        let receipt = guard.commit(txn).await.expect("commit empty");
        assert_eq!(receipt.file_ops_written, 0);
        assert_eq!(receipt.chunk_ops_written, 0);

        guard.close().await.expect("close");
    }
}
