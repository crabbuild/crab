//! Hold a cold reader while an external driver moves refs, rebuilds, and runs repository GC.
use std::io::{self, Write};
use std::sync::Arc;

use crab_remote_git::{GitPath, RemoteGitRuntime, RepositoryIdentity, RepositoryOptions, Revision};
use crab_storage::{Store, StoreLayout};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::support::{Measurements, Result, open_repository};

pub async fn qualify(
    layout: &StoreLayout<Store>,
    identity: &RepositoryIdentity,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    path: &GitPath,
    counters: &Measurements,
) -> Result<()> {
    let cancel = CancellationToken::new();
    let repository = counters
        .measure(
            "open",
            "cold",
            open_repository(layout.clone(), identity.clone(), runtime, options, &cancel),
        )
        .await?;
    let head = repository
        .refs()
        .head
        .as_ref()
        .ok_or_else(|| io::Error::other("repository has no HEAD"))?;
    let revision = Revision::Reference(head.name.clone());
    // Do not resolve a commit or read a directory here: that would warm the
    // exact physical/graph/path pages whose lifetime this experiment measures.
    println!(
        "{}",
        json!({"kind":"captured", "schema":1, "commit":head.target.to_string(), "generation":repository.generation()})
    );
    io::stdout().flush()?;
    let command = tokio::task::spawn_blocking(|| -> io::Result<String> {
        let mut command = String::new();
        io::stdin().read_line(&mut command)?;
        Ok(command)
    })
    .await??;
    if command.trim() != "continue" {
        return Err(io::Error::other(
            "capture probe requires a continue command after external GC",
        )
        .into());
    }
    for name in ["blob", "attribution", "path-history"] {
        let value = counters
            .measure(
                name,
                "after-external-gc",
                super::query(name, &repository, &revision, path, &cancel),
            )
            .await?;
        println!(
            "{}",
            json!({"kind":"captured-result", "operation":name, "value":value})
        );
    }
    Ok(())
}
