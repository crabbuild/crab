use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{LocalRepository, ensure_supported_mutation, local_error};
use crate::{CommitIdentity, Error, ErrorKind, ObjectId, OperationOptions, Result};

/// One path reported by Git's porcelain-v2 status contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    path: PathBuf,
    staged: bool,
    worktree: bool,
    conflicted: bool,
    untracked: bool,
}

impl StatusEntry {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn is_staged(&self) -> bool {
        self.staged
    }

    #[must_use]
    pub const fn has_worktree_change(&self) -> bool {
        self.worktree
    }

    #[must_use]
    pub const fn is_conflicted(&self) -> bool {
        self.conflicted
    }

    #[must_use]
    pub const fn is_untracked(&self) -> bool {
        self.untracked
    }
}

/// Stable local status snapshot returned from one Git invocation.
#[derive(Clone, Debug)]
pub struct LocalStatus {
    branch: Option<String>,
    head: Option<ObjectId>,
    entries: Vec<StatusEntry>,
}

impl LocalStatus {
    #[must_use]
    pub fn branch(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    #[must_use]
    pub const fn head(&self) -> Option<ObjectId> {
        self.head
    }

    #[must_use]
    pub fn entries(&self) -> &[StatusEntry] {
        &self.entries
    }

    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Number of exact paths admitted for staging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StageOutcome {
    paths: usize,
}

impl StageOutcome {
    #[must_use]
    pub const fn paths(&self) -> usize {
        self.paths
    }
}

/// Metadata for a local commit without consulting global Git identity.
#[derive(Clone, Debug)]
pub struct LocalCommitOptions {
    author: CommitIdentity,
    committer: CommitIdentity,
    message: Vec<u8>,
}

impl LocalCommitOptions {
    pub fn new(
        author: CommitIdentity,
        committer: CommitIdentity,
        message: impl Into<Vec<u8>>,
    ) -> Result<Self> {
        let message = message.into();
        if message.is_empty() || message.len() > 64 * 1024 * 1024 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "commit message must be nonempty and at most 64 MiB",
            ));
        }
        Ok(Self {
            author,
            committer,
            message,
        })
    }
}

/// Checkout behavior; changes are preserved unless Git proves the switch safe.
#[derive(Clone, Debug, Default)]
pub struct CheckoutOptions {
    create_branch: Option<String>,
}

impl CheckoutOptions {
    pub fn create_branch(mut self, name: &str) -> Result<Self> {
        crate::Revision::branch(name)?;
        self.create_branch = Some(name.to_owned());
        Ok(self)
    }
}

/// Git integration strategy selected for pull.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum PullMode {
    #[default]
    FastForwardOnly,
    Merge,
    Rebase,
}

/// Explicit pull destination and integration behavior.
#[derive(Clone, Debug)]
pub struct PullOptions {
    remote: String,
    branch: Option<String>,
    mode: PullMode,
    hydrate: bool,
}

impl PullOptions {
    /// Select the configured local remote to fetch from.
    pub fn with_remote(mut self, remote: &str) -> Result<Self> {
        super::validate_remote_name(remote)?;
        self.remote = remote.to_owned();
        Ok(self)
    }

    #[must_use]
    pub fn fast_forward_only() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn merge() -> Self {
        Self {
            mode: PullMode::Merge,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn rebase() -> Self {
        Self {
            mode: PullMode::Rebase,
            ..Self::default()
        }
    }

    pub fn with_branch(mut self, branch: &str) -> Result<Self> {
        crate::Revision::branch(branch)?;
        self.branch = Some(branch.to_owned());
        Ok(self)
    }

    #[must_use]
    pub fn hydrate(mut self, hydrate: bool) -> Self {
        self.hydrate = hydrate;
        self
    }
}

impl Default for PullOptions {
    fn default() -> Self {
        Self {
            remote: "origin".to_owned(),
            branch: None,
            mode: PullMode::FastForwardOnly,
            hydrate: true,
        }
    }
}

/// Integration operation currently awaiting conflict resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum IntegrationKind {
    Merge,
    Rebase,
}

/// Durable identity of one SDK-owned merge or rebase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationId(String);

impl IntegrationId {
    /// Reconstruct a persisted SDK integration identity.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidInput`] unless `value` is the canonical
    /// lowercase representation of a version-7 UUID issued by the SDK.
    pub fn from_string(value: &str) -> Result<Self> {
        let parsed = uuid::Uuid::parse_str(value).map_err(|source| {
            Error::with_source(
                ErrorKind::InvalidInput,
                "invalid local integration identity",
                source,
            )
        })?;
        if parsed.get_version_num() != 7 || parsed.hyphenated().to_string() != value {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid local integration identity",
            ));
        }
        Ok(Self(value.to_owned()))
    }

    /// Return the opaque identity used by continue and abort.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Conflicted paths and the resumable integration kind.
