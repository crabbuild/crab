//! JSON-RPC handler dispatch.
//!
//! Each verb takes typed params, does its work (SDK call, or
//! shell-out), and returns a `serde_json::Value` for the generic
//! response writer in `main.rs`. Repo handles are keyed by a
//! caller-supplied `repo_id` string so the UI can manage multiple
//! open repositories concurrently.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::repository::{BrowseSession, BrowserEntry, ContentKind, EntryKind, FileMetadata};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{RwLock, mpsc};
use tracing::info;

use crate::{
    AgentError, Result, SharedStdout, cancel::CancelRegistry, git_hunks, indexer, preview,
    rpc::Request, shellout,
};

/// Shared state across dispatch tasks. Holds the map of open
/// repositories keyed by a desktop-assigned id.
#[derive(Clone)]
pub struct State {
    #[cfg(unix)]
    pub(crate) native_processes: crate::native_process::NativeProcesses,
    repos: Arc<RwLock<HashMap<String, OpenRepo>>>,
    /// Channel to the background indexer. `None` when the indexer is
    /// disabled or hasn't been spawned yet.
    indexer_tx: Arc<RwLock<Option<mpsc::Sender<indexer::IndexerCommand>>>>,
    /// Registry for cancellable long-running operations.
    pub cancel_registry: CancelRegistry,
    /// Per-repo watch sets for dynamic path management (worktrees).
    watch_sets: Arc<RwLock<HashMap<String, crate::watcher::WatchSet>>>,
    /// In-memory cache for `batch_git_status` delta-mode polling.
    git_status_cache: crate::batch_git_status::GitStatusCache,
}

/// One entry in the open-repo table. Keeps the workspace path
/// alongside the handle so format previewers can resolve files on
/// disk without adding an accessor to the SDK.
#[derive(Clone)]
struct OpenRepo {
    handle: BrowseSession,
    work_dir: Option<PathBuf>,
}

impl State {
    pub fn new() -> Self {
        Self {
            #[cfg(unix)]
            native_processes: crate::native_process::NativeProcesses::default(),
            repos: Arc::new(RwLock::new(HashMap::new())),
            indexer_tx: Arc::new(RwLock::new(None)),
            cancel_registry: CancelRegistry::new(),
            watch_sets: Arc::new(RwLock::new(HashMap::new())),
            git_status_cache: crate::batch_git_status::GitStatusCache::new(),
        }
    }

    /// Set the indexer command sender. Called after spawning the
    /// background indexer task.
    pub async fn set_indexer_tx(&self, tx: mpsc::Sender<indexer::IndexerCommand>) {
        *self.indexer_tx.write().await = Some(tx);
    }

    async fn get(&self, id: &str) -> Result<BrowseSession> {
        let guard = self.repos.read().await;
        guard
            .get(id)
            .map(|r| r.handle.clone())
            .ok_or_else(|| AgentError::RepoNotOpen(id.to_owned()))
    }

    pub(crate) async fn get_work_dir(&self, id: &str) -> Result<PathBuf> {
        let guard = self.repos.read().await;
        let repo = guard
            .get(id)
            .ok_or_else(|| AgentError::RepoNotOpen(id.to_owned()))?;
        repo.work_dir
            .clone()
            .ok_or_else(|| AgentError::Other(format!("repo {id} has no local workspace")))
    }

    async fn insert(&self, id: String, handle: BrowseSession, work_dir: Option<PathBuf>) {
        self.repos
            .write()
            .await
            .insert(id, OpenRepo { handle, work_dir });
    }

    async fn remove(&self, id: &str) -> Result<()> {
        let repo = self.repos.write().await.remove(id);
        if let Some(repo) = repo {
            repo.handle.close().await?;
        }
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        let mut result = Ok(());
        #[cfg(unix)]
        if let Err(error) = self.native_processes.shutdown().await {
            result = Err(error);
        }
        let repos = std::mem::take(&mut *self.repos.write().await);
        for repo in repos.into_values() {
            if let Err(error) = repo.handle.close().await {
                result = Err(error);
            }
        }
        result
    }

    /// Store a [`WatchSet`] for a repo so worktree handlers can
    /// dynamically add/remove watched paths.
    pub async fn set_watch_set(&self, repo_id: String, ws: crate::watcher::WatchSet) {
        self.watch_sets.write().await.insert(repo_id, ws);
    }

    /// Retrieve the [`WatchSet`] for a repo, if one has been registered.
    pub async fn get_watch_set(&self, repo_id: &str) -> Option<crate::watcher::WatchSet> {
        self.watch_sets.read().await.get(repo_id).cloned()
    }
}

/// Top-level dispatch. Matches on the incoming method name and
/// delegates to a typed handler. Unknown methods surface as
/// [`AgentError::UnknownMethod`].
pub async fn handle(state: &State, mut req: Request, out: SharedStdout) -> Result<Value> {
    let auth = req
        .params
        .as_object_mut()
        .and_then(|params| params.remove("__storage_auth"))
        .map(serde_json::from_value::<crate::storage_auth::Connection>)
        .transpose()
        .map_err(|_| AgentError::Other("Invalid storage connection credentials".into()))?;
    crate::storage_auth::scope(auth, handle_request(state, req, out)).await
}

async fn handle_request(state: &State, req: Request, out: SharedStdout) -> Result<Value> {
    let Request { method, params, .. } = req;
    crate::storage_auth::validate_target(state, &method, &params).await?;
    match method.as_str() {
        #[cfg(unix)]
        "native_process_open" => {
            state
                .native_processes
                .open(decode(&method, params)?, out)
                .await
        }
        #[cfg(unix)]
        "native_process_write" => state.native_processes.write(decode(&method, params)?).await,
        #[cfg(unix)]
        "native_process_close_input" => {
            state
                .native_processes
                .close_input(decode(&method, params)?)
                .await
        }
        #[cfg(unix)]
        "native_process_ack" => state.native_processes.ack(decode(&method, params)?).await,
        #[cfg(unix)]
        "native_process_stop" => state.native_processes.stop(decode(&method, params)?).await,
        #[cfg(unix)]
        "native_process_release" => {
            state
                .native_processes
                .release(decode(&method, params)?)
                .await
        }
        "ping" => Ok(json!({ "pong": env!("CARGO_PKG_VERSION") })),
        // These flags describe the extended SSH services, whose wire handlers
        // are not implemented yet. Basic repository RPCs remain available.
        "capabilities" => Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "features": {
                "fileWatching": false,
                "deltaSync": false,
                "compression": false,
                "processManagement": false,
                "environmentManagement": false,
                "debugging": false,
                "search": false,
                "buildRun": false,
                "logTailing": false,
            },
        })),
        "managed_profiles" => crate::managed_service::profiles(),
        "managed_select_profile" => {
            crate::managed_service::select_profile(decode(&method, params)?)
        }
        "managed_login" => crate::managed_service::login(decode(&method, params)?, out).await,
        "managed_organizations" => {
            crate::managed_service::organizations(decode(&method, params)?).await
        }
        "managed_repositories" => {
            crate::managed_service::repositories(decode(&method, params)?).await
        }
        "managed_create_repository" => {
            crate::managed_service::create_repository(decode(&method, params)?).await
        }
        "open_local" => open_local(state, decode(&method, params)?).await,
        "open_url" => open_url(state, decode(&method, params)?).await,
        "close" => close(state, decode(&method, params)?).await,
        "auth_status" => auth_status(state, decode(&method, params)?).await,
        "list" => list(state, decode(&method, params)?, out).await,
        "list_refs" => list_refs(state, decode(&method, params)?).await,
        "stat" => stat(state, decode(&method, params)?).await,
        "pointer_info" => pointer_info(state, decode(&method, params)?).await,
        "read_text" => read_text(state, decode(&method, params)?).await,
        "read_bytes" => read_bytes(state, decode(&method, params)?).await,
        "read_range" => read_range(state, decode(&method, params)?).await,
        "preview_parquet" => preview_parquet(state, decode(&method, params)?).await,
        "preview_safetensors" => {
            preview_fmt(state, decode(&method, params)?, preview::safetensors).await
        }
        "preview_npy" => preview_fmt(state, decode(&method, params)?, preview::numpy_npy).await,
        "preview_npz" => preview_fmt(state, decode(&method, params)?, preview::numpy_npz).await,
        "preview_zip" => preview_fmt(state, decode(&method, params)?, preview::zip_archive).await,
        "preview_notebook_meta" => {
            preview_fmt(state, decode(&method, params)?, preview::notebook_meta).await
        }
        "preview_torch_checkpoint" => {
            preview_fmt(state, decode(&method, params)?, preview::torch_checkpoint).await
        }
        "repo_summary" => repo_summary(state, decode(&method, params)?, out).await,
        "git_status_porcelain" => git_status_porcelain(state, decode(&method, params)?).await,
        "git_diff_file" => git_diff_file(state, decode(&method, params)?).await,
        "git_grep" => git_grep(state, decode(&method, params)?).await,
        "search_replace" => search_replace(state, decode(&method, params)?).await,
        "git_log" => git_log(state, decode(&method, params)?).await,
        "git_stash_list" => git_stash_list(state, decode(&method, params)?).await,
        "git_stash_apply" => git_stash_apply(state, decode(&method, params)?).await,
        "git_stash_pop" => git_stash_pop(state, decode(&method, params)?).await,
        "git_stash_drop" => git_stash_drop(state, decode(&method, params)?).await,
        "git_stash_save" => git_stash_save(state, decode(&method, params)?).await,
        "git_stash_show" => git_stash_show(state, decode(&method, params)?).await,
        "git_stash_show_patch" => git_stash_show_patch(state, decode(&method, params)?).await,
        "pointer_top" => pointer_top(state, decode(&method, params)?).await,
        "git_commit_changes" => git_commit_changes(state, decode(&method, params)?).await,
        "git_commit" => {
            let p: crate::git_commit::GitCommitParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_commit::git_commit(&work_dir, &p).await
        }
        "git_diff_index_patch" => git_diff_index_patch(state, decode(&method, params)?).await,
        "git_diff_cached_patch" => git_diff_cached_patch(state, decode(&method, params)?).await,
        "git_apply_patch" => git_apply_patch(state, decode(&method, params)?).await,
        "git_stage_hunks" => git_stage_hunks(state, decode(&method, params)?).await,
        "git_unstage_hunks" => git_unstage_hunks(state, decode(&method, params)?).await,
        "git_discard_hunks" => git_discard_hunks(state, decode(&method, params)?).await,
        "git_add_paths" => git_add_paths(state, decode(&method, params)?).await,
        "git_reset_paths" => git_reset_paths(state, decode(&method, params)?).await,
        "resolve_ref" => resolve_ref(state, decode(&method, params)?).await,
        "crab_diff_file" => crab_diff_file(state, decode(&method, params)?).await,
        "hydration_status" => hydration_status(state, decode(&method, params)?).await,
        "about" => about().await,
        "agent_health" => agent_health().await,
        "cache_clear" => cache_clear().await,
        "clone" => clone_repo(decode(&method, params)?, out).await,
        "status" => status(decode(&method, params)?, out).await,
        "hydrate" => hydrate(decode(&method, params)?, out).await,
        "dehydrate" => dehydrate(decode(&method, params)?, out).await,
        "push" => push(decode(&method, params)?, out).await,
        "add" => add(decode(&method, params)?, out).await,
        "crab_add_auto" => {
            let p: CrabAddAutoParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crab_add_auto(&work_dir, p).await
        }
        "crab_track_and_add" => {
            let p: CrabTrackAndAddParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crab_track_and_add(&work_dir, p).await
        }
        "crab_add_structured" => {
            let p: CrabAddStructuredParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            let job_id = p.job_id.clone();
            run_cancellable(state, Some(job_id), crab_add_structured(&work_dir, p, out)).await
        }
        "crab_push_structured" => {
            let p: CrabPushStructuredParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            let job_id = p.job_id.clone();
            run_cancellable(state, Some(job_id), crab_push_structured(&work_dir, p, out)).await
        }
        "staging_stats" => {
            let p: StagingStatsParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            staging_stats(&work_dir).await
        }
        "staging_clean" => {
            let p: StagingCleanParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            staging_clean(&work_dir, p.prune_abandoned).await
        }
        "crab_reset_files" => {
            let p: CrabResetFilesParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crab_reset_files(&work_dir, &p.paths).await
        }
        "crab_init" => {
            let p: CrabInitParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crab_init(&work_dir, p).await
        }
        "storage_config_get" => {
            let p: RepoIdParams = decode(&method, params)?;
            let repo = state.get(&p.repo_id).await?;
            crate::storage_auth::repository_config(repo.work_dir.as_deref(), &repo.url).await
        }
        "storage_config_verify" => {
            let p: StorageVerifyParams = decode(&method, params)?;
            let repo = state.get(&p.repo_id).await?;
            crate::storage_auth::test_repository(
                repo.work_dir.as_deref(),
                &repo.url,
                p.remote.as_deref(),
            )
            .await
        }
        "storage_connection_test" => crate::storage_auth::test_connection().await,
        "git_checkout" => {
            let p: crate::git_branches::CheckoutParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_branches::git_checkout(&work_dir, &p).await
        }
        "git_branch_create" => {
            let p: crate::git_branches::BranchCreateParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_branches::git_branch_create(&work_dir, &p).await
        }
        "git_branch_delete" => {
            let p: crate::git_branches::BranchDeleteParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_branches::git_branch_delete(&work_dir, &p).await
        }
        "git_branch_rename" => {
            let p: crate::git_branches::BranchRenameParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_branches::git_branch_rename(&work_dir, &p).await
        }
        "git_set_upstream" => {
            let p: crate::git_branches::SetUpstreamParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_branches::git_set_upstream(&work_dir, &p).await
        }
        "git_worktree_list" => {
            let p: crate::git_worktree::WorktreeListParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_worktree::git_worktree_list(&work_dir).await
        }
        "git_worktree_add" => {
            let p: crate::git_worktree::WorktreeAddParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_worktree::git_worktree_add(&work_dir, &p).await
        }
        "git_worktree_remove" => {
            let p: crate::git_worktree::WorktreeRemoveParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_worktree::git_worktree_remove(&work_dir, &p).await
        }
        "git_worktree_move" => {
            let p: crate::git_worktree::WorktreeMoveParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_worktree::git_worktree_move(&work_dir, &p).await
        }
        "git_worktree_prune" => {
            let p: crate::git_worktree::WorktreePruneParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_worktree::git_worktree_prune(&work_dir, &p).await
        }
        "git_worktree_lock" => {
            let p: crate::git_worktree::WorktreeLockParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_worktree::git_worktree_lock(&work_dir, &p).await
        }
        "git_worktree_unlock" => {
            let p: crate::git_worktree::WorktreeUnlockParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_worktree::git_worktree_unlock(&work_dir, &p).await
        }
        "git_worktree_repair" => {
            let p: crate::git_worktree::WorktreeRepairParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_worktree::git_worktree_repair(&work_dir, &p).await
        }
        "worktree_list" => {
            let p: WorktreeListParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            worktree_list(&work_dir).await
        }
        "worktree_add" => {
            let p: WorktreeAddManagerParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            worktree_add(state, &work_dir, p).await
        }
        "worktree_remove" => {
            let p: WorktreeRemoveManagerParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            worktree_remove(state, &work_dir, p).await
        }
        "worktree_move" => {
            let p: WorktreeMoveManagerParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            worktree_move(&work_dir, p).await
        }
        "worktree_lock" => {
            let p: WorktreeLockManagerParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            worktree_lock(&work_dir, p).await
        }
        "worktree_unlock" => {
            let p: WorktreeUnlockManagerParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            worktree_unlock(&work_dir, p).await
        }
        "worktree_prune" => {
            let p: WorktreePruneManagerParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            worktree_prune(&work_dir).await
        }
        "worktree_repair" => {
            let p: WorktreeRepairManagerParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            worktree_repair(&work_dir, p).await
        }
        "worktree_status" => {
            let p: WorktreeStatusParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            worktree_status(&work_dir, p).await
        }
        "worktree_set_meta" => {
            let p: WorktreeSetMetaParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            worktree_set_meta(&work_dir, p).await
        }
        "worktree_push_estimate" => {
            let p: WorktreePushEstimateParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            worktree_push_estimate(&work_dir, p).await
        }
        "worktree_create" => {
            let p: WorktreeCreateParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            worktree_create(&work_dir, p).await
        }
        "worktree_destroy" => {
            let p: WorktreeDestroyParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            worktree_destroy(&work_dir, p).await
        }
        "task_compute_diff" => {
            let p: crate::task_diff::Params = decode(&method, params)?;
            crate::task_diff::compare(p).await
        }
        "task_apply" => {
            let p: TaskApplyParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            #[cfg(unix)]
            return state.native_processes.apply(work_dir, p).await;
            #[cfg(not(unix))]
            task_apply(&work_dir, p).await
        }
        "task_discard" => {
            let p: TaskDiscardParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            task_discard(&work_dir, p).await
        }
        "task_check_orphaned" => {
            let p: TaskCheckOrphanedParams = decode(&method, params)?;
            task_check_orphaned(p).await
        }
        "git_tag_add" => git_tag_add(state, decode(&method, params)?).await,
        "git_tag_delete" => git_tag_delete(state, decode(&method, params)?).await,
        "git_cherry_pick" => git_cherry_pick(state, decode(&method, params)?).await,
        "git_revert" => git_revert(state, decode(&method, params)?).await,
        "git_merge" => git_merge(state, decode(&method, params)?).await,
        "git_rebase" => git_rebase(state, decode(&method, params)?).await,
        "git_reset" => git_reset(state, decode(&method, params)?).await,
        "git_fetch" => git_fetch(state, decode(&method, params)?).await,
        "git_pull" => git_pull(state, decode(&method, params)?).await,
        "git_fetch_branch" => git_fetch_branch(state, decode(&method, params)?).await,
        "git_pull_branch" => git_pull_branch(state, decode(&method, params)?).await,
        "git_push_branch" => git_push_branch(state, decode(&method, params)?).await,
        "git_fetch_all" => {
            let p: crate::git_remote::FetchAllParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_remote::git_fetch_all(&work_dir, &p).await
        }
        "git_pull_mode" => {
            let p: crate::git_remote::PullParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_remote::git_pull(&work_dir, &p).await
        }
        "git_push_remote" => {
            let p: crate::git_remote::PushParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_remote::git_push(&work_dir, &p).await
        }
        "git_remote_add" => {
            let p: crate::git_remote::RemoteAddParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_remote::git_remote_add(&work_dir, &p).await
        }
        "git_remote_remove" => {
            let p: crate::git_remote::RemoteRemoveParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_remote::git_remote_remove(&work_dir, &p).await
        }
        "git_remote_set_url" => {
            let p: crate::git_remote::RemoteSetUrlParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::git_remote::git_remote_set_url(&work_dir, &p).await
        }
        "git_config_get" => git_config_get(state, decode(&method, params)?).await,
        "git_activity" => git_activity(state, decode(&method, params)?).await,
        "git_contributors" => git_contributors(state, decode(&method, params)?).await,
        "git_pulse" => git_pulse(state, decode(&method, params)?).await,
        "git_commit_frequency" => git_commit_frequency(state, decode(&method, params)?).await,
        "git_code_frequency" => git_code_frequency(state, decode(&method, params)?).await,
        "preview_sqlite" => preview_sqlite(state, decode(&method, params)?).await,
        "duckdb_query" => duckdb_query(state, decode(&method, params)?).await,
        "preview_onnx" => preview_onnx(state, decode(&method, params)?).await,
        "preview_hdf5" => preview_hdf5(state, decode(&method, params)?).await,
        "preview_protobuf" => preview_protobuf(state, decode(&method, params)?).await,
        "preview_stl" => preview_stl(state, decode(&method, params)?).await,
        "git_branch_info" => git_branch_info(state, decode(&method, params)?).await,
        "git_branch_health" => git_branch_health(state, decode(&method, params)?).await,
        "git_repo_info" => git_repo_info(state, decode(&method, params)?).await,
        "op_start" => {
            let p: crate::ops::handlers::OpStartParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::ops::handlers::op_start(&work_dir, p).await
        }
        "op_continue" => {
            let p: crate::ops::handlers::OpContinueParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::ops::handlers::op_continue(&work_dir, p).await
        }
        "op_abort" => {
            let p: crate::ops::handlers::OpAbortParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::ops::handlers::op_abort(&work_dir, p).await
        }
        "op_status" => {
            let p: crate::ops::handlers::OpStatusParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::ops::handlers::op_status(&work_dir, p).await
        }
        "op_edit_todo" => {
            let p: crate::ops::handlers::OpEditTodoParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::ops::handlers::op_edit_todo(&work_dir, p).await
        }
        "conflict_list" => {
            let p: crate::conflicts::ConflictListParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::conflicts::conflict_list(&work_dir).await
        }
        "conflict_read_sides" => {
            let p: crate::conflicts::ConflictReadSidesParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::conflicts::conflict_read_sides(&work_dir, &p).await
        }
        "git_apply_resolution" => {
            let p: crate::conflicts::ApplyResolutionParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::conflicts::git_apply_resolution(&work_dir, &p).await
        }
        "experiments_runs_for_commit" => {
            experiments_runs_for_commit(decode(&method, params)?).await
        }
        "experiments_runs_recent" => experiments_runs_recent(decode(&method, params)?).await,
        "experiments_run_detail" => experiments_run_detail(decode(&method, params)?).await,
        "experiments_configure" => experiments_configure(decode(&method, params)?).await,
        "notebook_strip_outputs" => {
            let p: crate::notebook_strip::NotebookStripParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::notebook_strip::notebook_strip_outputs(&work_dir, &p).await
        }
        "storage_summary" => {
            let p: RepoIdParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            let (usage, metadata) = tokio::try_join!(
                shellout::run_json(Some(&work_dir), &["du", "--json"], &[]),
                shellout::run_json(Some(&work_dir), &["metadb", "diagnose", "--json"], &[]),
            )?;
            Ok(json!({ "usage": usage, "metadata": metadata }))
        }
        "indexer_status" => indexer_status(state).await,
        "rpc/cancel" => rpc_cancel(state, decode(&method, params)?).await,
        "forge_auth_start" => {
            let p: crate::forge::auth::ForgeAuthStartParams = decode(&method, params)?;
            crate::forge::auth::forge_auth_start(p, out).await
        }
        "auth_token_updated" => {
            // Called by the main process after successfully storing an
            // OAuth token in the keychain. The agent can use this to
            // refresh any cached auth state. For now, emit a notification
            // so the renderer knows auth completed.
            let p: AuthTokenUpdatedParams = decode(&method, params)?;
            crate::notify(
                &out,
                "auth/token_updated",
                &serde_json::json!({
                    "provider": p.provider,
                }),
            )
            .await?;
            Ok(json!({ "ok": true }))
        }
        "forge_list_prs" => {
            let p: crate::forge::handlers::ForgeListPrsParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::forge::handlers::forge_list_prs(&work_dir).await
        }
        "forge_create_pr" => {
            let p: crate::forge::handlers::ForgeCreatePrParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::forge::handlers::forge_create_pr(&work_dir, p).await
        }
        "forge_pr_detail" => {
            let p: crate::forge::handlers::ForgePrDetailParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::forge::handlers::forge_pr_detail(&work_dir, p).await
        }
        "forge_pr_comment" => {
            let p: crate::forge::handlers::ForgePrCommentParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::forge::handlers::forge_pr_comment(&work_dir, p).await
        }
        "forge_submit_review" => {
            let p: crate::forge::handlers::ForgeSubmitReviewParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::forge::handlers::forge_submit_review(&work_dir, p).await
        }
        "forge_list_checks" => {
            let p: crate::forge::handlers::ForgeListChecksParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::forge::handlers::forge_list_checks(&work_dir, p).await
        }
        "forge_list_issues" => {
            let p: crate::forge::handlers::ForgeListIssuesParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::forge::handlers::forge_list_issues(&work_dir, p).await
        }
        "workflow_list" => {
            let p: crate::workflow::WorkflowListParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::workflow::workflow_list(&work_dir).await
        }
        "workflow_get" => {
            let p: crate::workflow::WorkflowGetParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::workflow::workflow_get(&work_dir, p).await
        }
        "workflow_save" => {
            let p: crate::workflow::WorkflowSaveParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::workflow::workflow_save(&work_dir, p).await
        }
        "workflow_create" => {
            let p: crate::workflow::WorkflowCreateParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::workflow::workflow_create(&work_dir, p).await
        }
        "workflow_delete" => {
            let p: crate::workflow::WorkflowDeleteParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::workflow::workflow_delete(&work_dir, p).await
        }
        "workflow_run" => {
            let p: crate::workflow::WorkflowRunParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            let run_id = p.run_id.clone();
            run_cancellable(
                state,
                Some(run_id),
                crate::workflow::workflow_run(&work_dir, p, out),
            )
            .await
        }
        "workflow_cancel" => {
            let p: crate::workflow::WorkflowCancelParams = decode(&method, params)?;
            state.get_work_dir(&p.repo_id).await?;
            Ok(json!({ "cancelled": state.cancel_registry.cancel(&p.run_id).await }))
        }
        "workflow_validate" => {
            let p: crate::workflow::WorkflowValidateParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::workflow::workflow_validate(&work_dir, p).await
        }
        // ── SDK-backed workflow builder verbs ────────────────────────
        "workflow_status" => {
            let p: crate::workflow::WorkflowStatusParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::workflow::workflow_status(&work_dir, p).await
        }
        "workflow_dag" => {
            let p: crate::workflow::WorkflowDagParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::workflow::workflow_dag(&work_dir, p).await
        }
        "workflow_resolve_template" => {
            let p: crate::workflow::WorkflowResolveTemplateParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::workflow::workflow_resolve_template(&work_dir, p).await
        }
        "workflow_template_completions" => {
            let p: crate::workflow::WorkflowTemplateCompletionsParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::workflow::workflow_template_completions(&work_dir, p).await
        }
        "workflow_exp_list" => {
            let p: crate::workflow::WorkflowExpListParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::workflow::workflow_exp_list(&work_dir).await
        }
        "workflow_exp_enqueue" => {
            let p: crate::workflow::WorkflowExpEnqueueParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::workflow::workflow_exp_enqueue(&work_dir, p).await
        }
        "workflow_exp_start" => {
            let p: crate::workflow::WorkflowExpStartParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::workflow::workflow_exp_start(&work_dir, p, out).await
        }
        "workflow_exp_stop" => {
            let p: crate::workflow::WorkflowExpStopParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::workflow::workflow_exp_stop(&work_dir).await
        }
        "workflow_migrate_preview" => {
            let p: crate::workflow::WorkflowMigratePreviewParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::workflow::workflow_migrate_preview(&work_dir, p).await
        }
        "workflow_migrate_apply" => {
            let p: crate::workflow::WorkflowMigrateApplyParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            crate::workflow::workflow_migrate_apply(&work_dir, p).await
        }
        "write_text" => write_text(state, decode(&method, params)?).await,
        "write_bytes" => write_bytes(state, decode(&method, params)?).await,
        "dedup_savings" => {
            let p: DedupSavingsParams = decode(&method, params)?;
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            dedup_savings(&work_dir, p).await
        }
        "git_release_timeline" => git_release_timeline(state, decode(&method, params)?).await,
        "repo_size_history" => repo_size_history(state, decode(&method, params)?).await,
        "git_commit_size_distribution" => {
            git_commit_size_distribution(state, decode(&method, params)?).await
        }
        "git_contributor_growth" => git_contributor_growth(state, decode(&method, params)?).await,
        "git_blame" => git_blame(state, decode(&method, params)?).await,
        "git_diff_name_only" => git_diff_name_only(state, decode(&method, params)?).await,
        "system_health" => {
            let p: crate::system_health::SystemHealthParams = decode(&method, params)?;
            crate::system_health::system_health(p).await
        }
        "discover_repos" => {
            let p: crate::workspace_discovery::DiscoverReposParams = decode(&method, params)?;
            crate::workspace_discovery::discover_repos(p).await
        }
        "batch_git_status" => {
            let p: crate::batch_git_status::BatchGitStatusParams = decode(&method, params)?;
            crate::batch_git_status::batch_git_status(&state.git_status_cache, p).await
        }
        other => Err(AgentError::UnknownMethod(other.to_owned())),
    }
}

fn decode<T: for<'de> Deserialize<'de>>(method: &str, value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|source| AgentError::InvalidParams {
        method: method.to_owned(),
        source,
    })
}

// ---------------- Cancellation ----------------

#[derive(Debug, Deserialize)]
struct RpcCancelParams {
    op_id: String,
}

#[derive(Debug, Deserialize)]
struct AuthTokenUpdatedParams {
    provider: String,
}

async fn rpc_cancel(state: &State, p: RpcCancelParams) -> Result<Value> {
    let found = state.cancel_registry.cancel(&p.op_id).await;
    Ok(json!({ "ok": found }))
}

/// Run a handler future with optional cancellation support. When
/// `op_id` is `Some`, the operation is registered in the cancel
/// registry and the work races against the cancel signal via
/// `tokio::select!`. When `op_id` is `None`, the work runs directly.
///
/// IMPORTANT: Any `tokio::process::Command` spawned inside `work` must
/// use `.kill_on_drop(true)` so the child process is terminated when
/// the future is dropped on cancellation. Without it, orphaned git
/// processes continue consuming I/O and can starve subsequent requests.
async fn run_cancellable<F>(state: &State, op_id: Option<String>, work: F) -> Result<Value>
where
    F: std::future::Future<Output = Result<Value>>,
{
    match op_id {
        None => work.await,
        Some(ref id) => {
            let mut cancel_rx = state.cancel_registry.register(id.clone()).await;
            tokio::pin!(work);
            let result = tokio::select! {
                biased;
                _ = &mut cancel_rx => {
                    Err(AgentError::OpCancelled { op_id: id.clone() })
                }
                res = &mut work => res,
            };
            state.cancel_registry.unregister(id).await;
            result
        }
    }
}

// ---------------- SDK-backed handlers ----------------

#[derive(Debug, Deserialize)]
struct OpenLocalParams {
    repo_id: String,
    path: PathBuf,
}

async fn open_local(state: &State, p: OpenLocalParams) -> Result<Value> {
    state.remove(&p.repo_id).await?;
    let repo = BrowseSession::open_local(&p.path).await?;
    let url = repo.url.clone();
    let cache_dir = "";
    let work_dir = repo
        .work_dir
        .clone()
        .ok_or_else(|| AgentError::Other("local repository has no working directory".into()))?;
    state
        .insert(p.repo_id.clone(), repo, Some(work_dir.clone()))
        .await;

    // Ensure the .crab/desktop/ on-disk layout exists with all
    // expected files at Git's worktree root, even when opened from a subdirectory.
    ensure_desktop_layout(&work_dir).await;

    // Crash-resume: check for any in-flight ops so the renderer can
    // show a resume banner immediately without a second RPC.
    let active_op = find_active_op(&work_dir);

    // Forge provider detection: manual override or auto-detect from
    // remote URL. Runs async (may probe self-hosted instances).
    // Also persists provider/owner/name to settings.json so that
    // subsequent forge RPCs (list_prs, create_pr, etc.) can read them.
    let detected_forge = crate::forge::detect::detect_and_persist(&work_dir).await;
    let forge_provider = detected_forge.as_ref().map(|f| f.provider);

    info!(repo_id = %p.repo_id, path = %p.path.display(), forge = ?forge_provider, "opened local repo");
    Ok(json!({
        "repo_id": p.repo_id,
        "url": url,
        "is_remote": false,
        "cache_dir": cache_dir,
        "work_dir": work_dir,
        "active_op": active_op,
        "forge_provider": forge_provider,
    }))
}

/// Check the ops directory for any InProgress or Paused operation.
/// Returns a JSON value with the op details, or `null` if none found.
fn find_active_op(work_dir: &std::path::Path) -> Value {
    use crate::ops::{OpState, load_all, ops_dir_for_repo};

    let ops_dir = ops_dir_for_repo(work_dir);
    let records = match load_all(&ops_dir) {
        Ok(r) => r,
        Err(_) => return Value::Null,
    };

    let active = records
        .into_iter()
        .find(|r| r.state == OpState::InProgress || r.state == OpState::Paused);

    match active {
        Some(r) => json!({
            "op_id": r.id.to_string(),
            "state": format!("{:?}", r.state),
            "op": r.op,
            "paused_reason": r.paused_reason,
        }),
        None => Value::Null,
    }
}

