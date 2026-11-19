use super::{BrowseSession, ReadScope};
use crate::{AgentError, Result};
use crab_workflow::results::{LocalResults, ResultsRead, ResultsReader, ResultsResponse};
impl BrowseSession {
    pub(crate) async fn results_read(
        &self,
        request: ResultsRead,
        scope: ReadScope,
    ) -> Result<ResultsResponse> {
        scope.check()?;
        if let Some(root) = &self.work_dir {
            let root = root.clone();
            let result = tokio::task::spawn_blocking(move || {
                let reader = ResultsReader::Local(LocalResults::new(&root)?);
                tokio::runtime::Handle::current().block_on(reader.read(request))
            })
            .await
            .map_err(|source| AgentError::Io(std::io::Error::other(source)))??;
            scope.check()?;
            return Ok(result);
        }
        let remote = self
            .remote
            .as_ref()
            .ok_or_else(|| AgentError::Other("Results repository session is unavailable".into()))?;
        Ok(remote
            .repository
            .remote()?
            .results()
            .read(request)
            .with_options(scope.options())
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::{RemoteSession, files, native};
    use crab_sdk::{Client, OpenOptions, RepositoryLocator, storage::DirectStoreOptions};
    use crab_workflow::{ExperimentId, WorkflowStore, results::*};
    use futures_util::TryStreamExt;
    use object_store::{ObjectStore, ObjectStoreExt};
    use serde_json::json;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn workflow_results_read_local_and_bare_evidence_and_reject_closed_scopes() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("source");
        let storage = fixture.path().join("storage");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&storage).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--initial-branch=main"])
                .current_dir(&root)
                .output()
                .unwrap()
                .status
                .success()
        );
        let local = LocalResults::new(&root).unwrap();
        let run = ResultSnapshot {
            schema_version: RESULTS_SCHEMA_VERSION,
            origin: ResultOrigin::Captured,
            experiment_id: ExperimentId::new_v7().to_string(),
            source_metadata_hash: "a".repeat(64),
            base_commit: "b".repeat(40),
            name: None,
            status: "success".into(),
            started_at: "2026-11-18T00:00:00Z".into(),
            ended_at: None,
            parameters: Vec::new(),
            metrics: flatten_scalars("metrics.json", &json!({"score":0.25})).unwrap(),
            stages: Vec::new(),
            attachments: Vec::new(),
            plots: Vec::new(),
            reproduction: None,
            capture_issues: Vec::new(),
        }
        .seal()
        .unwrap();
        local.install(&run).unwrap();
        let request = ResultsRead::List(RunQuery::default());
        let session = BrowseSession::open_local(&root, &ReadScope::default())
            .await
            .unwrap();
        let response = session
            .results_read(request.clone(), session.read_scope())
            .await
            .unwrap();
        let ResultsResponse::Page(page) = response else {
            panic!("expected local Results page")
        };
        assert_eq!(page.rows[0].metrics[0].value, json!(0.25));
        let captured = session.read_scope();
        session.close().await.unwrap();
        assert!(matches!(
            session.results_read(request.clone(), captured).await,
            Err(AgentError::OpCancelled { .. })
        ));
        let client = Client::builder()
            .direct_store(DirectStoreOptions::filesystem(&storage).unwrap())
            .build()
            .unwrap();
        let locator = RepositoryLocator::new("repository").unwrap();
        client
            .initialize_remote(locator.clone(), "refs/heads/main")
            .await
            .unwrap();
        let store = Arc::new(object_store::memory::InMemory::new());
        let remote = RemoteResults::new(
            WorkflowStore::new(store.clone()),
            "repository",
            CancellationToken::new(),
        );
        remote.push(&local, &run).await.unwrap();
        // Filesystem storage is a read fixture, not a conditional-publication
        // backend. Copy only canonical Results objects produced by the writer.
        for object in store.list(None).try_collect::<Vec<_>>().await.unwrap() {
            let key = object.location.as_ref();
            if !key.starts_with("repository/workflow/results/")
                && !key.starts_with("repository/refs/crab/results/")
            {
                continue;
            }
            let path = storage.join(key);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                path,
                store
                    .get(&object.location)
                    .await
                    .unwrap()
                    .bytes()
                    .await
                    .unwrap(),
            )
            .unwrap();
        }
        std::fs::remove_dir_all(&root).unwrap();
        let session = BrowseSession {
            url: "filesystem Results qualification".into(),
            work_dir: None,
            remote: Some(Arc::new(RemoteSession {
                repository: client.open(OpenOptions::remote(locator)).await.unwrap(),
                client,
            })),
            readers: native::NativeReads::default(),
            files: files::Files::default(),
        };
        let response = session
            .results_read(request.clone(), session.read_scope())
            .await
            .unwrap();
        let ResultsResponse::Page(page) = response else {
            panic!("expected bare Results page")
        };
        assert_eq!(page.rows[0].digest, run.digest);
        let captured = session.read_scope();
        session.close().await.unwrap();
        assert!(matches!(
            session.results_read(request, captured).await,
            Err(AgentError::OpCancelled { .. })
        ));
    }
}