#[derive(Clone, Debug)]
pub struct ConflictState {
    id: IntegrationId,
    kind: IntegrationKind,
    paths: Vec<PathBuf>,
    fetched: Option<ObjectId>,
}

impl ConflictState {
    /// Return the SDK integration identity required for continue or abort.
    #[must_use]
    pub fn id(&self) -> &IntegrationId {
        &self.id
    }

    #[must_use]
    pub const fn kind(&self) -> IntegrationKind {
        self.kind
    }

    #[must_use]
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Return the exact fetched commit being integrated, when created by pull.
    #[must_use]
    pub const fn fetched(&self) -> Option<ObjectId> {
        self.fetched
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum PersistedIntegrationKind {
    Merge,
    Rebase,
}

impl From<IntegrationKind> for PersistedIntegrationKind {
    fn from(value: IntegrationKind) -> Self {
        match value {
            IntegrationKind::Merge => Self::Merge,
            IntegrationKind::Rebase => Self::Rebase,
        }
    }
}

impl From<PersistedIntegrationKind> for IntegrationKind {
    fn from(value: PersistedIntegrationKind) -> Self {
        match value {
            PersistedIntegrationKind::Merge => Self::Merge,
            PersistedIntegrationKind::Rebase => Self::Rebase,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IntegrationIntent {
    version: u16,
    id: String,
    kind: PersistedIntegrationKind,
    fetched: String,
    tracked_status_hash: Option<String>,
}

const INTEGRATION_INTENT: &str = "crab-sdk-integration-v1.json";

/// Hydration result after Git integration has completed.
#[derive(Debug)]
#[non_exhaustive]
pub enum HydrationState {
    NotRequested,
    Ready,
    Failed(Error),
}

/// Result of pull or conflict continuation.
#[derive(Debug)]
#[non_exhaustive]
pub enum PullOutcome {
    Updated {
        head: ObjectId,
        hydration: HydrationState,
    },
    UpToDate {
        head: ObjectId,
        hydration: HydrationState,
    },
    Conflict(ConflictState),
}

impl LocalRepository {
    /// Read staged, worktree, conflict and untracked state without mutation.
    pub fn status(&self) -> crate::Request<'_, LocalStatus, OperationOptions> {
        crate::Request::new(move |operation| Box::pin(self.status_with_options(operation)))
    }

    async fn status_with_options(&self, operation: OperationOptions) -> Result<LocalStatus> {
        let tools = self.tools()?.clone();
        let root = self.root.clone();
        self.client
            .0
            .operations
            .run(operation, move |cancel| async move {
                let output = tools
                    .owner
                    .run_git(
                        Some(&root),
                        ["status", "--porcelain=v2", "--branch", "-z"],
                        false,
                        &cancel,
                    )
                    .await
                    .map_err(local_error)?;
                parse_status(&output.stdout)
            })
            .await
    }

    /// Stage exact repository-relative paths through Crab's canonical large-file path.
    pub fn stage(&self, paths: Vec<PathBuf>) -> crate::Request<'_, StageOutcome, OperationOptions> {
        crate::Request::new(move |operation| Box::pin(self.stage_with_options(paths, operation)))
    }

    async fn stage_with_options(
        &self,
        paths: Vec<PathBuf>,
        operation: OperationOptions,
    ) -> Result<StageOutcome> {
        if paths.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "at least one stage path is required",
            ));
        }
        validate_paths(&paths)?;
        let tools = self.tools()?.clone();
        let root = self.root.clone();
        let common = self.common_dir.clone();
        let trusted = self.client.0.local_execution_policy.is_trusted();
        self.client
            .0
            .operations
            .run(operation, move |cancel| async move {
                let _lease = crab_remote::local::acquire_repository_lease(&common, &cancel)
                    .await
                    .map_err(local_error)?;
                ensure_supported_mutation(&tools.owner, &root, &cancel).await?;
                let crab_configured =
                    root.join(".crab").exists() || root.join("crab.toml").exists();
                let (crab_paths, git_paths) = if crab_configured {
                    partition_stage_paths(&tools.owner, &root, paths, &cancel).await?
                } else {
                    (Vec::new(), paths)
                };
                if !crab_paths.is_empty() {
                    let mut args: Vec<OsString> = vec!["add".into(), "--json".into()];
                    args.extend(crab_paths.iter().map(|path| path.as_os_str().to_owned()));
                    tools
                        .owner
                        .run_crab(Some(&root), args, trusted, &cancel)
                        .await
                        .map_err(local_error)?;
                }
                if !git_paths.is_empty() {
                    let mut args: Vec<OsString> = vec!["add".into(), "--all".into(), "--".into()];
                    args.extend(git_paths.iter().map(|path| path.as_os_str().to_owned()));
                    tools
                        .owner
                        .run_git(Some(&root), args, trusted, &cancel)
                        .await
                        .map_err(local_error)?;
                }
                Ok(StageOutcome {
                    paths: crab_paths.len() + git_paths.len(),
                })
            })
            .await
    }