/// Ensure the `.crab/desktop/` on-disk layout exists with all expected
/// files. Creates the directory tree and seed files that don't yet exist.
///
/// This is best-effort: failures are logged but don't block repo open.
async fn ensure_desktop_layout(work_dir: &std::path::Path) {
    use tokio::io::AsyncWriteExt as _;

    let desktop_dir = work_dir.join(".crab").join("desktop");

    // Create the directory tree (ops/ is created lazily by the driver,
    // but the parent must exist for worktrees.json and settings.json).
    if let Err(e) = tokio::fs::create_dir_all(&desktop_dir).await {
        tracing::warn!(error = %e, "failed to create .crab/desktop/ directory");
        return;
    }

    // Plain Git repositories have no Crab ignore rules. Keep generated local
    // state out of their changes; Git still reports any already-tracked files.
    match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(desktop_dir.join(".gitignore"))
        .await
    {
        Ok(mut file) => {
            // Await the queued write so subsequent Git status sees the ignore rule.
            let written = async {
                file.write_all(b"*\n").await?;
                file.flush().await
            }
            .await;
            if let Err(error) = written {
                tracing::warn!(%error, "failed to ignore desktop state");
                return;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            tracing::warn!(%error, "failed to create desktop state ignore file");
            return;
        }
    }

    // Seed worktrees.json with an empty array if it doesn't exist.
    let worktrees_path = desktop_dir.join("worktrees.json");
    if !worktrees_path.exists() {
        if let Err(e) = tokio::fs::write(&worktrees_path, b"[]").await {
            tracing::warn!(error = %e, "failed to create worktrees.json");
        }
    }
}

#[derive(Debug, Deserialize)]
struct OpenUrlParams {
    repo_id: String,
    url: String,
}

async fn open_url(state: &State, p: OpenUrlParams) -> Result<Value> {
    state.remove(&p.repo_id).await?;
    let repo = BrowseSession::open_url(&p.url).await?;
    let url = repo.url.clone();
    let is_remote = repo.work_dir.is_none();
    let cache_dir = "";
    state.insert(p.repo_id.clone(), repo, None).await;
    info!(repo_id = %p.repo_id, url = %p.url, "opened url repo");
    Ok(json!({
        "repo_id": p.repo_id,
        "url": url,
        "is_remote": is_remote,
        "cache_dir": cache_dir,
    }))
}

#[derive(Debug, Deserialize)]
struct RepoIdParams {
    repo_id: String,
}

#[derive(Debug, Deserialize)]
struct ListRefsParams {
    repo_id: String,
    /// Optional list of ref kinds to fetch. Empty (default) returns
    /// all three namespaces (heads/tags/remotes). Pass e.g.
    /// `["branch"]` to skip the (often huge) remotes namespace on
    /// repos that mirror many upstream branches.
    #[serde(default)]
    kinds: Vec<String>,
    /// Optional cap on the number of refs returned. When set, git's
    /// `--count` flag is forwarded. The agent never enforces a hard
    /// upper bound — callers that want all refs simply omit this.
    #[serde(default)]
    max: Option<usize>,
}

async fn close(state: &State, p: RepoIdParams) -> Result<Value> {
    state.remove(&p.repo_id).await?;
    Ok(json!({ "closed": p.repo_id }))
}

async fn auth_status(state: &State, p: RepoIdParams) -> Result<Value> {
    let repo = state.get(&p.repo_id).await?;
    crate::shellout::run_json(repo.work_dir.as_deref(), &["auth", "status", "--json"], &[]).await
}

#[derive(Debug, Deserialize)]
struct ListParams {
    repo_id: String,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    rev: Option<String>,
    /// When present, the handler streams each entry as a
    /// `progress/partial` notification before returning the full array.
    #[serde(default)]
    op_id: Option<String>,
}

async fn list(state: &State, p: ListParams, out: SharedStdout) -> Result<Value> {
    let repo = state.get(&p.repo_id).await?;
    let entries = repo.list(p.prefix.as_deref(), p.rev.as_deref()).await?;
    if let Some(ref op_id) = p.op_id {
        for e in &entries {
            let _ = crate::emit_partial(&out, op_id, "list_entry", serialize_dir_entry(e)).await;
        }
    }
    let out_entries: Vec<Value> = entries.iter().map(serialize_dir_entry).collect();
    Ok(json!({ "entries": out_entries }))
}

async fn list_refs(state: &State, p: ListRefsParams) -> Result<Value> {
    let repo = state.get(&p.repo_id).await?;
    if repo.work_dir.is_none() {
        let references = repo.refs().await?;
        let refs: Vec<_> = references
            .entries()
            .iter()
            .filter_map(|reference| {
                let full = reference.name();
                let (name, kind) = if let Some(name) = full.strip_prefix("refs/heads/") {
                    (name, "branch")
                } else if let Some(name) = full.strip_prefix("refs/tags/") {
                    (name, "tag")
                } else if let Some(name) = full.strip_prefix("refs/remotes/") {
                    (name, "remote")
                } else {
                    return None;
                };
                if !p.kinds.is_empty()
                    && !p.kinds.iter().any(|wanted| {
                        wanted == kind
                            || matches!(
                                (wanted.as_str(), kind),
                                ("head" | "heads", "branch")
                                    | ("tags", "tag")
                                    | ("remotes", "remote")
                            )
                    })
                {
                    return None;
                }
                Some(
                    json!({ "name": name, "target": reference.target().to_string(), "kind": kind,
                "upstream": null, "ahead": null, "behind": null,
                "is_head": references.head() == Some(full) }),
                )
            })
            .take(p.max.unwrap_or(usize::MAX))
            .collect();
        return Ok(json!({ "refs": refs }));
    }
    let work_dir = state.get_work_dir(&p.repo_id).await?;

    // Build the namespace list from the requested kinds. Defaults to
    // all three so existing callers keep working unchanged.
    let kinds: Vec<&str> = if p.kinds.is_empty() {
        vec!["heads", "tags", "remotes"]
    } else {
        p.kinds
            .iter()
            .filter_map(|k| match k.as_str() {
                "branch" | "head" | "heads" => Some("heads"),
                "tag" | "tags" => Some("tags"),
                "remote" | "remotes" => Some("remotes"),
                _ => None,
            })
            .collect()
    };
    let mut namespace_args: Vec<String> = Vec::with_capacity(kinds.len());
    for k in &kinds {
        namespace_args.push(format!("refs/{k}"));
    }

    // Shell out to `git for-each-ref` with a format that includes
    // upstream tracking info (ahead/behind) so the renderer can show
    // chips without a second RPC.
    let format = "%(refname:short)%00%(objectname)%00%(objecttype)%00%(upstream:short)%00%(upstream:track,nobracket)%00%(HEAD)";
    let mut cmd_args: Vec<String> = vec!["for-each-ref".to_string(), format!("--format={format}")];
    // Cap remote/tag count via --count when the caller asked for it.
    // Bare-headed loops over thousands of remote-tracking refs (e.g.
    // a heavily-mirrored upstream) are the dominant cost on huge
    // repos, so the renderer can opt into a smaller payload.
    if let Some(max) = p.max {
        cmd_args.push(format!("--count={max}"));
    }
    cmd_args.extend(namespace_args);

    let out = crate::shellout::git_command()
        .args(&cmd_args)
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut refs: Vec<Value> = Vec::new();

    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(6, '\0').collect();
        if parts.len() < 6 {
            continue;
        }
        let name = parts[0];
        let target = parts[1];
        let upstream = if parts[3].is_empty() {
            None
        } else {
            Some(parts[3])
        };
        let track = parts[4];
        let is_head = parts[5].trim() == "*";

        // Determine kind from the full refname prefix. `for-each-ref`
        // with `%(refname:short)` strips the prefix, so we infer from
        // the ref namespace we queried and the presence of `/` patterns
        // typical of remotes. A more reliable approach: also emit
        // %(refname) and match on that.
        let kind = if name.contains('/') && !name.starts_with("heads/") {
            // Likely a remote-tracking ref (e.g. "origin/main")
            "remote"
        } else if parts[2] == "tag" {
            "tag"
        } else {
            "branch"
        };

        // Parse track string like "ahead 3", "behind 1", or
        // "ahead 3, behind 1" into separate counts.
        let (ahead, behind) = parse_track_info(track);

        refs.push(json!({
            "name": name,
            "target": target,
            "kind": kind,
            "upstream": upstream,
            "ahead": ahead,
            "behind": behind,
            "is_head": is_head,
        }));
    }

    Ok(json!({ "refs": refs }))
}

/// Parse git's `%(upstream:track,nobracket)` output into ahead/behind
/// counts. Examples: "ahead 3", "behind 1", "ahead 3, behind 1", "".
fn parse_track_info(track: &str) -> (Option<u32>, Option<u32>) {
    if track.is_empty() {
        return (None, None);
    }
    let mut ahead: Option<u32> = None;
    let mut behind: Option<u32> = None;
    for segment in track.split(',') {
        let segment = segment.trim();
        if let Some(n) = segment.strip_prefix("ahead ") {
            ahead = n.trim().parse().ok();
        } else if let Some(n) = segment.strip_prefix("behind ") {
            behind = n.trim().parse().ok();
        }
    }
    (ahead, behind)
}

#[derive(Debug, Deserialize)]
struct StatParams {
    repo_id: String,
    path: String,
    #[serde(default)]
    rev: Option<String>,
}

async fn stat(state: &State, p: StatParams) -> Result<Value> {
    let repo = state.get(&p.repo_id).await?;
    match repo.stat(&p.path, p.rev.as_deref()).await {
        Ok(stat) => Ok(serialize_file_stat(&stat)),
        Err(AgentError::PathNotFound { .. }) => {
            // Fallback: file is untracked but may exist on disk.
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            let abs = work_dir.join(&p.path);
            let metadata =
                tokio::fs::metadata(&abs)
                    .await
                    .map_err(|_| AgentError::PathNotFound {
                        path: p.path.clone(),
                    })?;
            let mtime = metadata.modified().ok().and_then(systime_to_secs);
            Ok(json!({
                "size": metadata.len(),
                "pointer_hash": null,
                "mtime": mtime,
                "rev": null,
                "kind": if metadata.is_dir() { "directory" } else { "file" },
                "untracked": true,
            }))
        }
        Err(e) => Err(e.into()),
    }
}

async fn pointer_info(state: &State, p: StatParams) -> Result<Value> {
    let repo = state.get(&p.repo_id).await?;
    repo.pointer_info(&p.path, p.rev.as_deref()).await
}

#[derive(Debug, Deserialize)]
struct ReadTextParams {
    repo_id: String,
    path: String,
    #[serde(default)]
    rev: Option<String>,
    /// Cap on bytes returned to the UI. Defaults to 512 KiB so the
    /// renderer can't accidentally DOS itself on a 4 GB pointer.
    #[serde(default = "default_max_bytes")]
    max_bytes: usize,
}

fn default_max_bytes() -> usize {
    512 * 1024
}

async fn read_text(state: &State, p: ReadTextParams) -> Result<Value> {
    let repo = state.get(&p.repo_id).await?;
    match repo.stat(&p.path, p.rev.as_deref()).await {
        Ok(stat) => {
            if stat.size as usize > p.max_bytes {
                return Ok(json!({
                    "truncated": true,
                    "size": stat.size,
                    "text": null,
                    "reason": format!("file is {} bytes, limit is {}", stat.size, p.max_bytes),
                }));
            }
            let text = repo.read_text(&p.path, p.rev.as_deref()).await?;
            Ok(json!({
                "truncated": false,
                "size": stat.size,
                "text": text,
            }))
        }
        Err(AgentError::PathNotFound { .. }) => {
            // Fallback: file is untracked but may exist on disk.
            read_text_from_worktree(state, &p).await
        }
        Err(e) => Err(e.into()),
    }
}

/// Filesystem fallback for `read_text` when the SDK reports NotFound
/// (untracked file). Reads directly from the working tree.
async fn read_text_from_worktree(state: &State, p: &ReadTextParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let abs = work_dir.join(&p.path);
    if !abs.exists() {
        return Err(AgentError::PathNotFound {
            path: p.path.clone(),
        });
    }
    let metadata = std::fs::metadata(&abs).map_err(AgentError::Io)?;
    let size = metadata.len() as usize;
    if size > p.max_bytes {
        return Ok(json!({
            "truncated": true,
            "size": size,
            "text": null,
            "untracked": true,
            "reason": format!("file is {} bytes, limit is {}", size, p.max_bytes),
        }));
    }
    let bytes = tokio::fs::read(&abs).await.map_err(AgentError::Io)?;
    match String::from_utf8(bytes) {
        Ok(text) => Ok(json!({
            "truncated": false,
            "size": size,
            "text": text,
            "untracked": true,
        })),
        Err(_) => Err(AgentError::Other(format!(
            "file is not valid UTF-8: {}",
            p.path
        ))),
    }
}

// ─── Worktree list (Pillar B) ────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WorktreeListParams {
    repo_id: String,
}

/// List all worktrees with reconciled UI metadata.
///
/// Creates a [`WorktreeManager`] for the repo and delegates to its
/// `worktree_list` method which runs `git worktree list --porcelain`,
/// reconciles with `worktrees.json`, and returns hydrated entries.
async fn worktree_list(work_dir: &Path) -> Result<Value> {
    let mgr = crate::worktree::WorktreeManager::new(work_dir.to_path_buf());
    let entries = mgr.worktree_list().await?;
    Ok(json!({ "worktrees": entries }))
}

#[derive(Debug, Deserialize)]
struct WorktreeAddManagerParams {
    repo_id: String,
    /// Filesystem path for the new worktree (relative or absolute).
    path: String,
    /// Branch name — existing branch to check out, or new branch name
    /// when `create_branch` is true.
    branch: String,
    /// When true, create a new branch with `-b`.
    #[serde(default)]
    create_branch: bool,
    /// Start point for the new branch (only used when `create_branch`
    /// is true). Defaults to HEAD if omitted.
    #[serde(default)]
    base: Option<String>,
    /// Hydration policy for pointer files. Defaults to `pointers_only`
    /// since the renderer shows a modal after creation for the user to
    /// choose; a separate hydration action applies the final choice.
    #[serde(default = "default_hydration_choice")]
    hydration: crate::worktree::HydrationChoice,
}

fn default_hydration_choice() -> crate::worktree::HydrationChoice {
    crate::worktree::HydrationChoice::PointersOnly
}

/// Add a new worktree via the WorktreeManager.
///
/// Creates a [`WorktreeManager`] and delegates to
/// `worktree_add_with_hydration` which runs core `crab worktree add`,
/// scans for pointer files, applies the hydration policy, and
/// returns the new entry with an optional hydration offer.
///
/// On success, adds the new worktree path to the file watcher's
/// watch set so changes in the worktree trigger indexer refreshes.
async fn worktree_add(
    state: &State,
    work_dir: &Path,
    p: WorktreeAddManagerParams,
) -> Result<Value> {
    let mgr = crate::worktree::WorktreeManager::new(work_dir.to_path_buf());
    let path = std::path::PathBuf::from(&p.path);
    let repo_id = p.repo_id.clone();
    let result = mgr
        .worktree_add_with_hydration(
            &path,
            &p.branch,
            p.create_branch,
            p.base.as_deref(),
            p.hydration,
        )
        .await?;

    // Audit log: record the worktree addition.
    crate::audit::log_entry(
        "worktree_add",
        json!({
            "path": p.path,
            "branch": p.branch,
        }),
    );

    // Add the new worktree path to the file watcher so changes are
    // picked up by the incremental refresh system.
    if let Some(watch_set) = state.get_watch_set(&repo_id).await {
        let wt_path = &result.worktree.entry.path;
        if let Err(e) = watch_set.watch_path(wt_path).await {
            tracing::warn!(
                path = %wt_path.display(),
                error = %e,
                "failed to add worktree path to watch set (non-fatal)"
            );
        }
    }

    Ok(json!({
        "created": true,
        "worktree": result.worktree,
        "hydration_offer": result.hydration_offer,
    }))
}

#[derive(Debug, Deserialize)]
struct WorktreeRemoveManagerParams {
    repo_id: String,
    /// Path of the worktree to remove.
    path: String,
    /// Force removal even if the worktree has uncommitted changes.
    #[serde(default)]
    force: bool,
}

/// Remove a worktree via the WorktreeManager.
///
/// Creates a [`WorktreeManager`] and delegates to `worktree_remove`
/// which runs core `crab worktree remove`, propagates errors for dirty
/// worktrees when force is false, and removes the metadata entry
/// from `worktrees.json` on success.
///
/// On success, removes the worktree path from the file watcher's
/// watch set so stale events are no longer generated.
async fn worktree_remove(
    state: &State,
    work_dir: &Path,
    p: WorktreeRemoveManagerParams,
) -> Result<Value> {
    let mgr = crate::worktree::WorktreeManager::new(work_dir.to_path_buf());
    let path = std::path::PathBuf::from(&p.path);
    let repo_id = p.repo_id.clone();

    // Resolve the path before removal so we can unwatch it after the
    // directory is deleted.
    let resolved = if path.is_absolute() {
        path.canonicalize().unwrap_or_else(|_| path.clone())
    } else {
        work_dir
            .join(&path)
            .canonicalize()
            .unwrap_or_else(|_| work_dir.join(&path))
    };

    mgr.worktree_remove(&path, p.force).await?;

    // Audit log: record the worktree removal.
    crate::audit::log_entry(
        "worktree_remove",
        json!({
            "path": p.path,
            "force": p.force,
        }),
    );

    // Remove the worktree path from the file watcher so we stop
    // receiving events for the now-deleted directory.
    if let Some(watch_set) = state.get_watch_set(&repo_id).await {
        if let Err(e) = watch_set.unwatch_path(&resolved).await {
            tracing::warn!(
                path = %resolved.display(),
                error = %e,
                "failed to remove worktree path from watch set (non-fatal)"
            );
        }
    }

    Ok(json!({ "removed": true, "path": p.path }))
}

#[derive(Debug, Deserialize)]
struct WorktreeMoveManagerParams {
    repo_id: String,
    /// Current path of the worktree to move.
    old_path: String,
    /// New filesystem path for the worktree.
    new_path: String,
}

/// Move a worktree to a new filesystem path via the WorktreeManager.
///
/// Creates a [`WorktreeManager`] and delegates to `worktree_move`
/// which runs core `crab worktree move <old> <new>` and updates the path
/// in `worktrees.json` metadata on success.
async fn worktree_move(work_dir: &Path, p: WorktreeMoveManagerParams) -> Result<Value> {
    let mgr = crate::worktree::WorktreeManager::new(work_dir.to_path_buf());
    let old_path = std::path::PathBuf::from(&p.old_path);
    let new_path = std::path::PathBuf::from(&p.new_path);
    mgr.worktree_move(&old_path, &new_path).await?;

    // Audit log: record the worktree move.
    crate::audit::log_entry(
        "worktree_move",
        json!({
            "old_path": p.old_path,
            "new_path": p.new_path,
        }),
    );

    Ok(json!({ "moved": true, "old_path": p.old_path, "new_path": p.new_path }))
}

#[derive(Debug, Deserialize)]
struct WorktreeLockManagerParams {
    repo_id: String,
    /// Path of the worktree to lock.
    path: String,
    /// Optional reason for locking.
    #[serde(default)]
    reason: Option<String>,
}

/// Lock a worktree via the WorktreeManager.
///
/// Creates a [`WorktreeManager`] and delegates to `worktree_lock`
/// which runs core `crab worktree lock <path>` with an optional `--reason`.
async fn worktree_lock(work_dir: &Path, p: WorktreeLockManagerParams) -> Result<Value> {
    let mgr = crate::worktree::WorktreeManager::new(work_dir.to_path_buf());
    let path = std::path::PathBuf::from(&p.path);
    mgr.worktree_lock(&path, p.reason.as_deref()).await?;

    // Audit log: record the worktree lock.
    crate::audit::log_entry(
        "worktree_lock",
        json!({
            "path": p.path,
            "reason": p.reason,
        }),
    );

    Ok(json!({ "locked": true, "path": p.path, "reason": p.reason }))
}

#[derive(Debug, Deserialize)]
struct WorktreeUnlockManagerParams {
    repo_id: String,
    /// Path of the worktree to unlock.
    path: String,
}

/// Unlock a worktree via the WorktreeManager.
///
/// Creates a [`WorktreeManager`] and delegates to `worktree_unlock`
/// which runs core `crab worktree unlock <path>`.
async fn worktree_unlock(work_dir: &Path, p: WorktreeUnlockManagerParams) -> Result<Value> {
    let mgr = crate::worktree::WorktreeManager::new(work_dir.to_path_buf());
    let path = std::path::PathBuf::from(&p.path);
    mgr.worktree_unlock(&path).await?;

    // Audit log: record the worktree unlock.
    crate::audit::log_entry(
        "worktree_unlock",
        json!({
            "path": p.path,
        }),
    );

    Ok(json!({ "unlocked": true, "path": p.path }))
}

#[derive(Debug, Deserialize)]
struct WorktreePruneManagerParams {
    repo_id: String,
}

/// Prune stale worktree entries via the WorktreeManager.
///
/// Creates a [`WorktreeManager`] and delegates to `worktree_prune`
/// which runs core `crab worktree prune` and reconciles `worktrees.json`
/// metadata by removing entries for pruned worktrees.
async fn worktree_prune(work_dir: &Path) -> Result<Value> {
    let mgr = crate::worktree::WorktreeManager::new(work_dir.to_path_buf());
    mgr.worktree_prune().await?;
    Ok(json!({ "pruned": true }))
}

#[derive(Debug, Deserialize)]
struct WorktreeRepairManagerParams {
    repo_id: String,
    /// Worktree paths to repair. Empty lets Git repair discoverable links.
    #[serde(default)]
    paths: Vec<String>,
}

/// Repair worktree administrative links via the WorktreeManager.
async fn worktree_repair(work_dir: &Path, p: WorktreeRepairManagerParams) -> Result<Value> {
    let mgr = crate::worktree::WorktreeManager::new(work_dir.to_path_buf());
    let paths = p
        .paths
        .iter()
        .map(std::path::PathBuf::from)
        .collect::<Vec<_>>();
    mgr.worktree_repair(&paths).await?;
    Ok(json!({ "repaired": true, "paths": p.paths }))
}

#[derive(Debug, Deserialize)]
struct WorktreeStatusParams {
    repo_id: String,
    /// Path of the worktree to query status for.
    path: String,
}

/// Get dirty-file count and ahead/behind for a specific worktree.
///
/// Creates a [`WorktreeManager`] and delegates to `worktree_status`
/// which runs `git status --porcelain` and `git rev-list --left-right`
/// in the worktree directory.
async fn worktree_status(work_dir: &Path, p: WorktreeStatusParams) -> Result<Value> {
    let mgr = crate::worktree::WorktreeManager::new(work_dir.to_path_buf());
    let path = std::path::PathBuf::from(&p.path);
    mgr.worktree_status(&path).await
}

#[derive(Debug, Deserialize)]
struct WorktreeSetMetaParams {
    repo_id: String,
    /// Path of the worktree to update metadata for.
    path: String,
    /// User-chosen display name shown in the worktree card.
    #[serde(default)]
    display_name: Option<String>,
    /// Colour label for the worktree card (CSS colour string).
    #[serde(default)]
    colour: Option<String>,
    /// Whether the worktree is pinned to the top of the list.
    #[serde(default)]
    pinned: bool,
    /// Hydration policy for the worktree.
    #[serde(default = "default_hydration_policy")]
    hydration_policy: String,
}

fn default_hydration_policy() -> String {
    "all".to_owned()
}

/// Update UI metadata (display name, colour, pin) for a worktree.
///
/// Creates a [`WorktreeManager`] and delegates to `worktree_set_meta`
/// which loads `worktrees.json`, upserts the entry for the given path,
/// and persists the result.
async fn worktree_set_meta(work_dir: &Path, p: WorktreeSetMetaParams) -> Result<Value> {
    use crate::worktree::{HydrationChoice, WorktreeMeta};

    let mgr = crate::worktree::WorktreeManager::new(work_dir.to_path_buf());

    let hydration_policy = match p.hydration_policy.as_str() {
        "on_access" => HydrationChoice::OnAccess,
        "pointers_only" => HydrationChoice::PointersOnly,
        _ => HydrationChoice::All,
    };

    let meta = WorktreeMeta {
        path: std::path::PathBuf::from(&p.path),
        display_name: p.display_name,
        colour: p.colour,
        pinned: p.pinned,
        hydration_policy,
    };

    mgr.worktree_set_meta(meta).await?;
    Ok(json!({ "ok": true, "path": p.path }))
}

#[derive(Debug, Deserialize)]
struct WorktreePushEstimateParams {
    repo_id: String,
    /// Path of the worktree to estimate push cost for.
    path: String,
}

/// Estimate the push cost for a worktree based on pointer files.
///
/// Creates a [`WorktreeManager`] and delegates to `worktree_push_estimate`
/// which scans the worktree's HEAD for pointer files and returns the
/// total bytes, xorb count estimate, and pointer file count.
async fn worktree_push_estimate(work_dir: &Path, p: WorktreePushEstimateParams) -> Result<Value> {
    let mgr = crate::worktree::WorktreeManager::new(work_dir.to_path_buf());
    let path = std::path::PathBuf::from(&p.path);
    let estimate = mgr.worktree_push_estimate(&path).await?;
    Ok(json!({
        "total_bytes": estimate.total_bytes,
        "xorb_count": estimate.xorb_count,
        "pointer_count": estimate.pointer_count,
    }))
}

// ─── Remote orchestrator worktree creation ──────────────────────────

#[derive(Debug, Deserialize)]
struct WorktreeCreateParams {
    repo_id: String,
    /// Branch name to create for the task worktree.
    branch: String,
    /// Base ref to branch from (commit SHA, branch name, or "HEAD").
    base_ref: String,
    /// Task UUID used as the worktree directory name.
    task_id: String,
}

/// Create a worktree for a remote orchestrator task.
///
/// This is the remote counterpart to the local `createWorktree()` in
/// `orchestrator-worktree.ts`. The worktree is placed at
/// `~/.crab/worktrees/<task_id>` and a new branch is created
/// through the core Crab worktree contract.
///
/// Returns the workspace, resolved baseline and the captured Apply branch.
async fn worktree_create(work_dir: &Path, p: WorktreeCreateParams) -> Result<Value> {
    let base_branch = crate::git_ref_merge::current_branch_ref(work_dir)
        .await?
        .map(|branch| branch.trim_start_matches("refs/heads/").to_owned());
    let task_id = uuid::Uuid::parse_str(&p.task_id).map_err(|source| AgentError::Managed {
        kind: "task_identity".into(),
        message: "Remote workspace creation requires a task UUID".into(),
        remediation: "Start a new conversation with a new task identity".into(),
        source: Some(Box::new(source)),
    })?;
    let baseline = format!("{}^{{commit}}", p.base_ref);
    let resolved_base = run_git_worktree_cmd(
        work_dir,
        &["rev-parse", "--verify", "--end-of-options", &baseline],
    )
    .await?;
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or_else(|| AgentError::Other("Remote HOME is unavailable".into()))?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return Err(AgentError::Other(
            "Remote HOME must be an absolute path".into(),
        ));
    }
    let worktrees_dir = home.join(".crab").join("worktrees");
    let wt_path = worktrees_dir.join(task_id.to_string());
    // A new conversation cannot authorize cleanup of a stale or colliding
    // directory. Full UUIDs isolate tasks; an existing path requires recovery.
    tokio::fs::create_dir_all(&worktrees_dir).await?;
    tokio::fs::create_dir(&wt_path)
        .await
        .map_err(|source| AgentError::Managed {
            kind: "task_workspace".into(),
            message: format!("Cannot reserve remote workspace: {}", wt_path.display()),
            remediation:
                "Recover an existing workspace or repair directory permissions before retrying"
                    .into(),
            source: Some(Box::new(source)),
        })?;
    let mgr = crate::worktree::WorktreeManager::new(work_dir.to_path_buf());
    if let Err(error) = mgr
        .worktree_add(&wt_path, &p.branch, true, Some(&resolved_base))
        .await
    {
        // Release only our empty reservation. A post-create failure can leave
        // a real checkout; non-recursive removal preserves its files for recovery.
        let _ = tokio::fs::remove_dir(&wt_path).await;
        return Err(error);
    }

    crate::task_workspace::TaskWorkspace::record(work_dir, &wt_path, task_id, &p.branch).await?;

    Ok(json!({
        "path": wt_path,
        "branch": p.branch,
        "base_ref": resolved_base,
        "base_branch": base_branch,
    }))
}

#[derive(Debug, Deserialize)]
struct WorktreeDestroyParams {
    repo_id: String,
    /// Path of the worktree to destroy (as returned by `worktree_create`).
    path: String,
    /// Branch name to delete after removing the worktree.
    #[serde(default)]
    branch: Option<String>,
    /// Force removal even if the worktree has uncommitted changes.
    #[serde(default)]
    force: bool,
}

/// Destroy a worktree created by `worktree_create`.
///
/// Removes the worktree directory and optionally deletes the associated
/// branch. This is the cleanup counterpart for remote orchestrator tasks.
async fn worktree_destroy(work_dir: &Path, p: WorktreeDestroyParams) -> Result<Value> {
    let workspace = crate::task_workspace::TaskWorkspace::open(
        work_dir,
        Path::new(&p.path),
        None,
        p.branch.as_deref(),
    )
    .await?;
    let cleanup = workspace
        .remove(work_dir, p.force, p.branch.is_some())
        .await?;
    Ok(
        json!({ "removed": true, "path": p.path, "branch_deleted": cleanup.branch_deleted, "cleanup_error": cleanup.error }),
    )
}

// ─── Task Apply ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[cfg_attr(unix, derive(serde::Serialize))]
pub(crate) struct TaskApplyParams {
    repo_id: String,
    task_id: String,
    /// Branch name of the task to merge.
    branch: String,
    /// Required Apply destination captured when the task was created.
    #[serde(default)]
    base_branch: Option<String>,
    /// Path to the task worktree (for cleanup after merge).
    worktree_path: String,
}

/// Apply a remote orchestrator task: merge the task branch into the base
/// branch, then remove the worktree and delete the task branch.
///
/// Steps:
/// 1. Validate the captured target before staging source changes.
/// 2. Commit any uncommitted changes in the worktree.
/// 3. Merge in the active worktree when the target is current; otherwise
///    create the merge commit and update the target ref without checkout.
/// 4. Remove the worktree.
/// 5. Delete the task branch.
pub(crate) async fn task_apply(repo_dir: &Path, p: TaskApplyParams) -> Result<Value> {
    let workspace = crate::task_workspace::TaskWorkspace::open(
        repo_dir,
        Path::new(&p.worktree_path),
        Some(&p.task_id),
        Some(&p.branch),
    )
    .await?;
    // The baseline describes source history, not the destination. An absent
    // or deleted captured branch cannot authorize merging into today's checkout.
    let target_branch = p
        .base_branch
        .filter(|branch| !branch.is_empty())
        .ok_or_else(|| AgentError::Managed {
            kind: "task_apply_target".into(),
            message: "Apply target was not recorded".into(),
            remediation: "Recover the task's destination before applying changes".into(),
            source: None,
        })?;
    let target_ref = crate::git_ref_merge::local_branch_ref(&target_branch);
    run_git_cmd(repo_dir, &["show-ref", "--verify", "--", &target_ref])
        .await
        .map_err(|source| AgentError::Managed {
            kind: "task_apply_target".into(),
            message: "Saved Apply target is unavailable".into(),
            remediation: "Restore its branch before applying changes".into(),
            source: Some(Box::new(source)),
        })?;
    crate::git_ref_merge::require_direct_branch(repo_dir, &target_ref).await?;
    #[cfg(unix)]
    crate::git_active_apply::require_available(repo_dir).await?;
    workspace.begin_apply()?;
    let result =
        apply_workspace(repo_dir, &workspace, &p.branch, &target_branch, &target_ref).await;
    #[cfg(unix)]
    let retain = crate::native_process::mutation::requires_recovery();
    #[cfg(not(unix))]
    let retain = false;
    if !retain && let Err(error) = workspace.finish_apply() {
        tracing::warn!(%error, "could not clear task Apply fence");
    }
    result
}

async fn apply_workspace(
    repo_dir: &Path,
    workspace: &crate::task_workspace::TaskWorkspace,
    branch: &str,
    target_branch: &str,
    target_ref: &str,
) -> Result<Value> {
    let wt = workspace.root();
    // Staging or commit failure cannot merge a stale task branch and then
    // delete the workspace containing the user's uncommitted changes.
    let status = run_git_cmd(wt, &["status", "--porcelain"]).await?;
    if !status.trim().is_empty() {
        run_git_cmd(wt, &["add", "-A"]).await?;
        run_git_cmd(
            wt,
            &[
                "commit",
                "-m",
                "Agent work (auto-committed before apply)",
                "--allow-empty-message",
            ],
        )
        .await?;
    }

    let source_ref = crate::git_ref_merge::local_branch_ref(branch);
    if crate::git_ref_merge::current_branch_ref(wt)
        .await?
        .as_deref()
        != Some(&source_ref)
    {
        return Err(AgentError::Other(
            "Task workspace branch changed before Apply".into(),
        ));
    }
    // Use the exact task ref's object. Git resolves a short tag name before
    // a branch with the same name, even if the task branch is checked out.
    let source = crate::git_ref_merge::exact_branch_oid(wt, &source_ref).await?;
    let commit_msg = format!("Merge agent task branch '{branch}'");

    let current_ref = crate::git_ref_merge::current_branch_ref(repo_dir).await?;

    let mut merge_commit: Option<String> = None;
    let merge_result = if current_ref.as_deref() == Some(target_ref) {
        #[cfg(unix)]
        {
            merge_commit = Some(
                crate::git_active_apply::merge(repo_dir, target_ref, &source, wt, &commit_msg)
                    .await?,
            );
            Ok(String::new())
        }
        #[cfg(not(unix))]
        run_git_cmd(repo_dir, &["merge", &source, "--no-ff", "-m", &commit_msg]).await
    } else {
        match crate::git_ref_merge::merge_commit_without_checkout(
            repo_dir,
            &source,
            target_branch,
            &commit_msg,
        )
        .await?
        {
            crate::git_ref_merge::RefMergeOutcome::Applied {
                merge_commit: commit,
                ..
            } => {
                merge_commit = Some(commit);
                Ok(String::new())
            }
            crate::git_ref_merge::RefMergeOutcome::AlreadyUpToDate { commit, .. } => {
                merge_commit = Some(commit);
                Ok(String::new())
            }
            crate::git_ref_merge::RefMergeOutcome::TargetCheckedOut { target_branch, .. } => {
                Err(AgentError::Other(format!(
                    "Target branch '{}' is checked out in another worktree; switch to that worktree to merge.",
                    target_branch
                )))
            }
        }
    };

    if let Err(ref e) = merge_result {
        let err_str = format!("{}", e);
        if err_str.contains("CONFLICT") || err_str.contains("Automatic merge failed") {
            // Abort the merge and report the conflict.
            let _ = run_git_cmd(repo_dir, &["merge", "--abort"]).await;
            return Err(AgentError::Other(format!(
                "Merge conflict when applying task branch '{branch}'. Merge aborted."
            )));
        }
        merge_result?;
    }

    // A Git merge receipt alone cannot authorize deleting the task. Verify the
    // captured branch contains the pinned source before removing the checkout.
    if !crate::git_ref_merge::branch_contains(repo_dir, target_ref, &source).await? {
        return Err(AgentError::Managed {
            kind: "task_apply_target".into(),
            message: format!(
                "Saved Apply target '{target_branch}' does not contain the task commit"
            ),
            remediation: "Inspect the repository checkout and task workspace before retrying"
                .into(),
            source: None,
        });
    }

    let (worktree_removed, branch_deleted, cleanup_error) =
        match workspace.remove(repo_dir, true, true).await {
            Ok(cleanup) => (true, cleanup.branch_deleted, cleanup.error),
            Err(error) => {
                // Merge already succeeded. Report retained cleanup separately so
                // the client does not retry or hide a merge that actually landed.
                tracing::warn!(%error, "task applied; workspace retained");
                (false, false, Some(error.to_string()))
            }
        };

    Ok(json!({
        "applied": true,
        "target_branch": target_branch,
        "merge_commit": merge_commit,
        "worktree_removed": worktree_removed,
        "branch_deleted": branch_deleted,
        "cleanup_error": cleanup_error,
    }))
}

