use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::{Client, Error, ErrorKind, OperationOptions, RepositoryLocator, Result};

#[path = "local/edit.rs"]
mod edit;
pub use edit::{
    CheckoutOptions, ConflictState, HydrationState, IntegrationId, IntegrationKind,
    LocalCommitOptions, LocalStatus, PullMode, PullOptions, PullOutcome, StageOutcome, StatusEntry,
};
#[path = "local/fetch.rs"]
mod fetch;
#[path = "local/push.rs"]
mod push;
pub use push::{LocalPushOutcome, LocalPushRecoveryToken, PreparedPush, PushOptions, PushRefspec};

/// Trust boundary for repository-provided hooks and executable Git drivers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum LocalExecutionPolicy {
    /// Disable hooks and executable drivers except Crab's validated filter.
    #[default]
    Untrusted,
    /// Permit repository-provided hooks and executable Git drivers.
    Trusted,
}

impl LocalExecutionPolicy {
    pub(crate) const fn is_trusted(self) -> bool {
        matches!(self, Self::Trusted)
    }
}

/// Exact local executables used by the SDK and all child Git processes.
#[derive(Clone)]
pub struct LocalTools {
    owner: crab_remote::local::LocalTools,
}

impl LocalTools {
    /// Validate absolute Git and Crab executable paths without running them.
    pub fn new(git: impl AsRef<Path>, crab: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            owner: crab_remote::local::LocalTools::new(git.as_ref(), crab.as_ref())
                .map_err(local_error)?,
        })
    }
}

/// Validated local tool versions and executable capabilities.
#[derive(Clone, Debug)]
pub struct LocalConfiguration {
    git_version: String,
    crab_version: String,
}

impl LocalConfiguration {
    /// Return the selected Git version.
    #[must_use]
    pub fn git_version(&self) -> &str {
        &self.git_version
    }

    /// Return the selected Crab version.
    #[must_use]
    pub fn crab_version(&self) -> &str {
        &self.crab_version
    }
}

/// Options for a local clone.
#[derive(Clone, Debug)]
pub struct CloneOptions {
    branch: Option<String>,
    depth: Option<u32>,
    lazy: Option<bool>,
    remote_name: String,
}

impl Default for CloneOptions {
    fn default() -> Self {
        Self {
            branch: None,
            depth: None,
            lazy: None,
            remote_name: "origin".to_owned(),
        }
    }
}

impl CloneOptions {
    /// Select a branch by its short name.
    pub fn with_branch(mut self, branch: &str) -> Result<Self> {
        crate::Revision::branch(branch)?;
        self.branch = Some(branch.to_owned());
        Ok(self)
    }

    /// Limit initial history to a nonzero commit depth.
    pub fn with_depth(mut self, depth: u32) -> Result<Self> {
        if depth == 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "clone depth must be nonzero",
            ));
        }
        self.depth = Some(depth);
        Ok(self)
    }

    /// Hydrate tracked content during the initial checkout.
    #[must_use]
    pub fn eager(mut self) -> Self {
        self.lazy = Some(false);
        self
    }

    /// Leave tracked content dehydrated, overriding committed hydration policy.
    #[must_use]
    pub fn lazy(mut self) -> Self {
        self.lazy = Some(true);
        self
    }

    /// Select a nonempty local remote name.
    pub fn with_remote_name(mut self, name: &str) -> Result<Self> {
        validate_remote_name(name)?;
        if name == "." || name == ".." {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid local remote name",
            ));
        }
        self.remote_name = name.to_owned();
        Ok(self)
    }
}

/// Requested shallow-history transition for one fetch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum FetchDepth {
    #[default]
    Preserve,
    Depth(u32),
    Deepen(u32),
    Unshallow,
}

/// Options for fetching Git objects and refs into a local repository.
#[derive(Clone, Debug)]
pub struct FetchOptions {
    remote: String,
    prune: bool,
    tags: Option<bool>,
    depth: FetchDepth,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            remote: "origin".to_owned(),
            prune: false,
            tags: None,
            depth: FetchDepth::Preserve,
        }
    }
}

impl FetchOptions {
    /// Select the configured local remote to fetch.
    pub fn with_remote(mut self, remote: &str) -> Result<Self> {
        validate_remote_name(remote)?;
        self.remote = remote.to_owned();
        Ok(self)
    }

