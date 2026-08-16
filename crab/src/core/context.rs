//! Application-wide context threaded through the call graph.
//!
//! `AppContext` is the single struct that carries configuration and shared
//! state into every subsystem. It is cheaply cloneable (`Arc`-backed
//! internals) so async tasks can hold their own handle.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::config::{Config, EngineConfig};
use super::error::{CrabError, Result};
use super::metrics::Metrics;

/// Shared application context passed through the entire call graph.
///
/// Cheap to clone — all interior data is behind `Arc`. Construct via
/// [`AppContext::new`] at startup and hand a clone to each subsystem.
#[derive(Debug, Clone)]
pub struct AppContext {
    inner: Arc<AppContextInner>,
}

#[derive(Debug)]
struct AppContextInner {
    config: Config,
    metrics: Arc<Metrics>,
    cancel: CancellationToken,
}

impl AppContext {
    /// Build a new context from the resolved configuration and a
    /// cancellation token (created in `main()` and wired to the signal
    /// handler).
    #[must_use]
    pub fn new(config: Config, cancel: CancellationToken) -> Self {
        Self {
            inner: Arc::new(AppContextInner {
                config,
                metrics: Arc::new(Metrics::new()),
                cancel,
            }),
        }
    }

    /// Full resolved configuration for the current session.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// Performance configuration for the current session.
    ///
    /// Shorthand for `self.config().perf` — keeps existing call sites
    /// working without a two-level dereference.
    #[must_use]
    pub fn perf(&self) -> &EngineConfig {
        &self.inner.config.perf
    }

    /// Cache service configuration for the current session.
    ///
    /// Shorthand for `self.config().cache`. Use this to construct a
    /// [`CachingStore`](crab_cache_store::CachingStore)
    /// from the bootstrap path.
    #[must_use]
    pub fn cache_config(&self) -> &super::config::CacheConfig {
        &self.inner.config.cache
    }

    /// Shared perf counters for the current session.
    #[must_use]
    pub fn metrics(&self) -> &Metrics {
        &self.inner.metrics
    }

    /// Shared perf counters as an `Arc`, for handing to subsystems that
    /// need an owned handle (e.g. background tasks, speculation driver).
    #[must_use]
    pub fn metrics_arc(&self) -> &Arc<Metrics> {
        &self.inner.metrics
    }

    /// Returns a clone of the cancellation token.
    ///
    /// Subsystems observe this token to unwind cleanly on SIGINT/SIGTERM.
    #[must_use]
    pub fn cancel_token(&self) -> CancellationToken {
        self.inner.cancel.clone()
    }

    /// Check whether the cancellation token has been triggered.
    ///
    /// Long-running operations call this between phases to bail out
    /// early on SIGINT/SIGTERM. Returns `Err(CrabError::Cancelled)`
    /// when the token is cancelled, `Ok(())` otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::Cancelled`] when the token has been triggered.
    pub fn check_cancelled(&self) -> Result<()> {
        if self.inner.cancel.is_cancelled() {
            return Err(CrabError::Cancelled);
        }
        Ok(())
    }
}

impl Default for AppContext {
    fn default() -> Self {
        Self::new(Config::default(), CancellationToken::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_context_has_perf_enabled() {
        let ctx = AppContext::default();
        assert!(ctx.perf().enabled);
    }

    #[test]
    fn clone_shares_same_arc() {
        let ctx = AppContext::default();
        let ctx2 = ctx.clone();
        assert!(std::ptr::eq(ctx.perf(), ctx2.perf()));
    }

    #[test]
    fn custom_perf_config_propagates() {
        let config = Config {
            perf: EngineConfig {
                enabled: true,
                shard_bloom: false,
                ..EngineConfig::default()
            },
            ..Config::default()
        };
        let ctx = AppContext::new(config, CancellationToken::new());
        assert!(!ctx.perf().shard_bloom_active());
        assert!(ctx.perf().compress_staging_active());
    }

    #[test]
    fn config_accessible_through_context() {
        let config = Config {
            upload_concurrency: 32,
            ..Config::default()
        };
        let ctx = AppContext::new(config, CancellationToken::new());
        assert_eq!(ctx.config().upload_concurrency, 32);
        // perf() is a shorthand for config().perf
        assert!(std::ptr::eq(ctx.perf(), &ctx.config().perf));
    }

    #[test]
    fn cancel_token_clones_from_context() {
        let token = CancellationToken::new();
        let ctx = AppContext::new(Config::default(), token.clone());

        let child = ctx.cancel_token();
        assert!(!child.is_cancelled());

        token.cancel();
        assert!(child.is_cancelled());
    }

    #[test]
    fn metrics_accessible_through_context() {
        let ctx = AppContext::default();
        ctx.metrics().inc_shard_bloom_queries();
        ctx.metrics().inc_shard_bloom_queries();
        assert_eq!(ctx.metrics().snapshot().shard_bloom_queries, 2);
    }

    #[test]
    fn cloned_contexts_share_metrics() {
        let ctx = AppContext::default();
        let ctx2 = ctx.clone();
        ctx.metrics().inc_chunk_index_persistent_hits();
        assert_eq!(ctx2.metrics().snapshot().chunk_index_persistent_hits, 1);
    }

    #[test]
    fn check_cancelled_returns_ok_when_not_cancelled() {
        let ctx = AppContext::default();
        assert!(ctx.check_cancelled().is_ok());
    }

    #[test]
    fn check_cancelled_returns_err_when_cancelled() {
        let token = CancellationToken::new();
        let ctx = AppContext::new(Config::default(), token.clone());
        token.cancel();
        let err = ctx.check_cancelled().unwrap_err();
        assert!(matches!(err, CrabError::Cancelled));
    }
}