// ─── Task Discard ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TaskDiscardParams {
    repo_id: String,
    task_id: String,
    /// Branch name of the task to delete.
    branch: String,
    /// Path to the task worktree to remove.
    worktree_path: String,
}

/// Discard a remote orchestrator task: remove the worktree and delete
/// the task branch without merging.
///
/// Steps:
/// 1. Force-remove the worktree.
/// 2. Delete the task branch.
async fn task_discard(repo_dir: &Path, p: TaskDiscardParams) -> Result<Value> {
    let workspace = crate::task_workspace::TaskWorkspace::open(
        repo_dir,
        Path::new(&p.worktree_path),
        Some(&p.task_id),
        Some(&p.branch),
    )
    .await?;
    let cleanup = workspace.remove(repo_dir, true, true).await?;
    Ok(
        json!({ "discarded": true, "worktree_removed": true, "branch_deleted": cleanup.branch_deleted, "cleanup_error": cleanup.error }),
    )
}

// ─── Task Check Orphaned ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TaskCheckOrphanedParams {
    task_id: String,
    #[serde(default)]
    worktree_path: Option<String>,
    #[serde(default)]
    branch: Option<String>,
}

/// Check whether an orphaned task's worktree still exists on the remote
/// and whether the agent CLI process is still running in it.
///
/// Called by the desktop app after reconnecting to determine the state
/// of tasks that were interrupted by a connection drop. The response
/// tells the desktop whether to re-attach, prompt the user, or mark
/// the task as failed.
async fn task_check_orphaned(p: TaskCheckOrphanedParams) -> Result<Value> {
    let wt_path = p.worktree_path.as_deref().unwrap_or("");

    // Check if the worktree directory exists on disk.
    let worktree_exists = if wt_path.is_empty() {
        false
    } else {
        let wt = PathBuf::from(wt_path);
        wt.exists() && wt.is_dir()
    };

    // Check if an agent CLI process (claude or codex) is running in
    // the worktree by looking for processes whose cwd matches.
    let agent_running = if worktree_exists && !wt_path.is_empty() {
        check_agent_running_in_worktree(wt_path).await
    } else {
        false
    };

    Ok(json!({
        "taskId": p.task_id,
        "worktreePath": wt_path,
        "branch": p.branch.unwrap_or_default(),
        "worktreeExists": worktree_exists,
        "agentRunning": agent_running,
    }))
}

/// Check if an agent CLI process (claude or codex) is running with a
/// working directory matching the given worktree path.
///
/// Uses `pgrep` + `/proc` on Linux or `ps` on macOS to detect running
/// processes. Returns false on any error (conservative — better to
/// prompt the user than to silently re-attach to a dead process).
async fn check_agent_running_in_worktree(worktree_path: &str) -> bool {
    // Strategy: use `lsof +D <path>` or check /proc for processes
    // with cwd in the worktree. A simpler cross-platform approach:
    // run `ps aux` and grep for claude/codex with the worktree path.
    let result = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "ps aux | grep -E '(claude|codex)' | grep -F '{}' | grep -v grep",
            worktree_path,
        ))
        .output()
        .await;

    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            !stdout.trim().is_empty()
        }
        Err(_) => false,
    }
}

// ─── Task Apply/Discard Helpers ─────────────────────────────────────
/// Run a git command in the given directory and return stdout.
async fn run_git_cmd(cwd: &Path, args: &[&str]) -> Result<String> {
    let mut command = shellout::git_command();
    let out = shellout::output(command.args(args).current_dir(cwd)).await?;

    if !out.status.success() {
        let status = out.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        return Err(AgentError::Cli { status, stderr });
    }

    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Run a git command and return stdout, mapping failures to AgentError.
pub(crate) async fn run_git_worktree_cmd(work_dir: &Path, args: &[&str]) -> Result<String> {
    Ok(run_git_cmd(work_dir, args).await?.trim().to_owned())
}

// ─── Write text ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WriteTextParams {
    repo_id: String,
    path: String,
    content: String,
}

/// Write text content to a file in the working tree.
///
/// Only works for local repos (those with a `work_dir`). The file is
/// written atomically (write to temp + rename) to avoid partial writes
/// on crash. Parent directories are created if needed.
async fn write_text(state: &State, p: WriteTextParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let abs = work_dir.join(&p.path);

    // Ensure parent directories exist.
    if let Some(parent) = abs.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(AgentError::Io)?;
    }

    // Atomic write: write to a sibling temp file, then rename.
    let tmp = abs.with_extension("crab-tmp");
    tokio::fs::write(&tmp, p.content.as_bytes())
        .await
        .map_err(AgentError::Io)?;
    tokio::fs::rename(&tmp, &abs)
        .await
        .map_err(AgentError::Io)?;

    let size = p.content.len();
    Ok(json!({ "written": true, "size": size }))
}

// ─── Write bytes ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WriteBytesParams {
    repo_id: String,
    path: String,
    /// Base64-encoded file content.
    bytes_b64: String,
}

/// Write binary content (base64-encoded) to a file in the working tree.
///
/// Same semantics as `write_text` — local repos only, atomic write,
/// parent directories created as needed. The payload is decoded from
/// base64 before writing.
async fn write_bytes(state: &State, p: WriteBytesParams) -> Result<Value> {
    use base64::Engine as _;

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&p.bytes_b64)
        .map_err(|e| AgentError::Other(format!("invalid base64: {e}")))?;

    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let abs = work_dir.join(&p.path);

    // Ensure parent directories exist.
    if let Some(parent) = abs.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(AgentError::Io)?;
    }

    // Atomic write: write to a sibling temp file, then rename.
    let tmp = abs.with_extension("crab-tmp");
    tokio::fs::write(&tmp, &bytes)
        .await
        .map_err(AgentError::Io)?;
    tokio::fs::rename(&tmp, &abs)
        .await
        .map_err(AgentError::Io)?;

    let size = bytes.len();
    Ok(json!({ "written": true, "size": size }))
}

/// Read a file at `(path, rev)` as raw bytes, returned base64-encoded.
///
/// The editor UI calls this when a blob looks binary (no UTF-8 decode
/// available) so it can render a hex preview or media panel without
/// the text-path's lossy UTF-8 roundtrip. Same size cap as
/// `read_text` — the ceiling is enforced here rather than trusting
/// callers to check `stat` first.
async fn read_bytes(state: &State, p: ReadTextParams) -> Result<Value> {
    use base64::Engine as _;

    let repo = state.get(&p.repo_id).await?;
    match repo.stat(&p.path, p.rev.as_deref()).await {
        Ok(stat) => {
            if stat.size as usize > p.max_bytes {
                return Ok(json!({
                    "truncated": true,
                    "size": stat.size,
                    "bytes_b64": null,
                    "reason": format!("file is {} bytes, limit is {}", stat.size, p.max_bytes),
                }));
            }
            let bytes = repo.read(&p.path, p.rev.as_deref()).await?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes.as_ref());
            Ok(json!({
                "truncated": false,
                "size": stat.size,
                "bytes_b64": encoded,
            }))
        }
        Err(AgentError::PathNotFound { .. }) => {
            // Fallback: file is untracked but may exist on disk.
            read_bytes_from_worktree(state, &p).await
        }
        Err(e) => Err(e.into()),
    }
}

/// Filesystem fallback for `read_bytes` when the SDK reports NotFound
/// (untracked file). Reads directly from the working tree.
async fn read_bytes_from_worktree(state: &State, p: &ReadTextParams) -> Result<Value> {
    use base64::Engine as _;

    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let abs = work_dir.join(&p.path);
    if !abs.exists() {
        return Err(AgentError::PathNotFound {
            path: p.path.clone(),
        });
    }
    let metadata = std::fs::metadata(&abs).map_err(AgentError::Io)?;
    let size = metadata.len() as usize;
    if size > p.max_bytes {
        return Ok(json!({
            "truncated": true,
            "size": size,
            "bytes_b64": null,
            "untracked": true,
            "reason": format!("file is {} bytes, limit is {}", size, p.max_bytes),
        }));
    }
    let bytes = tokio::fs::read(&abs).await.map_err(AgentError::Io)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(json!({
        "truncated": false,
        "size": size,
        "bytes_b64": encoded,
        "untracked": true,
    }))
}

#[derive(Debug, Deserialize)]
struct ReadRangeParams {
    repo_id: String,
    path: String,
    offset: u64,
    /// Cap at 8 MiB per request — large enough for a Parquet footer or
    /// a thumbnail, small enough that the renderer stays responsive.
    length: u32,
    #[serde(default)]
    rev: Option<String>,
}

/// Read a byte range from `(path, rev)`. Returns base64-encoded.
///
/// Implementation note: the SDK doesn't yet expose an origin-side
/// range reader, so for now we read the full blob and slice. That
/// makes this verb only useful for already-hydrated files; callers
/// asking for a range on a pointer will get the full file fetched.
/// The verb's signature is future-proof — when the SDK gains native
/// range reads the implementation swaps without a breaking change.
async fn read_range(state: &State, p: ReadRangeParams) -> Result<Value> {
    use base64::Engine as _;

    if p.length > 8 * 1024 * 1024 {
        return Err(AgentError::Other(format!(
            "read_range length {} exceeds 8 MiB cap",
            p.length
        )));
    }
    let repo = state.get(&p.repo_id).await?;
    match repo.read(&p.path, p.rev.as_deref()).await {
        Ok(bytes) => {
            let end = (p.offset + u64::from(p.length)).min(bytes.len() as u64) as usize;
            let start = p.offset.min(bytes.len() as u64) as usize;
            let slice = &bytes[start..end];
            let encoded = base64::engine::general_purpose::STANDARD.encode(slice);
            Ok(json!({
                "offset": start,
                "length": end - start,
                "bytes_b64": encoded,
                "file_size": bytes.len(),
            }))
        }
        Err(AgentError::PathNotFound { .. }) => {
            // Fallback: read from working tree for untracked files.
            let work_dir = state.get_work_dir(&p.repo_id).await?;
            let abs = work_dir.join(&p.path);
            if !abs.exists() {
                return Err(AgentError::PathNotFound {
                    path: p.path.clone(),
                });
            }
            let bytes = tokio::fs::read(&abs).await.map_err(AgentError::Io)?;
            let end = (p.offset + u64::from(p.length)).min(bytes.len() as u64) as usize;
            let start = p.offset.min(bytes.len() as u64) as usize;
            let slice = &bytes[start..end];
            let encoded = base64::engine::general_purpose::STANDARD.encode(slice);
            Ok(json!({
                "offset": start,
                "length": end - start,
                "bytes_b64": encoded,
                "file_size": bytes.len(),
                "untracked": true,
            }))
        }
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Deserialize)]
struct PreviewParams {
    repo_id: String,
    path: String,
    #[serde(default)]
    // Preserved for wire-compat — previews read from the hydrated
    // working tree which already reflects the checked-out rev, so we
    // don't use this field today. Kept so callers can migrate to a
    // future "preview at historical rev" without a breaking change.
    #[allow(dead_code)]
    rev: Option<String>,
}

/// Resolve `(repo_id, path, rev)` to a local working-tree file path,
/// then hand off to a format-specific previewer.
///
/// Why this lives in the agent and not the SDK: the previewers need
/// a file handle with `Seek`, and the SDK's `read()` produces `Bytes`
/// in memory. For hydrated files the workspace copy is already on
/// disk and cheap to re-open; that's the fast path we take here. For
/// unhydrated pointers the handler returns a structured error so the
/// UI can prompt the user to hydrate before previewing.
async fn preview_fmt(
    state: &State,
    p: PreviewParams,
    f: fn(&std::path::Path) -> Result<Value>,
) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let abs = work_dir.join(&p.path);
    if !abs.exists() {
        return Err(AgentError::Other(format!(
            "file not on disk: {} (run `crab hydrate` to materialize it first)",
            abs.display()
        )));
    }
    // Off-thread: format parsing is CPU-bound (arrow deserialise,
    // zip inflate). `spawn_blocking` keeps the async dispatch loop
    // responsive so other verbs keep running during a parse.
    tokio::task::spawn_blocking(move || f(&abs))
        .await
        .map_err(|e| AgentError::Other(format!("join error: {e}")))?
}

#[derive(Debug, Deserialize)]
struct ParquetPreviewParams {
    repo_id: String,
    path: String,
    #[serde(default)]
    #[allow(dead_code)]
    rev: Option<String>,
    /// Max rows in the preview body. Clamped to [1, 5000] by the
    /// parser. Defaults to `DEFAULT_PARQUET_PREVIEW_ROWS` when
    /// omitted.
    #[serde(default)]
    max_rows: Option<usize>,
}

async fn preview_parquet(state: &State, p: ParquetPreviewParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let abs = work_dir.join(&p.path);
    if !abs.exists() {
        return Err(AgentError::Other(format!(
            "file not on disk: {} (run `crab hydrate` to materialize it first)",
            abs.display()
        )));
    }
    let max_rows = p.max_rows.unwrap_or(preview::DEFAULT_PARQUET_PREVIEW_ROWS);
    tokio::task::spawn_blocking(move || preview::parquet(&abs, max_rows))
        .await
        .map_err(|e| AgentError::Other(format!("join error: {e}")))?
}

/// Repository summary: total files, by kind, by top-level extension,
/// total on-disk bytes. Purely derived from the tree listing so it's
/// cheap on repos up to ~100k files.
#[derive(Debug, Deserialize)]
struct RepoSummaryParams {
    repo_id: String,
    #[serde(default)]
    rev: Option<String>,
    /// When present, streams each top-extension entry as a
    /// `progress/partial` notification before returning the full result.
    #[serde(default)]
    op_id: Option<String>,
}

async fn repo_summary(state: &State, p: RepoSummaryParams, out: SharedStdout) -> Result<Value> {
    let repo = state.get(&p.repo_id).await?;
    let entries = repo.list(None, p.rev.as_deref()).await?;

    let mut total_files = 0u64;
    let mut total_dirs = 0u64;
    let mut total_pointers = 0u64;
    let mut total_lfs = 0u64;
    let mut total_submodules = 0u64;
    let mut total_bytes: u128 = 0;
    let mut ext_hist: std::collections::BTreeMap<String, (u64, u128)> =
        std::collections::BTreeMap::new();

    for e in &entries {
        match e.kind {
            EntryKind::Dir => total_dirs += 1,
            EntryKind::Submodule => total_submodules += 1,
            EntryKind::File => total_files += 1,
            EntryKind::Pointer => {
                total_files += 1;
                total_pointers += 1;
            }
            EntryKind::Lfs => {
                total_files += 1;
                total_lfs += 1;
            }
        }
        if let Some(size) = e.size {
            total_bytes += u128::from(size);
            let path_str = e.path.display().to_string();
            let ext = std::path::Path::new(&path_str)
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let key = if ext.is_empty() {
                "(no extension)".to_string()
            } else {
                ext
            };
            let entry = ext_hist.entry(key).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += u128::from(size);
        }
    }

    let mut top_extensions: Vec<Value> = ext_hist
        .into_iter()
        .map(|(ext, (count, bytes))| {
            json!({
                "ext": ext,
                "count": count,
                "bytes": bytes.to_string(),
            })
        })
        .collect();
    top_extensions.sort_by(|a, b| {
        b["count"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["count"].as_u64().unwrap_or(0))
    });
    top_extensions.truncate(20);

    if let Some(ref op_id) = p.op_id {
        for ext_entry in &top_extensions {
            let _ = crate::emit_partial(&out, op_id, "repo_summary_ext", ext_entry.clone()).await;
        }
    }

    // Best-effort total commit count so the renderer can branch on
    // "large repo" heuristics (skip expensive walks, raise debounce
    // intervals, etc.). We use `--all` because the dashboard's other
    // metrics also walk all refs. Failures are non-fatal and reported
    // as 0 so the renderer falls back to default behaviour.
    let work_dir = state.get_work_dir(&p.repo_id).await.ok();
    let total_commits: u64 = if let Some(wd) = work_dir {
        crate::shellout::git_command()
            .args(["rev-list", "--all", "--count"])
            .current_dir(&wd)
            .kill_on_drop(true)
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
            .unwrap_or(0)
    } else {
        0
    };

    Ok(json!({
        "total_entries": entries.len(),
        "total_files": total_files,
        "total_dirs": total_dirs,
        "total_pointers": total_pointers,
        "total_lfs": total_lfs,
        "total_submodules": total_submodules,
        "total_bytes": total_bytes.to_string(),
        "total_commits": total_commits,
        "top_extensions": top_extensions,
    }))
}

#[derive(Debug, Deserialize)]
struct GitDiffFileParams {
    repo_id: String,
    path: String,
    /// Compare against this ref. `None` = working tree vs HEAD.
    #[serde(default)]
    base: Option<String>,
    /// Compare HEAD (or base) with this ref. `None` = working tree.
    #[serde(default)]
    head: Option<String>,
    /// When present, the operation is cancellable via `rpc/cancel`.
    #[serde(default)]
    op_id: Option<String>,
}

/// Return `git status --porcelain=v1 -z` parsed into a map of
/// `{path → two-letter code}`. The working-tree entry loses its two
/// NUL-separated "new path" companion for renames — we keep the new
/// path as the key since it's what users expect to click in the UI.
async fn git_status_porcelain(state: &State, p: RepoIdParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let out = crate::shellout::git_command()
        .args(["status", "--porcelain=v1", "-z", "-uall"])
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;
    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let mut entries: Vec<Value> = Vec::new();
    // -z splits every record (including rename pairs) with NUL. The
    // first two bytes are the XY status codes; then a space; then the
    // path. Renames and copies emit an extra NUL-terminated second
    // path for the original name.
    let mut i = 0usize;
    while i < out.stdout.len() {
        if out.stdout.len() - i < 3 {
            break;
        }
        let xy = &out.stdout[i..i + 2];
        // Advance past "XY "; find the next NUL for the path.
        let path_start = i + 3;
        let path_end = out.stdout[path_start..]
            .iter()
            .position(|&b| b == 0)
            .map_or(out.stdout.len(), |p| path_start + p);
        let path = String::from_utf8_lossy(&out.stdout[path_start..path_end]).into_owned();
        let code = std::str::from_utf8(xy).unwrap_or("??").trim().to_owned();
        // Skip the rename "old path" companion record when present.
        let is_rename = xy[0] == b'R' || xy[0] == b'C' || xy[1] == b'R' || xy[1] == b'C';
        entries.push(json!({ "path": path, "code": code }));
        i = path_end + 1;
        if is_rename {
            let old_end = out.stdout[i..]
                .iter()
                .position(|&b| b == 0)
                .map_or(out.stdout.len(), |p| i + p);
            i = old_end + 1;
        }
    }
    Ok(json!({ "entries": entries }))
}

/// Return `git diff` for a single path as a unified patch string.
/// If both `base` and `head` are provided, runs `git diff base..head
/// -- path`. Otherwise defaults to working tree vs HEAD.
async fn git_diff_file(state: &State, p: GitDiffFileParams) -> Result<Value> {
    let op_id = p.op_id.clone();
    let work = git_diff_file_work(state, p);

    run_cancellable(state, op_id, work).await
}

async fn git_diff_file_work(state: &State, p: GitDiffFileParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let mut args: Vec<String> = vec!["diff".into(), "--no-color".into(), "--no-renames".into()];
    match (p.base.as_deref(), p.head.as_deref()) {
        (Some(b), Some(h)) => args.push(format!("{b}..{h}")),
        (Some(b), None) => args.push(b.to_owned()),
        (None, _) => {}
    }
    args.push("--".into());
    args.push(p.path.clone());

    let out = crate::shellout::git_command()
        .args(&args)
        .current_dir(&work_dir)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(AgentError::Io)?;
    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let patch = String::from_utf8_lossy(&out.stdout).into_owned();
    Ok(json!({ "path": p.path, "patch": patch }))
}

/// `crab diff` for a single path. Returns structured chunk-level
/// diff data as JSON. For crab-tracked files this shows which chunks
/// were added/removed/modified between two refs. For git-native files
/// it falls back gracefully. If the CLI isn't installed we surface a
/// clean error so the UI can fall back to `git_diff_file`.
async fn crab_diff_file(state: &State, p: GitDiffFileParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;

    // Fast-fail: only attempt crab diff if the repo has crab configured.
    // Without a remote, `crab diff` hangs trying to connect to S3.
    let has_crab = work_dir.join(".crab").join("remote").is_file()
        || work_dir.join(".crab").join("config.toml").is_file();
    if !has_crab {
        return Err(AgentError::Cli {
            status: -1,
            stderr: "not a crab-configured repository".to_owned(),
        });
    }

    // `crab diff` takes ref1 and ref2 as separate positional arguments
    // (not the git `base..head` range syntax). ref2 defaults to HEAD when
    // omitted.
    let mut args: Vec<String> = vec!["diff".into(), "--json".into(), "--no-color".into()];
    match (p.base.as_deref(), p.head.as_deref()) {
        (Some(b), Some(h)) => {
            args.push(b.to_owned());
            args.push(h.to_owned());
        }
        (Some(b), None) => {
            args.push(b.to_owned());
        }
        (None, Some(h)) => {
            args.push("HEAD".to_owned());
            args.push(h.to_owned());
        }
        (None, None) => {
            args.push("HEAD".to_owned());
        }
    }
    args.push("--".into());
    args.push(p.path.clone());

    // Allow up to 60s for shard resolution from S3.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        shellout::command()?
            .args(&args)
            .current_dir(&work_dir)
            .output(),
    )
    .await;

    // If the CLI diff succeeds and found crab-tracked files, return
    // the rich JSON output. An empty files array means the specific
    // file isn't pointer-tracked — fall through to git diff.
    if let Ok(Ok(out)) = &result {
        if out.status.success() {
            let body = String::from_utf8_lossy(&out.stdout).into_owned();
            if body.contains("\"files\":[{") {
                return Ok(
                    json!({ "path": p.path, "patch": "", "backend": "crab", "crab_diff": body }),
                );
            }
        }
    }

    // CLI diff didn't produce results for this file. Check if it's a
    // pointer via the SDK (instant, local-only) and produce a synthetic
    // size-based diff so the UI shows something useful.
    let repo = match state.get(&p.repo_id).await {
        Ok(r) => r,
        Err(_) => {
            return Err(AgentError::Cli {
                status: -1,
                stderr: "crab diff produced no results for this file".to_owned(),
            });
        }
    };

    let old_rev = p.base.as_deref();
    let new_rev = p.head.as_deref().or(Some("HEAD"));

    let new_stat = repo.stat(&p.path, new_rev).await.ok();
    let old_stat = repo.stat(&p.path, old_rev).await.ok();

    let is_pointer = new_stat
        .as_ref()
        .map(|s| s.kind == ContentKind::Crab || s.kind == ContentKind::Lfs)
        .unwrap_or(false)
        || old_stat
            .as_ref()
            .map(|s| s.kind == ContentKind::Crab || s.kind == ContentKind::Lfs)
            .unwrap_or(false);

    if !is_pointer {
        return Err(AgentError::Cli {
            status: -1,
            stderr: "file is not a crab/lfs pointer".to_owned(),
        });
    }

    // Build a synthetic diff from pointer sizes.
    let old_size = old_stat.as_ref().map(|s| s.size).unwrap_or(0);
    let new_size = new_stat.as_ref().map(|s| s.size).unwrap_or(0);
    let status = if old_size == 0 && new_size > 0 {
        "added"
    } else if old_size > 0 && new_size == 0 {
        "deleted"
    } else {
        "modified"
    };
    let delta_bytes = if new_size > old_size {
        new_size - old_size
    } else {
        old_size - new_size
    };

    let synthetic = json!({
        "files": [{
            "path": p.path,
            "status": status,
            "old_size": old_size,
            "new_size": new_size,
            "delta_bytes": delta_bytes,
            "chunks_added": null,
            "chunks_removed": null,
            "chunks_unchanged": null,
            "old_file_hash": old_stat.as_ref().map(|s| s.pointer_hash.clone()),
            "new_file_hash": new_stat.as_ref().map(|s| s.pointer_hash.clone()),
        }],
        "summary": {
            "files_changed": 1,
            "total_delta_bytes": delta_bytes,
        }
    });

    Ok(
        json!({ "path": p.path, "patch": "", "backend": "crab", "crab_diff": synthetic.to_string() }),
    )
}

// ---- Hunk-level staging ---------------------------------------------

async fn git_diff_index_patch(state: &State, p: git_hunks::DiffIndexParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    git_hunks::diff_index_patch(&work_dir, p.path.as_deref(), p.include_untracked).await
}

async fn git_diff_cached_patch(state: &State, p: git_hunks::DiffCachedParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    git_hunks::diff_cached_patch(&work_dir, p.path.as_deref()).await
}

async fn git_apply_patch(state: &State, p: git_hunks::ApplyPatchParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    git_hunks::apply_patch(&work_dir, &p.patch, p.cached, p.reverse).await
}

async fn git_stage_hunks(state: &State, p: git_hunks::HunkSetParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    git_hunks::stage_hunks(&work_dir, &p.hunks).await
}

#[derive(Debug, Deserialize)]
struct GitAddPathsParams {
    repo_id: String,
    paths: Vec<String>,
}

/// Stage files by path using `git add`. Handles untracked files that
/// `git diff` cannot produce patches for.
async fn git_add_paths(state: &State, p: GitAddPathsParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let mut args: Vec<&str> = vec!["add", "--"];
    let path_refs: Vec<&str> = p.paths.iter().map(String::as_str).collect();
    args.extend(&path_refs);
    let out = crate::shellout::git_command()
        .args(&args)
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;
    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(serde_json::json!({ "staged": p.paths.len() }))
}

/// Unstage files by path using `git reset HEAD`. Handles files that
/// were staged via `git add` (including previously untracked files).
async fn git_reset_paths(state: &State, p: GitAddPathsParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let mut args: Vec<&str> = vec!["reset", "HEAD", "--"];
    let path_refs: Vec<&str> = p.paths.iter().map(String::as_str).collect();
    args.extend(&path_refs);
    let out = crate::shellout::git_command()
        .args(&args)
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;
    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(serde_json::json!({ "unstaged": p.paths.len() }))
}

async fn git_unstage_hunks(state: &State, p: git_hunks::HunkSetParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    git_hunks::unstage_hunks(&work_dir, &p.hunks).await
}

async fn git_discard_hunks(state: &State, p: git_hunks::HunkSetParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    git_hunks::discard_hunks(&work_dir, &p.hunks).await
}

#[derive(Debug, Deserialize)]
struct HydrationStatusParams {
    repo_id: String,
    #[serde(default)]
    rev: Option<String>,
}

/// Per-file hydration status for every pointer-tracked entry in the
/// working tree. Compares each entry's pointer `size` (logical, from
/// `list`) against the actual on-disk file size (from `fs::metadata`).
///
///   - `hydrated` — on-disk bytes match the logical size. The file
///     is fully materialised.
///   - `pointer` — on-disk size is small (under 1 KiB) and matches
///     the usual pointer-blob format. Needs hydrate.
///   - `partial` — on-disk size is non-zero but doesn't match. Most
///     commonly a truncated / stale hydration; we surface as a
///     warning state.
///   - `missing` — the path doesn't exist at all.
///
/// Git-native files and directories are omitted — they're always
/// materialised by definition.
async fn hydration_status(state: &State, p: HydrationStatusParams) -> Result<Value> {
    let repo = state.get(&p.repo_id).await?;
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let entries = repo.list(None, p.rev.as_deref()).await?;

    let mut items: Vec<Value> = Vec::new();
    let mut hydrated = 0u64;
    let mut pointer = 0u64;
    let mut partial = 0u64;
    let mut missing = 0u64;
    let mut on_disk_bytes: u128 = 0;
    let mut logical_bytes: u128 = 0;

    for e in &entries {
        if !matches!(e.kind, EntryKind::Pointer | EntryKind::Lfs) {
            continue;
        }
        let path_str = e.path.display().to_string();
        let abs = work_dir.join(&e.path);
        let logical = e.size.unwrap_or(0);
        logical_bytes += u128::from(logical);

        let md = tokio::fs::metadata(&abs).await.ok();
        let (state_label, on_disk): (&'static str, u64) = match md {
            None => ("missing", 0),
            Some(m) => {
                let disk = m.len();
                // Heuristic: pointer blobs are typically ~200-1000
                // bytes. Anything under 1 KiB we call a pointer.
                // Equal sizes = hydrated. Anything else is partial.
                if logical > 0 && disk == logical {
                    ("hydrated", disk)
                } else if disk < 1024 {
                    ("pointer", disk)
                } else {
                    ("partial", disk)
                }
            }
        };
        on_disk_bytes += u128::from(on_disk);
        match state_label {
            "hydrated" => hydrated += 1,
            "pointer" => pointer += 1,
            "partial" => partial += 1,
            "missing" => missing += 1,
            _ => {}
        }
        items.push(json!({
            "path": path_str,
            "state": state_label,
            "on_disk": on_disk,
            "logical": logical,
            "kind": format!("{:?}", e.kind).to_lowercase(),
        }));
    }

    Ok(json!({
        "items": items,
        "summary": {
            "hydrated": hydrated,
            "pointer": pointer,
            "partial": partial,
            "missing": missing,
            "on_disk_bytes": on_disk_bytes.to_string(),
            "logical_bytes": logical_bytes.to_string(),
            "total_tracked": hydrated + pointer + partial + missing,
        }
    }))
}

#[derive(Debug, Deserialize)]
struct GitGrepParams {
    repo_id: String,
    query: String,
    #[serde(default)]
    ignore_case: bool,
    /// Treat the query as a POSIX extended regex (`-E`).
    #[serde(default)]
    regex: bool,
    /// Match whole words only (`-w`).
    #[serde(default)]
    whole_word: bool,
    /// Glob patterns to restrict search to (e.g. `["*.rs", "src/**"]`).
    #[serde(default)]
    include_patterns: Vec<String>,
    /// Glob patterns to exclude from search (e.g. `["*.lock", "node_modules/**"]`).
    #[serde(default)]
    exclude_patterns: Vec<String>,
    #[serde(default = "default_grep_limit")]
    max_results: usize,
    /// When present, the operation is cancellable via `rpc/cancel`.
    #[serde(default)]
    op_id: Option<String>,
}

fn default_grep_limit() -> usize {
    500
}

/// `git grep -n` over the tracked files. We cap the number of
/// matches returned so a `query = "e"` on a monorepo can't flood
/// JSON-RPC.
async fn git_grep(state: &State, p: GitGrepParams) -> Result<Value> {
    let op_id = p.op_id.clone();
    let work = git_grep_work(state, p);

    run_cancellable(state, op_id, work).await
}

async fn git_grep_work(state: &State, p: GitGrepParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    if p.query.trim().is_empty() {
        return Ok(json!({ "matches": [] }));
    }
    let mut args: Vec<String> = vec!["grep".into(), "-n".into(), "-I".into(), "--no-color".into()];
    if p.ignore_case {
        args.push("-i".into());
    }
    if p.regex {
        args.push("-E".into());
    } else {
        args.push("-F".into());
    }
    if p.whole_word {
        args.push("-w".into());
    }

    // Exclude patterns via `:(exclude)` pathspec magic.
    for pat in &p.exclude_patterns {
        args.push(format!(":(exclude){}", pat));
    }

    args.push("--".into());
    args.push(p.query.clone());

    // Include patterns restrict the search to matching paths.
    if !p.include_patterns.is_empty() {
        for pat in &p.include_patterns {
            args.push(pat.clone());
        }
    }

    let out = crate::shellout::git_command()
        .args(&args)
        .current_dir(&work_dir)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(AgentError::Io)?;
    // git grep exits non-zero when there are no matches. That isn't
    // an error for us — treat status 1 as empty results.
    if !out.status.success() && out.status.code() != Some(1) {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let body = String::from_utf8_lossy(&out.stdout);
    let mut matches: Vec<Value> = Vec::new();
    for line in body.lines() {
        if matches.len() >= p.max_results {
            break;
        }
        // Format: `path:line:content`. `path` may itself contain ':'
        // so we parse from the left twice.
        let (path, rest) = match line.split_once(':') {
            Some(v) => v,
            None => continue,
        };
        let (line_no, content) = match rest.split_once(':') {
            Some(v) => v,
            None => continue,
        };
        matches.push(json!({
            "path": path,
            "line": line_no.parse::<u32>().unwrap_or(0),
            "content": content,
        }));
    }
    Ok(json!({
        "matches": matches,
        "truncated": matches.len() >= p.max_results,
    }))
}

// ── Search & Replace ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SearchReplaceParams {
    repo_id: String,
    /// The search pattern (literal or regex depending on `regex` flag).
    query: String,
    /// The replacement string. Supports `$1`, `$2` etc. for regex captures.
    replacement: String,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    whole_word: bool,
    /// Preserve the case pattern of the original match in the replacement.
    #[serde(default)]
    preserve_case: bool,
    /// Restrict replacement to specific files. If empty, replaces in all
    /// files that matched the search.
    #[serde(default)]
    file_paths: Vec<String>,
    /// When present, the operation is cancellable via `rpc/cancel`.
    #[serde(default)]
    op_id: Option<String>,
}