    /// Remove remote-tracking refs deleted by the remote.
    #[must_use]
    pub fn prune(mut self, prune: bool) -> Self {
        self.prune = prune;
        self
    }

    /// Fetch every tag (`true`) or suppress tag following (`false`).
    #[must_use]
    pub fn tags(mut self, tags: bool) -> Self {
        self.tags = Some(tags);
        self
    }

    /// Select a validated shallow-history transition.
    pub fn with_depth(mut self, depth: FetchDepth) -> Result<Self> {
        if matches!(depth, FetchDepth::Depth(0) | FetchDepth::Deepen(0)) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "fetch depth must be nonzero",
            ));
        }
        self.depth = depth;
        Ok(self)
    }
}

/// Observable ref and shallow-state result of a successful fetch.
#[derive(Clone, Debug)]
pub struct FetchOutcome {
    head: Option<crate::ObjectId>,
    shallow: bool,
}

impl FetchOutcome {
    /// Return HEAD after fetch; fetch itself does not integrate it.
    #[must_use]
    pub fn head(&self) -> Option<crate::ObjectId> {
        self.head
    }

    /// Return whether the local repository remains shallow.
    #[must_use]
    pub const fn is_shallow(&self) -> bool {
        self.shallow
    }
}

/// An ordinary local Git repository and working directory.
#[derive(Clone)]
pub struct LocalRepository {
    client: Client,
    root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    locator: Option<RepositoryLocator>,
}

/// An immutable local commit selection that requires no remote access.
#[derive(Clone)]
pub struct LocalSnapshot {
    repository: LocalRepository,
    commit: crate::ObjectId,
}

impl LocalSnapshot {
    /// Return the exact commit captured when the snapshot was opened.
    #[must_use]
    pub const fn commit_id(&self) -> crate::ObjectId {
        self.commit
    }

    /// Return up to `limit` commits in Git's default ancestry order.
    pub fn history(
        &self,
        limit: usize,
    ) -> crate::Request<'_, Vec<crate::ObjectId>, OperationOptions> {
        crate::Request::new(move |operation| Box::pin(self.history_with_options(limit, operation)))
    }

    async fn history_with_options(
        &self,
        limit: usize,
        operation: OperationOptions,
    ) -> Result<Vec<crate::ObjectId>> {
        if limit == 0 || limit > 100_000 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "local history limit must be between 1 and 100000",
            ));
        }
        let tools = self.repository.tools()?.clone();
        let root = self.repository.root.clone();
        let commit = self.commit.to_string();
        self.repository
            .client
            .0
            .operations
            .run(operation, move |cancel| async move {
                let output = tools
                    .owner
                    .run_git(
                        Some(&root),
                        ["rev-list", &format!("--max-count={limit}"), commit.as_str()],
                        false,
                        &cancel,
                    )
                    .await
                    .map_err(local_error)?;
                std::str::from_utf8(&output.stdout)
                    .map_err(|source| {
                        Error::with_source(
                            ErrorKind::Corruption,
                            "Git history output is not UTF-8",
                            source,
                        )
                    })?
                    .lines()
                    .map(crate::ObjectId::from_hex)
                    .collect()
            })
            .await
    }
}

impl LocalRepository {
    /// Return the canonical working-tree root.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Return the canonical Git common directory shared by linked worktrees.
    #[must_use]
    pub fn common_directory(&self) -> &Path {
        &self.common_dir
    }