    /// Commit the current index with explicit author and committer metadata.
    pub fn commit(
        &self,
        options: LocalCommitOptions,
    ) -> crate::Request<'_, ObjectId, OperationOptions> {
        crate::Request::new(move |operation| Box::pin(self.commit_with_options(options, operation)))
    }

    async fn commit_with_options(
        &self,
        options: LocalCommitOptions,
        operation: OperationOptions,
    ) -> Result<ObjectId> {
        let tools = self.tools()?.clone();
        let root = self.root.clone();
        let common = self.common_dir.clone();
        let trusted = self.client.0.local_execution_policy.is_trusted();
        self.client
            .0
            .operations
            .run(operation, move |cancel| async move {
                let _lease = crab_remote::local::acquire_repository_lease(&common, &cancel)
                    .await
                    .map_err(local_error)?;
                ensure_supported_mutation(&tools.owner, &root, &cancel).await?;
                let message = tempfile::NamedTempFile::new_in(&common).map_err(|source| {
                    Error::with_source(ErrorKind::Io, "cannot create commit message", source)
                })?;
                std::fs::write(message.path(), &options.message).map_err(|source| {
                    Error::with_source(ErrorKind::Io, "cannot write commit message", source)
                })?;
                // Rust canonical paths use the verbatim prefix on Windows, which
                // Git for Windows does not accept for its `-F` path argument.
                let message_path = dunce::simplified(message.path());
                let mut environment = identity_environment(&options.author, &options.committer);
                environment.push(("GIT_EDITOR".into(), "true".into()));
                tools
                    .owner
                    .run_git_with_env(
                        Some(&root),
                        [
                            OsStr::new("commit"),
                            OsStr::new("--no-gpg-sign"),
                            OsStr::new("-F"),
                            message_path.as_os_str(),
                        ],
                        environment,
                        trusted,
                        &cancel,
                    )
                    .await
                    .map_err(local_error)?;
                head(&tools.owner, &root, &cancel).await
            })
            .await
    }

    /// Switch to an existing revision, preserving local changes by default.
    pub fn checkout(
        &self,
        revision: &str,
        options: CheckoutOptions,
    ) -> crate::Request<'_, ObjectId, OperationOptions> {
        let revision = revision.to_owned();
        crate::Request::new(move |operation| {
            Box::pin(self.checkout_with_options(revision, options, operation))
        })
    }

    async fn checkout_with_options(
        &self,
        revision: String,
        options: CheckoutOptions,
        operation: OperationOptions,
    ) -> Result<ObjectId> {
        validate_revision(&revision)?;
        let tools = self.tools()?.clone();
        let root = self.root.clone();
        let common = self.common_dir.clone();
        let trusted = self.client.0.local_execution_policy.is_trusted();
        self.client
            .0
            .operations
            .run(operation, move |cancel| async move {
                let _lease = crab_remote::local::acquire_repository_lease(&common, &cancel)
                    .await
                    .map_err(local_error)?;
                ensure_supported_mutation(&tools.owner, &root, &cancel).await?;
                let mut args: Vec<OsString> = vec!["checkout".into()];
                if let Some(branch) = options.create_branch {
                    args.extend(["-b".into(), branch.into()]);
                }
                args.extend([revision.into(), "--".into()]);
                tools
                    .owner
                    .run_git(Some(&root), args, trusted, &cancel)
                    .await
                    .map_err(local_error)?;
                head(&tools.owner, &root, &cancel).await
            })
            .await
    }

    /// Fetch and integrate the selected branch, returning conflicts as state.
    pub fn pull(&self, options: PullOptions) -> crate::Request<'_, PullOutcome, OperationOptions> {
        crate::Request::new(move |operation| Box::pin(self.pull_with_options(options, operation)))
    }

    async fn pull_with_options(
        &self,
        options: PullOptions,
        operation: OperationOptions,
    ) -> Result<PullOutcome> {
        let tools = self.tools()?.clone();
        let root = self.root.clone();
        let common = self.common_dir.clone();
        let git_dir = self.git_dir.clone();
        let locator = self.locator.clone();
        let state = self.client.0.clone();
        let trusted = self.client.0.local_execution_policy.is_trusted();
        self.client
            .0
            .operations
            .run(operation, move |cancel| async move {
                let _lease = crab_remote::local::acquire_repository_lease(&common, &cancel)
                    .await
                    .map_err(local_error)?;
                ensure_supported_mutation(&tools.owner, &root, &cancel).await?;
                prepare_pull_integration(&git_dir).await?;
                let before = head(&tools.owner, &root, &cancel).await?;
                let remote = options.remote.clone();
                let branch = options.branch.clone();
                let fetch = super::FetchOptions {
                    remote: remote.clone(),
                    prune: false,
                    tags: None,
                    depth: super::FetchDepth::Preserve,
                };
                super::fetch::fetch_locked(
                    &state,
                    locator,
                    &tools.owner,
                    &root,
                    &git_dir,
                    &common,
                    &fetch,
                    &cancel,
                )
                .await?;
                let fetched_ref = branch.map_or_else(
                    || "@{upstream}".to_owned(),
                    |branch| format!("refs/remotes/{remote}/{branch}"),
                );
                let fetched = git_commit(&tools.owner, &root, &fetched_ref, &cancel).await?;
                let fetched_text = fetched.to_string();
                let kind = match options.mode {
                    PullMode::FastForwardOnly => None,
                    PullMode::Merge => Some(IntegrationKind::Merge),
                    PullMode::Rebase => Some(IntegrationKind::Rebase),
                };
                let integration = kind.map(|kind| IntegrationIntent {
                    version: 1,
                    id: uuid::Uuid::now_v7().to_string(),
                    kind: kind.into(),
                    fetched: fetched_text.clone(),
                    tracked_status_hash: None,
                });
                if let Some(intent) = &integration {
                    write_integration_intent(&git_dir, intent).await?;
                }
                let integrate: Vec<OsString> = match options.mode {
                    PullMode::FastForwardOnly => {
                        vec!["merge".into(), "--ff-only".into(), fetched_text.into()]
                    }
                    PullMode::Merge => {
                        vec!["merge".into(), "--no-edit".into(), fetched_text.into()]
                    }
                    PullMode::Rebase => vec!["rebase".into(), fetched_text.into()],
                };
                let result = tools
                    .owner
                    .run_git(Some(&root), integrate, trusted, &cancel)
                    .await;
                if let Err(error) = result {
                    if let Some(intent) = integration
                        && let Some(mut conflict) =
                            conflict_state(&tools.owner, &root, &git_dir, &intent, &cancel).await?
                    {
                        let mut intent = intent;
                        intent.tracked_status_hash =
                            Some(tracked_status_hash(&tools.owner, &root, &cancel).await?);
                        write_integration_intent(&git_dir, &intent).await?;
                        conflict.fetched = Some(fetched);
                        return Ok(PullOutcome::Conflict(conflict));
                    }
                    remove_integration_intent(&git_dir).await?;
                    return Err(local_error(error));
                }
                remove_integration_intent(&git_dir).await?;
                let hydration = if options.hydrate
                    && (root.join(".crab").exists() || root.join("crab.toml").exists())
                {
                    match tools
                        .owner
                        .run_crab(
                            Some(&root),
                            ["hydrate", "--all", "--json"],
                            trusted,
                            &cancel,
                        )
                        .await
                    {
                        Ok(_) => HydrationState::Ready,
                        Err(error) => HydrationState::Failed(local_error(error)),
                    }
                } else {
                    HydrationState::NotRequested
                };
                let after = head(&tools.owner, &root, &cancel).await?;
                Ok(if before == after {
                    PullOutcome::UpToDate {
                        head: after,
                        hydration,
                    }
                } else {
                    PullOutcome::Updated {
                        head: after,
                        hydration,
                    }
                })
            })
            .await
    }

    /// Continue the currently conflicted merge or rebase.
    pub fn continue_integration(
        &self,
        id: IntegrationId,
    ) -> crate::Request<'_, PullOutcome, OperationOptions> {
        crate::Request::new(move |operation| Box::pin(self.continue_with_options(id, operation)))
    }

    async fn continue_with_options(
        &self,
        id: IntegrationId,
        operation: OperationOptions,
    ) -> Result<PullOutcome> {
        let tools = self.tools()?.clone();
        let root = self.root.clone();
        let common = self.common_dir.clone();
        let git_dir = self.git_dir.clone();
        let trusted = self.client.0.local_execution_policy.is_trusted();
        self.client
            .0
            .operations
            .run(operation, move |cancel| async move {
                let _lease = crab_remote::local::acquire_repository_lease(&common, &cancel)
                    .await
                    .map_err(local_error)?;
                ensure_supported_mutation(&tools.owner, &root, &cancel).await?;
                let intent = read_integration_intent(&git_dir).await?;
                ensure_integration_id(&intent, &id)?;
                let Some(state) =
                    conflict_state(&tools.owner, &root, &git_dir, &intent, &cancel).await?
                else {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        "no integration is in progress",
                    ));
                };
                let args = match state.kind {
                    IntegrationKind::Merge => vec![OsString::from("commit"), "--no-edit".into()],
                    IntegrationKind::Rebase => vec![OsString::from("rebase"), "--continue".into()],
                };
                let result = tools
                    .owner
                    .run_git_with_env(
                        Some(&root),
                        args,
                        [("GIT_EDITOR", "true")],
                        trusted,
                        &cancel,
                    )
                    .await;
                if let Err(error) = result {
                    if let Some(conflict) =
                        conflict_state(&tools.owner, &root, &git_dir, &intent, &cancel).await?
                    {
                        let mut intent = intent;
                        intent.tracked_status_hash =
                            Some(tracked_status_hash(&tools.owner, &root, &cancel).await?);
                        write_integration_intent(&git_dir, &intent).await?;
                        return Ok(PullOutcome::Conflict(conflict));
                    }
                    return Err(local_error(error));
                }
                remove_integration_intent(&git_dir).await?;
                Ok(PullOutcome::Updated {
                    head: head(&tools.owner, &root, &cancel).await?,
                    hydration: HydrationState::NotRequested,
                })
            })
            .await
    }

    /// Abort the currently conflicted merge or rebase through Git's owner path.
    pub fn abort_integration(
        &self,
        id: IntegrationId,
    ) -> crate::Request<'_, ObjectId, OperationOptions> {
        crate::Request::new(move |operation| Box::pin(self.abort_with_options(id, operation)))
    }

    async fn abort_with_options(
        &self,
        id: IntegrationId,
        operation: OperationOptions,
    ) -> Result<ObjectId> {
        let tools = self.tools()?.clone();
        let root = self.root.clone();
        let common = self.common_dir.clone();
        let git_dir = self.git_dir.clone();
        let trusted = self.client.0.local_execution_policy.is_trusted();
        self.client
            .0
            .operations
            .run(operation, move |cancel| async move {
                let _lease = crab_remote::local::acquire_repository_lease(&common, &cancel)
                    .await
                    .map_err(local_error)?;
                ensure_supported_mutation(&tools.owner, &root, &cancel).await?;
                let intent = read_integration_intent(&git_dir).await?;
                ensure_integration_id(&intent, &id)?;
                let Some(state) =
                    conflict_state(&tools.owner, &root, &git_dir, &intent, &cancel).await?
                else {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        "no integration is in progress",
                    ));
                };
                let args = match state.kind {
                    IntegrationKind::Merge => ["merge", "--abort"],
                    IntegrationKind::Rebase => ["rebase", "--abort"],
                };
                if intent.tracked_status_hash.as_deref()
                    != Some(&tracked_status_hash(&tools.owner, &root, &cancel).await?)
                {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        "tracked files changed after the integration conflict",
                    ));
                }
                tools
                    .owner
                    .run_git(Some(&root), args, trusted, &cancel)
                    .await
                    .map_err(local_error)?;
                remove_integration_intent(&git_dir).await?;
                head(&tools.owner, &root, &cancel).await
            })
            .await
    }

    /// Hydrate exact paths, or all tracked paths when the selection is empty.
    pub fn hydrate(&self, paths: Vec<PathBuf>) -> crate::Request<'_, (), OperationOptions> {
        self.crab_paths("hydrate", paths)
    }

    /// Replace exact hydrated paths with pointers, or all paths when empty.
    pub fn dehydrate(&self, paths: Vec<PathBuf>) -> crate::Request<'_, (), OperationOptions> {
        self.crab_paths("dehydrate", paths)
    }

    /// Warm content for exact paths, or all reachable HEAD content when empty.
    pub fn prefetch_content(
        &self,
        paths: Vec<PathBuf>,
    ) -> crate::Request<'_, (), OperationOptions> {
        self.crab_paths("fetch", paths)
    }

    fn crab_paths<'a>(
        &'a self,
        command: &'static str,
        paths: Vec<PathBuf>,
    ) -> crate::Request<'a, (), OperationOptions> {
        crate::Request::new(move |operation| {
            Box::pin(self.crab_paths_with_options(command, paths, operation))
        })
    }

    async fn crab_paths_with_options(
        &self,
        command: &'static str,
        paths: Vec<PathBuf>,
        operation: OperationOptions,
    ) -> Result<()> {
        validate_paths(&paths)?;
        let tools = self.tools()?.clone();
        let root = self.root.clone();
        let common = self.common_dir.clone();
        let trusted = self.client.0.local_execution_policy.is_trusted();
        self.client
            .0
            .operations
            .run(operation, move |cancel| async move {
                let _lease = crab_remote::local::acquire_repository_lease(&common, &cancel)
                    .await
                    .map_err(local_error)?;
                ensure_supported_mutation(&tools.owner, &root, &cancel).await?;
                let mut args: Vec<OsString> = vec![command.into(), "--json".into()];
                if paths.is_empty() {
                    args.push("--all".into());
                } else {
                    for path in paths {
                        args.push(path.into_os_string());
                    }
                }
                tools
                    .owner
                    .run_crab(Some(&root), args, trusted, &cancel)
                    .await
                    .map(drop)
                    .map_err(local_error)
            })
            .await
    }
}