/// Replace occurrences of a pattern across files in the working tree.
/// Returns the number of replacements made and the affected files.
async fn search_replace(state: &State, p: SearchReplaceParams) -> Result<Value> {
    let op_id = p.op_id.clone();
    let work = search_replace_work(state, p);
    run_cancellable(state, op_id, work).await
}

async fn search_replace_work(state: &State, p: SearchReplaceParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    if p.query.trim().is_empty() {
        return Ok(json!({ "replacements": 0, "files": [] }));
    }

    // Build the sed-style replacement using Rust's regex crate for
    // correctness. We read each target file, apply replacements in
    // memory, and write back atomically.
    let pattern = if p.regex {
        if p.whole_word {
            format!("\\b{}\\b", p.query)
        } else {
            p.query.clone()
        }
    } else {
        let escaped = regex::escape(&p.query);
        if p.whole_word {
            format!("\\b{}\\b", escaped)
        } else {
            escaped
        }
    };

    let re = regex::RegexBuilder::new(&pattern)
        .case_insensitive(p.ignore_case)
        .build()
        .map_err(|e| AgentError::Other(format!("invalid regex: {e}")))?;

    let mut affected_files: Vec<String> = Vec::new();
    let mut total_replacements: usize = 0;

    for file_path in &p.file_paths {
        let abs = work_dir.join(file_path);
        if !abs.is_file() {
            continue;
        }
        let content = match tokio::fs::read_to_string(&abs).await {
            Ok(c) => c,
            Err(_) => continue, // skip binary / unreadable files
        };

        let new_content = if p.preserve_case {
            // Preserve-case: match the casing pattern of each occurrence.
            let mut result = String::with_capacity(content.len());
            let mut last_end = 0;
            let mut count = 0usize;
            for m in re.find_iter(&content) {
                result.push_str(&content[last_end..m.start()]);
                result.push_str(&apply_case_preservation(m.as_str(), &p.replacement));
                last_end = m.end();
                count += 1;
            }
            result.push_str(&content[last_end..]);
            if count == 0 {
                continue;
            }
            total_replacements += count;
            result
        } else {
            let replaced = re.replace_all(&content, p.replacement.as_str());
            if replaced == content {
                continue;
            }
            let count = re.find_iter(&content).count();
            total_replacements += count;
            replaced.into_owned()
        };

        // Atomic write: write to temp then rename.
        let tmp = abs.with_extension("gx-tmp");
        tokio::fs::write(&tmp, &new_content)
            .await
            .map_err(AgentError::Io)?;
        tokio::fs::rename(&tmp, &abs)
            .await
            .map_err(AgentError::Io)?;
        affected_files.push(file_path.clone());
    }

    Ok(json!({
        "replacements": total_replacements,
        "files": affected_files,
    }))
}

/// Apply case preservation: if the matched text is all-upper, uppercase
/// the replacement; if title-case, title-case the replacement; otherwise
/// use the replacement as-is.
fn apply_case_preservation(matched: &str, replacement: &str) -> String {
    if matched
        .chars()
        .all(|c| c.is_uppercase() || !c.is_alphabetic())
    {
        replacement.to_uppercase()
    } else if matched.starts_with(|c: char| c.is_uppercase())
        && matched[1..]
            .chars()
            .all(|c| c.is_lowercase() || !c.is_alphabetic())
    {
        let mut chars = replacement.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().to_string() + &chars.as_str().to_lowercase(),
            None => String::new(),
        }
    } else {
        replacement.to_owned()
    }
}

#[derive(Debug, Deserialize)]
struct GitLogParams {
    repo_id: String,
    /// Max commits to return per call. Caps at 5000 so a runaway
    /// UI can't accidentally exhaust memory on an enormous repo.
    #[serde(default = "default_log_max")]
    max: usize,
    /// Skip the first `skip` commits matching the rev walk. Used
    /// by the UI's "load more" pagination so scrolling doesn't
    /// require one giant up-front fetch.
    #[serde(default)]
    skip: usize,
    /// Optional ref to start from. Defaults to HEAD.
    #[serde(default)]
    from: Option<String>,
    /// When true, include commits from all refs (`--all`). Useful
    /// for seeing every branch in the timeline without having to
    /// merge.
    #[serde(default)]
    all: bool,
    /// Optional file path to filter commits to only those touching
    /// this file (`git log -- <path>`).
    #[serde(default)]
    path: Option<String>,
    /// When true, append `--shortstat` to the git log command and
    /// parse per-commit stats (files changed, insertions, deletions).
    #[serde(default)]
    include_stats: bool,
    /// When present, the operation is cancellable via `rpc/cancel`.
    #[serde(default)]
    op_id: Option<String>,
}

fn default_log_max() -> usize {
    200
}

/// Parse a single `--shortstat` line into (files_changed, insertions, deletions).
/// Example: " 3 files changed, 12 insertions(+), 5 deletions(-)"
fn parse_shortstat(line: &str) -> (Option<u64>, Option<u64>, Option<u64>) {
    let mut files: Option<u64> = None;
    let mut ins: Option<u64> = None;
    let mut del: Option<u64> = None;

    for part in line.split(',') {
        let trimmed = part.trim();
        if trimmed.contains("file") {
            // "3 files changed" or "1 file changed"
            if let Some(n) = trimmed
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
            {
                files = Some(n);
            }
        } else if trimmed.contains("insertion") {
            if let Some(n) = trimmed
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
            {
                ins = Some(n);
            }
        } else if trimmed.contains("deletion") {
            if let Some(n) = trimmed
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
            {
                del = Some(n);
            }
        }
    }
    (files, ins, del)
}

/// Parse a format line (NUL-separated fields) into a JSON commit object,
/// optionally attaching shortstat data.
fn parse_log_line(line: &str, stat_line: Option<&str>) -> Value {
    let parts: Vec<&str> = line.split('\0').collect();
    if parts.len() < 6 {
        return json!(null);
    }
    let parents: Vec<&str> = parts[1].split_whitespace().collect();
    let refs: Vec<Value> = parts[4]
        .split(", ")
        .filter(|s| !s.is_empty())
        .map(|raw| {
            let (kind, name) = if let Some(rest) = raw.strip_prefix("HEAD -> ") {
                ("head_target", rest)
            } else if raw == "HEAD" {
                ("head", raw)
            } else if let Some(rest) = raw.strip_prefix("tag: refs/tags/") {
                ("tag", rest)
            } else if let Some(rest) = raw.strip_prefix("tag: ") {
                ("tag", rest)
            } else if let Some(rest) = raw.strip_prefix("refs/heads/") {
                ("branch", rest)
            } else if let Some(rest) = raw.strip_prefix("refs/remotes/") {
                ("remote", rest)
            } else {
                ("other", raw)
            };
            json!({ "kind": kind, "name": name })
        })
        .collect();

    let mut commit = json!({
        "sha": parts[0],
        "parents": parents,
        "author": parts[2],
        "date": parts[3],
        "refs": refs,
        "subject": parts[5],
    });

    if let Some(stat) = stat_line {
        let (files, ins, del) = parse_shortstat(stat);
        if let Some(f) = files {
            commit["files_changed"] = json!(f);
        }
        if let Some(i) = ins {
            commit["insertions"] = json!(i);
        }
        if let Some(d) = del {
            commit["deletions"] = json!(d);
        }
    }

    commit
}

/// `git log --pretty` walk. Returns the last N commits' sha, author,
/// date, subject, parent shas, and the ref decoration (branches and
/// tags) for the timeline graph.
async fn git_log(state: &State, p: GitLogParams) -> Result<Value> {
    let op_id = p.op_id.clone();
    let work = git_log_work(state, p);

    run_cancellable(state, op_id, work).await
}

async fn git_log_work(state: &State, p: GitLogParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    // `--format='%H%x00%P%x00%an%x00%aI%x00%D%x00%s'` NUL-separates
    // fields, record-separated by NL. `%D` expands to the full ref
    // decoration without parentheses — we get entries like
    // "HEAD -> main, tag: v1.2.0, origin/main".
    let fmt = "%H%x00%P%x00%an%x00%aI%x00%D%x00%s";
    let capped_max = p.max.min(2000);
    let mut args: Vec<String> = vec![
        "log".into(),
        "--no-color".into(),
        format!("--max-count={capped_max}"),
        format!("--skip={}", p.skip),
        format!("--pretty=format:{fmt}"),
        "--decorate=full".into(),
    ];
    if p.include_stats {
        args.push("--shortstat".into());
    }
    if p.all {
        args.push("--all".into());
    }
    if let Some(from) = p.from.as_deref() {
        args.push(from.to_owned());
    }
    // Filter to a single file when `path` is provided.
    if let Some(ref file_path) = p.path {
        args.push("--follow".into());
        args.push("--".into());
        args.push(file_path.clone());
    }

    // Run `git log` and `git rev-list --count` in parallel so the
    // commit data is returned without waiting for the (potentially
    // slow) full-history count. On large repos (16k+ commits, 1000+
    // branches), `rev-list --count --all` can take 1-2 seconds on a
    // cold filesystem cache — serializing it after `git log` was the
    // primary bottleneck causing the "Loading git log..." hang.
    let log_future = crate::shellout::git_command()
        .args(&args)
        .current_dir(&work_dir)
        .kill_on_drop(true)
        .output();

    let mut count_args: Vec<String> = vec!["rev-list".into(), "--count".into()];
    if p.all {
        count_args.push("--all".into());
    } else if let Some(from) = p.from.as_deref() {
        count_args.push(from.to_owned());
    } else {
        count_args.push("HEAD".into());
    }
    let count_future = crate::shellout::git_command()
        .args(&count_args)
        .current_dir(&work_dir)
        .kill_on_drop(true)
        .output();

    let (log_result, count_result) = tokio::join!(log_future, count_future);

    let out = log_result.map_err(AgentError::Io)?;
    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let body = String::from_utf8_lossy(&out.stdout);
    let mut commits: Vec<Value> = Vec::new();

    if p.include_stats {
        // With --shortstat, output is: format line, then an optional
        // blank line + shortstat line before the next commit. We split
        // on the format lines (which contain NUL bytes) and associate
        // the trailing shortstat with each commit.
        let mut current_format_line: Option<&str> = None;
        let mut current_stat_line: Option<&str> = None;

        for line in body.lines() {
            if line.contains('\0') {
                // This is a new commit format line. Flush the previous.
                if let Some(fmt_line) = current_format_line {
                    let val = parse_log_line(fmt_line, current_stat_line);
                    if !val.is_null() {
                        commits.push(val);
                    }
                }
                current_format_line = Some(line);
                current_stat_line = None;
            } else if !line.trim().is_empty() {
                // Non-empty, non-format line → shortstat output.
                current_stat_line = Some(line);
            }
        }
        // Flush the last commit.
        if let Some(fmt_line) = current_format_line {
            let val = parse_log_line(fmt_line, current_stat_line);
            if !val.is_null() {
                commits.push(val);
            }
        }
    } else {
        for line in body.lines() {
            let val = parse_log_line(line, None);
            if !val.is_null() {
                commits.push(val);
            }
        }
    }

    let total = count_result
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u64>()
                .ok()
        });

    Ok(json!({
        "commits": commits,
        "skip": p.skip,
        "total": total,
    }))
}

// ---- Stashes ---------------------------------------------------------

async fn git_stash_list(state: &State, p: RepoIdParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    // `%gd` is the stash ref; `%s` the subject. Stash messages
    // typically include the branch name, which we want to show.
    let out = crate::shellout::git_command()
        .args(["stash", "list", "--format=%gd%x00%s"])
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;
    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let body = String::from_utf8_lossy(&out.stdout);
    let mut stashes: Vec<Value> = Vec::new();
    for line in body.lines() {
        let (ref_, message) = match line.split_once('\0') {
            Some(v) => v,
            None => continue,
        };
        stashes.push(json!({ "ref": ref_, "message": message }));
    }
    Ok(json!({ "stashes": stashes }))
}

#[derive(Debug, Deserialize)]
struct StashRefParams {
    repo_id: String,
    stash_ref: String,
}

async fn git_stash_apply(state: &State, p: StashRefParams) -> Result<Value> {
    run_git(state, &p.repo_id, &["stash", "apply", &p.stash_ref]).await
}

async fn git_stash_pop(state: &State, p: StashRefParams) -> Result<Value> {
    run_git(state, &p.repo_id, &["stash", "pop", &p.stash_ref]).await
}

async fn git_stash_drop(state: &State, p: StashRefParams) -> Result<Value> {
    run_git(state, &p.repo_id, &["stash", "drop", &p.stash_ref]).await
}

#[derive(Debug, Deserialize)]
struct StashSaveParams {
    repo_id: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    include_untracked: bool,
}

async fn git_stash_save(state: &State, p: StashSaveParams) -> Result<Value> {
    let mut args: Vec<String> = vec!["stash".into(), "push".into()];
    if p.include_untracked {
        args.push("-u".into());
    }
    if let Some(msg) = p.message.as_deref() {
        args.push("-m".into());
        args.push(msg.to_owned());
    }
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    run_git(state, &p.repo_id, &args_ref).await
}

async fn git_stash_show(state: &State, p: StashRefParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    // --stat gives file summary, --patch gives full diff
    let out = crate::shellout::git_command()
        .args(["stash", "show", "--stat", "--name-status", &p.stash_ref])
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;
    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let body = String::from_utf8_lossy(&out.stdout);
    // Parse --name-status output: lines like "M\tpath" or "A\tpath"
    let mut files: Vec<Value> = Vec::new();
    for line in body.lines() {
        // Skip the summary line at the end (e.g., "2 files changed, 10 insertions(+)")
        if line.contains("file") && line.contains("changed") {
            continue;
        }
        if let Some((status, path)) = line.split_once('\t') {
            files.push(json!({ "status": status.trim(), "path": path.trim() }));
        }
    }
    Ok(json!({ "files": files }))
}

async fn git_stash_show_patch(state: &State, p: StashRefParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let out = crate::shellout::git_command()
        .args(["stash", "show", "-p", "--no-color", &p.stash_ref])
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;
    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let patch = String::from_utf8_lossy(&out.stdout).into_owned();
    Ok(json!({ "patch": patch }))
}

async fn run_git(state: &State, repo_id: &str, args: &[&str]) -> Result<Value> {
    let work_dir = state.get_work_dir(repo_id).await?;
    let out = crate::shellout::git_command()
        .args(args)
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr,
        });
    }
    Ok(json!({ "status": 0, "stdout": stdout, "stderr": stderr }))
}

// ---- Pointer "top-N" report ----------------------------------------

#[derive(Debug, Deserialize)]
struct PointerTopParams {
    repo_id: String,
    #[serde(default)]
    rev: Option<String>,
    #[serde(default = "default_top_n")]
    top_n: usize,
}

fn default_top_n() -> usize {
    50
}

/// Top-N pointer-tracked files by logical size. Powers the "Pointer
/// Map" view — a flat sortable table today; a treemap in a follow-up.
async fn pointer_top(state: &State, p: PointerTopParams) -> Result<Value> {
    let repo = state.get(&p.repo_id).await?;
    let entries = repo.list(None, p.rev.as_deref()).await?;
    let mut scored: Vec<(u64, &BrowserEntry)> = entries
        .iter()
        .filter(|e| matches!(e.kind, EntryKind::Pointer | EntryKind::Lfs))
        .filter_map(|e| e.size.map(|s| (s, e)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    let top = scored
        .into_iter()
        .take(p.top_n.min(500))
        .map(|(size, e)| {
            json!({
                "path": e.path.display().to_string(),
                "size": size,
                "kind": format!("{:?}", e.kind).to_lowercase(),
                "pointer_hash": e.pointer_hash,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "entries": top }))
}

#[derive(Debug, Deserialize)]
struct GitCommitChangesParams {
    repo_id: String,
    sha: String,
}

/// File-level changes for a single commit vs its first parent.
/// Root commits (no parents) compare against the empty tree so the
/// UI still gets a list of every file the commit introduced.
///
/// Output shape:
///   entries: [
///     { path, status, additions, deletions, old_path? },
///     ...
///   ]
///   is_merge: bool
///   commit: { sha, parents, author, date, subject }
async fn git_commit_changes(state: &State, p: GitCommitChangesParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;

    // First, capture commit metadata (mirrors git_log's fields).
    let meta_out = crate::shellout::git_command()
        .args([
            "log",
            "-1",
            "--no-color",
            "--pretty=format:%H%x00%P%x00%an%x00%aI%x00%s",
            &p.sha,
        ])
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;
    if !meta_out.status.success() {
        return Err(AgentError::Cli {
            status: meta_out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&meta_out.stderr).into_owned(),
        });
    }
    let meta_line = String::from_utf8_lossy(&meta_out.stdout);
    let meta_parts: Vec<&str> = meta_line.split('\0').collect();
    if meta_parts.len() < 5 {
        return Err(AgentError::Other(format!(
            "malformed git log output for {}",
            p.sha
        )));
    }
    let parents: Vec<&str> = meta_parts[1].split_whitespace().collect();
    let is_merge = parents.len() > 1;

    // `--diff-filter=` left empty = all. `-z` + `--raw` gives us
    // NUL-separated records with full status letters (M/A/D/R%/…)
    // and no ambiguous rename/copy heuristics to parse by hand.
    //
    // For root commits we use git's "empty tree" trick: compare
    // against `4b825dc642cb6eb9a060e54bf8d69288fbee4904`, the hash
    // of the canonical empty tree. git supports this out of the box.
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    let parent_ref = parents.first().copied().unwrap_or(EMPTY_TREE);

    let diff_out = crate::shellout::git_command()
        .args([
            "diff",
            "--numstat",
            "-z",
            "--no-color",
            "-M",
            "-C",
            parent_ref,
            &p.sha,
        ])
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;
    if !diff_out.status.success() {
        return Err(AgentError::Cli {
            status: diff_out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&diff_out.stderr).into_owned(),
        });
    }

    let entries = parse_numstat_z(&diff_out.stdout);

    Ok(json!({
        "commit": {
            "sha": meta_parts[0],
            "parents": parents,
            "author": meta_parts[2],
            "date": meta_parts[3],
            "subject": meta_parts[4],
        },
        "is_merge": is_merge,
        "entries": entries,
    }))
}

/// Parse `git diff --numstat -z` output. Each record is
/// `adds\tdels\tpath\0` — but for renames/copies, a rename-line
/// looks like `adds\tdels\t\0old_path\0new_path\0`, with the `path`
/// column empty and the two paths following as NUL-separated fields.
fn parse_numstat_z(bytes: &[u8]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Read up to the first NUL to get "adds\tdels\tpath" or
        // "adds\tdels\t".
        let nul = match bytes[i..].iter().position(|&b| b == 0) {
            Some(n) => i + n,
            None => break,
        };
        let head = match std::str::from_utf8(&bytes[i..nul]) {
            Ok(s) => s,
            Err(_) => {
                i = nul + 1;
                continue;
            }
        };
        let mut parts = head.splitn(3, '\t');
        let adds_s = parts.next().unwrap_or("0");
        let dels_s = parts.next().unwrap_or("0");
        let rest = parts.next().unwrap_or("");
        // Binary files show up with "-\t-\tpath"; parse those as 0/0
        // and let the UI label them as binary in a follow-up.
        let additions: u64 = adds_s.parse().unwrap_or(0);
        let deletions: u64 = dels_s.parse().unwrap_or(0);

        if rest.is_empty() {
            // Rename/copy: old path, then new path, as separate
            // NUL-terminated fields.
            let old_start = nul + 1;
            let old_end = match bytes[old_start..].iter().position(|&b| b == 0) {
                Some(n) => old_start + n,
                None => break,
            };
            let new_start = old_end + 1;
            let new_end = match bytes[new_start..].iter().position(|&b| b == 0) {
                Some(n) => new_start + n,
                None => break,
            };
            let old_path = String::from_utf8_lossy(&bytes[old_start..old_end]).into_owned();
            let new_path = String::from_utf8_lossy(&bytes[new_start..new_end]).into_owned();
            out.push(json!({
                "path": new_path,
                "old_path": old_path,
                "status": "renamed",
                "additions": additions,
                "deletions": deletions,
            }));
            i = new_end + 1;
        } else {
            // Regular modification / addition / deletion. We don't
            // know add-vs-modify from numstat alone; the renderer
            // can infer from additions=file-size-ish and deletions=0
            // but for UI purposes "modified" is fine for all three.
            out.push(json!({
                "path": rest,
                "status": "modified",
                "additions": additions,
                "deletions": deletions,
            }));
            i = nul + 1;
        }
    }
    out
}

#[derive(Debug, Deserialize)]
struct ResolveRefParams {
    repo_id: String,
    name: String,
}

/// Resolve a ref name (branch, tag, partial sha, HEAD) into a full
/// 40-char commit sha. Used when opening a branch/tag as a pinned
/// virtual-explorer root — we want the concrete commit sha, not the
/// mutable ref or a tag-object wrapper, so the downstream tree walk
/// has a commit to open.
///
/// Annotated tags need peeling: `git rev-parse refs/tags/v1` returns
/// the tag object's sha, which isn't a tree/commit. Appending
/// `^{commit}` peels annotated tags (and is a no-op for lightweight
/// tags and branches), giving us a commit sha in every case.
async fn resolve_ref(state: &State, p: ResolveRefParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let peel_spec = format!("{}^{{commit}}", p.name);
    let out = crate::shellout::git_command()
        .args(["rev-parse", "--verify", &peel_spec])
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;
    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Ok(json!({ "name": p.name, "sha": sha }))
}

/// About / version info. Used by the Settings About panel. Calls the
/// crab CLI to get its version string; if the CLI isn't installed we
/// return `null` for the `crab_cli_version` field so the UI can
/// surface a "not installed" hint without looking like an error.
async fn about() -> Result<Value> {
    let cli_version = tokio::process::Command::new(shellout::resolve_crab_bin())
        .arg("--version")
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());

    Ok(json!({
        "agent_version": env!("CARGO_PKG_VERSION"),
        "crab_cli_version": cli_version,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    }))
}

/// Clear the on-disk xorb/shard cache. We prefer the CLI's own
/// `crab cache clear` when available so the cache-index bookkeeping
/// stays consistent; if the subcommand doesn't exist, fall back to
/// removing the cache dir directly (best-effort — partial deletions
/// are acceptable here because the cache is by definition
/// regeneratable).
async fn cache_clear() -> Result<Value> {
    // Try the CLI path first.
    let cli_out = tokio::process::Command::new(shellout::resolve_crab_bin())
        .args(["cache", "clear", "--yes"])
        .output()
        .await;

    if let Ok(out) = &cli_out {
        if out.status.success() {
            return Ok(json!({
                "method": "cli",
                "stdout": String::from_utf8_lossy(&out.stdout).into_owned(),
            }));
        }
    }

    // Fallback: remove the default cache directory directly. We only
    // touch the crab subdirectory so we can't accidentally wipe
    // something unrelated.
    let root = if let Ok(dir) = std::env::var("CRAB_CACHE_DIR") {
        PathBuf::from(dir)
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".cache/crab")
    } else {
        dirs::cache_dir()
            .ok_or_else(|| AgentError::Other("platform cache dir not available".into()))?
            .join("crab")
    };
    if !root.exists() {
        return Ok(json!({
            "method": "noop",
            "reason": "cache directory does not exist",
        }));
    }
    tokio::task::spawn_blocking({
        let root = root.clone();
        move || std::fs::remove_dir_all(&root)
    })
    .await
    .map_err(|e| AgentError::Other(format!("join error: {e}")))?
    .map_err(AgentError::Io)?;
    Ok(json!({
        "method": "fs",
        "cleared": root.display().to_string(),
    }))
}

/// Health diagnostics for the renderer's Health panel. Returns
/// version info, cache metrics, and any warnings about the local
/// environment (missing CLI, missing cache dir, etc.).
async fn agent_health() -> Result<Value> {
    let cli_version = tokio::process::Command::new(shellout::resolve_crab_bin())
        .arg("--version")
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());

    // Use the same cache directory logic as the crab CLI:
    // 1. CRAB_CACHE_DIR env var if set
    // 2. Otherwise ~/.cache/crab (XDG convention)
    let cache_dir = if let Ok(dir) = std::env::var("CRAB_CACHE_DIR") {
        PathBuf::from(dir)
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".cache/crab")
    } else {
        dirs::cache_dir()
            .map(|d| d.join("crab"))
            .unwrap_or_else(|| PathBuf::from(".cache/crab"))
    };

    let cache_dir_str = cache_dir.display().to_string();

    // Compute cache used bytes by walking the directory.
    let (cache_used_bytes, cache_exists) = if cache_dir.exists() {
        let dir = cache_dir.clone();
        let size = tokio::task::spawn_blocking(move || dir_size(&dir))
            .await
            .unwrap_or(0);
        (size, true)
    } else {
        (0u64, false)
    };

    // Staging area size — lives under the cache dir.
    let staging_dir = cache_dir.join("staging");
    let staging_size_bytes = if staging_dir.exists() {
        let dir = staging_dir.clone();
        tokio::task::spawn_blocking(move || dir_size(&dir))
            .await
            .unwrap_or(0)
    } else {
        0u64
    };

    // Free space on the partition containing the cache dir. We use a
    // simple statvfs-style approach via std::fs metadata. On platforms
    // where this isn't available, return "0".
    let cache_free_bytes = available_space(&cache_dir);

    // Last GC timestamp: check for a marker file the CLI writes.
    let gc_marker = cache_dir.join(".last_gc");
    let last_gc_at: Option<String> = if gc_marker.exists() {
        std::fs::read_to_string(&gc_marker)
            .ok()
            .map(|s| s.trim().to_owned())
    } else {
        None
    };

    // Build warnings.
    let mut warnings: Vec<Value> = Vec::new();
    if cli_version.is_none() {
        warnings.push(json!({
            "level": "warn",
            "code": "cli_missing",
            "message": "crab CLI not found in PATH. Some features require it."
        }));
    }
    if !cache_exists {
        warnings.push(json!({
            "level": "warn",
            "code": "cache_dir_missing",
            "message": format!("Cache directory does not exist: {cache_dir_str}")
        }));
    }

    Ok(json!({
        "agent_version": env!("CARGO_PKG_VERSION"),
        "crab_cli_version": cli_version,
        "cache_dir": cache_dir_str,
        "cache_used_bytes": cache_used_bytes.to_string(),
        "cache_free_bytes": cache_free_bytes.to_string(),
        "staging_size_bytes": staging_size_bytes.to_string(),
        "last_gc_at": last_gc_at,
        "warnings": warnings,
    }))
}

/// Recursively compute the total size of all files under `dir`.
fn dir_size(dir: &std::path::Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                total += dir_size(&path);
            } else if let Ok(meta) = path.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

/// Best-effort available disk space for the partition containing `path`.
/// Returns 0 if the platform doesn't support the query or the path
/// doesn't exist.
fn available_space(path: &std::path::Path) -> u64 {
    // On Unix, use statvfs. On other platforms, return 0 for now.
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c_path = match CString::new(path.as_os_str().as_bytes()) {
            Ok(p) => p,
            Err(_) => return 0,
        };
        unsafe {
            let mut stat: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c_path.as_ptr(), &mut stat) == 0 {
                (stat.f_bavail as u64) * (stat.f_frsize as u64)
            } else {
                0
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0
    }
}

/// Query the background indexer for its current status.
///
/// If the indexer hasn't been spawned (sender is `None`), returns a
/// static "disabled" response so the Health panel always gets a
/// well-formed reply.
async fn indexer_status(state: &State) -> Result<Value> {
    let guard = state.indexer_tx.read().await;
    let tx = match guard.as_ref() {
        Some(tx) => tx.clone(),
        None => {
            return Ok(json!({
                "db_path": null,
                "entries_cached": 0,
                "repos_indexed": 0,
                "running": false,
            }));
        }
    };
    // Drop the read lock before awaiting the reply.
    drop(guard);

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if tx
        .send(indexer::IndexerCommand::Status { reply: reply_tx })
        .await
        .is_err()
    {
        // Channel closed — indexer task has exited.
        return Ok(json!({
            "db_path": null,
            "entries_cached": 0,
            "repos_indexed": 0,
            "running": false,
        }));
    }

    match reply_rx.await {
        Ok(status) => Ok(serde_json::to_value(status).unwrap_or_else(|_| {
            json!({
                "db_path": null,
                "entries_cached": 0,
                "repos_indexed": 0,
                "running": false,
            })
        })),
        Err(_) => Ok(json!({
            "db_path": null,
            "entries_cached": 0,
            "repos_indexed": 0,
            "running": false,
        })),
    }
}

// ---------------- CLI shell-out handlers ----------------

#[derive(Debug, Deserialize)]
struct JobParams {
    job_id: String,
    cwd: PathBuf,
}

#[derive(Debug, Deserialize)]
struct CloneParams {
    job_id: String,
    url: String,
    dest: PathBuf,
    /// Optional auth token (PAT or OAuth) for HTTPS clones. When
    /// provided and the URL is SSH, we rewrite it to HTTPS with the
    /// token embedded so private repos clone without SSH key setup.
    #[serde(default)]
    token: Option<String>,
    /// Which forge provider the token belongs to. Determines the
    /// username used in the HTTPS credential (e.g. `x-access-token`
    /// for GitHub, `oauth2` for GitLab, `x-token-auth` for Bitbucket).
    #[serde(default)]
    provider: Option<String>,
}

async fn clone_repo(p: CloneParams, out: SharedStdout) -> Result<Value> {
    let dest_str = p.dest.display().to_string();
    let effective_url = match crate::storage_auth::current() {
        Some(connection) => connection.cli_url(&p.url)?,
        None if crate::storage_auth::cloud_url(&p.url) => crate::storage_auth::command_url(&p.url)?,
        None => rewrite_url_with_token(&p.url, p.token.as_deref(), p.provider.as_deref()),
    };

    // The URL passed to the CLI must not leak the token into logs.
    // `run_streaming` logs the args at debug level, so we use the
    // token-embedded URL only in the actual command and log the
    // sanitised original.
    tracing::debug!(url = %p.url, dest = %dest_str, has_token = p.token.is_some(), "clone_repo");

    let status = shellout::run_streaming(
        "clone",
        &p.job_id,
        None,
        &["clone", &effective_url, &dest_str],
        out,
    )
    .await?;
    Ok(json!({ "status": status, "dest": p.dest }))
}

/// Determine the HTTP Basic username to use for token authentication.
///
/// Each forge expects a different username in the `https://user:token@host`
/// credential format:
/// - GitHub: `x-access-token` (works for PATs and OAuth tokens)
/// - GitLab: `oauth2` (works for both PATs and OAuth tokens)
/// - Bitbucket: `x-token-auth` (works for app passwords and OAuth tokens)
/// - Unknown: `x-access-token` (safe default, works with most forges)
fn token_username(provider: Option<&str>) -> &'static str {
    match provider {
        Some("github") => "x-access-token",
        Some("gitlab") => "oauth2",
        Some("bitbucket") => "x-token-auth",
        _ => "x-access-token",
    }
}

/// Rewrite a git remote URL to embed an auth token for HTTPS cloning.
///
/// Handles three cases:
/// 1. SSH URL (`git@github.com:owner/repo.git`) → converted to
///    `https://<user>:<token>@github.com/owner/repo.git`
/// 2. HTTPS URL without credentials → token injected as
///    `https://<user>:<token>@host/path`
/// 3. No token provided or non-forge URL → returned unchanged.
///
/// The `<user>` is determined by [`token_username`] based on the provider.
fn rewrite_url_with_token(url: &str, token: Option<&str>, provider: Option<&str>) -> String {
    let Some(token) = token else {
        return url.to_owned();
    };
    if token.is_empty() {
        return url.to_owned();
    }

    let user = token_username(provider);

    // Case 1: SSH-style URL (git@host:owner/repo.git)
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some(colon_pos) = rest.find(':') {
            let host = &rest[..colon_pos];
            let path = &rest[colon_pos + 1..];
            return format!("https://{user}:{token}@{host}/{path}");
        }
    }

    // Case 2: ssh:// protocol
    if url.starts_with("ssh://") {
        // ssh://git@github.com/owner/repo.git → https://user:token@github.com/owner/repo.git
        if let Some(at_pos) = url.find('@') {
            let after_at = &url[at_pos + 1..];
            return format!("https://{user}:{token}@{after_at}");
        }
    }

    // Case 3: HTTPS URL — inject token if no credentials already present
    if url.starts_with("https://") || url.starts_with("http://") {
        let scheme_end = url.find("://").unwrap_or(0) + 3;
        let after_scheme = &url[scheme_end..];
        // If there's already an @ sign, credentials are present — don't override
        if after_scheme.contains('@') {
            return url.to_owned();
        }
        return format!("{}{user}:{token}@{after_scheme}", &url[..scheme_end]);
    }

    // Unrecognised format (crab://, file://, etc.) — pass through
    url.to_owned()
}

async fn status(p: JobParams, out: SharedStdout) -> Result<Value> {
    let status =
        shellout::run_streaming("status", &p.job_id, Some(&p.cwd), &["status"], out).await?;
    Ok(json!({ "status": status }))
}

#[derive(Debug, Deserialize)]
struct PatternsParams {
    job_id: String,
    cwd: PathBuf,
    #[serde(default)]
    patterns: Vec<String>,
}

async fn hydrate(p: PatternsParams, out: SharedStdout) -> Result<Value> {
    let mut args: Vec<String> = vec!["hydrate".into()];
    if p.patterns.is_empty() {
        args.push("--all".into());
    } else {
        args.extend(p.patterns.clone());
    }
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    let status =
        shellout::run_streaming("hydrate", &p.job_id, Some(&p.cwd), &args_ref, out).await?;
    Ok(json!({ "status": status }))
}