    /// Resolve one local revision without contacting its remote.
    pub fn snapshot(&self, revision: &str) -> crate::Request<'_, LocalSnapshot, OperationOptions> {
        let revision = revision.to_owned();
        crate::Request::new(move |operation| {
            Box::pin(self.snapshot_with_options(revision, operation))
        })
    }

    async fn snapshot_with_options(
        &self,
        revision: String,
        operation: OperationOptions,
    ) -> Result<LocalSnapshot> {
        edit::validate_revision(&revision)?;
        let tools = self.tools()?.clone();
        let root = self.root.clone();
        let repository = self.clone();
        self.client
            .0
            .operations
            .run(operation, move |cancel| async move {
                let commit = edit::git_commit(&tools.owner, &root, &revision, &cancel).await?;
                Ok(LocalSnapshot { repository, commit })
            })
            .await
    }

    /// Fetch configured refspecs without integrating HEAD.
    pub fn fetch(
        &self,
        options: FetchOptions,
    ) -> crate::Request<'_, FetchOutcome, OperationOptions> {
        crate::Request::new(move |operation| Box::pin(self.fetch_with_options(options, operation)))
    }

    async fn fetch_with_options(
        &self,
        options: FetchOptions,
        operation: OperationOptions,
    ) -> Result<FetchOutcome> {
        let locator = self.locator.clone();
        let tools = self.tools()?.clone();
        let root = self.root.clone();
        let git_dir = self.git_dir.clone();
        let common_dir = self.common_dir.clone();
        let state = self.client.0.clone();
        self.client
            .0
            .operations
            .run(operation, move |cancel| async move {
                let _lease = crab_remote::local::acquire_repository_lease(&common_dir, &cancel)
                    .await
                    .map_err(local_error)?;
                ensure_supported_mutation(&tools.owner, &root, &cancel).await?;
                fetch::fetch_locked(
                    &state,
                    locator,
                    &tools.owner,
                    &root,
                    &git_dir,
                    &common_dir,
                    &options,
                    &cancel,
                )
                .await
            })
            .await
    }

    fn tools(&self) -> Result<&LocalTools> {
        self.client.0.local_tools.as_ref().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "local tools are required for local operations",
            )
        })
    }
}

fn validate_remote_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || name.bytes().any(|byte| byte.is_ascii_control())
        || name.contains([' ', '~', '^', ':', '?', '*', '[', '\\'])
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "invalid local remote name",
        ));
    }
    Ok(())
}

