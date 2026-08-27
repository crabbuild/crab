//! `crab lfs filter-process` — Git LFS process filter protocol endpoint.

use std::process::ExitCode;
use std::sync::Arc;

#[cfg(unix)]
use std::os::fd::AsRawFd;

use crate::core::context::AppContext;
use crate::core::error::Result;

use super::store_setup::resolve_lfs_remote_for_operation_sync;

/// Build the lazy direct-storage resolver used by the LFS filter process.
pub fn lazy_lfs_store_loader() -> crate::git::filter_process::LfsStoreLoader {
    Arc::new(|| {
        resolve_lfs_remote_for_operation_sync("smudge")
            .map(|remote_ctx| remote_ctx.store)
            .map_err(|error| {
                tracing::debug!(
                    error = %error,
                    "filter-process: LFS remote unavailable for non-lazy smudge"
                );
                error
            })
            .ok()
    })
}

/// Run `crab lfs filter-process`.
///
/// This exposes Crab's long-running packet-line filter protocol under the
/// Git LFS command name. Clean/smudge dispatch is still handled by the single
/// canonical filter-process engine, with LFS paths selected from
/// `.gitattributes`.
pub fn run_lfs_filter_process(skip: bool) -> Result<ExitCode> {
    super::block_on_runtime(run_lfs_filter_process_async(skip))?;
    Ok(ExitCode::SUCCESS)
}

async fn run_lfs_filter_process_async(skip: bool) -> Result<()> {
    let mut config = crate::core::config::Config::resolve_local().unwrap_or_default();
    config.checkout.lazy = effective_skip(skip, std::env::var("GIT_LFS_SKIP_SMUDGE").ok());

    let ctx = AppContext::new(config.clone(), tokio_util::sync::CancellationToken::new());
    let lfs_store_loader = (!config.checkout.lazy).then(lazy_lfs_store_loader);

    crate::git::filter_process::run_filter_process_with_lfs_loader(
        std::io::stdin(),
        std::io::stdout(),
        ctx,
        None,
        lfs_store_loader,
        None,
        None,
        #[cfg(unix)]
        Some((
            std::io::stdin().as_raw_fd(),
            crate::git::filter_process::FILTER_IDLE_TIMEOUT,
        )),
    )
    .await
}

fn effective_skip(flag: bool, env_value: Option<String>) -> bool {
    flag || env_value.as_deref().is_some_and(skip_smudge_value_enabled)
}

fn skip_smudge_value_enabled(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_skip_honors_flag() {
        assert!(effective_skip(true, None));
    }

    #[test]
    fn effective_skip_honors_env_value() {
        assert!(effective_skip(false, Some("1".to_owned())));
        assert!(effective_skip(false, Some("true".to_owned())));
    }

    #[test]
    fn effective_skip_ignores_disabled_env_values() {
        assert!(!effective_skip(false, None));
        assert!(!effective_skip(false, Some(String::new())));
        assert!(!effective_skip(false, Some("0".to_owned())));
        assert!(!effective_skip(false, Some("false".to_owned())));
    }
}