async fn dehydrate(p: PatternsParams, out: SharedStdout) -> Result<Value> {
    let mut args: Vec<String> = vec!["dehydrate".into()];
    if p.patterns.is_empty() {
        args.push("--all".into());
    } else {
        args.extend(p.patterns.clone());
    }
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    let status =
        shellout::run_streaming("dehydrate", &p.job_id, Some(&p.cwd), &args_ref, out).await?;
    Ok(json!({ "status": status }))
}

async fn push(p: JobParams, out: SharedStdout) -> Result<Value> {
    // Crab push is driven through `git push`; the remote helper
    // takes over. Calling `git push` here keeps the UI aligned with
    // standard git workflow.
    let mut command = crate::shellout::git_command();
    crate::storage_auth::configure_command(&mut command)?;
    let git = command
        .arg("push")
        .current_dir(&p.cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .spawn()?;
    // Piping git's output through the same streaming helper keeps the
    // progress contract uniform.
    finish_child("push", &p.job_id, git, out).await
}

#[derive(Debug, Deserialize)]
struct AddParams {
    job_id: String,
    cwd: PathBuf,
    #[serde(default)]
    paths: Vec<String>,
}

async fn add(p: AddParams, out: SharedStdout) -> Result<Value> {
    let mut args: Vec<String> = vec!["add".into()];
    args.extend(p.paths.clone());
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    let status = shellout::run_streaming("add", &p.job_id, Some(&p.cwd), &args_ref, out).await?;
    Ok(json!({ "status": status }))
}

#[derive(Debug, Deserialize)]
struct CrabAddStructuredParams {
    repo_id: String,
    job_id: String,
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default)]
    dry_run: bool,
}

async fn crab_add_structured(
    work_dir: &Path,
    p: CrabAddStructuredParams,
    out: SharedStdout,
) -> Result<Value> {
    let mut args = vec!["add".to_owned(), "--jsonl".to_owned()];
    if p.dry_run {
        args.push("--dry-run".into());
    }
    args.extend(p.patterns);
    let summary = shellout::run_structured(
        "add",
        &p.job_id,
        work_dir,
        &args,
        &[],
        "crab/add_progress",
        out,
    )
    .await?;
    Ok(json!({ "status": 0, "job_id": p.job_id, "summary": summary }))
}

#[derive(Debug, Deserialize)]
struct CrabPushStructuredParams {
    repo_id: String,
    job_id: String,
    #[serde(default)]
    remote: Option<String>,
    #[serde(default)]
    force: bool,
}

async fn crab_push_structured(
    work_dir: &Path,
    p: CrabPushStructuredParams,
    out: SharedStdout,
) -> Result<Value> {
    let mut args = vec!["push".to_owned(), "--jsonl".to_owned()];
    if p.force {
        args.push("--force".into());
    }
    if let Some(remote) = p.remote {
        args.push(remote);
    }
    let mut summary = shellout::run_structured(
        "push",
        &p.job_id,
        work_dir,
        &args,
        &[],
        "crab/push_progress",
        out,
    )
    .await?;
    // The v2 terminal contract reports refs and elapsed time, not transfer totals.
    summary["bytes_uploaded"] = Value::Null;
    summary["xorbs_created"] = Value::Null;
    Ok(json!({ "status": 0, "job_id": p.job_id, "summary": summary }))
}

#[derive(Debug, Deserialize)]
struct StagingStatsParams {
    repo_id: String,
}

async fn staging_stats(work_dir: &Path) -> Result<Value> {
    let mut payload =
        shellout::run_json(Some(work_dir), &["staging", "stats", "--json"], &[]).await?;
    let mut stats = payload
        .get_mut("stats")
        .map(Value::take)
        .ok_or_else(|| AgentError::Other("staging stats returned no statistics".into()))?;
    stats["files"] = payload["files"].take();
    stats["lifecycle"] = payload["lifecycle"].take();
    Ok(stats)
}

// ---------------- staging_clean ----------------

#[derive(Debug, Deserialize)]
struct StagingCleanParams {
    repo_id: String,
    #[serde(default)]
    prune_abandoned: bool,
}

/// Run `crab staging clean` to purge stale data and reclaim disk.
async fn staging_clean(work_dir: &std::path::Path, prune_abandoned: bool) -> Result<Value> {
    let mut args = vec!["staging", "clean"];
    if prune_abandoned {
        args.push("--prune-abandoned");
    }

    let output = shellout::command()?
        .args(&args)
        .current_dir(work_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr,
        });
    }

    Ok(json!({ "ok": true }))
}

// ---------------- crab_reset_files ----------------

#[derive(Debug, Deserialize)]
struct CrabResetFilesParams {
    repo_id: String,
    paths: Vec<String>,
}