impl Client {
    /// Validate the tools and install repository-local Crab filter configuration.
    pub fn configure_local(
        &self,
        path: impl AsRef<Path>,
    ) -> crate::Request<'_, LocalConfiguration, OperationOptions> {
        let path = path.as_ref().to_owned();
        crate::Request::new(move |options| {
            Box::pin(self.configure_local_with_options(path, options))
        })
    }

    async fn configure_local_with_options(
        &self,
        path: PathBuf,
        options: OperationOptions,
    ) -> Result<LocalConfiguration> {
        let tools = self.0.local_tools.clone().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "local tools are required for local operations",
            )
        })?;
        self.0
            .operations
            .run(options, move |cancel| async move {
                let handshake = tools.owner.handshake(&cancel).await.map_err(local_error)?;
                let root = git_path(&tools.owner, &path, "--show-toplevel", &cancel).await?;
                verify_object_format(&tools.owner, &root, &cancel).await?;
                let common = git_path(&tools.owner, &root, "--git-common-dir", &cancel).await?;
                let _lease = crab_remote::local::acquire_repository_lease(&common, &cancel)
                    .await
                    .map_err(local_error)?;
                tools
                    .owner
                    .configure_filters(&root, &cancel)
                    .await
                    .map_err(local_error)?;
                Ok(LocalConfiguration {
                    git_version: handshake.git_version,
                    crab_version: handshake.capabilities.crab_version,
                })
            })
            .await
    }

    pub(crate) async fn open_local_with_options(
        &self,
        path: PathBuf,
        options: OperationOptions,
    ) -> Result<LocalRepository> {
        let tools = self.0.local_tools.clone().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "local tools are required for local operations",
            )
        })?;
        let client = self.clone();
        self.0
            .operations
            .run(options, move |cancel| async move {
                tools.owner.handshake(&cancel).await.map_err(local_error)?;
                discover_local(client, &tools.owner, &path, None, &cancel).await
            })
            .await
    }

    /// Clone through the selected tools into a collision-safe destination.
    pub(crate) fn clone_repository(
        &self,
        locator: RepositoryLocator,
        destination: impl AsRef<Path>,
        options: CloneOptions,
    ) -> crate::Request<'_, LocalRepository, OperationOptions> {
        let destination = destination.as_ref().to_owned();
        crate::Request::new(move |operation| {
            Box::pin(self.clone_with_options(locator, destination, options, operation))
        })
    }

    async fn clone_with_options(
        &self,
        locator: RepositoryLocator,
        destination: PathBuf,
        options: CloneOptions,
        operation: OperationOptions,
    ) -> Result<LocalRepository> {
        let tools = self.0.local_tools.clone().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "local tools are required for local operations",
            )
        })?;
        let state = self.0.clone();
        let client = self.clone();
        let trusted = self.0.local_execution_policy.is_trusted();
        self.0
            .operations
            .run(operation, move |cancel| async move {
                tools.owner.handshake(&cancel).await.map_err(local_error)?;
                if destination.exists() {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        "clone destination already exists",
                    ));
                }
                let parent = destination.parent().ok_or_else(|| {
                    Error::new(ErrorKind::InvalidInput, "clone destination has no parent")
                })?;
                let parent = dunce::canonicalize(parent).map_err(|source| {
                    Error::with_source(ErrorKind::Io, "cannot resolve clone parent", source)
                })?;
                let staging = tempfile::Builder::new()
                    .prefix(".crab-sdk-clone-")
                    .tempdir_in(&parent)
                    .map_err(|source| {
                        Error::with_source(ErrorKind::Io, "cannot create clone staging", source)
                    })?;
                let staged = staging.path().join("repository");
                if let Some(url) = locator.http_url() {
                    clone_http_repository(
                        &tools.owner,
                        &parent,
                        &staged,
                        url,
                        &options,
                        trusted,
                        &cancel,
                    )
                    .await?;
                    if destination.exists() {
                        return Err(Error::new(
                            ErrorKind::Conflict,
                            "clone destination appeared during clone",
                        ));
                    }
                    tokio::fs::rename(&staged, &destination)
                        .await
                        .map_err(|source| {
                            Error::with_source(
                                ErrorKind::Io,
                                "cannot publish clone destination",
                                source,
                            )
                        })?;
                    return discover_local(client, &tools.owner, &destination, None, &cancel).await;
                }
                let resolved = state.resolve_repository(&locator, &cancel).await?;
                let (remote, environment, managed_token_cache) = if resolved.managed {
                    #[cfg(feature = "managed")]
                    {
                        let managed = state.managed.as_ref().ok_or_else(|| {
                            Error::new(
                                ErrorKind::InvalidInput,
                                "managed service options are required",
                            )
                        })?;
                        if !managed.local_compatible {
                            return Err(Error::new(
                                ErrorKind::UnsupportedCapability,
                                "explicit managed bearer tokens cannot be delegated to Crab filters",
                            ));
                        }
                        (
                            locator.managed_url().ok_or_else(|| {
                                Error::new(
                                    ErrorKind::Corruption,
                                    "managed repository lost its canonical URL",
                                )
                            })?,
                            Vec::new(),
                            Some(managed.token_cache_directory.as_path()),
                        )
                    }
                    #[cfg(not(feature = "managed"))]
                    return Err(Error::new(
                        ErrorKind::UnsupportedCapability,
                        "managed repository support is disabled",
                    ));
                } else {
                    let transport = state.local_store()?.local_transport(&locator)?;
                    (transport.remote, transport.environment, None)
                };
                let initial_branch = options.branch.as_deref().unwrap_or("main");
                let initial_branch_arg = format!("--initial-branch={initial_branch}");
                let staged_path = staged.to_str().ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        "clone destination must be valid Unicode",
                    )
                })?;
                tools
                    .owner
                    .run_git(
                        Some(&parent),
                        ["init", "--quiet", initial_branch_arg.as_str(), staged_path],
                        false,
                        &cancel,
                    )
                    .await
                    .map_err(local_error)?;
                tools
                    .owner
                    .run_git(
                        Some(&staged),
                        [
                            "remote",
                            "add",
                            options.remote_name.as_str(),
                            remote.as_str(),
                        ],
                        false,
                        &cancel,
                    )
                    .await
                    .map_err(local_error)?;
                tools
                    .owner
                    .configure_filters(&staged, &cancel)
                    .await
                    .map_err(local_error)?;
                let git_dir = staged.join(".git");
                let layout = crab_storage::StoreLayout::new(
                    resolved.store.clone(),
                    resolved.prefix.clone(),
                );
                let snapshot = crab_remote::transfer::fetch_snapshot(
                    &resolved.store,
                    &layout,
                    &git_dir.join("objects").join("pack"),
                    &cancel,
                )
                .await
                .map_err(transfer_error)?;
                let fetch = FetchOptions {
                    remote: options.remote_name.clone(),
                    prune: false,
                    tags: Some(true),
                    depth: options
                        .depth
                        .map_or(FetchDepth::Preserve, FetchDepth::Depth),
                };
                fetch::apply_fetch_state(
                    &tools.owner,
                    &staged,
                    &git_dir,
                    &git_dir,
                    &fetch,
                    &snapshot,
                    &cancel,
                )
                .await?;
                let head = cloned_head(options.branch.as_deref(), &snapshot)?;
                let hydration = clone_hydration(
                    &tools.owner,
                    &staged,
                    head.target.as_deref(),
                    options.lazy,
                    &cancel,
                )
                .await?;
                configure_clone(&staged, hydration.lazy(), managed_token_cache).await?;
                checkout_cloned_head(
                    &tools.owner,
                    &staged,
                    &environment,
                    &fetch.remote,
                    &head,
                    trusted,
                    &cancel,
                )
                .await?;
                hydrate_clone_patterns(
                    &tools.owner,
                    &staged,
                    hydration.patterns(),
                    &environment,
                    trusted,
                    &cancel,
                )
                .await?;
                fetch::verify_repository(&tools.owner, &staged, &cancel).await?;
                if destination.exists() {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        "clone destination appeared during clone",
                    ));
                }
                tokio::fs::rename(&staged, &destination)
                    .await
                    .map_err(|source| {
                        Error::with_source(
                            ErrorKind::Io,
                            "cannot publish clone destination",
                            source,
                        )
                    })?;
                discover_local(client, &tools.owner, &destination, Some(locator), &cancel).await
            })
            .await
    }
}

