use serde::{Deserialize, Serialize};

use super::{LocalRepository, ensure_supported_mutation, local_error};

mod content;
mod git;
use crate::{
    Client, CommitReceipt, Error, ErrorKind, MutationOutcome, ObjectId, OperationOptions,
    Readiness, RecoveryToken, RefBatch, RefUpdate, Result, WritePolicy,
};

const LOCAL_PUSH_TOKEN_VERSION: u32 = 3;
const MAX_LOCAL_PUSH_TOKEN_BYTES: usize = 1024 * 1024;

/// Atomic direct-push selection and recovery policy.
#[derive(Clone, Debug)]
pub struct PushOptions {
    remote: String,
    destination: Option<String>,
    refspecs: Vec<PushRefspec>,
    policy: WritePolicy,
    dry_run: bool,
}

impl Default for PushOptions {
    fn default() -> Self {
        Self {
            remote: "origin".to_owned(),
            destination: None,
            refspecs: Vec::new(),
            policy: WritePolicy::FastForward,
            dry_run: false,
        }
    }
}

impl PushOptions {
    /// Push the current branch to its configured upstream.
    #[must_use]
    pub fn current_branch() -> Self {
        Self::default()
    }

    /// Select the configured local remote to publish to.
    pub fn with_remote(mut self, remote: &str) -> Result<Self> {
        super::validate_remote_name(remote)?;
        self.remote = remote.to_owned();
        Ok(self)
    }

    /// Select a fully qualified branch or tag destination.
    pub fn with_destination(mut self, destination: &str) -> Result<Self> {
        validate_destination(destination)?;
        self.destination = Some(destination.to_owned());
        Ok(self)
    }

    /// Replace current-branch selection with an explicit atomic ref batch.
    pub fn with_refspecs(mut self, refspecs: Vec<PushRefspec>) -> Result<Self> {
        if refspecs.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "at least one push refspec is required",
            ));
        }
        let mut destinations = std::collections::BTreeSet::new();
        for refspec in &refspecs {
            if !destinations.insert(refspec.destination.as_str()) {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "push destinations must be unique",
                ));
            }
        }
        self.destination = None;
        self.refspecs = refspecs;
        Ok(self)
    }

    /// Allow a rewrite while retaining the exact prepared remote value.
    #[must_use]
    pub fn with_policy(mut self, policy: WritePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Validate and report the push without requesting publication.
    #[must_use]
    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }
}

/// One local source or deletion mapped to a fully qualified remote ref.
#[derive(Clone, Debug)]
pub struct PushRefspec {
    source: Option<String>,
    destination: String,
}

impl PushRefspec {
    /// Publish one local branch, tag, or exact object to a remote branch or tag.
    pub fn update(source: &str, destination: &str) -> Result<Self> {
        validate_source(source)?;
        validate_destination(destination)?;
        Ok(Self {
            source: Some(source.to_owned()),
            destination: destination.to_owned(),
        })
    }

    /// Delete one remote branch or tag under its prepared expected value.
    pub fn delete(destination: &str) -> Result<Self> {
        validate_destination(destination)?;
        Ok(Self {
            source: None,
            destination: destination.to_owned(),
        })
    }
}

/// Durable local push identity containing no credentials or prepared file bytes.
#[derive(Clone, Debug)]
pub struct LocalPushRecoveryToken(LocalPushBinding);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LocalPushBinding {
    version: u32,
    remote: String,
    updates: Vec<LocalPushUpdate>,
    dry_run: bool,
    policy: WritePolicy,
    trusted_execution: bool,
    backend: LocalPushBackend,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum LocalPushBackend {
    Crab {
        remote_recovery: String,
    },
    LocalOnly {
        repository: String,
        remote_url_hash: String,
    },
    Git {
        repository: String,
        remote_url_hash: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LocalPushUpdate {
    source: Option<String>,
    destination: String,
    expected: Option<String>,
    target: Option<String>,
}

impl LocalPushRecoveryToken {
    /// Encode the complete push identity for persistence before execution.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(&self.0).map_err(|source| {
            Error::with_source(ErrorKind::Io, "cannot encode local push token", source)
        })
    }

