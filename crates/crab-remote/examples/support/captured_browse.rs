//! Hold a cold reader while an external driver moves refs, rebuilds, and runs repository GC.
use std::io::{self, Write};
use std::sync::Arc;

use crab_remote_git::{GitPath, RemoteGitRuntime, RepositoryIdentity, RepositoryOptions, Revision};
use crab_storage::{Store, StoreLayout};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::support::{Measurements, Result, capture_repository_view};

pub async fn qualify(
    layout: &StoreLayout<Store>,
    identity: &RepositoryIdentity,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    path: &GitPath,
    counters: &Measurements,
) -> Result<()> {
    let cancel = CancellationToken::new();
    let view = counters
        .measure("open", "cold", capture_repository_view(layout))
        .await?;
    let head = view
        .refs()
        .get(view.head())
        .ok_or_else(|| io::Error::other("repository has no HEAD"))?;
    let revision = Revision::Reference(view.head().to_owned());
    // Repository construction proves visible tips through graph pages. Delay
    // it until after GC so even those pages stay cold in the captured view.
    println!(
        "{}",
        json!({"kind":"captured", "schema":1, "commit":head, "generation":view.root().root().generation()})
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
    let repository = counters
        .measure("reader-open", "after-external-gc", async {
            Ok(view
                .git_repository_from_store(
                    layout.clone(),
                    identity.clone(),
                    runtime,
                    options,
                    2 * 1024 * 1024 * 1024,
                    &cancel,
                )
                .await?)
        })
        .await?;
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
