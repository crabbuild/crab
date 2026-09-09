use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{
    LocalPushBackend, LocalPushOutcome, LocalPushRecoveryToken, LocalPushUpdate, git_line,
    local_error, validate_destination,
};
use crate::{Client, Error, ErrorKind, ObjectId, OperationOptions, Result};

pub(super) async fn validate(
    client: &Client,
    recovery: LocalPushRecoveryToken,
    options: OperationOptions,
) -> Result<()> {
    let tools = client.0.local_tools.clone().ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            "local tools are required for Git transport recovery",
        )
    })?;
    client
        .0
        .operations
        .run(options, move |cancel| async move {
            let root = validate_git_binding(&tools.owner, &recovery, &cancel).await?;
            validate_local_targets(&tools.owner, &root, &recovery.0.updates, &cancel).await?;
            let advertised = remote_refs(&tools.owner, &root, &recovery.0.remote, &cancel).await?;
            if !refs_match(&advertised, &recovery.0.updates, RefState::Expected) {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "remote refs changed after push preparation",
                ));
            }
            Ok(())
        })
        .await
}

pub(super) async fn validate_local_only(
    client: &Client,
    recovery: LocalPushRecoveryToken,
    options: OperationOptions,
) -> Result<()> {
    let tools = client.0.local_tools.clone().ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            "local tools are required for push validation",
        )
    })?;
    client
        .0
        .operations
        .run(options, move |cancel| async move {
            let root = validate_git_binding(&tools.owner, &recovery, &cancel).await?;
            validate_local_targets(&tools.owner, &root, &recovery.0.updates, &cancel).await
        })
        .await
}

pub(super) async fn execute(
    client: &Client,
    recovery: LocalPushRecoveryToken,
    dry_run: bool,
    options: OperationOptions,
) -> Result<LocalPushOutcome> {
    let tools = client.0.local_tools.clone().ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            "local tools are required for Git transport recovery",
        )
    })?;
    client
        .0
        .operations
        .run(options, move |cancel| async move {
            let root = validate_git_binding(&tools.owner, &recovery, &cancel).await?;
            let common =
                super::super::git_path(&tools.owner, &root, "--git-common-dir", &cancel).await?;
            let _lease = crab_remote::local::acquire_repository_lease(&common, &cancel)
                .await
                .map_err(local_error)?;
            validate_local_targets(&tools.owner, &root, &recovery.0.updates, &cancel).await?;
            let advertised = remote_refs(&tools.owner, &root, &recovery.0.remote, &cancel).await?;
            if !refs_match(&advertised, &recovery.0.updates, RefState::Expected) {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "remote refs changed after push preparation",
                ));
            }
            let args = git_push_arguments(&recovery, dry_run);
            match tools
                .owner
                .run_git(Some(&root), args, recovery.0.trusted_execution, &cancel)
                .await
            {
                Ok(_) => Ok(LocalPushOutcome::TransportCommitted {
                    destinations: destinations(&recovery),
                }),
                Err(crab_remote::local::LocalError::Cancelled) => {
                    Err(Error::new(ErrorKind::Cancelled, "local Git push cancelled"))
                }
                Err(crab_remote::local::LocalError::Spawn { source, .. }) => Err(
                    Error::with_source(ErrorKind::Io, "cannot start local Git push", source),
                ),
                Err(source) => {
                    let refreshed =
                        remote_refs(&tools.owner, &root, &recovery.0.remote, &cancel).await;
                    match refreshed {
                        Ok(refs) if refs_match(&refs, &recovery.0.updates, RefState::Target) => {
                            Ok(LocalPushOutcome::TransportCommitted {
                                destinations: destinations(&recovery),
                            })
                        }
                        Ok(refs) if refs_match(&refs, &recovery.0.updates, RefState::Expected) => {
                            Err(Error::with_source(
                                ErrorKind::Conflict,
                                "Git transport rejected the prepared push",
                                source,
                            ))
                        }
                        _ => Ok(LocalPushOutcome::Indeterminate { recovery }),
                    }
                }
            }
        })
        .await
}

pub(super) async fn reconcile(
    client: &Client,
    recovery: LocalPushRecoveryToken,
    options: OperationOptions,
) -> Result<LocalPushOutcome> {
    let tools = client.0.local_tools.clone().ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            "local tools are required for Git transport recovery",
        )
    })?;
    client
        .0
        .operations
        .run(options, move |cancel| async move {
            let root = validate_git_binding(&tools.owner, &recovery, &cancel).await?;
            let advertised = remote_refs(&tools.owner, &root, &recovery.0.remote, &cancel).await?;
            if refs_match(&advertised, &recovery.0.updates, RefState::Target) {
                return Ok(LocalPushOutcome::TransportCommitted {
                    destinations: destinations(&recovery),
                });
            }
            if refs_match(&advertised, &recovery.0.updates, RefState::Expected) {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "prepared Git transport push was not committed",
                ));
            }
            Ok(LocalPushOutcome::Indeterminate { recovery })
        })
        .await
}