fn validate_paths(paths: &[PathBuf]) -> Result<()> {
    if paths.len() > 4096 {
        return Err(Error::new(
            ErrorKind::LimitExceeded,
            "local path selection exceeds 4096 entries",
        ));
    }
    for path in paths {
        if path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::RootDir
                )
            })
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "local paths must be repository-relative",
            ));
        }
    }
    Ok(())
}

async fn partition_stage_paths(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    paths: Vec<PathBuf>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut args: Vec<OsString> = vec![
        "check-attr".into(),
        "-z".into(),
        "filter".into(),
        "--".into(),
    ];
    args.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
    let output = tools
        .run_git(Some(root), args, false, cancel)
        .await
        .map_err(local_error)?;
    let fields = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() != paths.len() * 3 {
        return Err(Error::new(
            ErrorKind::Corruption,
            "Git returned an invalid attribute result",
        ));
    }
    let mut crab = std::collections::HashSet::new();
    for fields in fields.chunks_exact(3) {
        if fields[1] != b"filter" {
            return Err(Error::new(
                ErrorKind::Corruption,
                "Git changed the requested attribute name",
            ));
        }
        if fields[2] == b"crab" {
            crab.insert(path_from_git(fields[0])?);
        }
    }
    let (crab_paths, git_paths) = paths.into_iter().partition(|path| crab.contains(path));
    Ok((crab_paths, git_paths))
}