async fn clone_http_repository(
    tools: &crab_remote::local::LocalTools,
    parent: &Path,
    staged: &Path,
    url: &str,
    options: &CloneOptions,
    trusted: bool,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    let mut args = vec![
        OsString::from("clone"),
        OsString::from("--no-checkout"),
        OsString::from("--origin"),
        OsString::from(&options.remote_name),
    ];
    if let Some(branch) = &options.branch {
        args.extend([OsString::from("--branch"), OsString::from(branch)]);
    }
    if let Some(depth) = options.depth {
        args.extend([OsString::from("--depth"), OsString::from(depth.to_string())]);
    }
    args.extend([OsString::from(url), staged.as_os_str().to_owned()]);
    tools
        .run_git(Some(parent), args, trusted, cancel)
        .await
        .map_err(local_error)?;
    verify_object_format(tools, staged, cancel).await?;
    tools
        .configure_filters(staged, cancel)
        .await
        .map_err(local_error)?;
    let hydration = clone_hydration(tools, staged, Some("HEAD"), options.lazy, cancel).await?;
    configure_clone(staged, hydration.lazy(), None).await?;
    match tools
        .run_git(
            Some(staged),
            ["show-ref", "--head", "--quiet"],
            false,
            cancel,
        )
        .await
    {
        Ok(_) => {
            tools
                .run_git(Some(staged), ["reset", "--hard", "HEAD"], trusted, cancel)
                .await
                .map_err(local_error)?;
        }
        Err(crab_remote::local::LocalError::Exit(output)) if output.status.code() == Some(1) => {}
        Err(source) => return Err(local_error(source)),
    }
    hydrate_clone_patterns(tools, staged, hydration.patterns(), &[], trusted, cancel).await?;
    fetch::verify_repository(tools, staged, cancel).await
}