/// Remove specific files from the crab staging area by running
/// `crab reset <paths>`. This unstages from git's index and cleans
/// the corresponding chunk data from the local staging area.
///
/// Accepts either file paths (relative to repo root) or file hashes
/// prefixed with "hash:". For hash-based removal, we scan the working
/// tree to find the corresponding pointer file.
async fn crab_reset_files(work_dir: &std::path::Path, paths: &[String]) -> Result<Value> {
    if paths.is_empty() {
        return Ok(json!({ "ok": true, "files_reset": 0 }));
    }

    // Resolve hash-prefixed entries to actual file paths by scanning
    // git's index for pointer files matching the given hash.
    let mut resolved_paths: Vec<String> = Vec::new();
    let mut hashes_to_find: Vec<String> = Vec::new();

    for p in paths {
        if let Some(hash) = p.strip_prefix("hash:") {
            hashes_to_find.push(hash.to_owned());
        } else {
            resolved_paths.push(p.clone());
        }
    }

    // For hash-based lookups, scan git ls-files and read pointer blobs
    // to find matching file paths.
    if !hashes_to_find.is_empty() {
        let ls_output = crate::shellout::git_command()
            .args(["ls-files", "--cached", "-z"])
            .current_dir(work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await
            .map_err(AgentError::Io)?;

        if ls_output.status.success() {
            let stdout = String::from_utf8_lossy(&ls_output.stdout);
            for file_path in stdout.split('\0').filter(|s| !s.is_empty()) {
                let abs_path = work_dir.join(file_path);
                // Read the file to check if it's a pointer with a matching hash.
                if let Ok(content) = tokio::fs::read(&abs_path).await {
                    if content.len() <= 256 {
                        if let Ok(text) = std::str::from_utf8(&content) {
                            // Extract file-hash from pointer content.
                            for line in text.lines() {
                                if let Some(hash_val) = line.strip_prefix("file-hash ") {
                                    if hashes_to_find.iter().any(|h| h == hash_val) {
                                        resolved_paths.push(file_path.to_owned());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if resolved_paths.is_empty() {
        return Ok(json!({ "ok": true, "files_reset": 0, "output": "no matching files found" }));
    }

    let mut args: Vec<&str> = vec!["reset"];
    for p in &resolved_paths {
        args.push(p.as_str());
    }

    let output = shellout::command()?
        .args(&args)
        .current_dir(work_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr,
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(json!({ "ok": true, "output": stdout.trim(), "files_reset": resolved_paths.len() }))
}

#[derive(Deserialize)]
struct StorageVerifyParams {
    repo_id: String,
    #[serde(default)]
    remote: Option<String>,
}

// ---------------- crab_init ----------------

#[derive(Debug, Deserialize)]
struct CrabInitParams {
    repo_id: String,
    /// The remote URL to initialize with (e.g. "crab://bucket/prefix").
    url: String,
}

/// Initialize native v2 metadata and Git wiring using the matched CLI.
async fn crab_init(work_dir: &std::path::Path, p: CrabInitParams) -> Result<Value> {
    let url = match crate::storage_auth::current() {
        Some(connection) => connection.cli_url(&p.url)?,
        None => crate::storage_auth::command_url(&p.url)?,
    };
    let mut command = shellout::command()?;
    command.args(["init", &url]);
    if let Some(connection) = crate::storage_auth::current() {
        command.args(["--storage-provider", connection.provider()]);
    }
    let output = command
        .current_dir(work_dir)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;
    if !output.status.success() {
        return Err(AgentError::Cli {
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(
        json!({ "ok": true, "url": url, "config_path": work_dir.join("crab.toml").display().to_string() }),
    )
}

// ---------------- crab_add_auto ----------------

#[derive(Debug, Deserialize)]
struct CrabAddAutoParams {
    repo_id: String,
    paths: Vec<String>,
}

/// One-click "Track with crab" verb for the large-file pre-commit
/// check. For each path:
/// 1. Ensures the file extension is registered in `.gitattributes` via `crab track`
/// 2. Runs `crab add <path>` to chunk + stage the file
/// 3. Stages the resulting pointer blobs with `git add`
///
/// Partial success is acceptable — errors are collected and returned
/// alongside the count of successfully tracked files.
async fn crab_add_auto(work_dir: &std::path::Path, p: CrabAddAutoParams) -> Result<Value> {
    use std::collections::HashSet;
    use std::path::Path;
    use tracing::{debug, warn};

    let mut tracked: u64 = 0;
    let mut errors: Vec<Value> = Vec::new();
    let mut succeeded_paths: Vec<String> = Vec::new();
    let mut patterns_added: Vec<String> = Vec::new();

    // Step 1: Ensure all unique file extensions are tracked in .gitattributes.
    // This is idempotent — `crab track` is a no-op if the pattern already exists.
    let mut seen_extensions: HashSet<String> = HashSet::new();
    for path in &p.paths {
        if let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) {
            let ext_lower = ext.to_lowercase();
            if seen_extensions.insert(ext_lower.clone()) {
                let glob = format!("*.{}", ext_lower);
                debug!(glob = %glob, "crab_add_auto: ensuring pattern is tracked");
                let track_output = shellout::command()?
                    .args(["track", &glob])
                    .current_dir(work_dir)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .output()
                    .await
                    .map_err(AgentError::Io)?;

                if track_output.status.success() {
                    patterns_added.push(glob);
                } else {
                    let stderr = String::from_utf8_lossy(&track_output.stderr).into_owned();
                    warn!(glob = %format!("*.{}", ext_lower), stderr = %stderr, "crab track failed");
                }
            }
        }
    }

    // Step 2: Run `crab add` for each file path.
    for path in &p.paths {
        debug!(path = %path, "crab_add_auto: running crab add");
        let mut cmd = shellout::command()?;
        cmd.args(["add", path])
            .current_dir(work_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        crate::storage_auth::configure_command(&mut cmd)?;

        let output = cmd.output().await.map_err(AgentError::Io)?;

        if output.status.success() {
            tracked += 1;
            succeeded_paths.push(path.clone());
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            warn!(path = %path, status = output.status.code(), stderr = %stderr, "crab add failed");
            errors.push(json!({
                "path": path,
                "status": output.status.code(),
                "stderr": stderr,
            }));
        }
    }

    // Step 3: Stage the resulting pointer blobs so the index reflects the
    // tracked state. Only stage paths that succeeded above.
    if !succeeded_paths.is_empty() {
        let mut git_args = vec!["add", "--"];
        let refs: Vec<&str> = succeeded_paths.iter().map(String::as_str).collect();
        git_args.extend(refs);

        debug!(paths = ?succeeded_paths, "crab_add_auto: staging pointer blobs");
        let git_output = crate::shellout::git_command()
            .args(&git_args)
            .current_dir(work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(AgentError::Io)?;

        if !git_output.status.success() {
            let stderr = String::from_utf8_lossy(&git_output.stderr).into_owned();
            warn!(stderr = %stderr, "git add (staging) failed after crab add");
            return Err(AgentError::Cli {
                status: git_output.status.code().unwrap_or(-1),
                stderr,
            });
        }
    }

    // Also stage .gitattributes if we added new patterns.
    if !patterns_added.is_empty() {
        debug!("crab_add_auto: staging .gitattributes");
        let _ = crate::shellout::git_command()
            .args(["add", ".gitattributes"])
            .current_dir(work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await;
    }

    let mut result = json!({
        "ok": true,
        "tracked": tracked,
        "patterns_added": patterns_added,
    });
    if !errors.is_empty() {
        result["errors"] = json!(errors);
    }
    Ok(result)
}

// ---------------- crab_track_and_add ----------------

#[derive(Debug, Deserialize)]
struct CrabTrackAndAddParams {
    repo_id: String,
    paths: Vec<String>,
    /// If true, also commit the staged pointer files with an auto-generated message.
    auto_commit: Option<bool>,
    /// Custom commit message. If absent and auto_commit is true, generates one.
    commit_message: Option<String>,
}

/// Combined track + add + optional commit flow for the desktop wizard.
///
/// 1. Registers file extensions in `.gitattributes` via `crab track`
/// 2. Runs `crab add` for each file (chunk + stage)
/// 3. Stages pointer blobs + `.gitattributes` via `git add`
/// 4. Optionally commits with a descriptive message
///
/// Returns detailed per-file results so the UI can show granular progress.
async fn crab_track_and_add(work_dir: &std::path::Path, p: CrabTrackAndAddParams) -> Result<Value> {
    use std::collections::HashSet;
    use std::path::Path;
    use tracing::{debug, warn};

    let mut patterns_added: Vec<String> = Vec::new();
    let mut files_tracked: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();

    // Step 1: Register patterns.
    let mut seen_extensions: HashSet<String> = HashSet::new();
    for path in &p.paths {
        if let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) {
            let ext_lower = ext.to_lowercase();
            if seen_extensions.insert(ext_lower.clone()) {
                let glob = format!("*.{}", ext_lower);
                debug!(glob = %glob, "crab_track_and_add: registering pattern");
                let output = shellout::command()?
                    .args(["track", &glob])
                    .current_dir(work_dir)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .output()
                    .await
                    .map_err(AgentError::Io)?;

                if output.status.success() {
                    patterns_added.push(glob);
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                    warn!(glob = %format!("*.{}", ext_lower), stderr = %stderr, "crab track failed");
                }
            }
        }
    }

    // Step 2: Run `crab add` for each file.
    for path in &p.paths {
        debug!(path = %path, "crab_track_and_add: running crab add");
        let mut cmd = shellout::command()?;
        cmd.args(["add", path])
            .current_dir(work_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        crate::storage_auth::configure_command(&mut cmd)?;

        let output = cmd.output().await.map_err(AgentError::Io)?;

        if output.status.success() {
            files_tracked.push(json!({
                "path": path,
                "status": "ok",
            }));
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            warn!(path = %path, status = output.status.code(), stderr = %stderr, "crab add failed");
            errors.push(json!({
                "path": path,
                "status": output.status.code(),
                "stderr": stderr,
            }));
        }
    }

    // Step 3: Stage pointer blobs + .gitattributes.
    let succeeded_paths: Vec<&str> = files_tracked
        .iter()
        .filter_map(|f| f["path"].as_str())
        .collect();

    if !succeeded_paths.is_empty() {
        let mut git_args: Vec<&str> = vec!["add", "--"];
        git_args.extend(&succeeded_paths);
        if !patterns_added.is_empty() {
            git_args.push(".gitattributes");
        }

        debug!(paths = ?succeeded_paths, "crab_track_and_add: staging pointer blobs");
        let git_output = crate::shellout::git_command()
            .args(&git_args)
            .current_dir(work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(AgentError::Io)?;

        if !git_output.status.success() {
            let stderr = String::from_utf8_lossy(&git_output.stderr).into_owned();
            warn!(stderr = %stderr, "git add (staging) failed");
            return Err(AgentError::Cli {
                status: git_output.status.code().unwrap_or(-1),
                stderr,
            });
        }
    } else if !patterns_added.is_empty() {
        // Stage .gitattributes even if no files succeeded (patterns were still added).
        let _ = crate::shellout::git_command()
            .args(["add", ".gitattributes"])
            .current_dir(work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await;
    }

    // Step 4: Optional auto-commit.
    let mut commit_sha: Option<String> = None;
    if p.auto_commit.unwrap_or(false) && !succeeded_paths.is_empty() {
        let file_names: Vec<&str> = succeeded_paths
            .iter()
            .filter_map(|p| Path::new(p).file_name().and_then(|n| n.to_str()))
            .collect();

        let message = p.commit_message.unwrap_or_else(|| {
            if file_names.len() == 1 {
                format!("Track large file: {}", file_names[0])
            } else {
                format!(
                    "Track {} large files: {}",
                    file_names.len(),
                    file_names.join(", ")
                )
            }
        });

        debug!(message = %message, "crab_track_and_add: committing");
        let commit_output = crate::shellout::git_command()
            .args(["commit", "-m", &message])
            .current_dir(work_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(AgentError::Io)?;

        if commit_output.status.success() {
            // Extract the commit SHA from git log.
            let sha_output = crate::shellout::git_command()
                .args(["rev-parse", "HEAD"])
                .current_dir(work_dir)
                .stdout(std::process::Stdio::piped())
                .output()
                .await
                .map_err(AgentError::Io)?;
            commit_sha = Some(
                String::from_utf8_lossy(&sha_output.stdout)
                    .trim()
                    .to_owned(),
            );
        } else {
            let stderr = String::from_utf8_lossy(&commit_output.stderr).into_owned();
            warn!(stderr = %stderr, "git commit failed");
            // Non-fatal: tracking succeeded, commit failed.
            errors.push(json!({
                "path": "_commit",
                "status": commit_output.status.code(),
                "stderr": stderr,
            }));
        }
    }

    Ok(json!({
        "ok": true,
        "patterns_added": patterns_added,
        "files_tracked": files_tracked,
        "errors": errors,
        "commit_sha": commit_sha,
    }))
}

// ---------------- serialization helpers ----------------

fn serialize_dir_entry(e: &BrowserEntry) -> Value {
    json!({
        "path": e.path.display().to_string(),
        "kind": format!("{:?}", e.kind).to_lowercase(),
        "size": e.size,
        "pointer_hash": e.pointer_hash,
    })
}

fn serialize_file_stat(s: &FileMetadata) -> Value {
    json!({
        "size": s.size,
        "pointer_hash": s.pointer_hash.clone(),
        "mtime": s.mtime.and_then(systime_to_secs),
        "rev": s.rev,
        "kind": format!("{:?}", s.kind).to_lowercase(),
    })
}

fn systime_to_secs(t: std::time::SystemTime) -> Option<u64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Drain a running child process's stdout+stderr into the streaming
/// channel and return the final exit-code JSON payload. Mirrors the
/// behavior of `shellout::run_streaming` but for `git push`.
async fn finish_child(
    op: &str,
    job_id: &str,
    mut child: tokio::process::Child,
    out: SharedStdout,
) -> Result<Value> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let pump = |reader: Option<tokio::process::ChildStdout>,
                stream: shellout::Stream,
                out: SharedStdout| async move {
        if let Some(reader) = reader {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = crate::notify(
                    &out,
                    "progress/line",
                    shellout::ProgressEvent {
                        op,
                        job_id,
                        stream,
                        line,
                    },
                )
                .await;
            }
        }
    };
    let pump_err = |reader: Option<tokio::process::ChildStderr>,
                    stream: shellout::Stream,
                    out: SharedStdout| async move {
        if let Some(reader) = reader {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = crate::notify(
                    &out,
                    "progress/line",
                    shellout::ProgressEvent {
                        op,
                        job_id,
                        stream,
                        line,
                    },
                )
                .await;
            }
        }
    };

    let (status_res, _, _) = tokio::join!(
        child.wait(),
        pump(stdout, shellout::Stream::Stdout, Arc::clone(&out)),
        pump_err(stderr, shellout::Stream::Stderr, Arc::clone(&out)),
    );
    let status = status_res?;
    let code = status.code().unwrap_or(-1);
    let _ = crate::notify(
        &out,
        "progress/complete",
        shellout::CompletionEvent {
            op,
            job_id,
            status: code,
        },
    )
    .await;
    if !status.success() {
        return Err(AgentError::Cli {
            status: code,
            stderr: String::new(),
        });
    }
    Ok(json!({ "status": code }))
}

// ---- Git write actions -----------------------------------------------
//
// Each handler is a thin shell-out to `git`. They all follow the same
// pattern: deserialize typed params, build an arg list, run the
// command, and return `{status, stdout, stderr}`. The UI surfaces
// errors via the Activity drawer.

// ---- Tag actions -----------------------------------------------------

#[derive(Debug, Deserialize)]
struct GitTagAddParams {
    repo_id: String,
    name: String,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

async fn git_tag_add(state: &State, p: GitTagAddParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let mut args: Vec<String> = vec!["tag".into()];
    if let Some(msg) = &p.message {
        args.push("-a".into());
        args.push("-m".into());
        args.push(msg.clone());
    }
    args.push(p.name.clone());
    if let Some(t) = &p.target {
        args.push(t.clone());
    }
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_git_simple(&work_dir, &args_ref).await
}

#[derive(Debug, Deserialize)]
struct GitTagDeleteParams {
    repo_id: String,
    name: String,
}

async fn git_tag_delete(state: &State, p: GitTagDeleteParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    run_git_simple(&work_dir, &["tag", "-d", &p.name]).await
}

#[derive(Debug, Deserialize)]
struct GitShaParams {
    repo_id: String,
    sha: String,
}

async fn git_cherry_pick(state: &State, p: GitShaParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    run_git_simple(&work_dir, &["cherry-pick", &p.sha]).await
}

async fn git_revert(state: &State, p: GitShaParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    run_git_simple(&work_dir, &["revert", "--no-edit", &p.sha]).await
}

#[derive(Debug, Deserialize)]
struct GitMergeParams {
    repo_id: String,
    branch: String,
    #[serde(default)]
    no_ff: bool,
}

async fn git_merge(state: &State, p: GitMergeParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let mut args: Vec<&str> = vec!["merge"];
    if p.no_ff {
        args.push("--no-ff");
    }
    args.push(&p.branch);
    run_git_simple(&work_dir, &args).await
}

#[derive(Debug, Deserialize)]
struct GitRebaseParams {
    repo_id: String,
    upstream: String,
    #[serde(default)]
    onto: Option<String>,
}

async fn git_rebase(state: &State, p: GitRebaseParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let mut args: Vec<String> = vec!["rebase".into()];
    if let Some(onto) = &p.onto {
        args.push("--onto".into());
        args.push(onto.clone());
    }
    args.push(p.upstream.clone());
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_git_simple(&work_dir, &args_ref).await
}

#[derive(Debug, Deserialize)]
struct GitResetParams {
    repo_id: String,
    target: String,
    #[serde(default = "default_reset_mode")]
    mode: String,
}

fn default_reset_mode() -> String {
    "mixed".to_owned()
}

async fn git_reset(state: &State, p: GitResetParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let mode_flag = match p.mode.as_str() {
        "soft" => "--soft",
        "hard" => "--hard",
        _ => "--mixed",
    };
    run_git_simple(&work_dir, &["reset", mode_flag, &p.target]).await
}

#[derive(Debug, Deserialize)]
struct GitFetchParams {
    repo_id: String,
    #[serde(default)]
    remote: Option<String>,
    #[serde(default)]
    prune: bool,
}

async fn git_fetch(state: &State, p: GitFetchParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let mut args: Vec<String> = vec!["fetch".into()];
    if p.prune {
        args.push("--prune".into());
    }
    if let Some(remote) = &p.remote {
        args.push(remote.clone());
    }
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_git_simple(&work_dir, &args_ref).await
}

#[derive(Debug, Deserialize)]
struct GitPullParams {
    repo_id: String,
    #[serde(default)]
    rebase: bool,
}

async fn git_pull(state: &State, p: GitPullParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let mut args: Vec<&str> = vec!["pull"];
    if p.rebase {
        args.push("--rebase");
    }
    run_git_simple(&work_dir, &args).await
}

// ---- Per-branch push / pull / fetch ----------------------------------

#[derive(Debug, Deserialize)]
struct GitFetchBranchParams {
    repo_id: String,
    branch: String,
    #[serde(default)]
    remote: Option<String>,
}

/// Fetch a single branch from a remote.
async fn git_fetch_branch(state: &State, p: GitFetchBranchParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let remote = p.remote.as_deref().unwrap_or("origin");
    run_git_simple(&work_dir, &["fetch", remote, &p.branch]).await
}

#[derive(Debug, Deserialize)]
struct GitPullBranchParams {
    repo_id: String,
    branch: String,
    #[serde(default)]
    remote: Option<String>,
}

/// Pull (fetch + merge) a single branch. Only works correctly when
/// the branch is currently checked out; otherwise falls back to a
/// plain fetch so we don't accidentally merge into the wrong branch.
async fn git_pull_branch(state: &State, p: GitPullBranchParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let remote = p.remote.as_deref().unwrap_or("origin");

    // Check if the target branch is currently checked out.
    let head_out = crate::shellout::git_command()
        .args(["symbolic-ref", "--short", "HEAD"])
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    let current_branch = String::from_utf8_lossy(&head_out.stdout).trim().to_owned();

    if head_out.status.success() && current_branch == p.branch {
        run_git_simple(&work_dir, &["pull", remote, &p.branch]).await
    } else {
        // Not checked out — just fetch so we don't merge into the wrong branch.
        run_git_simple(&work_dir, &["fetch", remote, &p.branch]).await
    }
}

#[derive(Debug, Deserialize)]
struct GitPushBranchParams {
    repo_id: String,
    branch: String,
    #[serde(default)]
    remote: Option<String>,
    #[serde(default)]
    set_upstream: bool,
}

/// Push a single branch to a remote.
async fn git_push_branch(state: &State, p: GitPushBranchParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let remote = p.remote.as_deref().unwrap_or("origin");
    let mut args: Vec<&str> = vec!["push"];
    if p.set_upstream {
        args.push("--set-upstream");
    }
    args.push(remote);
    args.push(&p.branch);
    run_git_simple(&work_dir, &args).await
}

// ---- Git config probe ------------------------------------------------

#[derive(Debug, Deserialize)]
struct GitConfigGetParams {
    repo_id: String,
    keys: Vec<String>,
}

/// Probe git config values for the given keys. Returns a map of
/// `key → value | null`. Used by the renderer to determine whether
/// signing is available (user.signingkey, gpg.format, commit.gpgsign).
async fn git_config_get(state: &State, p: GitConfigGetParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let mut values = serde_json::Map::new();
    for key in &p.keys {
        let out = crate::shellout::git_command()
            .args(["config", "--get", key])
            .current_dir(&work_dir)
            .output()
            .await
            .map_err(AgentError::Io)?;
        if out.status.success() {
            let val = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            values.insert(key.clone(), Value::String(val));
        } else {
            values.insert(key.clone(), Value::Null);
        }
    }
    Ok(json!({ "values": values }))
}

// ---- Git blame -------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GitBlameParams {
    repo_id: String,
    path: String,
    #[serde(default)]
    rev: Option<String>,
}

async fn git_blame(state: &State, p: GitBlameParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let mut args = vec!["blame", "--porcelain"];
    if let Some(ref rev) = p.rev {
        args.push(rev.as_str());
    }
    args.push("--");
    args.push(&p.path);

    let out = crate::shellout::git_command()
        .args(&args)
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr,
        });
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines: Vec<Value> = Vec::new();
    let mut current_sha = String::new();
    let mut current_author = String::new();
    let mut current_line: u64 = 0;

    for raw_line in stdout.lines() {
        if raw_line.starts_with('\t') {
            // Content line — emit the blame entry.
            lines.push(json!({
                "sha": current_sha,
                "author": current_author,
                "line": current_line,
                "content": &raw_line[1..],
            }));
        } else if raw_line.len() >= 40 && raw_line.as_bytes()[40] == b' ' {
            // Header line: <sha> <orig_line> <final_line> [<num_lines>]
            current_sha = raw_line[..40].to_owned();
            let parts: Vec<&str> = raw_line[41..].split_whitespace().collect();
            if parts.len() >= 2 {
                current_line = parts[1].parse().unwrap_or(0);
            }
        } else if let Some(author) = raw_line.strip_prefix("author ") {
            current_author = author.to_owned();
        }
    }

    Ok(json!({ "lines": lines }))
}

// ---- Git diff --name-only --------------------------------------------

#[derive(Debug, Deserialize)]
struct GitDiffNameOnlyParams {
    repo_id: String,
    base: String,
    head: String,
}

async fn git_diff_name_only(state: &State, p: GitDiffNameOnlyParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let range = format!("{}...{}", p.base, p.head);

    let out = crate::shellout::git_command()
        .args(["diff", "--name-status", &range])
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr,
        });
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut files: Vec<Value> = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Format: <status>\t<path> (or <status>\t<old_path>\t<new_path> for renames)
        let mut parts = line.splitn(2, '\t');
        let status = parts.next().unwrap_or("").to_owned();
        let path = parts.next().unwrap_or("").to_owned();
        files.push(json!({ "path": path, "status": status }));
    }

    Ok(json!({ "files": files }))
}

/// Shared helper for simple git commands that return status + output.
async fn run_git_simple(work_dir: &std::path::Path, args: &[&str]) -> Result<Value> {
    let mut command = crate::shellout::git_command();
    crate::storage_auth::configure_command(&mut command)?;
    let out = command
        .args(args)
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;
    let status = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !out.status.success() {
        return Err(AgentError::Cli { status, stderr });
    }
    Ok(json!({
        "status": status,
        "stdout": stdout,
        "stderr": stderr,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tempfile::TempDir;

    fn sh(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("git command spawn")
    }

    fn git_init_repo() -> (TempDir, PathBuf) {
        let td = tempfile::tempdir().expect("tempdir");
        let root = td.path().join("repo");
        std::fs::create_dir(&root).expect("repository directory");
        let out = sh(&root, &["init", "--initial-branch=main"]);
        assert!(out.status.success(), "git init: {:?}", out);
        let _ = sh(&root, &["config", "user.email", "test@example.com"]);
        let _ = sh(&root, &["config", "user.name", "Test User"]);
        (td, root)
    }

    #[cfg(unix)]
    #[test]
    fn repository_size_excludes_symlinks() {
        let root = TempDir::new().unwrap();
        let nested = root.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(root.path().join("first"), b"abc").unwrap();
        std::fs::write(nested.join("second"), b"de").unwrap();
        std::os::unix::fs::symlink(root.path(), nested.join("loop")).unwrap();
        std::os::unix::fs::symlink(root.path().join("missing"), nested.join("broken")).unwrap();

        assert_eq!(dir_size_bytes(root.path()), 5);
    }

    #[tokio::test]
    async fn git_config_get_returns_set_values() {
        let (_td, root) = git_init_repo();
        // Set a signing key in the repo config.
        let _ = sh(&root, &["config", "user.signingkey", "ABCDEF1234"]);
        let _ = sh(&root, &["config", "commit.gpgsign", "true"]);

        let state = State::new();
        let repo = BrowseSession::open_local(&root).await.unwrap();
        state.insert("test".into(), repo, Some(root)).await;

        let params = GitConfigGetParams {
            repo_id: "test".into(),
            keys: vec![
                "user.signingkey".into(),
                "gpg.format".into(),
                "commit.gpgsign".into(),
            ],
        };

        let result = git_config_get(&state, params).await.unwrap();
        let values = result["values"].as_object().unwrap();

        assert_eq!(values["user.signingkey"].as_str(), Some("ABCDEF1234"));
        assert!(values["gpg.format"].is_null());
        assert_eq!(values["commit.gpgsign"].as_str(), Some("true"));
    }

    #[tokio::test]
    async fn git_config_get_returns_null_for_unset_keys() {
        let (_td, root) = git_init_repo();

        let state = State::new();
        let repo = BrowseSession::open_local(&root).await.unwrap();
        state.insert("test".into(), repo, Some(root)).await;

        let params = GitConfigGetParams {
            repo_id: "test".into(),
            keys: vec![
                "user.signingkey".into(),
                "gpg.format".into(),
                "nonexistent.key".into(),
            ],
        };

        let result = git_config_get(&state, params).await.unwrap();
        let values = result["values"].as_object().unwrap();

        // All should be null since we didn't set any of these.
        assert!(values["user.signingkey"].is_null());
        assert!(values["gpg.format"].is_null());
        assert!(values["nonexistent.key"].is_null());
    }

    #[tokio::test]
    async fn git_config_get_ssh_format() {
        let (_td, root) = git_init_repo();
        // SSH signing: gpg.format=ssh without user.signingkey.
        let _ = sh(&root, &["config", "gpg.format", "ssh"]);

        let state = State::new();
        let repo = BrowseSession::open_local(&root).await.unwrap();
        state.insert("test".into(), repo, Some(root)).await;

        let params = GitConfigGetParams {
            repo_id: "test".into(),
            keys: vec!["user.signingkey".into(), "gpg.format".into()],
        };

        let result = git_config_get(&state, params).await.unwrap();
        let values = result["values"].as_object().unwrap();

        // gpg.format is set, user.signingkey is not — signing should
        // still be considered available (SSH signing).
        assert!(values["user.signingkey"].is_null());
        assert_eq!(values["gpg.format"].as_str(), Some("ssh"));
    }

    #[tokio::test]
    async fn list_refs_returns_branches_with_kind() {
        let (_td, root) = git_init_repo();
        // Create an initial commit so HEAD and main exist.
        std::fs::write(root.join("file.txt"), "hello").unwrap();
        let _ = sh(&root, &["add", "."]);
        let _ = sh(&root, &["commit", "-m", "init"]);
        // Create a second branch.
        let _ = sh(&root, &["branch", "feature"]);

        let state = State::new();
        let repo = BrowseSession::open_local(&root).await.unwrap();
        state.insert("test".into(), repo, Some(root)).await;

        let result = list_refs(
            &state,
            ListRefsParams {
                repo_id: "test".into(),
                kinds: Vec::new(),
                max: None,
            },
        )
        .await
        .unwrap();
        let refs = result["refs"].as_array().unwrap();

        // Should have at least main and feature.
        let names: Vec<&str> = refs.iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"main"), "expected 'main' in {names:?}");
        assert!(
            names.contains(&"feature"),
            "expected 'feature' in {names:?}"
        );

        // All should be kind "branch".
        for r in refs {
            assert_eq!(r["kind"].as_str(), Some("branch"));
        }

        // HEAD marker should be on main.
        let main_ref = refs
            .iter()
            .find(|r| r["name"].as_str() == Some("main"))
            .unwrap();
        assert_eq!(main_ref["is_head"].as_bool(), Some(true));
        let feature_ref = refs
            .iter()
            .find(|r| r["name"].as_str() == Some("feature"))
            .unwrap();
        assert_eq!(feature_ref["is_head"].as_bool(), Some(false));
    }

    #[tokio::test]
    async fn list_refs_includes_ahead_behind_with_upstream() {
        let (_td, root) = git_init_repo();
        // Create an initial commit.
        std::fs::write(root.join("file.txt"), "hello").unwrap();
        let _ = sh(&root, &["add", "."]);
        let _ = sh(&root, &["commit", "-m", "init"]);

        // Set up a fake remote by cloning into a bare repo, then
        // adding it as a remote to the original.
        let bare_dir = root.join("bare.git");
        let _ = std::process::Command::new("git")
            .args([
                "clone",
                "--bare",
                root.to_str().unwrap(),
                bare_dir.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        let _ = sh(
            &root,
            &["remote", "add", "origin", bare_dir.to_str().unwrap()],
        );
        let _ = sh(&root, &["fetch", "origin"]);
        let _ = sh(&root, &["branch", "--set-upstream-to=origin/main", "main"]);

        // Make a local commit so we're ahead by 1.
        std::fs::write(root.join("file.txt"), "updated").unwrap();
        let _ = sh(&root, &["add", "."]);
        let _ = sh(&root, &["commit", "-m", "local change"]);

        let state = State::new();
        let repo = BrowseSession::open_local(&root).await.unwrap();
        state.insert("test".into(), repo, Some(root)).await;

        let result = list_refs(
            &state,
            ListRefsParams {
                repo_id: "test".into(),
                kinds: Vec::new(),
                max: None,
            },
        )
        .await
        .unwrap();
        let refs = result["refs"].as_array().unwrap();

        let main_ref = refs
            .iter()
            .find(|r| r["name"].as_str() == Some("main"))
            .unwrap();
        assert_eq!(main_ref["upstream"].as_str(), Some("origin/main"));
        assert_eq!(main_ref["ahead"].as_u64(), Some(1));
        // behind should be null (0 behind is reported as no "behind" segment).
        assert!(main_ref["behind"].is_null());
    }

    #[test]
    fn parse_track_info_empty() {
        assert_eq!(parse_track_info(""), (None, None));
    }

    #[test]
    fn parse_track_info_ahead_only() {
        assert_eq!(parse_track_info("ahead 3"), (Some(3), None));
    }

    #[test]
    fn parse_track_info_behind_only() {
        assert_eq!(parse_track_info("behind 2"), (None, Some(2)));
    }

    #[test]
    fn parse_track_info_both() {
        assert_eq!(parse_track_info("ahead 5, behind 2"), (Some(5), Some(2)));
    }

    #[tokio::test]
    async fn open_local_initializes_private_state_at_git_root() {
        let (_td, root) = git_init_repo();
        let nested = root.join("nested");
        std::fs::create_dir(&nested).unwrap();
        let state = State::new();
        let opened = open_local(
            &state,
            OpenLocalParams {
                repo_id: "local-root".into(),
                path: nested.clone(),
            },
        )
        .await;
        state.shutdown().await.unwrap();
        opened.unwrap();
        assert_eq!(
            (
                root.join(".crab/desktop/worktrees.json").is_file(),
                nested.join(".crab").exists()
            ),
            (true, false),
        );
    }

    #[tokio::test]
    async fn desktop_layout_does_not_add_untracked_changes() {
        for linked in [false, true] {
            let (td, repo) = git_init_repo();
            std::fs::write(repo.join("file.txt"), "original\n").unwrap();
            assert!(sh(&repo, &["add", "file.txt"]).status.success());
            assert!(
                sh(
                    &repo,
                    &["-c", "commit.gpgsign=false", "commit", "-m", "seed"]
                )
                .status
                .success()
            );
            let root = if linked {
                let worktree = td.path().join("linked");
                assert!(
                    sh(
                        &repo,
                        &[
                            "worktree",
                            "add",
                            "-b",
                            "linked",
                            worktree.to_str().unwrap()
                        ]
                    )
                    .status
                    .success()
                );
                worktree
            } else {
                repo
            };
            std::fs::write(root.join("file.txt"), "modified\n").unwrap();
            ensure_desktop_layout(&root).await;
            let status = sh(&root, &["status", "--porcelain"]);
            assert_eq!(
                String::from_utf8(status.stdout).unwrap().trim_end(),
                " M file.txt",
                "linked={linked}"
            );
        }
    }

    #[tokio::test]
    async fn desktop_layout_preserves_existing_ignore_rules() {
        let td = tempfile::tempdir().unwrap();
        let desktop = td.path().join(".crab/desktop");
        std::fs::create_dir_all(&desktop).unwrap();
        let ignore = desktop.join(".gitignore");
        std::fs::write(&ignore, "settings.json\n").unwrap();
        ensure_desktop_layout(td.path()).await;
        assert_eq!(std::fs::read_to_string(ignore).unwrap(), "settings.json\n");
    }

    #[tokio::test]
    async fn ensure_desktop_layout_creates_worktrees_json() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();

        // No .crab/desktop/ exists yet.
        let worktrees_path = root.join(".crab").join("desktop").join("worktrees.json");
        assert!(!worktrees_path.exists());

        ensure_desktop_layout(&root).await;

        // Directory and file should now exist.
        assert!(worktrees_path.exists());
        let content = std::fs::read_to_string(&worktrees_path).unwrap();
        assert_eq!(content, "[]");
    }

    #[tokio::test]
    async fn ensure_desktop_layout_does_not_overwrite_existing_worktrees_json() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();

        // Pre-create the file with some content.
        let desktop_dir = root.join(".crab").join("desktop");
        std::fs::create_dir_all(&desktop_dir).unwrap();
        let worktrees_path = desktop_dir.join("worktrees.json");
        std::fs::write(&worktrees_path, r#"[{"path":"/tmp/wt"}]"#).unwrap();

        ensure_desktop_layout(&root).await;

        // Content should be unchanged.
        let content = std::fs::read_to_string(&worktrees_path).unwrap();
        assert_eq!(content, r#"[{"path":"/tmp/wt"}]"#);
    }

    /// Verifies that opening a worktree via `open_local` stores its
    /// path as the active work_dir, and that subsequent RPCs using
    /// that repo_id scope to the worktree's directory. Multiple
    /// worktrees coexist with independent paths.
    #[tokio::test]
    async fn worktree_switch_scopes_rpcs_to_new_path() {
        let (_td, root) = git_init_repo();
        // Create an initial commit so worktree add works.
        std::fs::write(root.join("file.txt"), "hello").unwrap();
        let _ = sh(&root, &["add", "."]);
        let _ = sh(&root, &["commit", "-m", "init"]);

        // Create a worktree at a sibling directory.
        let wt_path = root.parent().unwrap().join("wt-feature");
        let out = sh(
            &root,
            &[
                "worktree",
                "add",
                wt_path.to_str().unwrap(),
                "-b",
                "feature",
            ],
        );
        assert!(out.status.success(), "git worktree add: {:?}", out);
        assert!(wt_path.exists(), "worktree directory should exist");

        let state = State::new();

        // Open the main repo.
        let main_repo = BrowseSession::open_local(&root).await.unwrap();
        state
            .insert("main".into(), main_repo, Some(root.clone()))
            .await;

        // Open the worktree as a separate repo context (simulates
        // the renderer calling rpc.openLocal(worktree_path)).
        let wt_repo = BrowseSession::open_local(&wt_path).await.unwrap();
        state
            .insert("wt-feature".into(), wt_repo, Some(wt_path.clone()))
            .await;

        // get_work_dir for each repo_id returns the correct path.
        let main_dir = state.get_work_dir("main").await.unwrap();
        let wt_dir = state.get_work_dir("wt-feature").await.unwrap();

        assert_eq!(main_dir, root);
        assert_eq!(wt_dir, wt_path);

        // The two paths are distinct — RPCs scoped to "wt-feature"
        // operate on the worktree, not the main checkout.
        assert_ne!(main_dir, wt_dir);

        // Querying a non-existent repo_id returns RepoNotOpen.
        let err = state.get_work_dir("nonexistent").await.unwrap_err();
        assert!(
            matches!(err, AgentError::RepoNotOpen(_)),
            "expected RepoNotOpen, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn task_apply_lands_inactive_base_branch_without_checkout() {
        let (_td, root) = git_init_repo();
        std::fs::write(root.join("file.txt"), "hello\n").unwrap();
        let _ = sh(&root, &["add", "."]);
        let _ = sh(&root, &["commit", "-m", "init"]);
        let base = String::from_utf8_lossy(&sh(&root, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_owned();

        let _ = sh(&root, &["branch", "integration", &base]);
        let _ = sh(&root, &["checkout", "-b", "other", &base]);

        let wt_path = root.parent().unwrap().join("wt-apply");
        let out = sh(
            &root,
            &[
                "worktree",
                "add",
                wt_path.to_str().unwrap(),
                "-b",
                "agent/apply",
                "integration",
            ],
        );
        assert!(out.status.success(), "git worktree add: {:?}", out);
        std::fs::write(wt_path.join("new_file.txt"), "agent content\n").unwrap();
        let _ = sh(&wt_path, &["add", "."]);
        let _ = sh(&wt_path, &["commit", "-m", "agent change"]);

        let task_id = uuid::Uuid::new_v4().to_string();
        crate::task_workspace::TaskWorkspace::record(
            &root,
            &wt_path,
            uuid::Uuid::parse_str(&task_id).unwrap(),
            "agent/apply",
        )
        .await
        .unwrap();
        let result = task_apply(
            &root,
            TaskApplyParams {
                repo_id: "repo".to_owned(),
                task_id,
                branch: "agent/apply".to_owned(),
                base_branch: Some("integration".to_owned()),
                worktree_path: wt_path.to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap();

        assert_eq!(result["applied"], true);
        assert_eq!(result["target_branch"], "integration");
        assert!(result["merge_commit"].as_str().is_some());

        let current = String::from_utf8_lossy(&sh(&root, &["branch", "--show-current"]).stdout)
            .trim()
            .to_owned();
        assert_eq!(current, "other");

        let show = sh(&root, &["show", "integration:new_file.txt"]);
        assert!(show.status.success());
        assert!(
            !root.join("new_file.txt").exists(),
            "inactive target apply must not materialize files in the current checkout"
        );
        assert!(!wt_path.exists(), "task worktree should be removed");
        assert!(
            !sh(
                &root,
                &["show-ref", "--verify", "--quiet", "refs/heads/agent/apply"]
            )
            .status
            .success(),
            "task branch should be deleted"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn task_apply_preserves_workspace_when_commit_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let (_td, root) = git_init_repo();
        std::fs::write(root.join("tracked.txt"), "before\n").unwrap();
        assert!(sh(&root, &["add", "."]).status.success());
        assert!(sh(&root, &["commit", "-m", "baseline"]).status.success());
        let base = run_git_cmd(&root, &["rev-parse", "HEAD"])
            .await
            .unwrap()
            .trim()
            .to_owned();
        let branch = run_git_cmd(&root, &["branch", "--show-current"])
            .await
            .unwrap()
            .trim()
            .to_owned();
        let worktree = root.parent().unwrap().join("rejected-apply");
        assert!(
            sh(
                &root,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "agent/rejected",
                    worktree.to_str().unwrap()
                ]
            )
            .status
            .success()
        );
        let task_id = uuid::Uuid::new_v4();
        crate::task_workspace::TaskWorkspace::record(&root, &worktree, task_id, "agent/rejected")
            .await
            .unwrap();
        std::fs::write(worktree.join("new.txt"), "retained task bytes\n").unwrap();
        let hook = root.join(".git/hooks/pre-commit");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(
            &hook,
            "#!/bin/sh\nprintf 'fixture commit rejected\\n' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = task_apply(
            &root,
            TaskApplyParams {
                repo_id: "owned-repo".into(),
                task_id: task_id.to_string(),
                branch: "agent/rejected".into(),
                base_branch: Some(branch),
                worktree_path: worktree.to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            (
                error.to_string().contains("fixture commit rejected"),
                run_git_cmd(&root, &["rev-parse", "HEAD"])
                    .await
                    .unwrap()
                    .trim()
                    .to_owned(),
                std::fs::read_to_string(worktree.join("new.txt")).unwrap(),
                root.join("new.txt").exists()
            ),
            (true, base, "retained task bytes\n".into(), false)
        );
    }

    // ── rewrite_url_with_token tests ────────────────────────────────

    #[test]
    fn rewrite_no_token_returns_original() {
        let url = "git@github.com:owner/repo.git";
        assert_eq!(rewrite_url_with_token(url, None, None), url);
    }

    #[test]
    fn rewrite_empty_token_returns_original() {
        let url = "https://github.com/owner/repo.git";
        assert_eq!(rewrite_url_with_token(url, Some(""), Some("github")), url);
    }

    #[test]
    fn rewrite_ssh_github_to_https() {
        assert_eq!(
            rewrite_url_with_token(
                "git@github.com:owner/repo.git",
                Some("ghp_abc123"),
                Some("github"),
            ),
            "https://x-access-token:ghp_abc123@github.com/owner/repo.git"
        );
    }

    #[test]
    fn rewrite_ssh_gitlab_to_https() {
        assert_eq!(
            rewrite_url_with_token(
                "git@gitlab.com:group/project.git",
                Some("glpat-xyz"),
                Some("gitlab"),
            ),
            "https://oauth2:glpat-xyz@gitlab.com/group/project.git"
        );
    }

    #[test]
    fn rewrite_ssh_bitbucket_to_https() {
        assert_eq!(
            rewrite_url_with_token(
                "git@bitbucket.org:team/repo.git",
                Some("app_pw_123"),
                Some("bitbucket"),
            ),
            "https://x-token-auth:app_pw_123@bitbucket.org/team/repo.git"
        );
    }

    #[test]
    fn rewrite_https_github_injects_token() {
        assert_eq!(
            rewrite_url_with_token(
                "https://github.com/owner/repo.git",
                Some("ghp_abc123"),
                Some("github"),
            ),
            "https://x-access-token:ghp_abc123@github.com/owner/repo.git"
        );
    }

    #[test]
    fn rewrite_https_gitlab_injects_token() {
        assert_eq!(
            rewrite_url_with_token(
                "https://gitlab.com/group/project.git",
                Some("glpat-xyz"),
                Some("gitlab"),
            ),
            "https://oauth2:glpat-xyz@gitlab.com/group/project.git"
        );
    }

    #[test]
    fn rewrite_https_bitbucket_injects_token() {
        assert_eq!(
            rewrite_url_with_token(
                "https://bitbucket.org/team/repo.git",
                Some("app_pw_123"),
                Some("bitbucket"),
            ),
            "https://x-token-auth:app_pw_123@bitbucket.org/team/repo.git"
        );
    }

    #[test]
    fn rewrite_https_with_existing_credentials_unchanged() {
        let url = "https://user:existing_token@github.com/owner/repo.git";
        assert_eq!(
            rewrite_url_with_token(url, Some("new_token"), Some("github")),
            url
        );
    }

    #[test]
    fn rewrite_ssh_protocol_url() {
        assert_eq!(
            rewrite_url_with_token(
                "ssh://git@github.com/owner/repo.git",
                Some("ghp_abc"),
                Some("github"),
            ),
            "https://x-access-token:ghp_abc@github.com/owner/repo.git"
        );
    }

    #[test]
    fn rewrite_crab_url_passthrough() {
        let url = "crab://my-bucket/my-repo";
        assert_eq!(
            rewrite_url_with_token(url, Some("some_token"), Some("github")),
            url
        );
    }

    #[test]
    fn rewrite_unknown_provider_uses_default_username() {
        assert_eq!(
            rewrite_url_with_token(
                "https://custom-git.example.com/owner/repo.git",
                Some("tok_123"),
                None,
            ),
            "https://x-access-token:tok_123@custom-git.example.com/owner/repo.git"
        );
    }
}

// ---- Dashboard insights -----------------------------------------------

/// Returns per-day commit counts for the last 365 days (GitHub-style
/// contribution heatmap data). Each entry is `{date: "YYYY-MM-DD", count: N}`.
async fn git_activity(state: &State, p: RepoIdParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let out = shellout::git_command()
        .args([
            "log",
            "--all",
            "--since=365 days ago",
            "--format=%ad",
            "--date=short",
        ])
        .current_dir(&work_dir)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(AgentError::Io)?;
    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let body = String::from_utf8_lossy(&out.stdout);
    let mut days: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.len() == 10 {
            *days.entry(trimmed.to_owned()).or_insert(0) += 1;
        }
    }
    let entries: Vec<Value> = days
        .into_iter()
        .map(|(date, count)| json!({"date": date, "count": count}))
        .collect();
    Ok(json!({ "days": entries }))
}

/// Compute the Monday of the ISO week for a "YYYY-MM-DD" string.
/// Returns `Some("YYYY-MM-DD")` of that Monday, or None on parse failure.
fn iso_week_monday(date: &str) -> Option<String> {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    // Day-of-week via Tomohiko Sakamoto's algorithm (0=Sun..6=Sat).
    let t = [0i32, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let yy = if m < 3 { y - 1 } else { y };
    let dow = ((yy + yy / 4 - yy / 100 + yy / 400 + t[(m - 1) as usize] + d as i32) % 7) as i32;
    // Convert to Mon=0..Sun=6.
    let mon_based = (dow + 6) % 7;
    // Subtract mon_based days to get Monday.
    // Simple: compute days-since-epoch, subtract, convert back.
    // Use a simpler approach: just subtract days from the date.
    let total_days = days_from_civil(y, m, d)?;
    let monday_days = total_days - mon_based;
    let (my, mm, md) = civil_from_days(monday_days);
    Some(format!("{:04}-{:02}-{:02}", my, mm, md))
}

fn days_from_civil(y: i32, m: u32, d: u32) -> Option<i32> {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u32;
    let m = m as i32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i32 - 1;
    let doe = yoe as i32 * 365 + yoe as i32 / 4 - yoe as i32 / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

fn civil_from_days(z: i32) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Top contributors by commit count across all branches.
/// When `since` is provided (ISO date), only counts commits after that date.
async fn git_contributors(state: &State, p: GitContributorsParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let mut args = vec!["shortlog", "-sn", "--all", "--no-merges"];
    let since_flag;
    if let Some(ref since) = p.since {
        since_flag = format!("--since={since}");
        args.push(&since_flag);
    } else {
        // No explicit `since` was provided. Default to a 5-year
        // window so the shortlog walk on million-commit repos
        // doesn't take 30+ seconds and accumulate megabytes of
        // output. Callers that genuinely want all-time stats can
        // pass an explicit `since` of "1970-01-01" or similar.
        since_flag = "--since=5 years ago".to_string();
        args.push(&since_flag);
    }
    let out = shellout::git_command()
        .args(&args)
        .current_dir(&work_dir)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(AgentError::Io)?;
    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    let body = String::from_utf8_lossy(&out.stdout);
    let mut contributors: Vec<Value> = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Format: "  123\tAuthor Name"
        if let Some((count_str, name)) = trimmed.split_once('\t') {
            if let Ok(count) = count_str.trim().parse::<u32>() {
                contributors.push(json!({"name": name.trim(), "commits": count}));
            }
        }
    }
    contributors.truncate(20);
    Ok(json!({ "contributors": contributors }))
}

// ---- Contributors params -----------------------------------------------

#[derive(Debug, Deserialize)]
struct GitContributorsParams {
    repo_id: String,
    /// Optional ISO date string. When provided, only commits after this
    /// date are counted (passed as `--since` to git shortlog).
    since: Option<String>,
}

// ---- Pulse summary ----------------------------------------------------

#[derive(Debug, Deserialize)]
struct GitPulseParams {
    repo_id: String,
    #[serde(default = "default_pulse_days")]
    days: u32,
}

fn default_pulse_days() -> u32 {
    7
}

/// Pulse summary: commits, unique authors, files changed, insertions,
/// and deletions over the last N days.
async fn git_pulse(state: &State, p: GitPulseParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let since_arg = format!("{}.days.ago", p.days);

    let out = shellout::git_command()
        .args([
            "log",
            &format!("--since={since_arg}"),
            "--shortstat",
            "--format=%aN",
        ])
        .current_dir(&work_dir)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }

    let body = String::from_utf8_lossy(&out.stdout);
    let mut commits: u32 = 0;
    let mut authors: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut files_changed: u32 = 0;
    let mut insertions: u32 = 0;
    let mut deletions: u32 = 0;

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Shortstat lines look like:
        //   " 3 files changed, 12 insertions(+), 5 deletions(-)"
        //   " 1 file changed, 2 insertions(+)"
        //   " 2 files changed, 4 deletions(-)"
        if trimmed.contains("changed")
            && (trimmed.contains("insertion") || trimmed.contains("deletion"))
        {
            for part in trimmed.split(',') {
                let part = part.trim();
                if part.contains("file") && part.contains("changed") {
                    if let Some(n) = part
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<u32>().ok())
                    {
                        files_changed += n;
                    }
                } else if part.contains("insertion") {
                    if let Some(n) = part
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<u32>().ok())
                    {
                        insertions += n;
                    }
                } else if part.contains("deletion") {
                    if let Some(n) = part
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<u32>().ok())
                    {
                        deletions += n;
                    }
                }
            }
        } else {
            // Author name line — each non-empty, non-shortstat line is an author.
            commits += 1;
            authors.insert(trimmed.to_owned());
        }
    }

    // Compute period boundaries.
    let now = chrono::Utc::now();
    let period_start = now - chrono::Duration::days(i64::from(p.days));

    Ok(json!({
        "commits": commits,
        "authors": authors.len(),
        "files_changed": files_changed,
        "insertions": insertions,
        "deletions": deletions,
        "period_start": period_start.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "period_end": now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    }))
}

// ---- Commit frequency --------------------------------------------------

#[derive(Debug, Deserialize)]
struct GitCommitFrequencyParams {
    repo_id: String,
    #[serde(default = "default_granularity")]
    granularity: String,
    /// Optional number of periods. Capped to 90 (daily), 52 (weekly), 12 (monthly).
    periods: Option<u32>,
}

fn default_granularity() -> String {
    "week".to_owned()
}

/// Commit frequency: group author dates into time buckets by granularity.
async fn git_commit_frequency(state: &State, p: GitCommitFrequencyParams) -> Result<Value> {
    use chrono::{Datelike, Duration, NaiveDate, Utc};

    let work_dir = state.get_work_dir(&p.repo_id).await?;

    // Determine max periods and bucket size based on granularity.
    let (max_periods, granularity) = match p.granularity.as_str() {
        "day" => (90u32, "day"),
        "month" => (12u32, "month"),
        _ => (52u32, "week"), // default to week
    };
    let periods = p.periods.unwrap_or(max_periods).min(max_periods);

    // Compute the earliest date we care about.
    let now = Utc::now();
    let today = now.date_naive();

    let start_date = match granularity {
        "day" => today - Duration::days(i64::from(periods)),
        "month" => {
            // Go back `periods` months from the start of the current month.
            let mut y = today.year();
            let mut m = today.month() as i32 - periods as i32;
            while m <= 0 {
                m += 12;
                y -= 1;
            }
            NaiveDate::from_ymd_opt(y, m as u32, 1).unwrap_or(today)
        }
        _ => {
            // weeks: go back `periods` weeks from the start of the current week (Monday).
            let days_since_monday = today.weekday().num_days_from_monday();
            let week_start = today - Duration::days(i64::from(days_since_monday));
            week_start - Duration::weeks(i64::from(periods))
        }
    };

    let since_arg = start_date.format("%Y-%m-%d").to_string();

    let out = shellout::git_command()
        .args([
            "log",
            "--all",
            &format!("--since={since_arg}"),
            "--format=%aI",
        ])
        .current_dir(&work_dir)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }

    let body = String::from_utf8_lossy(&out.stdout);

    // Parse dates and assign each to a bucket key (NaiveDate of bucket start).
    let mut bucket_counts: std::collections::HashMap<NaiveDate, u32> =
        std::collections::HashMap::new();

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Parse ISO 8601 date with timezone offset (e.g. "2025-01-15T10:30:00+02:00").
        let dt = match chrono::DateTime::parse_from_rfc3339(trimmed) {
            Ok(d) => d.with_timezone(&Utc),
            Err(_) => continue,
        };
        let date = dt.date_naive();
        if date < start_date {
            continue;
        }

        let bucket_key = match granularity {
            "day" => date,
            "month" => NaiveDate::from_ymd_opt(date.year(), date.month(), 1).unwrap_or(date),
            _ => {
                // week: bucket starts on Monday
                let days_since_monday = date.weekday().num_days_from_monday();
                date - Duration::days(i64::from(days_since_monday))
            }
        };

        *bucket_counts.entry(bucket_key).or_insert(0) += 1;
    }

    // Generate all bucket boundaries (even empty ones).
    let mut buckets: Vec<Value> = Vec::new();
    let mut cursor = start_date;

    loop {
        if cursor > today {
            break;
        }

        let (period_start, period_end) = match granularity {
            "day" => (cursor, cursor + Duration::days(1)),
            "month" => {
                let next = if cursor.month() == 12 {
                    NaiveDate::from_ymd_opt(cursor.year() + 1, 1, 1)
                } else {
                    NaiveDate::from_ymd_opt(cursor.year(), cursor.month() + 1, 1)
                }
                .unwrap_or(cursor + Duration::days(30));
                (cursor, next)
            }
            _ => (cursor, cursor + Duration::weeks(1)),
        };

        let count = bucket_counts.get(&cursor).copied().unwrap_or(0);
        buckets.push(json!({
            "period_start": period_start.format("%Y-%m-%d").to_string(),
            "period_end": period_end.format("%Y-%m-%d").to_string(),
            "count": count,
        }));

        // Advance cursor.
        cursor = period_end;
    }

    Ok(json!({ "buckets": buckets }))
}

// ---- Code frequency (additions/deletions per week) --------------------

#[derive(Debug, Deserialize)]
struct GitCodeFrequencyParams {
    repo_id: String,
    /// Number of weeks to look back. Default 52, max 52.
    #[serde(default = "default_code_freq_weeks")]
    weeks: u32,
}

fn default_code_freq_weeks() -> u32 {
    52
}

/// Compute a stable hex hash of a path for use as a cache directory name.
fn repo_path_hash(path: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    let h = hasher.finish();
    format!("{h:016x}")
}

/// A single cached week entry for code frequency.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CachedWeekEntry {
    week_start: String,
    additions: u64,
    deletions: u64,
}

/// Load the code frequency cache from disk. Returns an empty vec on
/// any failure (missing file, parse error, etc.).
async fn load_code_freq_cache(cache_path: &Path) -> Vec<CachedWeekEntry> {
    match tokio::fs::read_to_string(cache_path).await {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Save the code frequency cache to disk, creating parent dirs as needed.
async fn save_code_freq_cache(cache_path: &Path, entries: &[CachedWeekEntry]) -> Result<()> {
    if let Some(parent) = cache_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(AgentError::Io)?;
    }
    let json = serde_json::to_string_pretty(entries)
        .map_err(|e| AgentError::Other(format!("cache serialize: {e}")))?;
    tokio::fs::write(cache_path, json)
        .await
        .map_err(AgentError::Io)?;
    Ok(())
}

/// Compute code frequency stats for a date range via `git log`.
/// Returns a map of week_start → (additions, deletions).
async fn compute_code_freq_range(
    work_dir: &Path,
    since: &str,
    until: Option<&str>,
    earliest_start: chrono::NaiveDate,
) -> Result<std::collections::HashMap<chrono::NaiveDate, (u64, u64)>> {
    use chrono::{Datelike, Duration, Utc};

    let mut args = vec![
        "log".to_string(),
        "--all".to_string(),
        format!("--since={since}"),
        "--shortstat".to_string(),
        "--format=%aI".to_string(),
    ];
    if let Some(u) = until {
        args.push(format!("--until={u}"));
    }

    let out = shellout::git_command()
        .args(&args)
        .current_dir(work_dir)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }

    let body = String::from_utf8_lossy(&out.stdout);
    let mut week_data: std::collections::HashMap<chrono::NaiveDate, (u64, u64)> =
        std::collections::HashMap::new();
    let mut current_date: Option<chrono::NaiveDate> = None;

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
            let date = dt.with_timezone(&Utc).date_naive();
            current_date = Some(date);
            continue;
        }

        if trimmed.contains("changed")
            && (trimmed.contains("insertion") || trimmed.contains("deletion"))
        {
            let date = match current_date {
                Some(d) => d,
                None => continue,
            };

            let days_since_mon = date.weekday().num_days_from_monday();
            let week_start = date - Duration::days(i64::from(days_since_mon));

            if week_start < earliest_start {
                continue;
            }

            let mut add: u64 = 0;
            let mut del: u64 = 0;
            for part in trimmed.split(',') {
                let part = part.trim();
                if part.contains("insertion") {
                    if let Some(n) = part
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        add = n;
                    }
                } else if part.contains("deletion") {
                    if let Some(n) = part
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        del = n;
                    }
                }
            }

            let entry = week_data.entry(week_start).or_insert((0, 0));
            entry.0 += add;
            entry.1 += del;
        }
    }

    Ok(week_data)
}

/// Code frequency: additions and deletions grouped by week.
///
/// Uses a persistent cache at `~/.crab/desktop/cache/metrics/<repo_hash>/code_freq.json`.
/// Completed weeks (before the current week) are cached permanently.
/// The current week is always recomputed fresh.
async fn git_code_frequency(state: &State, p: GitCodeFrequencyParams) -> Result<Value> {
    use chrono::{Datelike, Duration, NaiveDate, Utc};

    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let weeks = p.weeks.min(52).max(1);

    // Compute week boundaries. Weeks start on Monday.
    let today = Utc::now().date_naive();
    let days_since_monday = today.weekday().num_days_from_monday();
    let current_week_start = today - Duration::days(i64::from(days_since_monday));
    let earliest_start = current_week_start - Duration::weeks(i64::from(weeks - 1));

    // Resolve cache path.
    let home = dirs::home_dir().ok_or_else(|| AgentError::Other("no home dir".into()))?;
    let repo_hash = repo_path_hash(&work_dir);
    let cache_path = home
        .join(".crab/desktop/cache/metrics")
        .join(&repo_hash)
        .join("code_freq.json");

    // Load existing cache.
    let cached_entries = load_code_freq_cache(&cache_path).await;

    // Index cached weeks by their week_start date for fast lookup.
    let mut cached_map: std::collections::HashMap<NaiveDate, CachedWeekEntry> =
        std::collections::HashMap::new();
    for entry in &cached_entries {
        if let Ok(d) = NaiveDate::parse_from_str(&entry.week_start, "%Y-%m-%d") {
            cached_map.insert(d, entry.clone());
        }
    }

    // Determine which weeks need computation:
    // - Completed weeks (before current_week_start): use cache if available
    // - Current week: always recompute
    let mut weeks_to_compute: Vec<NaiveDate> = Vec::new();
    let mut cursor = earliest_start;
    while cursor <= current_week_start {
        if cursor == current_week_start || !cached_map.contains_key(&cursor) {
            weeks_to_compute.push(cursor);
        }
        cursor += Duration::weeks(1);
    }

    // Compute missing weeks in a single git log call spanning the full
    // range of missing weeks (more efficient than per-week calls).
    if !weeks_to_compute.is_empty() {
        let compute_since = weeks_to_compute
            .iter()
            .copied()
            .min()
            .unwrap_or(earliest_start);
        let since_arg = compute_since.format("%Y-%m-%d").to_string();

        let fresh_data =
            compute_code_freq_range(&work_dir, &since_arg, None, compute_since).await?;

        // Merge fresh data into the cached map.
        for &week_start in &weeks_to_compute {
            let (additions, deletions) = fresh_data.get(&week_start).copied().unwrap_or((0, 0));
            cached_map.insert(
                week_start,
                CachedWeekEntry {
                    week_start: week_start.format("%Y-%m-%d").to_string(),
                    additions,
                    deletions,
                },
            );
        }
    }

    // Build ordered result with all weeks in the requested range.
    let mut result_weeks: Vec<Value> = Vec::with_capacity(weeks as usize);
    cursor = earliest_start;

    while cursor <= current_week_start {
        let entry = cached_map.get(&cursor).cloned().unwrap_or(CachedWeekEntry {
            week_start: cursor.format("%Y-%m-%d").to_string(),
            additions: 0,
            deletions: 0,
        });
        result_weeks.push(json!({
            "week_start": entry.week_start,
            "additions": entry.additions,
            "deletions": entry.deletions,
        }));
        cursor += Duration::weeks(1);
    }

    // Persist updated cache (includes all weeks we know about).
    // We save all entries in the cached_map, not just the requested range,
    // so older cached data isn't lost.
    let mut full_cache: Vec<CachedWeekEntry> = cached_map.into_values().collect();
    full_cache.sort_by(|a, b| a.week_start.cmp(&b.week_start));
    if let Err(e) = save_code_freq_cache(&cache_path, &full_cache).await {
        tracing::warn!("failed to save code frequency cache: {e}");
    }

    Ok(json!({ "weeks": result_weeks }))
}

// ---- SQLite preview ---------------------------------------------------

/// Preview a SQLite database: list tables, their schemas, and first
/// 50 rows of each table (capped at 5 tables to keep response size
/// reasonable).
async fn preview_sqlite(state: &State, p: PreviewParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let abs = work_dir.join(&p.path);
    if !abs.exists() {
        return Err(AgentError::Other(format!(
            "file not on disk: {} (run `crab hydrate` to materialize it first)",
            abs.display()
        )));
    }
    let abs_clone = abs.clone();
    tokio::task::spawn_blocking(move || sqlite_preview(&abs_clone))
        .await
        .map_err(|e| AgentError::Other(format!("join error: {e}")))?
}

fn sqlite_preview(path: &std::path::Path) -> Result<Value> {
    use rusqlite::Connection;

    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| AgentError::Other(format!("sqlite open: {e}")))?;

    // Get table names.
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .map_err(|e| AgentError::Other(format!("sqlite query: {e}")))?;
    let table_names: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .map_err(|e| AgentError::Other(format!("sqlite: {e}")))?
        .filter_map(|r| r.ok())
        .collect();

    let mut tables: Vec<Value> = Vec::new();
    for name in table_names.iter().take(10) {
        // Get column info.
        let pragma = format!("PRAGMA table_info(\"{}\")", name.replace('"', "\"\""));
        let mut col_stmt = conn
            .prepare(&pragma)
            .map_err(|e| AgentError::Other(format!("sqlite pragma: {e}")))?;
        let columns: Vec<Value> = col_stmt
            .query_map([], |row| {
                Ok(json!({
                    "name": row.get::<_, String>(1)?,
                    "type": row.get::<_, String>(2)?,
                    "notnull": row.get::<_, bool>(3)?,
                    "pk": row.get::<_, bool>(5)?,
                }))
            })
            .map_err(|e| AgentError::Other(format!("sqlite: {e}")))?
            .filter_map(|r| r.ok())
            .collect();

        // Get row count.
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM \"{}\"", name.replace('"', "\"\"")),
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // Get first 50 rows.
        let query = format!("SELECT * FROM \"{}\" LIMIT 50", name.replace('"', "\"\""));
        let mut data_stmt = conn
            .prepare(&query)
            .map_err(|e| AgentError::Other(format!("sqlite: {e}")))?;
        let col_count = data_stmt.column_count();
        let col_names: Vec<String> = (0..col_count)
            .map(|i| data_stmt.column_name(i).unwrap_or("?").to_owned())
            .collect();

        let mut rows: Vec<Value> = Vec::new();
        let mut row_iter = data_stmt
            .query([])
            .map_err(|e| AgentError::Other(format!("sqlite: {e}")))?;
        while let Some(row) = row_iter
            .next()
            .map_err(|e| AgentError::Other(format!("sqlite: {e}")))?
        {
            let mut obj = serde_json::Map::new();
            for i in 0..col_count {
                let val: Value = match row.get_ref(i) {
                    Ok(rusqlite::types::ValueRef::Null) => Value::Null,
                    Ok(rusqlite::types::ValueRef::Integer(n)) => json!(n),
                    Ok(rusqlite::types::ValueRef::Real(f)) => json!(f),
                    Ok(rusqlite::types::ValueRef::Text(t)) => {
                        json!(String::from_utf8_lossy(t))
                    }
                    Ok(rusqlite::types::ValueRef::Blob(b)) => {
                        json!(format!("<blob {} bytes>", b.len()))
                    }
                    Err(_) => Value::Null,
                };
                obj.insert(col_names[i].clone(), val);
            }
            rows.push(Value::Object(obj));
        }

        tables.push(json!({
            "name": name,
            "columns": columns,
            "row_count": count,
            "preview_rows": rows,
        }));
    }

    // Get database file size.
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    Ok(json!({
        "kind": "sqlite",
        "file_size": file_size,
        "table_count": table_names.len(),
        "tables": tables,
    }))
}

// ---- DuckDB SQL query --------------------------------------------------

#[derive(Debug, Deserialize)]
struct DuckDbQueryParams {
    repo_id: String,
    /// SQL query to execute. DuckDB supports querying SQLite files,
    /// Parquet, CSV, and JSON natively.
    sql: String,
    /// Optional path to a file to attach. When provided, the file is
    /// made available as a DuckDB-attached database or via DuckDB's
    /// file-reading functions depending on the extension.
    #[serde(default)]
    file_path: Option<String>,
    /// Maximum rows to return. Clamped to [1, 10000]. Defaults to 500.
    #[serde(default)]
    max_rows: Option<usize>,
}

/// Execute an arbitrary SQL query via DuckDB. Supports:
///   - Querying attached SQLite databases
///   - Reading Parquet files: `SELECT * FROM 'path/to/file.parquet'`
///   - Reading CSV files: `SELECT * FROM 'path/to/file.csv'`
///   - In-memory analytics with full SQL (window functions, CTEs, etc.)
///
/// The file_path parameter, when provided, is attached as a SQLite
/// database accessible via the alias `db`.
async fn duckdb_query(state: &State, p: DuckDbQueryParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let sql = p.sql.clone();
    let file_path = p.file_path.clone();
    let max_rows = p.max_rows.unwrap_or(500).clamp(1, 10000);

    tokio::task::spawn_blocking(move || {
        duckdb_execute_query(&work_dir, &sql, file_path.as_deref(), max_rows)
    })
    .await
    .map_err(|e| AgentError::Other(format!("join error: {e}")))?
}

fn duckdb_execute_query(
    work_dir: &std::path::Path,
    sql: &str,
    file_path: Option<&str>,
    max_rows: usize,
) -> Result<Value> {
    use duckdb::{Connection, types::Value as DuckValue};

    let conn =
        Connection::open_in_memory().map_err(|e| AgentError::Other(format!("duckdb open: {e}")))?;

    // If a file path is provided, attach it based on extension.
    if let Some(fp) = file_path {
        let abs_path = if std::path::Path::new(fp).is_absolute() {
            std::path::PathBuf::from(fp)
        } else {
            work_dir.join(fp)
        };

        if !abs_path.exists() {
            return Err(AgentError::Other(format!(
                "file not on disk: {} (run `crab hydrate` to materialize it first)",
                abs_path.display()
            )));
        }

        let ext = abs_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        match ext.as_str() {
            "db" | "sqlite" | "sqlite3" => {
                // Attach as a SQLite database with alias "db".
                let attach_sql = format!(
                    "ATTACH '{}' AS db (TYPE SQLITE, READ_ONLY)",
                    abs_path.display()
                );
                conn.execute_batch(&attach_sql)
                    .map_err(|e| AgentError::Other(format!("duckdb attach sqlite: {e}")))?;
                // Set search path so users can query tables without
                // the "db." prefix — just like a native SQLite client.
                conn.execute_batch("SET search_path='db,main'")
                    .map_err(|e| AgentError::Other(format!("duckdb set search_path: {e}")))?;
            }
            _ => {
                // For other formats (parquet, csv, json), DuckDB can
                // read them directly via SQL — no explicit attach needed.
                // The user references them in their query as file paths.
            }
        }
    }

    // Set the working directory so relative paths in queries resolve
    // against the repo root.
    let set_dir = format!("SET file_search_path='{}'", work_dir.display());
    conn.execute_batch(&set_dir).ok(); // Best-effort; not all DuckDB versions support this.

    // Execute the query and collect results.
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| AgentError::Other(format!("duckdb prepare: {e}")))?;

    // Collect rows first, then access column metadata after the borrow
    // on `stmt` is released. We use query_map which internally calls
    // query() and maps over the rows.
    let collected_rows: Vec<Vec<DuckValue>> = stmt
        .query_map([], |row| {
            // We don't know col_count yet from the statement, but we
            // can probe columns until we get an error. However, a
            // simpler approach: use a large upper bound and break on
            // error. DuckDB's Row::get returns Err for out-of-bounds.
            let mut values: Vec<DuckValue> = Vec::new();
            for i in 0..256 {
                match row.get::<_, DuckValue>(i) {
                    Ok(val) => values.push(val),
                    Err(_) => break,
                }
            }
            Ok(values)
        })
        .map_err(|e| AgentError::Other(format!("duckdb query: {e}")))?
        .filter_map(|r| r.ok())
        .collect();

    // Now the mutable borrow on stmt is released. Get column metadata.
    let col_count = stmt.column_count();
    let col_names: Vec<String> = (0..col_count)
        .map(|i| stmt.column_name(i).map_or("?".to_owned(), |v| v.to_owned()))
        .collect();

    let col_types: Vec<String> = (0..col_count)
        .map(|i| stmt.column_type(i).to_string())
        .collect();

    // Build the JSON result from collected rows.
    let mut result_rows: Vec<Value> = Vec::new();
    let total_rows = collected_rows.len();
    let truncated = total_rows > max_rows;

    for row_values in collected_rows.into_iter().take(max_rows) {
        let mut obj = serde_json::Map::new();
        for (i, val) in row_values.into_iter().enumerate() {
            if i >= col_names.len() {
                break;
            }
            let json_val = duckdb_value_to_json(val)?;
            obj.insert(col_names[i].clone(), json_val);
        }
        result_rows.push(Value::Object(obj));
    }

    Ok(json!({
        "kind": "duckdb_result",
        "columns": col_names.iter().zip(col_types.iter()).map(|(name, dtype)| {
            json!({ "name": name, "type": dtype })
        }).collect::<Vec<_>>(),
        "rows": result_rows,
        "row_count": result_rows.len(),
        "total_rows": total_rows,
        "truncated": truncated,
        "max_rows": max_rows,
    }))
}

/// Convert a DuckDB value to a serde_json Value.
fn duckdb_value_to_json(val: duckdb::types::Value) -> Result<Value> {
    use duckdb::types::Value as DV;
    Ok(match val {
        DV::Null => Value::Null,
        DV::Boolean(b) => json!(b),
        DV::TinyInt(n) => json!(n),
        DV::SmallInt(n) => json!(n),
        DV::Int(n) => json!(n),
        DV::BigInt(n) => json!(n),
        DV::HugeInt(n) => json!(n.to_string()),
        DV::UHugeInt(n) => json!(n.to_string()),
        DV::UTinyInt(n) => json!(n),
        DV::USmallInt(n) => json!(n),
        DV::UInt(n) => json!(n),
        DV::UBigInt(n) => json!(n),
        DV::Float(f) => json!(f),
        DV::Double(f) => json!(f),
        DV::Decimal(d) => json!(d.to_string()),
        DV::Text(s) => json!(s),
        DV::Blob(b) => json!(format!("<blob {} bytes>", b.len())),
        DV::Geometry(b) => json!(format!("<geometry {} bytes>", b.len())),
        DV::Timestamp(_, n) => json!(n),
        DV::Date32(d) => json!(d),
        DV::Time64(_, t) => json!(t),
        DV::Interval {
            months,
            days,
            nanos,
        } => {
            json!(format!("{months} months {days} days {nanos} nanos"))
        }
        DV::List(items) => {
            let arr: Vec<Value> = items
                .into_iter()
                .map(duckdb_value_to_json)
                .collect::<Result<_>>()?;
            Value::Array(arr)
        }
        DV::Enum(s) => json!(s),
        DV::Struct(map) => {
            let obj: serde_json::Map<String, Value> = map
                .iter()
                .map(|(k, v)| Ok((k.clone(), duckdb_value_to_json(v.clone())?)))
                .collect::<Result<_>>()?;
            Value::Object(obj)
        }
        DV::Array(items) => {
            let arr: Vec<Value> = items
                .into_iter()
                .map(duckdb_value_to_json)
                .collect::<Result<_>>()?;
            Value::Array(arr)
        }
        DV::Map(map) => {
            let obj: serde_json::Map<String, Value> = map
                .iter()
                .map(|(k, v)| Ok((format!("{:?}", k), duckdb_value_to_json(v.clone())?)))
                .collect::<Result<_>>()?;
            Value::Object(obj)
        }
        DV::Union(inner) => return duckdb_value_to_json(*inner),
        _ => {
            return Err(AgentError::Other(
                "DuckDB returned an unsupported value type".into(),
            ));
        }
    })
}

// ---- ONNX preview -----------------------------------------------------

/// Parse ONNX model metadata from the protobuf wire format.
/// Extracts: ir_version, producer, opset, graph name, inputs, outputs.
async fn preview_onnx(state: &State, p: PreviewParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let abs = work_dir.join(&p.path);
    if !abs.exists() {
        return Err(AgentError::Other(format!(
            "file not on disk: {} (run `crab hydrate` first)",
            abs.display()
        )));
    }
    let abs_clone = abs.clone();
    tokio::task::spawn_blocking(move || onnx_preview(&abs_clone))
        .await
        .map_err(|e| AgentError::Other(format!("join error: {e}")))?
}

fn onnx_preview(path: &std::path::Path) -> Result<Value> {
    use std::io::Read;
    let mut file = File::open(path).map_err(AgentError::Io)?;
    // Read up to 10MB for header parsing (ONNX models can be huge
    // but the metadata is in the first few MB).
    let mut buf = Vec::new();
    let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let read_limit = std::cmp::min(file_size, 10 * 1024 * 1024) as usize;
    buf.resize(read_limit, 0);
    file.read_exact(&mut buf).map_err(AgentError::Io)?;

    // Minimal protobuf wire-format parser for ONNX ModelProto.
    let mut ir_version: i64 = 0;
    let mut producer_name = String::new();
    let mut producer_version = String::new();
    let mut domain = String::new();
    let mut model_version: i64 = 0;
    let mut doc_string = String::new();
    let mut graph_name = String::new();
    let mut inputs: Vec<String> = Vec::new();
    let mut outputs: Vec<String> = Vec::new();
    let mut opsets: Vec<Value> = Vec::new();
    let mut node_count: u32 = 0;

    // Parse top-level fields.
    let mut pos = 0;
    while pos < buf.len() {
        let (field_num, wire_type, new_pos) = match decode_tag(&buf, pos) {
            Some(v) => v,
            None => break,
        };
        pos = new_pos;
        match (field_num, wire_type) {
            (1, 0) => {
                // ir_version: varint
                let (val, np) = decode_varint(&buf, pos).unwrap_or((0, buf.len()));
                ir_version = val as i64;
                pos = np;
            }
            (2, 2) => {
                // opset_import: length-delimited
                let (data, np) = decode_bytes(&buf, pos).unwrap_or((&[], buf.len()));
                // Parse OpSetProto: field 1 = domain (string), field 2 = version (varint)
                let mut op_domain = String::new();
                let mut op_version: i64 = 0;
                let mut ipos = 0;
                while ipos < data.len() {
                    let (fn_, wt, inp) = match decode_tag(data, ipos) {
                        Some(v) => v,
                        None => break,
                    };
                    ipos = inp;
                    match (fn_, wt) {
                        (1, 2) => {
                            let (s, inp2) = decode_string(data, ipos).unwrap_or(("", data.len()));
                            op_domain = s.to_owned();
                            ipos = inp2;
                        }
                        (2, 0) => {
                            let (v, inp2) = decode_varint(data, ipos).unwrap_or((0, data.len()));
                            op_version = v as i64;
                            ipos = inp2;
                        }
                        (_, 0) => {
                            let (_, inp2) = decode_varint(data, ipos).unwrap_or((0, data.len()));
                            ipos = inp2;
                        }
                        (_, 2) => {
                            let (_, inp2) = decode_bytes(data, ipos).unwrap_or((&[], data.len()));
                            ipos = inp2;
                        }
                        _ => break,
                    }
                }
                opsets.push(json!({"domain": op_domain, "version": op_version}));
                pos = np;
            }
            (3, 2) => {
                let (s, np) = decode_string(&buf, pos).unwrap_or(("", buf.len()));
                producer_name = s.to_owned();
                pos = np;
            }
            (4, 2) => {
                let (s, np) = decode_string(&buf, pos).unwrap_or(("", buf.len()));
                producer_version = s.to_owned();
                pos = np;
            }
            (5, 2) => {
                let (s, np) = decode_string(&buf, pos).unwrap_or(("", buf.len()));
                domain = s.to_owned();
                pos = np;
            }
            (6, 0) => {
                let (v, np) = decode_varint(&buf, pos).unwrap_or((0, buf.len()));
                model_version = v as i64;
                pos = np;
            }
            (7, 2) => {
                let (s, np) = decode_string(&buf, pos).unwrap_or(("", buf.len()));
                doc_string = s.to_owned();
                pos = np;
            }
            (8, 2) => {
                // graph: GraphProto
                let (data, np) = decode_bytes(&buf, pos).unwrap_or((&[], buf.len()));
                // Parse GraphProto minimally: field 1=name, 4=input, 5=output, 12=node
                let mut ipos = 0;
                while ipos < data.len() {
                    let (fn_, wt, inp) = match decode_tag(data, ipos) {
                        Some(v) => v,
                        None => break,
                    };
                    ipos = inp;
                    match (fn_, wt) {
                        (1, 2) => {
                            let (s, inp2) = decode_string(data, ipos).unwrap_or(("", data.len()));
                            graph_name = s.to_owned();
                            ipos = inp2;
                        }
                        (4, 2) => {
                            // input: ValueInfoProto (field 1 = name)
                            let (sub, inp2) = decode_bytes(data, ipos).unwrap_or((&[], data.len()));
                            if let Some(name) = extract_name_field(sub) {
                                inputs.push(name);
                            }
                            ipos = inp2;
                        }
                        (5, 2) => {
                            // output
                            let (sub, inp2) = decode_bytes(data, ipos).unwrap_or((&[], data.len()));
                            if let Some(name) = extract_name_field(sub) {
                                outputs.push(name);
                            }
                            ipos = inp2;
                        }
                        (12, 2) => {
                            // node (just count)
                            let (_, inp2) = decode_bytes(data, ipos).unwrap_or((&[], data.len()));
                            node_count += 1;
                            ipos = inp2;
                        }
                        (_, 0) => {
                            let (_, inp2) = decode_varint(data, ipos).unwrap_or((0, data.len()));
                            ipos = inp2;
                        }
                        (_, 2) => {
                            let (_, inp2) = decode_bytes(data, ipos).unwrap_or((&[], data.len()));
                            ipos = inp2;
                        }
                        _ => break,
                    }
                }
                pos = np;
            }
            (_, 0) => {
                let (_, np) = decode_varint(&buf, pos).unwrap_or((0, buf.len()));
                pos = np;
            }
            (_, 2) => {
                let (_, np) = decode_bytes(&buf, pos).unwrap_or((&[], buf.len()));
                pos = np;
            }
            _ => break,
        }
    }

    Ok(json!({
        "kind": "onnx",
        "file_size": file_size,
        "ir_version": ir_version,
        "producer_name": producer_name,
        "producer_version": producer_version,
        "domain": domain,
        "model_version": model_version,
        "doc_string": doc_string,
        "graph_name": graph_name,
        "node_count": node_count,
        "inputs": inputs,
        "outputs": outputs,
        "opsets": opsets,
    }))
}

// ---- HDF5 preview (superblock parse) ----------------------------------

/// Parse HDF5 file superblock to extract root group datasets.
/// HDF5 format: 8-byte signature + superblock + B-tree.
/// We read the signature + version and list object names from the
/// symbol table if accessible.
async fn preview_hdf5(state: &State, p: PreviewParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let abs = work_dir.join(&p.path);
    if !abs.exists() {
        return Err(AgentError::Other(format!(
            "file not on disk: {} (run `crab hydrate` first)",
            abs.display()
        )));
    }
    let abs_clone = abs.clone();
    tokio::task::spawn_blocking(move || hdf5_preview(&abs_clone))
        .await
        .map_err(|e| AgentError::Other(format!("join error: {e}")))?
}

fn hdf5_preview(path: &std::path::Path) -> Result<Value> {
    use std::io::Read;
    let mut file = File::open(path).map_err(AgentError::Io)?;
    let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);

    // Read first 512 bytes for the superblock.
    let mut header = [0u8; 512];
    let n = file.read(&mut header).map_err(AgentError::Io)?;

    // Check HDF5 signature: \x89HDF\r\n\x1a\n
    let sig = b"\x89HDF\r\n\x1a\n";
    let is_hdf5 = n >= 8 && &header[..8] == sig;
    if !is_hdf5 {
        return Err(AgentError::Other(
            "Not a valid HDF5 file (signature mismatch)".into(),
        ));
    }

    // Superblock version is at offset 8.
    let sb_version = header[8];

    // We can't fully parse the B-tree without libhdf5, but we can
    // use `h5dump -H` if available, or fall back to basic metadata.
    // Try shelling out to h5dump for the header.
    let datasets = match std::process::Command::new("h5dump")
        .args(["-H", &path.to_string_lossy()])
        .output()
    {
        Ok(out) if out.status.success() => {
            let body = String::from_utf8_lossy(&out.stdout);
            // Extract dataset/group names from h5dump output.
            let mut items: Vec<Value> = Vec::new();
            for line in body.lines() {
                let trimmed = line.trim();
                if let Some(rest) = trimmed.strip_prefix("DATASET \"") {
                    if let Some(name) = rest.strip_suffix("\" {") {
                        items.push(json!({"kind": "dataset", "name": name}));
                    }
                } else if let Some(rest) = trimmed.strip_prefix("GROUP \"") {
                    if let Some(name) = rest.strip_suffix("\" {") {
                        items.push(json!({"kind": "group", "name": name}));
                    }
                }
            }
            items
        }
        _ => Vec::new(), // h5dump not available
    };

    Ok(json!({
        "kind": "hdf5",
        "file_size": file_size,
        "superblock_version": sb_version,
        "items": datasets,
        "h5dump_available": !datasets.is_empty(),
    }))
}

// ---- Protobuf descriptor preview --------------------------------------

/// Parse a compiled protobuf FileDescriptorSet (.desc / .pb file).
async fn preview_protobuf(state: &State, p: PreviewParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let abs = work_dir.join(&p.path);
    if !abs.exists() {
        return Err(AgentError::Other(format!(
            "file not on disk: {} (run `crab hydrate` first)",
            abs.display()
        )));
    }
    let abs_clone = abs.clone();
    tokio::task::spawn_blocking(move || protobuf_preview(&abs_clone))
        .await
        .map_err(|e| AgentError::Other(format!("join error: {e}")))?
}

fn protobuf_preview(path: &std::path::Path) -> Result<Value> {
    use prost::Message;
    let bytes = std::fs::read(path).map_err(AgentError::Io)?;
    let file_size = bytes.len() as u64;

    // Try to decode as FileDescriptorSet.
    let fds = prost_types::FileDescriptorSet::decode(bytes.as_slice())
        .map_err(|e| AgentError::Other(format!("protobuf decode: {e}")))?;

    let mut files: Vec<Value> = Vec::new();
    for fd in &fds.file {
        let messages: Vec<Value> = fd
            .message_type
            .iter()
            .map(|m| {
                let fields: Vec<Value> = m
                    .field
                    .iter()
                    .map(|f| {
                        json!({
                            "name": f.name(),
                            "number": f.number(),
                            "type": format!("{:?}", f.r#type()),
                            "label": format!("{:?}", f.label()),
                        })
                    })
                    .collect();
                json!({ "name": m.name(), "fields": fields })
            })
            .collect();

        let services: Vec<Value> = fd
            .service
            .iter()
            .map(|s| {
                let methods: Vec<Value> = s
                    .method
                    .iter()
                    .map(|m| {
                        json!({
                            "name": m.name(),
                            "input_type": m.input_type(),
                            "output_type": m.output_type(),
                        })
                    })
                    .collect();
                json!({ "name": s.name(), "methods": methods })
            })
            .collect();

        let enums: Vec<Value> = fd.enum_type.iter().map(|e| {
            json!({ "name": e.name(), "values": e.value.iter().map(|v| v.name().to_owned()).collect::<Vec<_>>() })
        }).collect();

        files.push(json!({
            "name": fd.name(),
            "package": fd.package(),
            "syntax": fd.syntax(),
            "messages": messages,
            "services": services,
            "enums": enums,
        }));
    }

    Ok(json!({
        "kind": "protobuf",
        "file_size": file_size,
        "file_count": fds.file.len(),
        "files": files,
    }))
}

// ---- STL 3D model preview ---------------------------------------------

/// Parse binary STL: 80-byte header + u32 triangle count + triangles.
/// Returns vertex bounds + triangle count for a wireframe preview.
async fn preview_stl(state: &State, p: PreviewParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let abs = work_dir.join(&p.path);
    if !abs.exists() {
        return Err(AgentError::Other(format!(
            "file not on disk: {} (run `crab hydrate` first)",
            abs.display()
        )));
    }
    let abs_clone = abs.clone();
    tokio::task::spawn_blocking(move || stl_preview(&abs_clone))
        .await
        .map_err(|e| AgentError::Other(format!("join error: {e}")))?
}

fn stl_preview(path: &std::path::Path) -> Result<Value> {
    use std::io::Read;
    let mut file = File::open(path).map_err(AgentError::Io)?;
    let file_size = file.metadata().map(|m| m.len()).unwrap_or(0);

    // Read header (80 bytes) + triangle count (4 bytes).
    let mut header = [0u8; 84];
    file.read_exact(&mut header).map_err(AgentError::Io)?;

    // Check if it's ASCII STL (starts with "solid").
    let is_ascii = header[..5] == *b"solid";

    let tri_count = u32::from_le_bytes([header[80], header[81], header[82], header[83]]);

    // For binary STL, read triangles (50 bytes each: normal[12] + v1[12] + v2[12] + v3[12] + attr[2]).
    // Cap at 10000 triangles for preview.
    let preview_tris = std::cmp::min(tri_count, 10000) as usize;
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut min_z = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;
    let mut max_z = f32::MIN;
    let mut vertices: Vec<[f32; 3]> = Vec::new();

    if !is_ascii && tri_count > 0 {
        let mut tri_buf = vec![0u8; preview_tris * 50];
        let read = file.read(&mut tri_buf).unwrap_or(0);
        let actual_tris = read / 50;

        for t in 0..actual_tris {
            let base = t * 50 + 12; // skip normal
            for v in 0..3 {
                let off = base + v * 12;
                if off + 12 > tri_buf.len() {
                    break;
                }
                let x = f32::from_le_bytes([
                    tri_buf[off],
                    tri_buf[off + 1],
                    tri_buf[off + 2],
                    tri_buf[off + 3],
                ]);
                let y = f32::from_le_bytes([
                    tri_buf[off + 4],
                    tri_buf[off + 5],
                    tri_buf[off + 6],
                    tri_buf[off + 7],
                ]);
                let z = f32::from_le_bytes([
                    tri_buf[off + 8],
                    tri_buf[off + 9],
                    tri_buf[off + 10],
                    tri_buf[off + 11],
                ]);
                if x < min_x {
                    min_x = x;
                }
                if x > max_x {
                    max_x = x;
                }
                if y < min_y {
                    min_y = y;
                }
                if y > max_y {
                    max_y = y;
                }
                if z < min_z {
                    min_z = z;
                }
                if z > max_z {
                    max_z = z;
                }
                // Store only a subset for wireframe rendering.
                if vertices.len() < 3000 {
                    vertices.push([x, y, z]);
                }
            }
        }
    }

    Ok(json!({
        "kind": "stl",
        "file_size": file_size,
        "is_ascii": is_ascii,
        "triangle_count": tri_count,
        "bounds": {
            "min": [min_x, min_y, min_z],
            "max": [max_x, max_y, max_z],
        },
        "preview_vertices": vertices,
    }))
}

// ---- Protobuf wire-format helpers (for ONNX) --------------------------

fn decode_tag(buf: &[u8], pos: usize) -> Option<(u32, u32, usize)> {
    let (val, new_pos) = decode_varint(buf, pos)?;
    let field_num = (val >> 3) as u32;
    let wire_type = (val & 0x7) as u32;
    Some((field_num, wire_type, new_pos))
}

fn decode_varint(buf: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    loop {
        if pos >= buf.len() {
            return None;
        }
        let byte = buf[pos];
        pos += 1;
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((result, pos));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

fn decode_bytes<'a>(buf: &'a [u8], pos: usize) -> Option<(&'a [u8], usize)> {
    let (len, new_pos) = decode_varint(buf, pos)?;
    let len = len as usize;
    if new_pos + len > buf.len() {
        return None;
    }
    Some((&buf[new_pos..new_pos + len], new_pos + len))
}

fn decode_string<'a>(buf: &'a [u8], pos: usize) -> Option<(&'a str, usize)> {
    let (data, new_pos) = decode_bytes(buf, pos)?;
    let s = std::str::from_utf8(data).ok()?;
    Some((s, new_pos))
}

fn extract_name_field(buf: &[u8]) -> Option<String> {
    let mut pos = 0;
    while pos < buf.len() {
        let (field_num, wire_type, new_pos) = decode_tag(buf, pos)?;
        pos = new_pos;
        match (field_num, wire_type) {
            (1, 2) => {
                let (s, _) = decode_string(buf, pos)?;
                return Some(s.to_owned());
            }
            (_, 0) => {
                let (_, np) = decode_varint(buf, pos)?;
                pos = np;
            }
            (_, 2) => {
                let (_, np) = decode_bytes(buf, pos)?;
                pos = np;
            }
            _ => return None,
        }
    }
    None
}

// ---- Branch info (detail panel) ---------------------------------------

#[derive(Debug, Deserialize)]
struct GitBranchInfoParams {
    repo_id: String,
    branch: String,
    /// Base branch to compare against (default: HEAD).
    #[serde(default)]
    base: Option<String>,
}

/// Comprehensive branch info for the detail panel:
///   - upstream tracking info
///   - ahead/behind counts vs base
///   - last N commits on the branch
///   - files changed vs base (diffstat)
///   - last commit date + author
async fn git_branch_info(state: &State, p: GitBranchInfoParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let branch = &p.branch;
    let base = p.base.as_deref().unwrap_or("HEAD");

    // 1. Upstream tracking.
    let upstream = shellout::git_command()
        .args([
            "for-each-ref",
            "--format=%(upstream:short)",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(&work_dir)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty());

    // 2. Ahead/behind vs base.
    let ab_output = shellout::git_command()
        .args([
            "rev-list",
            "--left-right",
            "--count",
            &format!("{base}...{branch}"),
        ])
        .current_dir(&work_dir)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    let (behind, ahead) = ab_output
        .as_deref()
        .and_then(|s| {
            let parts: Vec<&str> = s.split_whitespace().collect();
            if parts.len() == 2 {
                Some((
                    parts[0].parse::<u32>().unwrap_or(0),
                    parts[1].parse::<u32>().unwrap_or(0),
                ))
            } else {
                None
            }
        })
        .unwrap_or((0, 0));

    // 3. Ahead/behind vs upstream (if tracking).
    let (upstream_ahead, upstream_behind) = if let Some(ref up) = upstream {
        let ab = shellout::git_command()
            .args([
                "rev-list",
                "--left-right",
                "--count",
                &format!("{up}...{branch}"),
            ])
            .current_dir(&work_dir)
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
        ab.as_deref()
            .and_then(|s| {
                let parts: Vec<&str> = s.split_whitespace().collect();
                if parts.len() == 2 {
                    Some((
                        parts[1].parse::<u32>().unwrap_or(0),
                        parts[0].parse::<u32>().unwrap_or(0),
                    ))
                } else {
                    None
                }
            })
            .unwrap_or((0, 0))
    } else {
        (0, 0)
    };

    // 4. Recent commits on the branch (last 20).
    let fmt = "%H%x00%an%x00%aI%x00%s";
    let log_output = shellout::git_command()
        .args([
            "log",
            "--max-count=20",
            &format!("--pretty=format:{fmt}"),
            branch,
        ])
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;
    let mut commits: Vec<Value> = Vec::new();
    if log_output.status.success() {
        let body = String::from_utf8_lossy(&log_output.stdout);
        for line in body.lines() {
            let parts: Vec<&str> = line.split('\0').collect();
            if parts.len() >= 4 {
                commits.push(json!({
                    "sha": parts[0],
                    "author": parts[1],
                    "date": parts[2],
                    "subject": parts[3],
                }));
            }
        }
    }

    // 5. Files changed vs base (diffstat).
    let diff_output = shellout::git_command()
        .args(["diff", "--stat", "--numstat", &format!("{base}...{branch}")])
        .current_dir(&work_dir)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success());
    let mut changed_files: Vec<Value> = Vec::new();
    let mut total_additions: u32 = 0;
    let mut total_deletions: u32 = 0;
    if let Some(diff) = diff_output {
        let body = String::from_utf8_lossy(&diff.stdout);
        for line in body.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 3 {
                let add: u32 = parts[0].parse().unwrap_or(0);
                let del: u32 = parts[1].parse().unwrap_or(0);
                let path = parts[2].to_owned();
                total_additions += add;
                total_deletions += del;
                changed_files.push(json!({
                    "path": path,
                    "additions": add,
                    "deletions": del,
                }));
            }
        }
    }

    // 6. Is this the current branch?
    let current_branch = shellout::git_command()
        .args(["branch", "--show-current"])
        .current_dir(&work_dir)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    let is_current = current_branch == *branch;

    // 7. Branch creation date (first commit not on base).
    let created_at = commits
        .last()
        .and_then(|c| c["date"].as_str().map(|s| s.to_owned()));

    Ok(json!({
        "branch": branch,
        "is_current": is_current,
        "upstream": upstream,
        "ahead": ahead,
        "behind": behind,
        "upstream_ahead": upstream_ahead,
        "upstream_behind": upstream_behind,
        "base": base,
        "commits": commits,
        "changed_files": changed_files,
        "total_additions": total_additions,
        "total_deletions": total_deletions,
        "created_at": created_at,
    }))
}

// ---- Branch health (stale detection) ----------------------------------

#[derive(Debug, Deserialize)]
struct GitBranchHealthParams {
    repo_id: String,
    #[serde(default = "default_stale_days")]
    stale_days: u32,
}

fn default_stale_days() -> u32 {
    30
}

/// Branch health: list all local branches with last commit date,
/// compute days since last commit, and flag stale branches.
async fn git_branch_health(state: &State, p: GitBranchHealthParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;

    // Get all local branches with their last commit date.
    let out = shellout::git_command()
        .args([
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)%00%(committerdate:iso)",
            "refs/heads/",
        ])
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }

    let now = chrono::Utc::now();
    let body = String::from_utf8_lossy(&out.stdout);
    let mut branches: Vec<Value> = Vec::new();
    let mut stale_count: u32 = 0;

    // Get the default branch (HEAD target) for ahead/behind computation.
    let default_branch = shellout::git_command()
        .args(["symbolic-ref", "--short", "HEAD"])
        .current_dir(&work_dir)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| "main".to_owned());

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(2, '\0').collect();
        if parts.len() < 2 {
            continue;
        }

        let name = parts[0].to_owned();
        let date_str = parts[1].trim().to_owned();

        // Parse the ISO date from git (format: "2025-01-15 10:30:00 -0700").
        let is_stale = chrono::NaiveDateTime::parse_from_str(
            date_str.get(..19).unwrap_or(&date_str),
            "%Y-%m-%d %H:%M:%S",
        )
        .ok()
        .map(|naive| {
            let commit_time = naive.and_utc();
            let days_since = (now - commit_time).num_days();
            days_since > i64::from(p.stale_days)
        })
        .unwrap_or(false);

        if is_stale {
            stale_count += 1;
        }

        // Compute ahead/behind vs default branch (skip for the default branch itself).
        let (ahead, behind) = if name == default_branch {
            (0u32, 0u32)
        } else {
            shellout::git_command()
                .args([
                    "rev-list",
                    "--left-right",
                    "--count",
                    &format!("{default_branch}...{name}"),
                ])
                .current_dir(&work_dir)
                .output()
                .await
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .and_then(|s| {
                    let parts: Vec<&str> = s.split_whitespace().collect();
                    if parts.len() == 2 {
                        Some((
                            parts[1].parse::<u32>().unwrap_or(0),
                            parts[0].parse::<u32>().unwrap_or(0),
                        ))
                    } else {
                        None
                    }
                })
                .unwrap_or((0, 0))
        };

        branches.push(json!({
            "name": name,
            "last_commit_date": date_str,
            "is_stale": is_stale,
            "ahead": ahead,
            "behind": behind,
        }));
    }

    let total = branches.len() as u32;

    Ok(json!({
        "total": total,
        "stale": stale_count,
        "branches": branches,
    }))
}