    /// Decode and validate a bounded local push identity.
    pub fn from_json(json: &str) -> Result<Self> {
        if json.len() > MAX_LOCAL_PUSH_TOKEN_BYTES {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "local push token exceeds 1 MiB",
            ));
        }
        let binding: LocalPushBinding = serde_json::from_str(json).map_err(|source| {
            Error::with_source(ErrorKind::InvalidInput, "invalid local push token", source)
        })?;
        if binding.version != LOCAL_PUSH_TOKEN_VERSION
            || binding.remote.is_empty()
            || binding.updates.is_empty()
        {
            return Err(Error::new(
                ErrorKind::UnsupportedCapability,
                "unsupported local push token version",
            ));
        }
        let mut destinations = std::collections::BTreeSet::new();
        for update in &binding.updates {
            validate_destination(&update.destination)?;
            if !destinations.insert(update.destination.as_str()) {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "local push token contains duplicate destinations",
                ));
            }
            match (&update.source, &update.target) {
                (Some(source), Some(target)) => {
                    validate_source(source)?;
                    ObjectId::from_hex(target)?;
                }
                (None, None) => {}
                _ => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "local push token contains an incomplete update",
                    ));
                }
            }
            if let Some(expected) = &update.expected {
                ObjectId::from_hex(expected)?;
            }
        }
        match &binding.backend {
            LocalPushBackend::Crab { remote_recovery } => {
                RecoveryToken::from_json(remote_recovery)?;
            }
            LocalPushBackend::LocalOnly {
                repository,
                remote_url_hash,
            }
            | LocalPushBackend::Git {
                repository,
                remote_url_hash,
            } => {
                if repository.is_empty()
                    || remote_url_hash.len() != 64
                    || !remote_url_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "local Git push token has an invalid repository binding",
                    ));
                }
            }
        }
        Ok(Self(binding))
    }

    fn remote_recovery(&self) -> Result<Option<RecoveryToken>> {
        match &self.0.backend {
            LocalPushBackend::Crab { remote_recovery } => {
                RecoveryToken::from_json(remote_recovery).map(Some)
            }
            LocalPushBackend::LocalOnly { .. } | LocalPushBackend::Git { .. } => Ok(None),
        }
    }
}

/// Prepared push holding its local repository and durable identity.
pub struct PreparedPush {
    client: Client,
    recovery: LocalPushRecoveryToken,
    mutation: Option<crate::client::mutation::PreparedMutation>,
}

impl PreparedPush {
    /// Return the token that must be persisted before execution.
    #[must_use]
    pub fn recovery_token(&self) -> &LocalPushRecoveryToken {
        &self.recovery
    }

    /// Execute this exact source OID and destination under its durable plan ID.
    pub async fn execute(self, options: OperationOptions) -> Result<LocalPushOutcome> {
        let recovery = self.recovery;
        if recovery.0.dry_run {
            validate_push(&self.client, &recovery, options).await?;
            return Ok(LocalPushOutcome::DryRun {
                destinations: recovery
                    .0
                    .updates
                    .iter()
                    .map(|update| update.destination.clone())
                    .collect(),
            });
        }
        match recovery.remote_recovery()? {
            Some(_) => {
                let mutation = self.mutation.ok_or_else(|| {
                    Error::new(
                        ErrorKind::Corruption,
                        "prepared local push lost its Crab mutation",
                    )
                })?;
                outcome(mutation.execute(options).await?, recovery)
            }
            None => git::execute(&self.client, recovery, false, options).await,
        }
    }
}