async fn discover_local(
    client: Client,
    tools: &crab_remote::local::LocalTools,
    path: &Path,
    locator: Option<RepositoryLocator>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<LocalRepository> {
    let root = git_path(tools, path, "--show-toplevel", cancel).await?;
    verify_object_format(tools, &root, cancel).await?;
    let git_dir = git_path(tools, &root, "--git-dir", cancel).await?;
    let common_dir = git_path(tools, &root, "--git-common-dir", cancel).await?;
    let attributes = match tokio::fs::read(root.join(".gitattributes")).await {
        Ok(attributes) => attributes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(source) => {
            return Err(Error::with_source(
                ErrorKind::Io,
                "cannot read repository attributes",
                source,
            ));
        }
    };
    if attributes.split(|byte| *byte == b'\n').any(|line| {
        line.windows(b"filter=crab".len())
            .any(|part| part == b"filter=crab")
    }) && !tools
        .filters_match(&root, cancel)
        .await
        .map_err(local_error)?
    {
        return Err(Error::new(
            ErrorKind::LocalSetupRequired,
            "repository requires Crab filter configuration",
        ));
    }
    fetch::validate_fetch_intent_on_open(tools, &root, &git_dir, cancel).await?;
    edit::validate_integration_intent_on_open(&git_dir).await?;
    let locator = match locator {
        Some(locator) => Some(locator),
        None => discover_locator(&client, tools, &root, cancel).await?,
    };
    Ok(LocalRepository {
        client,
        root,
        git_dir,
        common_dir,
        locator,
    })
}

async fn verify_object_format(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    let output = tools
        .run_git(
            Some(root),
            ["rev-parse", "--show-object-format"],
            false,
            cancel,
        )
        .await
        .map_err(local_error)?;
    if output.stdout == b"sha1\n" || output.stdout == b"sha1\r\n" {
        return Ok(());
    }
    Err(Error::new(
        ErrorKind::UnsupportedCapability,
        "SHA-256 Git repositories are not supported",
    ))
}

pub(super) async fn ensure_supported_mutation(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    let output = match tools
        .run_git(
            Some(root),
            [
                "config",
                "--get-regexp",
                "^(extensions\\.partialclone|remote\\..*\\.promisor|core\\.sparsecheckout|core\\.sparsecheckoutcone)$",
            ],
            false,
            cancel,
        )
        .await
    {
        Ok(output) => output,
        Err(crab_remote::local::LocalError::Exit(output)) if output.status.code() == Some(1) => {
            return Ok(());
        }
        Err(error) => return Err(local_error(error)),
    };
    for line in output.stdout.split(|byte| *byte == b'\n') {
        let Some(separator) = line.iter().position(u8::is_ascii_whitespace) else {
            continue;
        };
        let key = &line[..separator];
        let value = line[separator..]
            .iter()
            .copied()
            .skip_while(u8::is_ascii_whitespace)
            .collect::<Vec<_>>();
        let partial_clone = key.eq_ignore_ascii_case(b"extensions.partialclone");
        let enabled = [b"true".as_slice(), b"yes", b"on", b"1"]
            .iter()
            .any(|enabled| value.eq_ignore_ascii_case(enabled));
        if partial_clone || enabled {
            return Err(Error::new(
                ErrorKind::UnsupportedCapability,
                "partial-clone and sparse-checkout mutations are not supported",
            ));
        }
    }
    Ok(())
}

async fn discover_locator(
    client: &Client,
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<RepositoryLocator>> {
    let output = match tools
        .run_git(Some(root), ["remote", "get-url", "origin"], false, cancel)
        .await
    {
        Ok(output) => output,
        Err(crab_remote::local::LocalError::Exit(_)) => return Ok(None),
        Err(error) => return Err(local_error(error)),
    };
    let remote = std::str::from_utf8(&output.stdout)
        .map_err(|source| {
            Error::with_source(
                ErrorKind::Corruption,
                "local remote URL is not UTF-8",
                source,
            )
        })?
        .trim();
    #[cfg(feature = "managed")]
    if let Some(managed) = client.0.managed.as_ref() {
        match managed.resolver.classify(remote) {
            Ok(crab_git::RepositoryLocator::Managed(repository)) => {
                return Ok(Some(RepositoryLocator::from_managed_owner(repository)));
            }
            Ok(crab_git::RepositoryLocator::Direct(_)) => {}
            Err(source) => return Err(crate::managed_impl::managed_repository_error(source)),
        }
    }
    let Some(store) = client.0.local_store.as_ref() else {
        return Ok(None);
    };
    match store.locator_from_remote(remote) {
        Ok(locator) => Ok(Some(locator)),
        Err(error) if error.kind() == ErrorKind::UnsupportedCapability => Ok(None),
        Err(error) => Err(error),
    }
}

async fn git_path(
    tools: &crab_remote::local::LocalTools,
    path: &Path,
    selector: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<PathBuf> {
    let output = tools
        .run_git(
            Some(path),
            ["rev-parse", "--path-format=absolute", selector],
            false,
            cancel,
        )
        .await
        .map_err(local_error)?;
    let raw = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    if raw.is_empty() || raw.contains(&0) {
        return Err(Error::new(
            ErrorKind::Corruption,
            "Git returned an invalid repository path",
        ));
    }
    git_output_path(raw)?.canonicalize().map_err(|source| {
        Error::with_source(ErrorKind::Io, "cannot resolve Git repository path", source)
    })
}

#[cfg(unix)]
fn git_output_path(raw: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;
    Ok(PathBuf::from(OsString::from_vec(raw.to_vec())))
}

#[cfg(windows)]
fn git_output_path(raw: &[u8]) -> Result<PathBuf> {
    let raw = std::str::from_utf8(raw).map_err(|source| {
        Error::with_source(ErrorKind::Corruption, "Git path is not UTF-8", source)
    })?;
    Ok(PathBuf::from(raw))
}

async fn configure_clone(root: &Path, lazy: bool, token_cache: Option<&Path>) -> Result<()> {
    let directory = root.join(".crab");
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|source| Error::with_source(ErrorKind::Io, "cannot create .crab", source))?;
    let mut configuration = format!("# Crab local settings\n[checkout]\nlazy = {lazy}\n");
    if let Some(token_cache) = token_cache {
        let token_cache = token_cache.to_str().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "managed token-cache directory must be valid Unicode for Crab filters",
            )
        })?;
        configuration.push_str("\n[auth]\ntoken_cache_path = ");
        configuration.push_str(&toml::Value::String(token_cache.to_owned()).to_string());
        configuration.push('\n');
    }
    tokio::fs::write(directory.join("local.toml"), configuration)
        .await
        .map_err(|source| {
            Error::with_source(ErrorKind::Io, "cannot configure lazy checkout", source)
        })?;
    let exclude = root.join(".git").join("info").join("exclude");
    let mut body = match tokio::fs::read(&exclude).await {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(source) => {
            return Err(Error::with_source(
                ErrorKind::Io,
                "cannot read local excludes",
                source,
            ));
        }
    };
    if !body
        .split(|byte| *byte == b'\n')
        .any(|line| line == b"/.crab/")
    {
        if !body.is_empty() && !body.ends_with(b"\n") {
            body.push(b'\n');
        }
        body.extend_from_slice(b"/.crab/\n");
        atomic_write(&exclude, &body, "local excludes").await?;
    }
    Ok(())
}

