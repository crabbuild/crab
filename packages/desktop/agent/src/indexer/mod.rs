//! Background indexer for Crab Desktop.
//!
//! Maintains a SlateDB-backed cache at `~/.crab/desktop/indexer/`
//! keyed by `(repo_id, kind, rev)`. The indexer runs as a background
//! tokio task processing commands from an mpsc channel.

pub mod priority;
mod store;
pub mod warmers;

pub use priority::{IndexJob, IndexKind, IndexPriority, PriorityQueue};
pub use store::IndexStore;

use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::handlers::State;

/// Configuration for the background indexer.
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    /// Path to the SlateDB database directory.
    pub db_path: PathBuf,
    /// Whether the indexer is enabled.
    pub enabled: bool,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        let db_path = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".crab")
            .join("desktop")
            .join("indexer");
        Self {
            db_path,
            enabled: true,
        }
    }
}

/// Commands sent to the indexer background task.
#[derive(Debug)]
pub enum IndexerCommand {
    /// Warm the cache for a repository (pre-fetch summaries, logs, etc.).
    WarmRepo { repo_id: String, work_dir: PathBuf },
    /// Invalidate cached entries for a specific path in a repository.
    Invalidate { repo_id: String, path: String },
    /// Promote all queued jobs for a repo to the given priority.
    ///
    /// Sent when a repo becomes active (user opens it) or recent (user
    /// switches away but it's still in the workspace).
    Promote {
        repo_id: String,
        priority: IndexPriority,
    },
    /// Request the current indexer status.
    Status {
        reply: tokio::sync::oneshot::Sender<IndexerStatus>,
    },
    /// Gracefully shut down the indexer.
    Shutdown,
}

/// Snapshot of the indexer's current state.
#[derive(Debug, Clone, serde::Serialize)]
pub struct IndexerStatus {
    /// Path to the SlateDB database.
    pub db_path: PathBuf,
    /// Number of entries currently cached.
    pub entries_cached: u64,
    /// Number of repositories that have been indexed.
    pub repos_indexed: u32,
    /// Whether the indexer background task is running.
    pub running: bool,
}

/// Handle to the background indexer task.
pub struct Indexer {
    handle: JoinHandle<()>,
    tx: mpsc::Sender<IndexerCommand>,
    cancel: CancellationToken,
}

impl Indexer {
    /// Spawn the background indexer task.
    ///
    /// Opens (or creates) a SlateDB instance at the configured path and
    /// enters a loop processing commands from the returned channel.
    /// Returns immediately — the actual DB open happens inside the
    /// spawned task.
    pub fn spawn(_state: State, config: IndexerConfig) -> Self {
        let (tx, rx) = mpsc::channel::<IndexerCommand>(64);

        let cancel = CancellationToken::new();
        let handle = tokio::spawn(indexer_loop(config, rx, cancel.clone()));

        Self { handle, tx, cancel }
    }

    /// Send a command to the indexer.
    pub async fn send(
        &self,
        cmd: IndexerCommand,
    ) -> std::result::Result<(), mpsc::error::SendError<IndexerCommand>> {
        self.tx.send(cmd).await
    }

    /// Clone the command sender so it can be stored in shared state.
    pub fn tx_clone(&self) -> mpsc::Sender<IndexerCommand> {
        self.tx.clone()
    }

    /// Gracefully shut down the indexer and await task completion.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        // Await the background task.
        if let Err(e) = self.handle.await {
            error!(error = %e, "indexer task panicked during shutdown");
        }
    }
}

/// The main indexer loop. Opens the store and processes commands until
/// shutdown or channel close.
async fn indexer_loop(
    config: IndexerConfig,
    mut rx: mpsc::Receiver<IndexerCommand>,
    cancel: CancellationToken,
) {
    if !config.enabled {
        info!("indexer disabled by configuration");
        // Drain any commands that arrive so senders don't block.
        tokio::select! {
            _ = cancel.cancelled() => {},
            _ = async { while rx.recv().await.is_some() {} } => {},
        }
        return;
    }

    info!(db_path = %config.db_path.display(), "indexer: opening store");

    let store = match IndexStore::open(&config.db_path).await {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "indexer: failed to open store, shutting down");
            return;
        }
    };

    info!("indexer: store opened, ready for commands");

    loop {
        let command = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            command = rx.recv() => command,
        };
        match command {
            Some(IndexerCommand::WarmRepo { repo_id, work_dir }) => {
                debug!(repo_id = %repo_id, "indexer: warming repo");
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    result = warmers::warm_all(&store, &work_dir, &repo_id, &cancel) => {
                        if let Err(error) = result { warn!(repo_id = %repo_id, %error, "indexer: warm_all failed"); }
                    }
                }
            }
            Some(IndexerCommand::Invalidate { repo_id, path }) => {
                debug!(repo_id = %repo_id, path = %path, "indexer: invalidate");
                // Invalidation will scan keys with the repo_id prefix
                // and remove matching entries. For now, this is a no-op
                // placeholder.
            }
            Some(IndexerCommand::Promote { repo_id, priority }) => {
                debug!(repo_id = %repo_id, priority = ?priority, "indexer: promote");
                // Priority promotion will be wired to the PriorityQueue
                // once warmers are implemented in task 27.3.
            }
            Some(IndexerCommand::Status { reply }) => {
                let status = IndexerStatus {
                    db_path: config.db_path.clone(),
                    entries_cached: 0,
                    repos_indexed: 0,
                    running: true,
                };
                if reply.send(status).is_err() {
                    warn!("indexer: status reply channel dropped");
                }
            }
            Some(IndexerCommand::Shutdown) => {
                info!("indexer: shutdown requested");
                break;
            }
            None => {
                info!("indexer: command channel closed, shutting down");
                break;
            }
        }
    }

    if let Err(e) = store.close().await {
        error!(error = %e, "indexer: error closing store");
    } else {
        info!("indexer: store closed cleanly");
    }
}