/// Proven or unresolved result of a local prepared push.
#[derive(Debug)]
#[non_exhaustive]
pub enum LocalPushOutcome {
    Committed {
        receipt: Box<CommitReceipt>,
        readiness: Readiness,
    },
    DryRun {
        destinations: Vec<String>,
    },
    TransportCommitted {
        destinations: Vec<String>,
    },
    Indeterminate {
        recovery: LocalPushRecoveryToken,
    },
}

impl LocalRepository {
    /// Prepare an exact atomic push and durable historical recovery identity.
    pub fn prepare_push(
        &self,
        options: PushOptions,
    ) -> crate::Request<'_, PreparedPush, OperationOptions> {
        crate::Request::new(move |operation| {
            Box::pin(self.prepare_push_with_options(options, operation))
        })
    }

    async fn prepare_push_with_options(
        &self,
        options: PushOptions,
        operation: OperationOptions,
    ) -> Result<PreparedPush> {
        let locator = self.locator.clone();
        let tools = self.tools()?.clone();
        let root = self.root.clone();
        let common = self.common_dir.clone();
        let state = self.client.0.clone();
        let client = self.client.clone();
        let trusted = self.client.0.local_execution_policy.is_trusted();
        let mutation_options = operation.clone();
        self.client
            .0
            .operations
            .run(operation, move |cancel| async move {
                let _lease = crab_remote::local::acquire_repository_lease(&common, &cancel)
                    .await
                    .map_err(local_error)?;
                ensure_supported_mutation(&tools.owner, &root, &cancel).await?;
                let refspecs = if options.refspecs.is_empty() {
                    let source = git_line(
                        &tools.owner,
                        &root,
                        ["symbolic-ref", "--quiet", "HEAD"],
                        &cancel,
                    )
                    .await?;
                    validate_source(&source)?;
                    let destination = match options.destination {
                        Some(destination) => destination,
                        None => {
                            upstream_destination(
                                &tools.owner,
                                &root,
                                &source,
                                &options.remote,
                                &cancel,
                            )
                            .await?
                        }
                    };
                    vec![PushRefspec {
                        source: Some(source),
                        destination,
                    }]
                } else {
                    options.refspecs
                };
                let crab = locator
                    .as_ref()
                    .filter(|locator| locator.http_url().is_none());
                let advertised = if let Some(locator) = crab {
                    let resolved = state.resolve_repository(locator, &cancel).await?;
                    let layout = crab_storage::StoreLayout::new(
                        resolved.store.clone(),
                        resolved.prefix,
                    );
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Err(Error::new(ErrorKind::Cancelled, "local push preparation cancelled")),
                        result = crab_metadata::manifest_store::read_repository_snapshot(&resolved.store, &layout) => {
                            result.map_err(metadata_error)?.journal.refs
                        }
                    }
                } else {
                    git::remote_refs(&tools.owner, &root, &options.remote, &cancel).await?
                };
                let mut edits = Vec::with_capacity(refspecs.len());
                let mut local_updates = Vec::with_capacity(refspecs.len());
                for refspec in refspecs {
                    let expected = advertised
                        .get(&refspec.destination)
                        .map(|oid| ObjectId::from_hex(oid))
                        .transpose()?;
                    let target = match &refspec.source {
                        Some(source) => Some(ObjectId::from_hex(
                            &git_line(
                                &tools.owner,
                                &root,
                                ["rev-parse", "--verify", source],
                                &cancel,
                            )
                            .await?,
                        )?),
                        None => None,
                    };
                    let edit = match (expected, target) {
                        (Some(expected), Some(target)) => {
                            if matches!(options.policy, WritePolicy::FastForward) {
                                if refspec.destination.starts_with("refs/heads/") {
                                    tools
                                        .owner
                                        .run_git(
                                            Some(&root),
                                            [
                                                "merge-base",
                                                "--is-ancestor",
                                                &expected.to_string(),
                                                &target.to_string(),
                                            ],
                                            false,
                                            &cancel,
                                        )
                                        .await
                                        .map_err(local_error)?;
                                } else if expected != target {
                                    return Err(Error::new(
                                        ErrorKind::Conflict,
                                        "tag replacement requires force with lease",
                                    ));
                                }
                            }
                            RefUpdate::update(&refspec.destination, expected, target)?
                        }
                        (None, Some(target)) => RefUpdate::create(&refspec.destination, target)?,
                        (Some(expected), None) => {
                            RefUpdate::delete(&refspec.destination, expected)?
                        }
                        (None, None) => {
                            return Err(Error::new(
                                ErrorKind::NotFound,
                                "cannot delete a missing remote ref",
                            ));
                        }
                    };
                    edits.push(edit);
                    local_updates.push(LocalPushUpdate {
                        source: refspec.source,
                        destination: refspec.destination,
                        expected: expected.map(|expected| expected.to_string()),
                        target: target.map(|target| target.to_string()),
                    });
                }
                if crab.is_none() {
                    let repository = root.to_str().ok_or_else(|| {
                        Error::new(
                            ErrorKind::UnsupportedCapability,
                            "Git transport recovery requires a Unicode repository path",
                        )
                    })?;
                    let remote_url = git_line(
                        &tools.owner,
                        &root,
                        ["remote", "get-url", &options.remote],
                        &cancel,
                    )
                    .await?;
                    let recovery = LocalPushRecoveryToken(LocalPushBinding {
                        version: LOCAL_PUSH_TOKEN_VERSION,
                        remote: options.remote,
                        updates: local_updates,
                        dry_run: options.dry_run,
                        policy: options.policy,
                        trusted_execution: trusted,
                        backend: LocalPushBackend::Git {
                            repository: repository.to_owned(),
                            remote_url_hash: blake3::hash(remote_url.as_bytes()).to_hex().to_string(),
                        },
                    });
                    validate_token_size(&recovery)?;
                    return Ok(PreparedPush {
                        client,
                        recovery,
                        mutation: None,
                    });
                }
                let locator = crab.cloned().ok_or_else(|| {
                    Error::new(ErrorKind::Corruption, "Crab push locator disappeared")
                })?;
                let batch = RefBatch::new(edits)?.with_policy(options.policy);
                let pack_directory = tempfile::tempdir_in(&common).map_err(|source| {
                    Error::with_source(ErrorKind::Io, "cannot create local push scratch", source)
                })?;
                let pack = build_push_pack(
                    &tools.owner,
                    &root,
                    &advertised,
                    &local_updates,
                    pack_directory.path(),
                    &cancel,
                )
                .await?;
                let content = content::prepare(
                    &tools.owner,
                    &root,
                    &common,
                    &advertised,
                    &local_updates,
                    pack_directory.path(),
                    mutation_options.read_limits().max_fetched_bytes,
                    &cancel,
                )
                .await?;
                let mutation = crate::client::mutation::prepare_local_pack_update(
                    crate::client::mutation::LocalPackPreparation {
                        client: client.clone(),
                        locator,
                        batch,
                        pack,
                        content,
                        owned_directories: vec![pack_directory],
                        scratch: common,
                        persist_recovery: !options.dry_run,
                        options: mutation_options,
                    },
                    &cancel,
                )
                .await?;
                let backend = match mutation.as_ref() {
                    Some(mutation) => LocalPushBackend::Crab {
                        remote_recovery: mutation.recovery_token().to_json()?,
                    },
                    None => {
                        let repository = root.to_str().ok_or_else(|| {
                            Error::new(
                                ErrorKind::UnsupportedCapability,
                                "local push recovery requires a Unicode repository path",
                            )
                        })?;
                        let remote_url = git_line(
                            &tools.owner,
                            &root,
                            ["remote", "get-url", &options.remote],
                            &cancel,
                        )
                        .await?;
                        LocalPushBackend::LocalOnly {
                            repository: repository.to_owned(),
                            remote_url_hash: blake3::hash(remote_url.as_bytes())
                                .to_hex()
                                .to_string(),
                        }
                    }
                };
                let recovery = LocalPushRecoveryToken(LocalPushBinding {
                    version: LOCAL_PUSH_TOKEN_VERSION,
                    remote: options.remote,
                    updates: local_updates,
                    dry_run: options.dry_run,
                    policy: options.policy,
                    trusted_execution: trusted,
                    backend,
                });
                validate_token_size(&recovery)?;
                Ok(PreparedPush {
                    client,
                    recovery,
                    mutation,
                })
            })
            .await
    }
}

