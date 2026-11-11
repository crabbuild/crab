//! Background indexer for Crab Desktop.
//!
//! Maintains a process-owned SlateDB cache keyed by `(repo_id, kind, rev)`.
//! The background task closes the database before removing its private files.

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
    /// Starts a process-owned derived cache; fails if its private directory cannot be created.
    pub fn spawn() -> crate::Result<Self> {
        // SlateDB permits one writer per path. Distinct sidecars and reconnects
        // must not fence each other through an account-wide cache directory.
        let cache = tempfile::Builder::new()
            .prefix("crab-desktop-indexer-")
            .tempdir()?;
        let (tx, rx) = mpsc::channel::<IndexerCommand>(64);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(indexer_loop(cache, rx, cancel.clone()));
        Ok(Self { handle, tx, cancel })
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
    cache: tempfile::TempDir,
    mut rx: mpsc::Receiver<IndexerCommand>,
    cancel: CancellationToken,
) {
    let db_path = cache.path().to_owned();
    info!(db_path = %db_path.display(), "indexer: opening store");

    let store = match IndexStore::open(&db_path).await {
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
                    db_path: db_path.clone(),
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
    // All database tasks and file handles must retire before directory cleanup,
    // particularly on Windows where an open file prevents removing its parent.
    if let Err(error) = cache.close() {
        error!(%error, "indexer: failed to remove private cache");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn status(indexer: &Indexer) -> IndexerStatus {
        let (reply, received) = tokio::sync::oneshot::channel();
        indexer
            .send(IndexerCommand::Status { reply })
            .await
            .unwrap();
        received.await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_indexers_own_and_retire_independent_caches() {
        let first = Indexer::spawn().unwrap();
        let second = Indexer::spawn().unwrap();
        let first_path = status(&first).await.db_path;
        let second_path = status(&second).await.db_path;
        let distinct = first_path != second_path;
        first.shutdown().await;
        let first_removed = !first_path.exists();
        let second_alive = status(&second).await.running && second_path.exists();
        second.shutdown().await;
        assert_eq!(
            (distinct, first_removed, second_alive, second_path.exists()),
            (true, true, true, false)
        );
    }
}
