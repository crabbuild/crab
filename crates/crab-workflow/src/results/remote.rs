use std::collections::BTreeSet;

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt, stream};
use object_store::path::Path;
use tokio_util::sync::CancellationToken;

use super::local::{RunRoot, report_evidence};
use super::{check_digest, decode, digest, encode, invalid, *};
use crate::{ExperimentId, Result, WorkflowError, WorkflowStore};

pub struct RemoteResults {
    store: WorkflowStore,
    prefix: String,
    cancellation: CancellationToken,
}

impl RemoteResults {
    #[must_use]
    pub fn new(store: WorkflowStore, prefix: &str, cancellation: CancellationToken) -> Self {
        Self {
            store,
            prefix: prefix.trim_end_matches('/').into(),
            cancellation,
        }
    }

    pub async fn run(&self, id: &str) -> Result<SealedResult> {
        id.parse::<ExperimentId>()?;
        let root: RunRoot = decode(&self.read(&format!("refs/crab/results/{id}"), 1024).await?)?;
        root.verify(id).map_err(|error| stored(id, error))?;
        self.run_at(id, &root.digest).await
    }

    pub async fn run_at(&self, id: &str, hash: &str) -> Result<SealedResult> {
        id.parse::<ExperimentId>()?;
        check_digest(hash)?;
        let bytes = self
            .read(
                &format!("workflow/results/runs/{id}/{hash}.json"),
                MAX_DOCUMENT_BYTES,
            )
            .await?;
        let run = SealedResult {
            digest: hash.into(),
            snapshot: decode(&bytes).map_err(|error| stored(hash, error))?,
        };
        run.verify().map_err(|error| stored(hash, error))?;
        if run.snapshot.experiment_id != id {
            return Err(stored(hash, invalid("remote run ID mismatch")));
        }
        Ok(run)
    }

