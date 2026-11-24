use std::time::Duration;

use crab_git::receive_plan::RefUpdate;

use crate::{
    app,
    auth::Identity,
    cells::{
        directory::IdentityKey,
        repository::recent_pushes::{Branch, Intent, MAX_BRANCHES, Record, RecordInput},
    },
    repository::Repository,
    server::Server,
};

pub(super) async fn record(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    plan: &str,
    updates: &[RefUpdate],
    default: Option<&str>,
) {
    let branches: Vec<_> = updates
        .iter()
        .filter_map(|update| {
            (update.name.starts_with("refs/heads/")
                && update.name.len() <= 1_024
                && Some(update.name.as_str()) != default
                && update.old != update.new)
                .then_some(update.new)
                .flatten()
                .map(|oid| Branch {
                    name: update.name.clone(),
                    oid: oid.to_string(),
                })
        })
        .take(MAX_BRANCHES)
        .collect();
    if branches.is_empty() || server.cells().is_none() {
        return;
    }
    // This is advisory data. Bound its cost independently of the ref
    // publication, and require the real receipt before displaying any hint.
    let recorded = tokio::time::timeout(Duration::from_millis(300), async {
        let cell = app::repository_cell(server, repo)?;
        app::command_output(
            cell.client
                .command::<Record>(
                    &cell.target,
                    app::mutation_identity()?,
                    RecordInput {
                        actor: IdentityKey {
                            issuer: actor.issuer.clone(),
                            subject: actor.subject.clone(),
                        },
                        intent: Intent {
                            plan: plan.to_owned(),
                            branches,
                        },
                    },
                )
                .await,
        )
    })
    .await;
    if !matches!(recorded, Ok(Ok(()))) {
        tracing::debug!(repository = %repo.id, ?recorded, "Recent push hint was not recorded");
    }
}
