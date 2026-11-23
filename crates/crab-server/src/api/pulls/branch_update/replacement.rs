use std::time::Duration;

use gix_hash::ObjectId;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::cells::repository::pull_head;

pub(super) struct Intent {
    pub branch: String,
    pub version: u64,
    pub base: ObjectId,
    pub head: ObjectId,
    pub plan_id: String,
}

impl Intent {
    pub(super) async fn complete(
        self,
        server: Arc<Server>,
        principal: Principal,
        destination: Destination,
        pull: storage::PullRequest,
        commit: String,
    ) -> Result<Json<Value>> {
        let (send, result) = tokio::sync::oneshot::channel();
        // A disconnected client must not drop a leased Cell mutation. The same
        // Git receipt can resume this step if the server stops before it runs.
        let worker = Arc::clone(&server);
        server.receives.spawn(async move {
            let saved = self
                .switch(&worker, &principal, &destination, &pull, &commit)
                .await
                .map(|switched| {
                    Json(json!({"commit":commit, "branch":self.branch, "pull_updated":switched}))
                });
            let _ = send.send(saved);
        });
        result.await.map_err(|_| Error::MergeBusy)?
    }

    async fn switch(
        &self,
        server: &Server,
        principal: &Principal,
        destination: &Destination,
        pull: &storage::PullRequest,
        commit: &str,
    ) -> Result<bool> {
        let upstream = destination.discussion();
        let persist = async |comparison_current| {
            destination.permit(server, principal).await?;
            let cell = app::repository_cell(server, upstream)?;
            app::command_output(
                cell.client
                    .command::<pull_head::Switch>(
                        &cell.target,
                        app::mutation_identity()?,
                        pull_head::Input {
                            number: pull.number,
                            version: self.version,
                            actor: (&app::actor(principal)?).into(),
                            plan_id: self.plan_id.clone(),
                            head_repository: pull.head_repository,
                            previous_ref: pull.head_ref.clone(),
                            new_ref: self.branch.clone(),
                            base_oid: self.base.to_string(),
                            new_oid: commit.to_owned(),
                            comparison_current,
                        },
                    )
                    .await,
            )
        };
        if pull.version != self.version {
            // A completed switch advanced the PR version. Its Cell receipt
            // takes precedence over live refs and later edits.
            return persist(false).await;
        }
        let head_refs = [pull.head_ref.clone(), self.branch.clone()];
        let work = async |cancel: CancellationToken| {
            let head = destination.head.open_git_view().await?;
            let base = if let Some(upstream) = &destination.upstream {
                upstream.ref_oid(&pull.base_ref).await?
            } else {
                head.refs().get(&pull.base_ref).cloned()
            };
            let current = base.as_deref() == Some(self.base.to_string().as_str())
                && head.refs().get(&pull.head_ref) == Some(&self.head.to_string())
                && head
                    .refs()
                    .get(&self.branch)
                    .is_some_and(|oid| oid == commit)
                && !settings::load_lifecycle(server, &destination.head)
                    .await?
                    .archived;
            if cancel.is_cancelled() {
                return Err(Error::MergeBusy);
            }
            persist(current).await
        };
        let ttl = Duration::from_secs(300);
        // Fork merges, branch updates and this switch all lock upstream first.
        let mut upstream_refs = vec![pull.base_ref.clone()];
        if destination.upstream.is_none() {
            upstream_refs.extend(head_refs.iter().cloned());
        }
        crab_remote::publication::with_leases(
            &upstream.store,
            &upstream.layout,
            upstream_refs,
            ttl,
            &server.cancellation,
            |_holders, cancel| async move {
                if destination.upstream.is_some() {
                    crab_remote::publication::with_leases(
                        &destination.head.store,
                        &destination.head.layout,
                        head_refs,
                        ttl,
                        &cancel,
                        |_holders, cancel| work(cancel),
                    )
                    .await
                } else {
                    work(cancel).await
                }
            },
        )
        .await
    }
}
