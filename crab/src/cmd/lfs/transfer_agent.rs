//! `crab lfs-transfer-agent` — standalone LFS transfer agent entry point.
//!
//! This is the binary entry point that `git-lfs` invokes as a custom
//! standalone transfer agent. It wires stdin/stdout to the transfer agent
//! protocol loop implemented in [`crate::lfs::transfer_agent`].

use std::io::{BufReader, BufWriter};

use crate::core::error::Result;
use crab_lfs::LfsObjectStore;

use super::store_setup::resolve_lfs_remote;

/// Run the standalone LFS transfer agent, reading JSON-line events from
/// stdin and writing responses to stdout.
///
/// Returns an error if the remote object store cannot be configured or
/// the protocol loop encounters a fatal error.
pub async fn run_lfs_transfer_agent() -> Result<()> {
    let ctx = resolve_lfs_remote().await?;

    // StdinLock/StdoutLock are not Send, so we use the owned Stdin/Stdout
    // wrapped in BufReader/BufWriter. The run_transfer_agent function
    // moves the reader into a spawn_blocking task internally.
    let input = BufReader::new(std::io::stdin());
    let output = BufWriter::new(std::io::stdout());

    // Unwrap the Arc to get the owned LfsObjectStore for the transfer agent.
    let store = match std::sync::Arc::try_unwrap(ctx.store) {
        Ok(s) => s,
        Err(arc) => LfsObjectStore::new(arc.store().clone(), &ctx.prefix),
    };

    crate::lfs::transfer_agent::run_transfer_agent(input, output, store).await
}