impl Client {
    /// Resume an unattempted prepared push or reconcile its historical attempt.
    pub async fn resume_push(
        &self,
        recovery: LocalPushRecoveryToken,
        scratch: std::path::PathBuf,
        options: OperationOptions,
    ) -> Result<LocalPushOutcome> {
        match recovery.remote_recovery()? {
            Some(remote) => {
                let mutation = self.resume_mutation(remote, scratch, options).await?;
                outcome(mutation, recovery)
            }
            None => git::execute(self, recovery, false, options).await,
        }
    }

    /// Reconcile a restarted local push without replaying it.
    pub async fn reconcile_push(
        &self,
        recovery: LocalPushRecoveryToken,
        options: OperationOptions,
    ) -> Result<LocalPushOutcome> {
        match recovery.remote_recovery()? {
            Some(remote) => {
                let mutation = self.reconcile(remote, options).await?;
                outcome(mutation, recovery)
            }
            None => git::reconcile(self, recovery, options).await,
        }
    }
}

fn validate_token_size(recovery: &LocalPushRecoveryToken) -> Result<()> {
    if recovery.to_json()?.len() > MAX_LOCAL_PUSH_TOKEN_BYTES {
        return Err(Error::new(
            ErrorKind::LimitExceeded,
            "local push token exceeds 1 MiB",
        ));
    }
    Ok(())
}

