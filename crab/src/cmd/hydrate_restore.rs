//! Restore-aware xorb resolution for the hydrate pipeline.
//!
//! When `crab hydrate` encounters an archived xorb, this module
//! probes the storage class via [`head_with_class`] and either:
//!
//! - returns immediately for warm classes (no action needed),
//! - delegates to [`RestoreOrchestrator::ensure_warm`] when
//!   `auto_restore` is enabled,
//! - fails with [`ArchiveRestoreRequired`] when auto-restore is off.
//!
//! This keeps the restore logic out of the main hydrate loop and
//! makes it testable in isolation.
//!
//! ## CLI flags
//!
//! [`RestoreFlags`] captures the `--restore` / `--no-restore` /
//! `--restore-tier` / `--restore-duration-days` CLI flags that
//! override the config-file defaults for a single hydrate invocation.

use std::sync::Arc;
use std::time::Duration;

use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::storage::Store;
use crate::storage::head_class::head_with_class;
use crate::tier::provider::HeadMeta;
use crate::tier::restore::RestoreOrchestrator;

use object_store::path::Path;

// ── CLI restore flags ───────────────────────────────────────────────

/// CLI flags that override the config-file restore defaults for a
/// single `crab hydrate` invocation.
///
/// `--restore` and `--no-restore` are mutually exclusive. When neither
/// is set, the config value `hydrate.auto_restore` applies.
///
/// These fields map 1:1 to clap arguments on the hydrate subcommand.
/// The struct is constructed by the CLI layer and threaded into the
/// hydrate pipeline.
#[derive(Debug, Clone, Default)]
pub struct RestoreFlags {
    /// `--restore`: force auto-restore on for this invocation.
    pub restore: bool,
    /// `--no-restore`: force auto-restore off for this invocation.
    pub no_restore: bool,
    /// `--restore-tier=<T>`: override `hydrate.restore_tier`.
    /// Valid values: `expedited`, `standard`, `bulk` (S3);
    /// `high`, `standard` (Azure).
    pub restore_tier: Option<String>,
    /// `--restore-duration-days=D`: override
    /// `hydrate.restore_duration_days`.
    pub restore_duration_days: Option<u32>,
}

/// Restore gate shared by hydrate, mount, and other v2 read surfaces.
///
/// Every external xorb/shard read passes through this adapter before bytes are
/// requested.  Keeping the `auto_restore` decision here makes `--no-restore`
/// fail with the same explicit archive error as a mount or browser read,
/// instead of leaking a provider-specific GET failure.
pub(crate) struct RestoreAvailability {
    origin: Store,
    orchestrator: Option<Arc<RestoreOrchestrator>>,
    auto_restore: bool,
}

impl RestoreAvailability {
    pub(crate) fn new(
        origin: Store,
        orchestrator: Option<Arc<RestoreOrchestrator>>,
        auto_restore: bool,
    ) -> Self {
        Self {
            origin,
            orchestrator,
            auto_restore,
        }
    }
}

#[async_trait::async_trait]
impl crab_read::XorbAvailability for RestoreAvailability {
    async fn ensure_available(&self, path: &Path) -> crab_read::Result<()> {
        resolve_xorb_with_class_probe(
            &self.origin,
            path,
            self.orchestrator.as_deref(),
            self.auto_restore,
        )
        .await
        .map(|_| ())
        .map_err(crab_read::ReadError::availability)
    }
}

/// Build the restore gate for a resolved v2 read store.
///
/// The backend is selected from the store's physical identity rather than the
/// logical URL, which is required for managed repositories and replica views.
/// Local/in-memory stores have no archive class and therefore do not need a
/// provider backend.
pub(crate) async fn build_restore_availability(
    config: &Config,
    store: &Store,
    repo_prefix: &str,
    auto_restore: bool,
) -> Result<Option<Arc<dyn crab_read::XorbAvailability>>> {
    if !config.tier.enabled {
        return Ok(None);
    }
    if store.bucket_identity().cloud == crab_types::storage::StorageProviderKind::Local {
        return Ok(None);
    }

    let orchestrator = if auto_restore {
        let backend =
            crate::tier::runtime::build_restore_backend_for_store(config, store, repo_prefix)
                .await?;
        let options = crate::tier::runtime::restore_options_from_config(config)?;
        Some(Arc::new(RestoreOrchestrator::with_options(
            backend,
            config.tier.restore_max_concurrency,
            Duration::from_secs(config.tier.restore_timeout_secs),
            options,
        )))
    } else {
        None
    };
    Ok(Some(Arc::new(RestoreAvailability::new(
        store.clone(),
        orchestrator,
        auto_restore,
    ))))
}

