//! `crab lfs-transfer-agent` — standalone LFS transfer agent entry point.
//!
//! This is the binary entry point that `git-lfs` invokes as a custom
//! standalone transfer agent. It wires stdin/stdout to the transfer agent
//! protocol loop implemented in [`crate::lfs::transfer_agent`].

use std::io::{BufReader, BufWriter};

use crate::core::error::Result;
use crab_lfs::LfsObjectStore;

use super::store_setup::resolve_lfs_remote_for_operation_with_remote;

/// Run the standalone LFS transfer agent, reading JSON-line events from
/// stdin and writing responses to stdout.
///
/// Returns an error if the remote object store cannot be configured or
/// the protocol loop encounters a fatal error.
pub async fn run_lfs_transfer_agent() -> Result<()> {
    // StdinLock/StdoutLock are not Send, so we use the owned Stdin/Stdout
    // wrapped in BufReader/BufWriter. The transfer-agent loop
    // moves the reader into a spawn_blocking task internally.
    let input = BufReader::new(std::io::stdin());
    let output = BufWriter::new(std::io::stdout());

    crate::lfs::transfer_agent::run_transfer_agent_with_resolver(
        input,
        output,
        |operation, remote| async move {
            let ctx =
                resolve_lfs_remote_for_operation_with_remote(&operation, remote.as_deref()).await?;
            let transfer_config = crate::lfs::transfer_agent::TransferAgentConfig {
                max_retries: ctx.config.transfer_max_retries,
                max_retry_delay: ctx.config.transfer_max_retry_delay,
                temp_dir: ctx.local_lfs_dir.join("tmp"),
            };

            // The resolver shares the store with the command context. The
            // transfer agent owns its store for the lifetime of the protocol.
            let store = match std::sync::Arc::try_unwrap(ctx.store) {
                Ok(store) => store,
                Err(store) => LfsObjectStore::new(store.store().clone(), &ctx.prefix),
            };
            Ok((store, transfer_config))
        },
    )
    .await
}
