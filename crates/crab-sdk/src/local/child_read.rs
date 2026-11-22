use std::ffi::OsString;
use std::path::Path;

use crab_remote::local::LocalTools;
use tokio_util::sync::CancellationToken;

use crate::{Result, client::ClientState};

pub(super) struct WorktreeRead {
    pub(super) tools: LocalTools,
    #[cfg(feature = "service")]
    binding: Option<tempfile::NamedTempFile>,
}

impl WorktreeRead {
    pub(super) async fn prepare(
        state: &ClientState,
        tools: &LocalTools,
        root: &Path,
        explicit_content: bool,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        #[cfg(not(feature = "service"))]
        let _ = (state, root, explicit_content, cancel);
        let native = Self {
            tools: tools.clone(),
            #[cfg(feature = "service")]
            binding: None,
        };
        #[cfg(feature = "service")]
        if let Some(cdn) = state
            .direct_cdn
            .as_ref()
            .filter(|cdn| cdn.can_delegate_checkout(state.cdn_consumer))
        {
            use super::{issuer_only_checkout, local_error, persist_checkout_binding};
            use crate::{Error, ErrorKind};

            let tools = if cdn.uses_checkout_issuer() {
                issuer_only_checkout(tools)
            } else {
                tools.clone()
            };
            if !explicit_content
                && !tools
                    .filters_match(root, cancel)
                    .await
                    .map_err(local_error)?
            {
                return Ok(native);
            }
            let handshake = tools.handshake(cancel).await.map_err(local_error)?;
            if !handshake
                .capabilities
                .operations
                .iter()
                .any(|operation| operation == crab_remote::local::DIRECT_CDN_WORKTREE_READ)
            {
                return Err(Error::new(
                    ErrorKind::UnsupportedCapability,
                    "selected Crab executable does not support direct CDN reads in existing worktrees",
                ));
            }
            // Resolve the explicit content configuration in Crab. Git's
            // origin can refer to an unrelated repository in mirrored worktrees.
            let output = tools
                .run_crab(Some(root), ["sdk-content-source"], false, cancel)
                .await
                .map_err(local_error)?;
            let source: Option<String> =
                serde_json::from_slice(&output.stdout).map_err(|error| {
                    Error::with_source(
                        ErrorKind::UnsupportedCapability,
                        "Crab returned an invalid SDK worktree content source",
                        error,
                    )
                })?;
            let Some(locator) = source
                .as_deref()
                .and_then(|source| cdn.worktree_locator(source))
            else {
                return Ok(native);
            };
            let selected = state.resolve_repository(&locator, cancel).await?;
            let selected = cdn
                .resolve(selected, state.cdn_consumer, "hydrate", cancel)
                .await?;
            let mut binding = cdn.checkout_binding(&selected, root, state.cdn_consumer)?;
            binding.require_configured_source = true;
            return Ok(Self {
                tools,
                binding: Some(persist_checkout_binding(&std::env::temp_dir(), binding)?),
            });
        }
        Ok(native)
    }

    pub(super) fn environment(&self) -> Option<(&'static str, OsString)> {
        #[cfg(feature = "service")]
        {
            self.binding.as_ref().map(|binding| {
                (
                    crab_remote::local::DIRECT_CDN_CHECKOUT_BINDING_ENV,
                    binding.path().as_os_str().to_owned(),
                )
            })
        }
        #[cfg(not(feature = "service"))]
        None
    }
}