async fn validate_push(
    client: &Client,
    recovery: &LocalPushRecoveryToken,
    options: OperationOptions,
) -> Result<()> {
    match &recovery.0.backend {
        LocalPushBackend::Crab { remote_recovery } => {
            let remote = RecoveryToken::from_json(remote_recovery)?;
            let store = client.0.store.clone();
            client
                .0
                .operations
                .run(options, move |cancel| async move {
                    validate_direct_remote_snapshot(&store, &remote, &cancel).await
                })
                .await
        }
        LocalPushBackend::LocalOnly { .. } => {
            git::validate_local_only(client, recovery.clone(), options).await
        }
        LocalPushBackend::Git { .. } => git::validate(client, recovery.clone(), options).await,
    }
}

fn outcome(
    mutation: MutationOutcome,
    recovery: LocalPushRecoveryToken,
) -> Result<LocalPushOutcome> {
    match mutation {
        MutationOutcome::Committed { receipt, readiness } => Ok(LocalPushOutcome::Committed {
            receipt: Box::new(receipt),
            readiness,
        }),
        MutationOutcome::Indeterminate { .. } => Ok(LocalPushOutcome::Indeterminate { recovery }),
        MutationOutcome::Rejected { .. } => Err(Error::new(
            ErrorKind::Conflict,
            "prepared local push was rejected",
        )),
    }
}