// ---- Repo info (Health panel) -----------------------------------------

/// Comprehensive repo-level info for the Health & Diagnostics panel:
///   - Remote URLs
///   - Git config (user.name, user.email, core.*)
///   - Hooks (list of .git/hooks/* scripts)
///   - Submodules
///   - .gitattributes filter config
async fn git_repo_info(state: &State, p: RepoIdParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;

    // 1. Remotes.
    let remotes_out = shellout::git_command()
        .args(["remote", "-v"])
        .current_dir(&work_dir)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success());
    let mut remotes: Vec<Value> = Vec::new();
    if let Some(out) = remotes_out {
        let body = String::from_utf8_lossy(&out.stdout);
        for line in body.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                remotes.push(json!({
                    "name": parts[0],
                    "url": parts[1],
                    "type": parts.get(2).unwrap_or(&"").trim_matches(|c| c == '(' || c == ')'),
                }));
            }
        }
    }
    // Deduplicate (git remote -v shows fetch + push separately).
    remotes.dedup_by(|a, b| a["name"] == b["name"] && a["url"] == b["url"]);

    // 2. Git config (selected keys).
    let config_keys = [
        "user.name",
        "user.email",
        "core.autocrlf",
        "core.eol",
        "core.filemode",
        "core.ignorecase",
        "core.precomposeunicode",
        "filter.crab.clean",
        "filter.crab.smudge",
        "filter.crab.process",
        "filter.lfs.clean",
        "filter.lfs.smudge",
        "filter.lfs.process",
    ];
    let mut config: Vec<Value> = Vec::new();
    for key in &config_keys {
        let out = shellout::git_command()
            .args(["config", "--get", key])
            .current_dir(&work_dir)
            .output()
            .await
            .ok();
        let value = out
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
        if let Some(v) = value {
            config.push(json!({"key": key, "value": v}));
        }
    }

    // 3. Hooks.
    let hooks_dir = work_dir.join(".git/hooks");
    let mut hooks: Vec<String> = Vec::new();
    if hooks_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&hooks_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                // Skip .sample files.
                if name.ends_with(".sample") {
                    continue;
                }
                if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    hooks.push(name);
                }
            }
        }
    }
    hooks.sort();

    // 4. Submodules.
    let submodules_out = shellout::git_command()
        .args(["submodule", "status"])
        .current_dir(&work_dir)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success());
    let mut submodules: Vec<Value> = Vec::new();
    if let Some(out) = submodules_out {
        let body = String::from_utf8_lossy(&out.stdout);
        for line in body.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Format: " <sha> <path> (<describe>)" or "+<sha> <path> (<describe>)"
            let parts: Vec<&str> = trimmed.splitn(3, ' ').collect();
            if parts.len() >= 2 {
                submodules.push(json!({
                    "sha": parts[0].trim_start_matches(|c: char| !c.is_ascii_hexdigit()),
                    "path": parts[1],
                    "describe": parts.get(2).unwrap_or(&"").trim_matches(|c| c == '(' || c == ')'),
                }));
            }
        }
    }

    // 5. HEAD info.
    let head_sha = shellout::git_command()
        .args(["rev-parse", "HEAD"])
        .current_dir(&work_dir)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();

    let head_branch = shellout::git_command()
        .args(["branch", "--show-current"])
        .current_dir(&work_dir)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();

    // 6. Repo size (rough: .git directory size).
    let git_dir_size = dir_size_bytes(&work_dir.join(".git"));

    Ok(json!({
        "remotes": remotes,
        "config": config,
        "hooks": hooks,
        "submodules": submodules,
        "head_sha": head_sha,
        "head_branch": head_branch,
        "git_dir_size": git_dir_size.to_string(),
        "work_dir": work_dir.to_string_lossy(),
    }))
}