pub(super) fn validate_revision(revision: &str) -> Result<()> {
    if revision.is_empty()
        || revision.starts_with('-')
        || revision.contains('\0')
        || revision.contains("..")
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "invalid local revision",
        ));
    }
    Ok(())
}

fn identity_environment(
    author: &CommitIdentity,
    committer: &CommitIdentity,
) -> Vec<(OsString, OsString)> {
    vec![
        ("GIT_AUTHOR_NAME".into(), author.name().into()),
        ("GIT_AUTHOR_EMAIL".into(), author.email().into()),
        ("GIT_AUTHOR_DATE".into(), author.git_date().into()),
        ("GIT_COMMITTER_NAME".into(), committer.name().into()),
        ("GIT_COMMITTER_EMAIL".into(), committer.email().into()),
        ("GIT_COMMITTER_DATE".into(), committer.git_date().into()),
    ]
}

async fn head(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<ObjectId> {
    let output = tools
        .run_git(Some(root), ["rev-parse", "--verify", "HEAD"], false, cancel)
        .await
        .map_err(local_error)?;
    let oid = std::str::from_utf8(&output.stdout)
        .map_err(|source| {
            Error::with_source(
                ErrorKind::Corruption,
                "Git returned a non-UTF-8 object ID",
                source,
            )
        })?
        .trim();
    ObjectId::from_hex(oid)
}

async fn conflict_state(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    git_dir: &Path,
    intent: &IntegrationIntent,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<ConflictState>> {
    let kind = active_integration_kind(git_dir);
    let Some(kind) = kind else { return Ok(None) };
    if kind != IntegrationKind::from(intent.kind) {
        return Err(Error::new(
            ErrorKind::Conflict,
            "active Git integration does not match the SDK operation",
        ));
    }
    let output = tools
        .run_git(
            Some(root),
            ["diff", "--name-only", "--diff-filter=U", "-z"],
            false,
            cancel,
        )
        .await
        .map_err(local_error)?;
    let paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(path_from_git)
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(ConflictState {
        id: IntegrationId(intent.id.clone()),
        kind,
        paths,
        fetched: Some(ObjectId::from_hex(&intent.fetched)?),
    }))
}

fn ensure_integration_id(intent: &IntegrationIntent, id: &IntegrationId) -> Result<()> {
    if intent.version != 1 || intent.id != id.0 {
        return Err(Error::new(
            ErrorKind::Conflict,
            "integration identity does not match the active SDK operation",
        ));
    }
    Ok(())
}

async fn read_integration_intent(git_dir: &Path) -> Result<IntegrationIntent> {
    let bytes = tokio::fs::read(git_dir.join(INTEGRATION_INTENT))
        .await
        .map_err(|source| {
            Error::with_source(
                ErrorKind::Conflict,
                "no SDK integration is in progress",
                source,
            )
        })?;
    if bytes.len() > 64 * 1024 {
        return Err(Error::new(
            ErrorKind::Corruption,
            "SDK integration intent exceeds 64 KiB",
        ));
    }
    let intent: IntegrationIntent = serde_json::from_slice(&bytes).map_err(|source| {
        Error::with_source(
            ErrorKind::Corruption,
            "SDK integration intent is invalid",
            source,
        )
    })?;
    if intent.version != 1
        || IntegrationId::from_string(&intent.id).is_err()
        || ObjectId::from_hex(&intent.fetched).is_err()
    {
        return Err(Error::new(
            ErrorKind::Corruption,
            "SDK integration intent has invalid identities",
        ));
    }
    Ok(intent)
}

async fn write_integration_intent(git_dir: &Path, intent: &IntegrationIntent) -> Result<()> {
    let bytes = serde_json::to_vec(intent).map_err(|source| {
        Error::with_source(
            ErrorKind::Io,
            "cannot encode SDK integration intent",
            source,
        )
    })?;
    let temporary = git_dir.join(format!(".{INTEGRATION_INTENT}.tmp-{}", intent.id));
    let target = git_dir.join(INTEGRATION_INTENT);
    let mut file = tokio::fs::File::create(&temporary)
        .await
        .map_err(|source| {
            Error::with_source(
                ErrorKind::Io,
                "cannot create SDK integration intent",
                source,
            )
        })?;
    tokio::io::AsyncWriteExt::write_all(&mut file, &bytes)
        .await
        .map_err(|source| {
            Error::with_source(ErrorKind::Io, "cannot write SDK integration intent", source)
        })?;
    file.sync_all().await.map_err(|source| {
        Error::with_source(ErrorKind::Io, "cannot sync SDK integration intent", source)
    })?;
    drop(file);
    tokio::fs::rename(&temporary, &target)
        .await
        .map_err(|source| {
            Error::with_source(
                ErrorKind::Io,
                "cannot publish SDK integration intent",
                source,
            )
        })
}

async fn remove_integration_intent(git_dir: &Path) -> Result<()> {
    match tokio::fs::remove_file(git_dir.join(INTEGRATION_INTENT)).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::with_source(
            ErrorKind::Io,
            "cannot clear SDK integration intent",
            source,
        )),
    }
}