async fn build_push_pack(
    tools: &crab_remote::local::LocalTools,
    root: &std::path::Path,
    advertised: &std::collections::BTreeMap<String, String>,
    updates: &[LocalPushUpdate],
    scratch: &std::path::Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Option<std::path::PathBuf>> {
    let targets = updates
        .iter()
        .filter_map(|update| update.target.as_deref())
        .collect::<std::collections::BTreeSet<_>>();
    if targets.is_empty() {
        return Ok(None);
    }
    let revisions = revision_input(targets, advertised);
    let pack = scratch.join("push.pack");
    tools
        .run_git_to_file_with_input(
            Some(root),
            [
                "pack-objects",
                "--stdout",
                "--revs",
                "--thin",
                "--delta-base-offset",
            ],
            revisions,
            &pack,
            false,
            cancel,
        )
        .await
        .map_err(local_error)?;
    Ok(Some(pack))
}

fn revision_input<'a>(
    targets: std::collections::BTreeSet<&'a str>,
    advertised: &'a std::collections::BTreeMap<String, String>,
) -> Vec<u8> {
    let mut revisions = Vec::new();
    for target in targets {
        revisions.extend_from_slice(target.as_bytes());
        revisions.push(b'\n');
    }
    for target in advertised
        .values()
        .collect::<std::collections::BTreeSet<_>>()
    {
        revisions.push(b'^');
        revisions.extend_from_slice(target.as_bytes());
        revisions.push(b'\n');
    }
    revisions
}

async fn upstream_destination(
    tools: &crab_remote::local::LocalTools,
    root: &std::path::Path,
    source: &str,
    requested_remote: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String> {
    let short = source.strip_prefix("refs/heads/").ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            "current HEAD is not a local branch",
        )
    })?;
    let configured_remote = git_line(
        tools,
        root,
        ["config", "--get", &format!("branch.{short}.remote")],
        cancel,
    )
    .await?;
    if configured_remote != requested_remote {
        return Err(Error::new(
            ErrorKind::Conflict,
            "current branch upstream belongs to another remote",
        ));
    }
    let merge = git_line(
        tools,
        root,
        ["config", "--get", &format!("branch.{short}.merge")],
        cancel,
    )
    .await?;
    validate_destination(&merge)?;
    Ok(merge)
}

async fn validate_direct_remote_snapshot(
    store: &crab_storage::Store,
    recovery: &RecoveryToken,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    let locator = recovery.locator()?;
    let layout = crab_storage::StoreLayout::new(store.clone(), locator.direct_prefix()?.to_owned());
    let current = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            return Err(Error::new(ErrorKind::Cancelled, "local push dry-run cancelled"));
        }
        result = crab_metadata::manifest_store::read_repository_snapshot(store, &layout) => {
            result.map_err(metadata_error)?.journal.refs
        }
    };
    for update in recovery.ref_batch().edits() {
        let actual = current
            .get(update.name())
            .map(|oid| ObjectId::from_hex(oid))
            .transpose()?;
        if actual != update.expected()? {
            return Err(Error::new(
                ErrorKind::Conflict,
                "remote ref changed after dry-run preparation",
            ));
        }
    }
    Ok(())
}

async fn git_line<const N: usize>(
    tools: &crab_remote::local::LocalTools,
    root: &std::path::Path,
    args: [&str; N],
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String> {
    let output = tools
        .run_git(Some(root), args, false, cancel)
        .await
        .map_err(local_error)?;
    std::str::from_utf8(&output.stdout)
        .map(str::trim)
        .map(str::to_owned)
        .map_err(|source| {
            Error::with_source(ErrorKind::Corruption, "Git output is not UTF-8", source)
        })
}

fn validate_destination(destination: &str) -> Result<()> {
    if !(destination.starts_with("refs/heads/") || destination.starts_with("refs/tags/")) {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "push destination must be a fully qualified branch or tag",
        ));
    }
    crab_git::refname::validate_push_refname(destination)
        .map(|_| ())
        .map_err(|source| {
            Error::with_source(ErrorKind::InvalidInput, "invalid push destination", source)
        })
}

fn validate_source(source: &str) -> Result<()> {
    if source.len() == 40 && source.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        ObjectId::from_hex(source).map(drop)
    } else {
        validate_destination(source)
    }
}

fn metadata_error(source: crab_metadata::error::MetadataError) -> Error {
    Error::with_source(
        crate::remote_error::metadata_kind(&source),
        "cannot read direct remote refs",
        source,
    )
}

#[cfg(all(test, unix))]
mod tests;