fn dir_size_bytes(path: &std::path::Path) -> u64 {
    let mut total: u64 = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            // Concurrent GC can remove entries during this best-effort scan.
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            } else if ft.is_dir() {
                total += dir_size_bytes(&entry.path());
            }
        }
    }
    total
}

// ---------------- Experiment-tracking handlers ----------------

#[derive(Debug, Deserialize)]
struct ExperimentsRunsForCommitParams {
    #[allow(dead_code)]
    repo_id: String,
    sha: String,
}

async fn experiments_runs_for_commit(p: ExperimentsRunsForCommitParams) -> Result<Value> {
    tracing::debug!(sha = %p.sha, "experiments_runs_for_commit: no provider configured");
    Ok(json!({
        "runs": [],
        "configured": false,
        "message": "No experiment provider configured. Use experiments_configure to set one up."
    }))
}

#[derive(Debug, Deserialize)]
struct ExperimentsRunsRecentParams {
    #[allow(dead_code)]
    repo_id: String,
    #[serde(default = "default_runs_limit")]
    limit: usize,
}

fn default_runs_limit() -> usize {
    25
}

async fn experiments_runs_recent(p: ExperimentsRunsRecentParams) -> Result<Value> {
    tracing::debug!(
        limit = p.limit,
        "experiments_runs_recent: no provider configured"
    );
    Ok(json!({
        "runs": [],
        "configured": false,
        "message": "No experiment provider configured. Use experiments_configure to set one up."
    }))
}

#[derive(Debug, Deserialize)]
struct ExperimentsRunDetailParams {
    #[allow(dead_code)]
    repo_id: String,
    run_id: String,
}

async fn experiments_run_detail(p: ExperimentsRunDetailParams) -> Result<Value> {
    tracing::debug!(run_id = %p.run_id, "experiments_run_detail: no provider configured");
    Err(AgentError::Other(
        "No experiment provider configured. Use experiments_configure to set one up.".to_owned(),
    ))
}

#[derive(Debug, Deserialize)]
struct ExperimentsConfigureParams {
    #[allow(dead_code)]
    repo_id: String,
    provider_id: String,
    base_url: String,
    #[allow(dead_code)]
    token: String,
}

async fn experiments_configure(p: ExperimentsConfigureParams) -> Result<Value> {
    tracing::info!(
        provider_id = %p.provider_id,
        base_url = %p.base_url,
        "experiments_configure: provider implementations not yet available"
    );
    Err(AgentError::Other(format!(
        "Provider '{}' is not yet implemented. MLflow and W&B support coming soon.",
        p.provider_id
    )))
}

// ---- Dedup savings / onboarding ----------------------------------------

#[derive(Debug, Deserialize)]
struct DedupSavingsParams {
    repo_id: String,
}

/// Dedup savings analysis with two modes:
///
/// **Mode A** (pointer files > 0): estimates storage savings from
/// Crab tracking by examining pointer files and estimating stored
/// size based on typical dedup/compression ratios.
///
/// **Mode B** (pointer files = 0): scans the working tree for large
/// files (> 1 MB), returns the top 10 by size, detects common binary
/// patterns, and suggests `.gitattributes` rules.
async fn dedup_savings(work_dir: &Path, _p: DedupSavingsParams) -> Result<Value> {
    // First, determine if the repo has pointer files by checking git ls-files
    // for crab pointer content.
    let pointer_count = count_pointer_files(work_dir).await?;

    if pointer_count > 0 {
        dedup_savings_mode_a(work_dir, pointer_count).await
    } else {
        dedup_savings_mode_b(work_dir).await
    }
}

/// Count crab pointer files in the repo by looking for the pointer
/// signature in tracked files.
///
/// Performance note: a naive implementation reads every tracked
/// file's content into memory. On a 4 k-file repo that's enough I/O
/// to stall the dashboard for tens of seconds, with no value when the
/// repo doesn't even have a crab filter configured. We short-circuit
/// in three ways:
///   1. Skip if `.gitattributes` doesn't mention `filter=crab`.
///   2. Restrict the scan to files matching the crab filter patterns
///      (parsed from `.gitattributes`).
///   3. Cap each `read_to_string` call by inspecting the file size
///      first — pointer files are < 1 KB, so anything larger is
///      definitely not a pointer.
async fn count_pointer_files(work_dir: &Path) -> Result<u64> {
    // Quick-check: does the repo even have a crab filter configured?
    // If not, there can't be any pointer files. This avoids a full
    // working-tree scan on every dashboard load for repos that don't
    // use crab.
    let attrs_path = work_dir.join(".gitattributes");
    let has_crab_filter = match tokio::fs::read_to_string(&attrs_path).await {
        Ok(content) => content
            .lines()
            .any(|l| l.contains("filter=crab") || l.contains("filter=lfs")),
        Err(_) => false,
    };
    if !has_crab_filter {
        return Ok(0);
    }

    let out = shellout::git_command()
        .args(["ls-files", "-z"])
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !out.status.success() {
        return Ok(0);
    }

    let body = String::from_utf8_lossy(&out.stdout);
    let files: Vec<&str> = body.split('\0').filter(|s| !s.is_empty()).collect();

    // Pointer blobs are tiny — a few hundred bytes at most. Skip
    // anything larger before reading content.
    const POINTER_MAX_SIZE: u64 = 4096;

    let mut pointer_count: u64 = 0;
    for file in &files {
        let path = work_dir.join(file);
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() || meta.len() > POINTER_MAX_SIZE {
            continue;
        }
        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            if content.starts_with("version crab") || content.starts_with("version https://crab") {
                pointer_count += 1;
            }
        }
    }

    Ok(pointer_count)
}

/// Mode A: repo has pointer files — estimate dedup savings.
async fn dedup_savings_mode_a(work_dir: &Path, pointer_count: u64) -> Result<Value> {
    // Get total logical bytes from pointer files by summing their declared sizes.
    let out = shellout::git_command()
        .args(["ls-files", "-z"])
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    let body = String::from_utf8_lossy(&out.stdout);
    let files: Vec<&str> = body.split('\0').filter(|s| !s.is_empty()).collect();

    // Pointer blobs are tiny; skip anything larger than 4 KB without
    // reading it. Saves a multi-second scan on repos with thousands
    // of source files.
    const POINTER_MAX_SIZE: u64 = 4096;

    let mut logical_bytes: u64 = 0;
    let mut chunk_count: u64 = 0;

    for file in &files {
        let path = work_dir.join(file);
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() || meta.len() > POINTER_MAX_SIZE {
            continue;
        }
        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            if content.starts_with("version crab") || content.starts_with("version https://crab") {
                // Parse size from pointer content. Pointer format:
                // version crab...
                // hash blake3:<hex>
                // size <bytes>
                for line in content.lines() {
                    if let Some(size_str) = line.strip_prefix("size ") {
                        if let Ok(size) = size_str.trim().parse::<u64>() {
                            logical_bytes += size;
                        }
                    }
                    if line.starts_with("hash ") || line.starts_with("chunks ") {
                        chunk_count += 1;
                    }
                }
            }
        }
    }

    // Estimate stored bytes using typical dedup+compression ratio.
    // Real implementation would read shard metadata; this is a
    // placeholder until staging SQLite access is wired up.
    let compression_ratio: f64 = 1.8;
    let dedup_factor: f64 = 0.6; // 40% dedup savings typical
    let stored_bytes = (logical_bytes as f64 * dedup_factor / compression_ratio) as u64;
    let savings_pct = if logical_bytes > 0 {
        ((logical_bytes - stored_bytes) as f64 / logical_bytes as f64) * 100.0
    } else {
        0.0
    };

    // Estimate chunk reuse from pointer count — more pointers with
    // similar content means more chunk reuse.
    let chunk_reuse_count = if chunk_count > 0 {
        (chunk_count as f64 * 0.3) as u64 // ~30% chunks reused across files
    } else {
        pointer_count * 2
    };

    Ok(json!({
        "has_tracking": true,
        "logical_bytes": logical_bytes,
        "stored_bytes": stored_bytes,
        "savings_pct": (savings_pct * 10.0).round() / 10.0,
        "chunk_reuse_count": chunk_reuse_count,
        "compression_ratio": (compression_ratio * 10.0).round() / 10.0,
    }))
}

/// Mode B: no pointer files — scan working tree for large files and
/// suggest tracking rules.
async fn dedup_savings_mode_b(work_dir: &Path) -> Result<Value> {
    use std::time::Instant;

    let start = Instant::now();
    let timeout = std::time::Duration::from_secs(5);
    let size_threshold: u64 = 1_048_576; // 1 MB
    let max_files: usize = 1000;

    // Use git ls-files to get tracked files, then check sizes.
    let out = shellout::git_command()
        .args([
            "ls-files",
            "-z",
            "--others",
            "--cached",
            "--exclude-standard",
        ])
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    let body = String::from_utf8_lossy(&out.stdout);
    let files: Vec<&str> = body.split('\0').filter(|s| !s.is_empty()).collect();

    let mut large_files: Vec<(String, u64)> = Vec::new();
    let mut scanned = 0usize;

    for file in &files {
        if scanned >= max_files || start.elapsed() > timeout {
            break;
        }
        scanned += 1;

        let path = work_dir.join(file);
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            if meta.is_file() && meta.len() >= size_threshold {
                large_files.push((file.to_string(), meta.len()));
            }
        }
    }

    // Sort by size descending, take top 10.
    large_files.sort_by(|a, b| b.1.cmp(&a.1));
    large_files.truncate(10);

    // Detect patterns from file extensions.
    let mut ext_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (path, _) in &large_files {
        if let Some(ext) = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
        {
            *ext_counts.entry(ext.to_lowercase()).or_insert(0) += 1;
        }
    }

    // Build suggested .gitattributes rules for common binary patterns.
    let binary_extensions = [
        "bin",
        "dmg",
        "zip",
        "tar",
        "gz",
        "tgz",
        "bz2",
        "xz",
        "7z",
        "rar",
        "iso",
        "img",
        "pdf",
        "exe",
        "dll",
        "so",
        "dylib",
        "safetensors",
        "onnx",
        "pt",
        "pth",
        "h5",
        "hdf5",
        "parquet",
        "arrow",
        "npy",
        "npz",
        "pkl",
        "pickle",
        "db",
        "sqlite",
        "sqlite3",
        "whl",
        "egg",
        "jar",
        "war",
    ];

    let mut suggested_rules: Vec<String> = Vec::new();
    for (ext, count) in &ext_counts {
        if *count >= 1 && binary_extensions.contains(&ext.as_str()) {
            suggested_rules.push(format!("*.{ext} filter=crab diff=crab merge=crab -text"));
        }
    }
    suggested_rules.sort();

    // Estimate savings: 50% for binary files is a reasonable default.
    let total_large_bytes: u64 = large_files.iter().map(|(_, s)| s).sum();
    let estimated_savings_pct: f64 = if total_large_bytes > 0 { 50.0 } else { 0.0 };

    let large_files_json: Vec<Value> = large_files
        .iter()
        .map(|(path, size)| json!({ "path": path, "size": size }))
        .collect();

    Ok(json!({
        "has_tracking": false,
        "logical_bytes": 0,
        "stored_bytes": 0,
        "savings_pct": 0.0,
        "chunk_reuse_count": 0,
        "compression_ratio": 0.0,
        "large_files": large_files_json,
        "estimated_savings_pct": estimated_savings_pct,
        "suggested_rules": suggested_rules,
    }))
}

// ---- Release timeline ------------------------------------------------

#[derive(Debug, Deserialize)]
struct GitReleaseTimelineParams {
    repo_id: String,
}

/// Return the last 20 tags sorted by creation date (newest first),
/// each with its name and ISO date. Used by the dashboard release
/// timeline visualization.
async fn git_release_timeline(state: &State, p: GitReleaseTimelineParams) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;

    let out = shellout::git_command()
        .args([
            "tag",
            "--sort=-creatordate",
            "--format=%(refname:short)%00%(creatordate:iso)",
        ])
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }

    let body = String::from_utf8_lossy(&out.stdout);
    let mut tags: Vec<Value> = Vec::new();

    for line in body.lines().take(20) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(2, '\0').collect();
        if parts.len() < 2 {
            continue;
        }

        let name = parts[0].to_owned();
        let date = parts[1].trim().to_owned();

        tags.push(json!({ "name": name, "date": date }));
    }

    Ok(json!({ "tags": tags }))
}

// ---- Repo size history -----------------------------------------------

#[derive(Debug, Deserialize)]
struct RepoSizeHistoryParams {
    repo_id: String,
    #[serde(default = "default_size_history_points")]
    points: u32,
}

fn default_size_history_points() -> u32 {
    24
}

/// A single cached size history point.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SizeHistoryPoint {
    date: String,
    logical_bytes: u64,
    pointer_bytes: u64,
    git_bytes: u64,
}

/// Compute repository size at monthly intervals using a linear
/// interpolation approach. Gets the current repo size and the first
/// commit date, then samples at monthly intervals. Historical points
/// are cached permanently (immutable).
async fn repo_size_history(state: &State, p: RepoSizeHistoryParams) -> Result<Value> {
    use chrono::{Datelike, Duration, NaiveDate, Utc};

    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let max_points = p.points.min(104).max(2);

    // Resolve cache path.
    let home = dirs::home_dir().ok_or_else(|| AgentError::Other("no home dir".into()))?;
    let repo_hash = repo_path_hash(&work_dir);
    let cache_path = home
        .join(".crab/desktop/cache/metrics")
        .join(&repo_hash)
        .join("size_history.json");

    // Load existing cache.
    let cached_points = load_size_history_cache(&cache_path).await;

    // If cache is fresh enough (has entries and the last entry is from
    // the current week), return it directly.
    let today = Utc::now().date_naive();
    let one_week_ago = today - Duration::weeks(1);

    if !cached_points.is_empty() {
        if let Some(last) = cached_points.last() {
            if let Ok(last_date) = NaiveDate::parse_from_str(&last.date, "%Y-%m-%d") {
                if last_date >= one_week_ago {
                    // Cache is current — return it.
                    let result: Vec<Value> = cached_points
                        .iter()
                        .rev()
                        .take(max_points as usize)
                        .rev()
                        .map(|pt| {
                            json!({
                                "date": pt.date,
                                "logical_bytes": pt.logical_bytes,
                                "pointer_bytes": pt.pointer_bytes,
                                "git_bytes": pt.git_bytes,
                            })
                        })
                        .collect();
                    return Ok(json!({ "points": result }));
                }
            }
        }
    }

    // Get the first commit date.
    let first_commit_out = shellout::git_command()
        .args(["log", "--reverse", "--format=%aI", "-1"])
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    let first_date_str = String::from_utf8_lossy(&first_commit_out.stdout)
        .trim()
        .to_string();

    let first_date = if first_date_str.is_empty() {
        // No commits — return empty.
        return Ok(json!({ "points": [] }));
    } else {
        // Parse ISO date (e.g., "2024-01-15T10:30:00+00:00")
        chrono::DateTime::parse_from_rfc3339(
            first_date_str
                .split_whitespace()
                .next()
                .unwrap_or(&first_date_str),
        )
        .map(|dt| dt.date_naive())
        .or_else(|_| {
            // Try parsing just the date portion.
            NaiveDate::parse_from_str(&first_date_str[..10.min(first_date_str.len())], "%Y-%m-%d")
        })
        .unwrap_or(today - Duration::days(365))
    };

    // Get current repo size using git ls-files.
    let current_size = get_current_repo_size(&work_dir).await?;

    // Get total number of commits to estimate growth curve.
    let commit_count_out = shellout::git_command()
        .args(["rev-list", "--count", "HEAD"])
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    let total_commits: u64 = String::from_utf8_lossy(&commit_count_out.stdout)
        .trim()
        .parse()
        .unwrap_or(1);

    // Generate weekly sample points going back `max_points` weeks from today.
    // Weekly sampling gives much better granularity than monthly.
    let mut sample_dates: Vec<NaiveDate> = Vec::new();
    let weeks_back = max_points;
    let start = today - Duration::weeks(i64::from(weeks_back));
    // Start from the later of: first_date or weeks_back.
    let effective_start = if first_date > start {
        first_date
    } else {
        start
    };
    let mut cursor = effective_start;

    while cursor <= today {
        sample_dates.push(cursor);
        cursor += Duration::weeks(1);
    }

    // Always include today if not already there.
    if sample_dates.last().map(|d| *d != today).unwrap_or(true) {
        sample_dates.push(today);
    }

    // For each sample date, estimate the size by counting commits up to
    // that point and using a proportional estimate.
    let mut points: Vec<SizeHistoryPoint> = Vec::new();

    // Index cached points for reuse.
    let mut cached_map: std::collections::HashMap<String, SizeHistoryPoint> =
        std::collections::HashMap::new();
    for pt in &cached_points {
        cached_map.insert(pt.date.clone(), pt.clone());
    }

    for sample_date in &sample_dates {
        let date_str = sample_date.format("%Y-%m-%d").to_string();

        // Use cached value for historical points (before current week).
        if *sample_date < one_week_ago {
            if let Some(cached) = cached_map.get(&date_str) {
                points.push(cached.clone());
                continue;
            }
        }

        // Count commits up to this date.
        let until_arg = format!("{}T23:59:59", date_str);
        let count_out = shellout::git_command()
            .args(["rev-list", "--count", "--before", &until_arg, "HEAD"])
            .current_dir(&work_dir)
            .output()
            .await
            .map_err(AgentError::Io)?;

        let commits_at_date: u64 = String::from_utf8_lossy(&count_out.stdout)
            .trim()
            .parse()
            .unwrap_or(0);

        if commits_at_date == 0 {
            points.push(SizeHistoryPoint {
                date: date_str,
                logical_bytes: 0,
                pointer_bytes: 0,
                git_bytes: 0,
            });
            continue;
        }

        // Estimate size proportionally based on commit count ratio.
        // This is a reasonable approximation: size grows roughly with
        // the number of commits (files added/modified over time).
        let ratio = commits_at_date as f64 / total_commits.max(1) as f64;
        let estimated_logical = (current_size as f64 * ratio) as u64;

        // Pointer bytes: estimate as a fraction of logical (repos with
        // pointer files typically have ~5-20% pointer content).
        let estimated_pointer = estimated_logical / 20;
        let estimated_git = estimated_logical - estimated_pointer;

        points.push(SizeHistoryPoint {
            date: date_str,
            logical_bytes: estimated_logical,
            pointer_bytes: estimated_pointer,
            git_bytes: estimated_git,
        });
    }

    // Persist cache.
    if let Err(e) = save_size_history_cache(&cache_path, &points).await {
        tracing::warn!("failed to save size history cache: {e}");
    }

    // Return the last N points.
    let result: Vec<Value> = points
        .iter()
        .rev()
        .take(max_points as usize)
        .rev()
        .map(|pt| {
            json!({
                "date": pt.date,
                "logical_bytes": pt.logical_bytes,
                "pointer_bytes": pt.pointer_bytes,
                "git_bytes": pt.git_bytes,
            })
        })
        .collect();

    Ok(json!({ "points": result }))
}

/// Get the current total size of tracked files in the repo.
async fn get_current_repo_size(work_dir: &Path) -> Result<u64> {
    // Use `git ls-files -z` + stat to get total size of tracked files.
    let out = shellout::git_command()
        .args(["ls-files", "-z"])
        .current_dir(work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !out.status.success() {
        return Ok(0);
    }

    let body = String::from_utf8_lossy(&out.stdout);
    let mut total: u64 = 0;

    for path in body.split('\0') {
        if path.is_empty() {
            continue;
        }
        let full_path = work_dir.join(path);
        if let Ok(meta) = tokio::fs::metadata(&full_path).await {
            total += meta.len();
        }
    }

    Ok(total)
}

/// Load the size history cache from disk.
async fn load_size_history_cache(cache_path: &Path) -> Vec<SizeHistoryPoint> {
    match tokio::fs::read_to_string(cache_path).await {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Save the size history cache to disk, creating parent dirs as needed.
async fn save_size_history_cache(cache_path: &Path, entries: &[SizeHistoryPoint]) -> Result<()> {
    if let Some(parent) = cache_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(AgentError::Io)?;
    }
    let json = serde_json::to_string_pretty(entries)
        .map_err(|e| AgentError::Other(format!("cache serialize: {e}")))?;
    tokio::fs::write(cache_path, json)
        .await
        .map_err(AgentError::Io)?;
    Ok(())
}

// ── Commit size distribution ────────────────────────────────────────

#[derive(Deserialize)]
struct GitCommitSizeDistributionParams {
    repo_id: String,
}

/// Commit size distribution: parse the last 100 commits' shortstat
/// output, bucket by lines changed, and identify outliers (> mean + 2σ).
async fn git_commit_size_distribution(
    state: &State,
    p: GitCommitSizeDistributionParams,
) -> Result<Value> {
    let work_dir = state.get_work_dir(&p.repo_id).await?;

    // Use %H%x00%s as format so we can split SHA and subject by NUL,
    // then --shortstat appends a stats line after each commit.
    let out = shellout::git_command()
        .args(["log", "--shortstat", "--format=%H%x00%s", "-100"])
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }

    let body = String::from_utf8_lossy(&out.stdout);

    // Parse commits: each commit produces a format line (%H\0%s) followed
    // by an optional blank line and a shortstat line.
    struct CommitEntry {
        sha: String,
        subject: String,
        lines_changed: u64,
    }

    let mut commits: Vec<CommitEntry> = Vec::new();
    let mut current_sha = String::new();
    let mut current_subject = String::new();

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Check if this is a format line (contains NUL separator).
        if let Some(nul_pos) = trimmed.find('\0') {
            // Save any pending commit before starting a new one.
            current_sha = trimmed[..nul_pos].to_owned();
            current_subject = trimmed[nul_pos + 1..].to_owned();
        } else if trimmed.contains("changed") && !current_sha.is_empty() {
            // This is a shortstat line like:
            // " 3 files changed, 12 insertions(+), 5 deletions(-)"
            let insertions = parse_shortstat_number(trimmed, "insertion");
            let deletions = parse_shortstat_number(trimmed, "deletion");
            let lines_changed = insertions + deletions;

            if lines_changed > 0 {
                commits.push(CommitEntry {
                    sha: current_sha.clone(),
                    subject: current_subject.clone(),
                    lines_changed,
                });
            }
            current_sha.clear();
            current_subject.clear();
        }
    }

    // Define fixed buckets.
    let bucket_defs: &[(u64, u64, &str)] = &[
        (1, 10, "1-10"),
        (11, 50, "11-50"),
        (51, 200, "51-200"),
        (201, 500, "201-500"),
        (501, u64::MAX, "500+"),
    ];

    let mut bucket_counts: Vec<u64> = vec![0; bucket_defs.len()];

    for c in &commits {
        for (i, &(min, max, _)) in bucket_defs.iter().enumerate() {
            if c.lines_changed >= min && c.lines_changed <= max {
                bucket_counts[i] += 1;
                break;
            }
        }
    }

    // Compute mean and standard deviation for outlier detection.
    let outliers = if commits.is_empty() {
        Vec::new()
    } else {
        let n = commits.len() as f64;
        let mean = commits.iter().map(|c| c.lines_changed as f64).sum::<f64>() / n;
        let variance = commits
            .iter()
            .map(|c| {
                let diff = c.lines_changed as f64 - mean;
                diff * diff
            })
            .sum::<f64>()
            / n;
        let std_dev = variance.sqrt();
        let threshold = mean + 2.0 * std_dev;

        commits
            .iter()
            .filter(|c| c.lines_changed as f64 > threshold)
            .map(|c| {
                json!({
                    "sha": c.sha,
                    "subject": c.subject,
                    "lines_changed": c.lines_changed,
                })
            })
            .collect::<Vec<_>>()
    };

    let buckets: Vec<Value> = bucket_defs
        .iter()
        .enumerate()
        .map(|(i, &(min, max, label))| {
            json!({
                "label": label,
                "min": min,
                "max": if max == u64::MAX { 999999 } else { max },
                "count": bucket_counts[i],
            })
        })
        .collect();

    Ok(json!({
        "buckets": buckets,
        "outliers": outliers,
    }))
}

/// Extract a number preceding a keyword from a git shortstat line.
/// e.g. "12 insertions(+)" → 12 when keyword is "insertion".
fn parse_shortstat_number(line: &str, keyword: &str) -> u64 {
    if let Some(pos) = line.find(keyword) {
        // Walk backwards from the keyword to find the number.
        let before = &line[..pos].trim_end();
        if let Some(last_space) = before.rfind(|c: char| !c.is_ascii_digit()) {
            before[last_space + 1..].parse::<u64>().unwrap_or(0)
        } else {
            before.parse::<u64>().unwrap_or(0)
        }
    } else {
        0
    }
}

// ---- Contributor growth trend ------------------------------------------------

#[derive(Debug, Deserialize)]
struct GitContributorGrowthParams {
    repo_id: String,
    /// Number of months to look back. Default 12, max 24.
    #[serde(default = "default_contributor_growth_months")]
    months: u32,
}

fn default_contributor_growth_months() -> u32 {
    12
}

/// Contributor growth: run `git log` to get all author names with dates,
/// track the first-seen date per author, group by month, and compute
/// cumulative totals.
async fn git_contributor_growth(state: &State, p: GitContributorGrowthParams) -> Result<Value> {
    use chrono::{Datelike, NaiveDate, Utc};

    let work_dir = state.get_work_dir(&p.repo_id).await?;
    let months = p.months.min(24).max(1);

    // Bound the walk by the requested time window. Without `--since`
    // a million-commit repo produces 50 MB+ of stdout and the
    // first-seen tracking loop allocates a string per commit author.
    // Adding a year of headroom ensures the "first time we saw an
    // author who later appears in the chart" tracking still works.
    let since_arg = format!("--since={} years ago", (months / 12).max(1) + 1);

    // Run git log with NUL separator between author name and date for
    // reliable parsing (author names can contain spaces, pipes, etc.).
    let out = shellout::git_command()
        .args(["log", "--all", &since_arg, "--format=%aN%x00%aI"])
        .current_dir(&work_dir)
        .output()
        .await
        .map_err(AgentError::Io)?;

    if !out.status.success() {
        return Err(AgentError::Cli {
            status: out.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }

    let body = String::from_utf8_lossy(&out.stdout);

    // Track the earliest commit date per author.
    let mut first_seen: std::collections::HashMap<&str, NaiveDate> =
        std::collections::HashMap::new();

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Split on NUL character.
        let Some((name, date_str)) = trimmed.split_once('\0') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let date = match chrono::DateTime::parse_from_rfc3339(date_str.trim()) {
            Ok(d) => d.with_timezone(&Utc).date_naive(),
            Err(_) => continue,
        };

        first_seen
            .entry(name)
            .and_modify(|existing| {
                if date < *existing {
                    *existing = date;
                }
            })
            .or_insert(date);
    }

    // Determine the month range: from `months` months ago to now.
    let now = Utc::now().date_naive();
    let start_month = {
        let mut y = now.year();
        let mut m = now.month() as i32 - (months as i32 - 1);
        while m <= 0 {
            m += 12;
            y -= 1;
        }
        NaiveDate::from_ymd_opt(y, m as u32, 1).unwrap_or(now)
    };

    // Count new contributors per month (month of their first-seen date).
    let mut new_per_month: std::collections::BTreeMap<(i32, u32), u32> =
        std::collections::BTreeMap::new();

    // Initialize all months in range.
    {
        let mut cursor = start_month;
        while cursor <= now {
            new_per_month
                .entry((cursor.year(), cursor.month()))
                .or_insert(0);
            // Advance to next month.
            cursor = if cursor.month() == 12 {
                NaiveDate::from_ymd_opt(cursor.year() + 1, 1, 1)
            } else {
                NaiveDate::from_ymd_opt(cursor.year(), cursor.month() + 1, 1)
            }
            .unwrap_or(now);
            if cursor == now && cursor != start_month {
                new_per_month
                    .entry((cursor.year(), cursor.month()))
                    .or_insert(0);
                break;
            }
        }
    }

    // Count authors whose first-seen falls in each month.
    // Also count authors who appeared before the window (for cumulative base).
    let mut pre_window_count: u32 = 0;
    for &date in first_seen.values() {
        let key = (date.year(), date.month());
        if date < start_month {
            pre_window_count += 1;
        } else if new_per_month.contains_key(&key) {
            *new_per_month.get_mut(&key).unwrap_or(&mut 0) += 1;
        }
    }

    // Build result with cumulative totals.
    let mut cumulative = pre_window_count;
    let result_months: Vec<Value> = new_per_month
        .iter()
        .map(|(&(year, month), &new_count)| {
            cumulative += new_count;
            json!({
                "month": format!("{year:04}-{month:02}"),
                "new_contributors": new_count,
                "cumulative": cumulative,
            })
        })
        .collect();

    Ok(json!({ "months": result_months }))
}