struct ClonedHead {
    branch: String,
    target: Option<String>,
}

fn cloned_head(
    requested_branch: Option<&str>,
    snapshot: &crab_remote::transfer::FetchSnapshot,
) -> Result<ClonedHead> {
    let branch = requested_branch
        .map(|branch| format!("refs/heads/{branch}"))
        .unwrap_or_else(|| snapshot.head.clone());
    let target = snapshot.refs.get(&branch).cloned();
    if requested_branch.is_some() && target.is_none() {
        return Err(Error::new(
            ErrorKind::NotFound,
            "requested clone branch does not exist",
        ));
    }
    Ok(ClonedHead { branch, target })
}

async fn clone_hydration(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    revision: Option<&str>,
    explicit_lazy: Option<bool>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<crab_remote::config::CheckoutHydration> {
    let repository = if let Some(revision) = revision {
        let entry = tools
            .run_git(
                Some(root),
                ["ls-tree", "--name-only", "-z", revision, "--", "crab.toml"],
                false,
                cancel,
            )
            .await
            .map_err(local_error)?;
        if entry.stdout.is_empty() {
            None
        } else {
            if entry.stdout != b"crab.toml\0" {
                return Err(Error::new(
                    ErrorKind::Corruption,
                    "Git returned an invalid crab.toml tree entry",
                ));
            }
            let selector = format!("{revision}:crab.toml");
            let output = tools
                .run_git(Some(root), ["show", selector.as_str()], false, cancel)
                .await
                .map_err(local_error)?;
            let document = std::str::from_utf8(&output.stdout).map_err(|source| {
                Error::with_source(
                    ErrorKind::Corruption,
                    "committed crab.toml is not UTF-8",
                    source,
                )
            })?;
            crab_remote::config::parse_repository_hydration(document).map_err(|source| {
                Error::with_source(
                    ErrorKind::Corruption,
                    "committed crab.toml has invalid checkout configuration",
                    source,
                )
            })?
        }
    } else {
        None
    };
    Ok(crab_remote::config::resolve_checkout_hydration(
        explicit_lazy,
        &[],
        repository.as_ref(),
    ))
}

async fn hydrate_clone_patterns(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    patterns: &[String],
    environment: &[(OsString, OsString)],
    trusted: bool,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    if patterns.is_empty() {
        return Ok(());
    }
    let mut args = vec![
        OsString::from("hydrate"),
        OsString::from("--json"),
        OsString::from("--"),
    ];
    args.extend(patterns.iter().map(OsString::from));
    tools
        .run_crab_with_env(
            Some(root),
            args,
            environment.iter().cloned(),
            trusted,
            cancel,
        )
        .await
        .map(drop)
        .map_err(local_error)
}

async fn checkout_cloned_head(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    environment: &[(OsString, OsString)],
    remote: &str,
    head: &ClonedHead,
    trusted_execution: bool,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    let branch = &head.branch;
    let Some(target) = &head.target else {
        tools
            .run_git(
                Some(root),
                ["symbolic-ref", "HEAD", branch.as_str()],
                false,
                cancel,
            )
            .await
            .map_err(local_error)?;
        return Ok(());
    };
    let short = branch.strip_prefix("refs/heads/").ok_or_else(|| {
        Error::new(
            ErrorKind::Corruption,
            "remote symbolic HEAD does not name a branch",
        )
    })?;
    let zero = "0".repeat(40);
    tools
        .run_git(
            Some(root),
            [
                "update-ref",
                branch.as_str(),
                target.as_str(),
                zero.as_str(),
            ],
            false,
            cancel,
        )
        .await
        .map_err(local_error)?;
    tools
        .run_git(
            Some(root),
            ["symbolic-ref", "HEAD", branch.as_str()],
            false,
            cancel,
        )
        .await
        .map_err(local_error)?;
    for (key, value) in [
        (format!("branch.{short}.remote"), remote.to_owned()),
        (format!("branch.{short}.merge"), branch.clone()),
    ] {
        tools
            .run_git(
                Some(root),
                ["config", "--local", key.as_str(), value.as_str()],
                false,
                cancel,
            )
            .await
            .map_err(local_error)?;
    }
    tools
        .run_git_with_env(
            Some(root),
            ["reset", "--hard", "HEAD"],
            environment.iter().cloned(),
            trusted_execution,
            cancel,
        )
        .await
        .map(drop)
        .map_err(local_error)
}

async fn atomic_write(path: &Path, body: &[u8], label: &'static str) -> Result<()> {
    let temporary = path.with_extension(format!("crab-sdk-{}.tmp", std::process::id()));
    tokio::fs::write(&temporary, body)
        .await
        .map_err(|source| Error::with_source(ErrorKind::Io, label, source))?;
    tokio::fs::rename(&temporary, path)
        .await
        .map_err(|source| Error::with_source(ErrorKind::Io, label, source))
}

fn local_error(source: crab_remote::local::LocalError) -> Error {
    let kind = match source {
        crab_remote::local::LocalError::Cancelled => ErrorKind::Cancelled,
        crab_remote::local::LocalError::Capability(_)
        | crab_remote::local::LocalError::CapabilityJson(_)
        | crab_remote::local::LocalError::GitVersion { .. }
        | crab_remote::local::LocalError::GitVersionSyntax => ErrorKind::UnsupportedCapability,
        crab_remote::local::LocalError::Exit(_) => ErrorKind::Conflict,
        crab_remote::local::LocalError::OutputLimit => ErrorKind::LimitExceeded,
        _ => ErrorKind::Io,
    };
    Error::with_source(kind, "local repository operation failed", source)
}

fn transfer_error(source: crab_remote::transfer::Error) -> Error {
    let kind = match source {
        crab_remote::transfer::Error::Cancelled => ErrorKind::Cancelled,
        crab_remote::transfer::Error::Metadata(_) | crab_remote::transfer::Error::Pack(_) => {
            ErrorKind::Corruption
        }
        crab_remote::transfer::Error::Storage(ref error) => {
            crate::remote_error::storage_kind(error)
        }
        crab_remote::transfer::Error::Scratch(_) | crab_remote::transfer::Error::Installer(_) => {
            ErrorKind::Io
        }
    };
    Error::with_source(kind, "direct repository transfer failed", source)
}
