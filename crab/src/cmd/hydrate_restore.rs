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