impl RestoreFlags {
    /// Resolve the effective `auto_restore` setting by merging CLI
    /// flags with the config default.
    ///
    /// Priority: `--no-restore` > `--restore` > config value.
    pub fn resolve_auto_restore(&self, config_auto_restore: bool) -> bool {
        if self.no_restore {
            return false;
        }
        if self.restore {
            return true;
        }
        config_auto_restore
    }
}

// ── Xorb resolution with class probe ────────────────────────────────

/// Resolve a xorb path with a storage-class probe, triggering a
/// restore if the object is archived and auto-restore is enabled.
///
/// # Arguments
///
/// * `store` — object store handle for the HEAD call.
/// * `path` — full object path of the xorb.
/// * `orchestrator` — restore orchestrator; `None` when auto-restore
///   is disabled or no tier feature is compiled in.
/// * `auto_restore` — mirrors `hydrate.auto_restore` from config.
///
/// # Returns
///
/// `Ok(HeadMeta)` when the object is warm (or has been restored).
/// `Err(ArchiveRestoreRequired)` when the object is archived and
/// auto-restore is off.
pub async fn resolve_xorb_with_class_probe(
    store: &Store,
    path: &Path,
    orchestrator: Option<&RestoreOrchestrator>,
    auto_restore: bool,
) -> Result<HeadMeta> {
    let meta = head_with_class(store, path).await?;

    if meta.class.is_warm_class() {
        return Ok(meta);
    }

    // Archive class — decide whether to restore or fail.
    if auto_restore {
        if let Some(orch) = orchestrator {
            orch.ensure_warm(&path.to_string()).await?;
            return Ok(meta);
        }
        // No orchestrator available (shouldn't happen when auto_restore
        // is true and a tier feature is on, but handle gracefully).
        return Err(CrabError::ArchiveRestoreRequired {
            xorb: path.to_string(),
            class: format!("{}", meta.class),
            estimated_eta: None,
        });
    }

    Err(CrabError::ArchiveRestoreRequired {
        xorb: path.to_string(),
        class: format!("{}", meta.class),
        estimated_eta: None,
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use crate::storage::store::BucketIdentity;
    use crate::tier::StorageClass;
    use object_store::ObjectStoreExt;

    /// Warm class returns Ok immediately without needing an orchestrator.
    #[tokio::test]
    async fn warm_class_returns_ok() {
        let mem = std::sync::Arc::new(object_store::memory::InMemory::new());
        let store = Store::new(mem.clone());
        let path = Path::from("test/warm-xorb");
        let data = bytes::Bytes::from_static(b"xorb-data");
        mem.put(&path, data.into()).await.unwrap();

        // head_with_class returns Unknown (warm) for in-memory store.
        let result = resolve_xorb_with_class_probe(&store, &path, None, true).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().class, StorageClass::Unknown);
    }

    /// Missing object returns an error.
    #[tokio::test]
    async fn missing_object_returns_error() {
        let mem = std::sync::Arc::new(object_store::memory::InMemory::new());
        let store = Store::new(mem);
        let path = Path::from("does/not/exist");

        let result = resolve_xorb_with_class_probe(&store, &path, None, true).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn local_store_does_not_require_archive_backend() {
        let mut config = Config::default();
        config.tier.enabled = true;
        let store = Store::new(std::sync::Arc::new(object_store::memory::InMemory::new()));

        let availability = build_restore_availability(&config, &store, "org/repo", true)
            .await
            .unwrap();

        assert!(availability.is_none());
    }

    #[tokio::test]
    async fn no_restore_gate_does_not_initialize_provider_backend() {
        let mut config = Config::default();
        config.tier.enabled = true;
        let store = Store::new(std::sync::Arc::new(object_store::memory::InMemory::new()))
            .with_bucket_identity(BucketIdentity::new(
                crab_types::storage::StorageProviderKind::Gcs,
                "storage.example",
                "repo",
            ));

        let availability = build_restore_availability(&config, &store, "org/repo", false)
            .await
            .unwrap();

        assert!(availability.is_some());
    }

    // ── RestoreFlags tests ──────────────────────────────────────────

    #[test]
    fn no_restore_flag_overrides_config() {
        let flags = RestoreFlags {
            no_restore: true,
            restore: true, // --restore is set too, but --no-restore wins
            ..Default::default()
        };
        assert!(!flags.resolve_auto_restore(true));
    }

    #[test]
    fn restore_flag_overrides_config_false() {
        let flags = RestoreFlags {
            restore: true,
            ..Default::default()
        };
        assert!(flags.resolve_auto_restore(false));
    }

    #[test]
    fn neither_flag_uses_config() {
        let flags = RestoreFlags::default();
        assert!(flags.resolve_auto_restore(true));
        assert!(!flags.resolve_auto_restore(false));
    }
}