#[derive(Clone, Copy)]
enum RefState {
    Expected,
    Target,
}

async fn validate_git_binding(
    tools: &crab_remote::local::LocalTools,
    recovery: &LocalPushRecoveryToken,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<PathBuf> {
    let (repository, remote_url_hash) = match &recovery.0.backend {
        LocalPushBackend::Git {
            repository,
            remote_url_hash,
        }
        | LocalPushBackend::LocalOnly {
            repository,
            remote_url_hash,
        } => (repository, remote_url_hash),
        LocalPushBackend::Crab { .. } => {
            return Err(Error::new(
                ErrorKind::Corruption,
                "Git push recovery has a Crab mutation binding",
            ));
        }
    };
    let root = PathBuf::from(repository);
    let current = git_line(
        tools,
        &root,
        ["remote", "get-url", &recovery.0.remote],
        cancel,
    )
    .await?;
    if blake3::hash(current.as_bytes()).to_hex().as_str() != remote_url_hash {
        return Err(Error::new(
            ErrorKind::Conflict,
            "local push remote changed after preparation",
        ));
    }
    Ok(root)
}

async fn validate_local_targets(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    updates: &[LocalPushUpdate],
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    for update in updates {
        let (Some(source), Some(target)) = (&update.source, &update.target) else {
            continue;
        };
        let actual = git_line(tools, root, ["rev-parse", "--verify", source], cancel).await?;
        if &actual != target {
            return Err(Error::new(
                ErrorKind::Conflict,
                "local push source changed after preparation",
            ));
        }
    }
    Ok(())
}

pub(super) async fn remote_refs(
    tools: &crab_remote::local::LocalTools,
    root: &Path,
    remote: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<std::collections::BTreeMap<String, String>> {
    let output = tools
        .run_git(Some(root), ["ls-remote", "--refs", remote], false, cancel)
        .await
        .map_err(local_error)?;
    let text = std::str::from_utf8(&output.stdout).map_err(|source| {
        Error::with_source(
            ErrorKind::Corruption,
            "Git remote ref inventory is not UTF-8",
            source,
        )
    })?;
    let mut refs = std::collections::BTreeMap::new();
    for line in text.lines() {
        let (oid, name) = line.split_once('\t').ok_or_else(|| {
            Error::new(
                ErrorKind::Corruption,
                "Git returned a malformed remote ref inventory",
            )
        })?;
        ObjectId::from_hex(oid)?;
        if !(name.starts_with("refs/heads/") || name.starts_with("refs/tags/")) {
            continue;
        }
        validate_destination(name)?;
        if refs.insert(name.to_owned(), oid.to_owned()).is_some() {
            return Err(Error::new(
                ErrorKind::Corruption,
                "Git returned a duplicate remote ref",
            ));
        }
    }
    Ok(refs)
}

fn refs_match(
    refs: &std::collections::BTreeMap<String, String>,
    updates: &[LocalPushUpdate],
    state: RefState,
) -> bool {
    updates.iter().all(|update| {
        let expected = match state {
            RefState::Expected => update.expected.as_ref(),
            RefState::Target => update.target.as_ref(),
        };
        refs.get(&update.destination) == expected
    })
}

fn git_push_arguments(recovery: &LocalPushRecoveryToken, dry_run: bool) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("push"),
        OsString::from("--porcelain"),
        OsString::from("--atomic"),
    ];
    if dry_run {
        args.push(OsString::from("--dry-run"));
    }
    for update in &recovery.0.updates {
        let expected = update.expected.as_deref().unwrap_or_default();
        args.push(OsString::from(format!(
            "--force-with-lease={}:{}",
            update.destination, expected
        )));
    }
    args.push(OsString::from(&recovery.0.remote));
    for update in &recovery.0.updates {
        args.push(OsString::from(match &update.target {
            Some(target) => format!("{target}:{}", update.destination),
            None => format!(":{}", update.destination),
        }));
    }
    args
}

fn destinations(recovery: &LocalPushRecoveryToken) -> Vec<String> {
    recovery
        .0
        .updates
        .iter()
        .map(|update| update.destination.clone())
        .collect()
}
