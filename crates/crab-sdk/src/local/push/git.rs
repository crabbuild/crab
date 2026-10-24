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
            let root = local_binding(&tools.owner, &recovery, &cancel).await?;
            validate_local_targets(&tools.owner, &root, &recovery.0.updates, &cancel).await?;
            let advertised = remote_refs(&tools.owner, &root, &recovery.0.remote, &cancel).await?;
            if !expected_refs_match(&advertised, &recovery.0.updates) {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "remote refs changed after push preparation",
                ));
            }
            Ok(())
        })
        .await
}

pub(super) async fn execute(
    client: &Client,
    recovery: LocalPushRecoveryToken,
    options: OperationOptions,
) -> Result<LocalPushOutcome> {
    if recovery.0.trusted_execution && !client.0.local_execution_policy.is_trusted() {
        return Err(Error::new(
            ErrorKind::Authorization,
            "prepared Git push requires explicitly trusted local execution",
        ));
    }
    let tools = client.0.local_tools.clone().ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            "local tools are required for Git transport recovery",
        )
    })?;
    client
        .0
        .operations
        .run_mutation(options, move |cancel| async move {
            let root = local_binding(&tools.owner, &recovery, &cancel).await?;
            let common =
                super::super::git_path(&tools.owner, &root, "--git-common-dir", &cancel).await?;
            let _lease = crab_remote::local::acquire_repository_lease(&common, &cancel)
                .await
                .map_err(local_error)?;
            validate_local_targets(&tools.owner, &root, &recovery.0.updates, &cancel).await?;
            let advertised = remote_refs(&tools.owner, &root, &recovery.0.remote, &cancel).await?;
            if !expected_refs_match(&advertised, &recovery.0.updates) {
                return Err(Error::new(
                    ErrorKind::Conflict,
                    "remote refs changed after push preparation",
                ));
            }
            let args = git_push_arguments(&recovery);
            match tools
                .owner
                .run_git_with_env(
                    Some(&root),
                    args,
                    [("LC_ALL", "C")],
                    recovery.0.trusted_execution,
                    &cancel,
                )
                .await
            {
                Ok(_) => Ok(satisfied(&recovery)),
                Err(source @ crab_remote::local::LocalError::Spawn { .. }) => Err(
                    Error::with_source(ErrorKind::Io, "cannot start local Git push", source),
                ),
                Err(crab_remote::local::LocalError::Exit(output))
                    if rejected(&output.stdout, &recovery.0.updates) =>
                {
                    Err(Error::with_source(
                        ErrorKind::Conflict,
                        "Git transport rejected the prepared push",
                        crab_remote::local::LocalError::Exit(output),
                    ))
                }
                // After invocation, cancellation and transport errors can hide a
                // successful receive-pack. Current refs cannot attribute history.
                Err(_) => Ok(LocalPushOutcome::Indeterminate { recovery }),
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
            local_binding(&tools.owner, &recovery, &cancel).await?;
            // Git has no durable attempt receipt. A saved token cannot distinguish
            // unattempted work from a lost acknowledgement, so it cannot replay.
            Ok(LocalPushOutcome::Indeterminate { recovery })
        })
        .await
}

// git-push(1) porcelain distinguishes definite rejection from "remote failure",
// which explicitly includes lost replies. Require proof for every selected ref.
fn rejected(stdout: &[u8], updates: &[LocalPushUpdate]) -> bool {
    let Ok(text) = std::str::from_utf8(stdout) else {
        return false;
    };
    let mut rejected = std::collections::BTreeSet::new();
    for line in text.lines() {
        let mut fields = line.split('\t');
        let (Some("!"), Some(refspec), Some(summary), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Some((_, destination)) = refspec.split_once(':') else {
            continue;
        };
        if summary.starts_with("[rejected]") || summary.starts_with("[remote rejected]") {
            rejected.insert(destination);
        }
    }
    updates
        .iter()
        .all(|update| rejected.contains(update.destination.as_str()))
}

pub(super) async fn local_binding(
    tools: &crab_remote::local::LocalTools,
    recovery: &LocalPushRecoveryToken,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<PathBuf> {
    let (repository, remote_url_hash) = match &recovery.0.backend {
        LocalPushBackend::Git {
            repository,
            remote_url_hash,
        }
        | LocalPushBackend::NativeValidation {
            repository,
            remote_url_hash,
            ..
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
        ["remote", "get-url", "--push", "--all", &recovery.0.remote],
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

pub(super) async fn validate_local_targets(
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
    let url = git_line(
        tools,
        root,
        ["remote", "get-url", "--push", "--all", remote],
        cancel,
    )
    .await?;
    let output = tools
        .run_git(
            Some(root),
            ["ls-remote", "--refs", "--", url.as_str()],
            false,
            cancel,
        )
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

fn expected_refs_match(
    refs: &std::collections::BTreeMap<String, String>,
    updates: &[LocalPushUpdate],
) -> bool {
    updates
        .iter()
        .all(|update| refs.get(&update.destination) == update.expected.as_ref())
}

fn git_push_arguments(recovery: &LocalPushRecoveryToken) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("push"),
        OsString::from("--porcelain"),
        OsString::from("--atomic"),
    ];
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

pub(super) fn satisfied(recovery: &LocalPushRecoveryToken) -> LocalPushOutcome {
    LocalPushOutcome::TransportCommitted {
        destinations: recovery
            .0
            .updates
            .iter()
            .map(|update| update.destination.clone())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_complete_explicit_porcelain_rejections_prove_failure() {
        let updates = ["refs/heads/main", "refs/tags/release"].map(|destination| LocalPushUpdate {
            source: None,
            destination: destination.to_owned(),
            expected: None,
            target: None,
        });
        for (statuses, expected) in [
            (
                [
                    "[remote rejected] (hook declined)",
                    "[rejected] (stale info)",
                ],
                true,
            ),
            (
                [
                    "[remote failure] (remote failed to report status)",
                    "[rejected] (stale info)",
                ],
                false,
            ),
            (
                ["[remote rejected] (hook declined)", "[remote failure]"],
                false,
            ),
        ] {
            let stdout = format!(
                "!\t:refs/heads/main\t{}\n!\t:refs/tags/release\t{}\n",
                statuses[0], statuses[1]
            );
            assert_eq!(
                rejected(stdout.as_bytes(), &updates),
                expected,
                "{statuses:?}"
            );
        }
        assert!(!rejected(
            b"!\t:refs/heads/main\t[remote rejected] (hook declined)\n",
            &updates
        ));
    }
}