    pub async fn list(&self) -> Result<Vec<SealedResult>> {
        let prefix = self.path("refs/crab/results/");
        let roots = self
            .store
            .as_storage()
            .list_prefix_bounded(&prefix, MAX_CATALOG_RUNS)
            .await?
            .ok_or_else(|| invalid("remote catalog exceeds root limit"))?;
        let ids = roots
            .into_iter()
            .map(|root| {
                root.location
                    .as_ref()
                    .rsplit('/')
                    .next()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid("invalid remote run root"))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut pending = stream::iter(ids)
            .map(|id| async move { self.run(&id).await })
            .buffer_unordered(16);
        let mut runs = Vec::new();
        let mut total = 0_usize;
        while let Some(run) = pending.try_next().await? {
            self.check_cancelled()?;
            total = total.saturating_add(encode(&run.snapshot)?.len());
            if total > MAX_CAPTURE_BYTES {
                return Err(invalid(
                    "Catalog scan exceeds 256 MiB; narrow repository retention before listing",
                ));
            }
            runs.push(run);
        }
        Ok(runs)
    }

    /// Removes run discovery while retaining immutable evidence owned by reports.
    pub async fn remove_run(&self, id: &str) -> Result<()> {
        id.parse::<ExperimentId>()?;
        self.check_cancelled()?;
        let path = self.path(&format!("refs/crab/results/{id}"));
        match self.store.as_storage().delete(&path).await {
            Ok(()) | Err(crab_storage::StorageError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn blob(&self, hash: &str) -> Result<Vec<u8>> {
        check_digest(hash)?;
        let bytes = self
            .read(
                &format!("workflow/results/objects/{hash}"),
                MAX_ATTACHMENT_BYTES,
            )
            .await?;
        if digest(&bytes) != hash {
            return Err(stored(hash, invalid("remote evidence digest mismatch")));
        }
        Ok(bytes)
    }

    pub async fn report(&self, id: &str) -> Result<Report> {
        check_digest(id)?;
        let root = self
            .read(&format!("refs/crab/results-reports/{id}"), 128)
            .await?;
        if root != id.as_bytes() {
            return Err(stored(id, invalid("remote report root identity mismatch")));
        }
        let bytes = self
            .read(
                &format!("workflow/results/reports/{id}.json"),
                MAX_DOCUMENT_BYTES,
            )
            .await?;
        let report: Report = decode(&bytes).map_err(|error| stored(id, error))?;
        if report.id().map_err(|error| stored(id, error))? != id {
            return Err(stored(id, invalid("remote report identity mismatch")));
        }
        Ok(report)
    }

    pub async fn reports(&self) -> Result<Vec<String>> {
        let prefix = self.path("refs/crab/results-reports/");
        let roots = self
            .store
            .as_storage()
            .list_prefix_bounded(&prefix, 100_000)
            .await?
            .ok_or_else(|| invalid("remote report catalog exceeds root limit"))?;
        roots
            .into_iter()
            .map(|root| {
                let id = root
                    .location
                    .as_ref()
                    .rsplit('/')
                    .next()
                    .ok_or_else(|| invalid("invalid report root"))?;
                check_digest(id)?;
                Ok(id.to_owned())
            })
            .collect()
    }

    pub async fn push(&self, local: &LocalResults, run: &SealedResult) -> Result<()> {
        run.verify()?;
        let lease = self.store.acquire_gc_writer(&self.prefix).await?;
        let lease_cancel = lease.cancellation();
        let work = async {
            self.upload_evidence(local, run.snapshot.evidence()).await?;
            let id = &run.snapshot.experiment_id;
            self.immutable(
                &format!("workflow/results/runs/{id}/{}.json", run.digest),
                encode(&run.snapshot)?,
            )
            .await?;
            let path = self.path(&format!("refs/crab/results/{id}"));
            let root = RunRoot {
                schema_version: RESULTS_SCHEMA_VERSION,
                experiment_id: id.clone(),
                digest: run.digest.clone(),
            };
            let bytes = Bytes::from(encode(&root)?);
            match self
                .store
                .as_storage()
                .get_with_etag_bounded(&path, 1024)
                .await
            {
                Ok((old_bytes, etag)) => {
                    let old: RunRoot = decode(&old_bytes)?;
                    old.verify(id)?;
                    if old.digest != run.digest {
                        self.store.as_storage().update(&path, bytes, etag).await?;
                    }
                }
                Err(crab_storage::StorageError::NotFound { .. }) => {
                    self.store.as_storage().create_strict(&path, bytes).await?;
                }
                Err(error) => return Err(error.into()),
            }
            Ok(())
        };
        let outcome = tokio::select! {
            result = work => result,
            _ = self.cancellation.cancelled() => Err(WorkflowError::Cancelled),
            _ = lease_cancel.cancelled() => Err(crate::store::WorkflowGcWriter::lease_lost_error(&self.prefix)),
        };
        let released = lease.release().await;
        outcome.and(released)
    }

    pub async fn push_report(&self, local: &LocalResults, report: &Report) -> Result<String> {
        let id = report.id()?;
        let lease = self.store.acquire_gc_writer(&self.prefix).await?;
        let lease_cancel = lease.cancellation();
        let work = async {
            self.upload_evidence(local, report_evidence(report)).await?;
            self.immutable(
                &format!("workflow/results/reports/{id}.json"),
                encode(report)?,
            )
            .await?;
            self.immutable(
                &format!("refs/crab/results-reports/{id}"),
                id.as_bytes().to_vec(),
            )
            .await?;
            Ok(id)
        };
        let outcome = tokio::select! {
            result = work => result,
            _ = self.cancellation.cancelled() => Err(WorkflowError::Cancelled),
            _ = lease_cancel.cancelled() => Err(crate::store::WorkflowGcWriter::lease_lost_error(&self.prefix)),
        };
        let released = lease.release().await;
        match outcome {
            Ok(id) => {
                released?;
                Ok(id)
            }
            Err(error) => Err(error),
        }
    }

    pub async fn pull(&self, local: &LocalResults, id: &str) -> Result<SealedResult> {
        let run = self.run(id).await?;
        self.download_evidence(local, run.snapshot.evidence())
            .await?;
        self.check_cancelled()?;
        // Only the discovery root is installed after every referenced body was
        // verified. Interrupted downloads leave reusable immutable objects.
        local.install(&run)?;
        Ok(run)
    }

    pub async fn pull_report(&self, local: &LocalResults, id: &str) -> Result<Report> {
        let report = self.report(id).await?;
        self.download_evidence(local, report_evidence(&report))
            .await?;
        self.check_cancelled()?;
        local.save_report(&report)?;
        Ok(report)
    }

    async fn upload_evidence<'a>(
        &self,
        local: &LocalResults,
        attachments: impl Iterator<Item = &'a Attachment>,
    ) -> Result<()> {
        let mut uploaded = BTreeSet::new();
        for attachment in attachments.filter(|a| a.state == Availability::Present) {
            self.check_cancelled()?;
            if !attachment.push || attachment.remote.is_some() {
                return Err(invalid(format!(
                    "evidence {} is excluded from this remote by output policy",
                    attachment.path
                )));
            }
            let hash = attachment
                .hash
                .as_deref()
                .ok_or_else(|| invalid("attachment lacks hash"))?;
            if uploaded.insert(hash) {
                let bytes = local.blob(hash)?;
                if bytes.len() as u64 != attachment.size {
                    return Err(invalid("evidence size mismatch"));
                }
                self.immutable(&format!("workflow/results/objects/{hash}"), bytes)
                    .await?;
            }
        }
        Ok(())
    }

    async fn download_evidence<'a>(
        &self,
        local: &LocalResults,
        attachments: impl Iterator<Item = &'a Attachment>,
    ) -> Result<()> {
        let mut downloaded = BTreeSet::new();
        for attachment in attachments.filter(|a| a.state == Availability::Present) {
            self.check_cancelled()?;
            let hash = attachment
                .hash
                .as_deref()
                .ok_or_else(|| invalid("attachment lacks hash"))?;
            if downloaded.insert(hash) {
                let bytes = self.blob(hash).await?;
                if bytes.len() as u64 != attachment.size {
                    return Err(invalid("remote evidence size mismatch"));
                }
                local.put_blob(&bytes)?;
            }
        }
        Ok(())
    }

    async fn immutable(&self, relative: &str, bytes: Vec<u8>) -> Result<()> {
        self.check_cancelled()?;
        let path = self.path(relative);
        let expected = Bytes::from(bytes);
        let outcome = self
            .store
            .as_storage()
            .create_or_read_immutable(&path, expected.clone(), MAX_ATTACHMENT_BYTES as u64)
            .await?;
        if let crab_storage::ImmutableCreateOutcome::Existing { bytes, .. } = outcome
            && bytes != expected
        {
            return Err(invalid("remote immutable identity conflict"));
        }
        Ok(())
    }

    async fn read(&self, relative: &str, limit: usize) -> Result<Vec<u8>> {
        self.check_cancelled()?;
        let path = self.path(relative);
        tokio::select! {
            _ = self.cancellation.cancelled() => Err(WorkflowError::Cancelled),
            result = self.store.as_storage().get_with_etag_bounded(&path, limit as u64) => {
                Ok(result?.0.to_vec())
            }
        }
    }

    fn path(&self, relative: &str) -> Path {
        Path::from(format!("{}/{relative}", self.prefix))
    }

    fn check_cancelled(&self) -> Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(WorkflowError::Cancelled);
        }
        Ok(())
    }
}