pub(super) async fn validate_integration_intent_on_open(git_dir: &Path) -> Result<()> {
    let path = git_dir.join(INTEGRATION_INTENT);
    if !path.exists() {
        return Ok(());
    }
    let intent = read_integration_intent(git_dir).await?;
    let active = active_integration_kind(git_dir);
    match active {
        None => Ok(()),
        Some(kind) if kind == IntegrationKind::from(intent.kind) => Ok(()),
        Some(_) => Err(Error::new(
            ErrorKind::Conflict,
            "active Git integration does not match its SDK recovery intent",
        )),
    }
}

async fn prepare_pull_integration(git_dir: &Path) -> Result<()> {
    let intent_path = git_dir.join(INTEGRATION_INTENT);
    let active = active_integration_kind(git_dir);
    if !intent_path.exists() {
        return match active {
            None => Ok(()),
            Some(_) => Err(Error::new(
                ErrorKind::Conflict,
                "a Git integration outside this SDK is already in progress",
            )),
        };
    }
    let intent = read_integration_intent(git_dir).await?;
    match active {
        None => remove_integration_intent(git_dir).await,
        Some(kind) if kind == IntegrationKind::from(intent.kind) => Err(Error::new(
            ErrorKind::Conflict,
            "an SDK integration is already in progress",
        )),
        Some(_) => Err(Error::new(
            ErrorKind::Conflict,
            "active Git integration does not match its SDK recovery intent",
        )),
    }
}

fn active_integration_kind(git_dir: &Path) -> Option<IntegrationKind> {
    if git_dir.join("MERGE_HEAD").exists() {
        Some(IntegrationKind::Merge)
    } else if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
        Some(IntegrationKind::Rebase)
    } else {
        None
    }
}

async fn tracked_status_hash(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String> {
    let status = tools
        .run_git(
            Some(root),
            ["status", "--porcelain=v2", "-z", "--untracked-files=no"],
            false,
            cancel,
        )
        .await
        .map_err(local_error)?;
    let tracked = tools
        .run_git(Some(root), ["ls-files", "-z"], false, cancel)
        .await
        .map_err(local_error)?;
    let paths = tracked
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(path_from_git)
        .collect::<Result<std::collections::BTreeSet<_>>>()?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&status.stdout);
    let mut buffer = vec![0; 64 * 1024];
    for path in &paths {
        let raw = path_to_git(path)?;
        hasher.update(&(raw.len() as u64).to_le_bytes());
        hasher.update(&raw);
        let absolute = root.join(path);
        let metadata = match tokio::fs::symlink_metadata(&absolute).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                hasher.update(b"missing");
                continue;
            }
            Err(source) => {
                return Err(Error::with_source(
                    ErrorKind::Io,
                    "cannot inspect tracked worktree path",
                    source,
                ));
            }
        };
        if metadata.file_type().is_symlink() {
            hasher.update(b"symlink");
            let target = tokio::fs::read_link(&absolute).await.map_err(|source| {
                Error::with_source(ErrorKind::Io, "cannot read tracked symbolic link", source)
            })?;
            hasher.update(&path_to_git(&target)?);
        } else if metadata.is_file() {
            hasher.update(b"file");
            let mut file = tokio::fs::File::open(&absolute).await.map_err(|source| {
                Error::with_source(ErrorKind::Io, "cannot read tracked worktree file", source)
            })?;
            loop {
                let read = tokio::io::AsyncReadExt::read(&mut file, &mut buffer)
                    .await
                    .map_err(|source| {
                        Error::with_source(
                            ErrorKind::Io,
                            "cannot hash tracked worktree file",
                            source,
                        )
                    })?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
        } else {
            hasher.update(b"other");
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub(super) async fn git_commit(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    revision: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<ObjectId> {
    let expression = format!("{revision}^{{commit}}");
    let output = tools
        .run_git(
            Some(root),
            ["rev-parse", "--verify", expression.as_str()],
            false,
            cancel,
        )
        .await
        .map_err(local_error)?;
    let oid = std::str::from_utf8(&output.stdout).map_err(|source| {
        Error::with_source(
            ErrorKind::Corruption,
            "Git returned a non-UTF-8 object ID",
            source,
        )
    })?;
    ObjectId::from_hex(oid.trim())
}

fn parse_status(bytes: &[u8]) -> Result<LocalStatus> {
    let mut branch = None;
    let mut head = None;
    let mut entries = Vec::new();
    let mut records = bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        if let Some(value) = record.strip_prefix(b"# branch.head ") {
            branch = (value != b"(detached)").then(|| String::from_utf8_lossy(value).into_owned());
            continue;
        }
        if let Some(value) = record.strip_prefix(b"# branch.oid ") {
            if value != b"(initial)" {
                head = ObjectId::from_hex(std::str::from_utf8(value).map_err(|source| {
                    Error::with_source(
                        ErrorKind::Corruption,
                        "Git status object ID is not UTF-8",
                        source,
                    )
                })?)
                .ok();
            }
            continue;
        }
        if record.starts_with(b"# ") {
            continue;
        }
        let marker = record[0];
        let (xy, path) = match marker {
            b'1' => (field(record, 1)?, field(record, 8)?),
            b'2' => {
                let parsed = (field(record, 1)?, field(record, 9)?);
                let _original = records.next().ok_or_else(|| {
                    Error::new(
                        ErrorKind::Corruption,
                        "Git rename status omitted its source path",
                    )
                })?;
                parsed
            }
            b'u' => (field(record, 1)?, field(record, 10)?),
            b'?' => (b"??".as_slice(), record.get(2..).unwrap_or_default()),
            b'!' => continue,
            _ => {
                return Err(Error::new(
                    ErrorKind::Corruption,
                    "Git returned an unknown status record",
                ));
            }
        };
        let conflicted = marker == b'u' || xy.contains(&b'U');
        entries.push(StatusEntry {
            path: path_from_git(path)?,
            staged: !matches!(xy.first(), None | Some(b'.') | Some(b'?')),
            worktree: !matches!(xy.get(1), None | Some(b'.') | Some(b'?')),
            conflicted,
            untracked: marker == b'?',
        });
    }
    Ok(LocalStatus {
        branch,
        head,
        entries,
    })
}

fn field(record: &[u8], index: usize) -> Result<&[u8]> {
    record
        .splitn(index + 2, |byte| *byte == b' ')
        .nth(index)
        .ok_or_else(|| Error::new(ErrorKind::Corruption, "Git status record is incomplete"))
}

#[cfg(unix)]
fn path_from_git(bytes: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn path_from_git(bytes: &[u8]) -> Result<PathBuf> {
    String::from_utf8(bytes.to_vec())
        .map(PathBuf::from)
        .map_err(|source| {
            Error::with_source(ErrorKind::Corruption, "Git path is not UTF-8", source)
        })
}

#[cfg(unix)]
fn path_to_git(path: &Path) -> Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt as _;
    Ok(path.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn path_to_git(path: &Path) -> Result<Vec<u8>> {
    path.to_str()
        .map(|path| path.as_bytes().to_vec())
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "local path is not valid Unicode"))
}
