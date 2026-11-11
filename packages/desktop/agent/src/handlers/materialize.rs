use super::*;

#[derive(Deserialize)]
pub(super) struct FileMaterializeParams {
    #[serde(flatten)]
    file: FileIdParams,
    op_id: String,
}

pub(super) async fn one(
    state: &State,
    p: FileMaterializeParams,
    out: SharedStdout,
) -> Result<Value> {
    let repo = state.get(&p.file.repo_id).await?;
    let scope = repo.read_scope();
    let job = Materialization {
        repo_id: &p.file.repo_id,
        op_id: &p.op_id,
        out,
    };
    let work = async {
        let _publication = tokio::select! {
            guard = repo.files.materializing.lock() => guard,
            error = scope.cancelled() => return Err(error),
        };
        let file = repo.files.get(&p.file.file_id).await?;
        let (_guard, source) = file.materialization(scope.clone()).await?;
        let total_bytes = source.size;
        let written = job.write(source, 0, total_bytes, None).await?;
        Ok(json!({ "written_bytes": written }))
    };
    job.run(state, &scope, work).await
}

#[derive(Deserialize)]
pub(super) struct FilesMaterializeParams {
    repo_id: String,
    paths: Vec<String>,
    op_id: String,
}

pub(super) async fn many(
    state: &State,
    p: FilesMaterializeParams,
    out: SharedStdout,
) -> Result<Value> {
    let repo = state.get(&p.repo_id).await?;
    let scope = repo.read_scope();
    let job = Materialization {
        repo_id: &p.repo_id,
        op_id: &p.op_id,
        out,
    };
    let work = async {
        let _publication = tokio::select! {
            guard = repo.files.materializing.lock() => guard,
            error = scope.cancelled() => return Err(error),
        };
        if p.paths.is_empty() {
            return Err(AgentError::Other(
                "Select at least one file to hydrate".into(),
            ));
        }
        let mut paths = std::collections::HashSet::new();
        let mut captures = Vec::new();
        let mut total = 0u64;
        for path in &p.paths {
            if !paths.insert(path) {
                continue;
            }
            let source =
                crate::repository::FileSource::capture_materialization(&repo, path, scope.child())
                    .await
                    .map_err(|error| materialization_error(path, error))?;
            total = total.saturating_add(source.size);
            captures.push((path, source));
        }
        let count = captures.len();
        let mut written = 0u64;
        for (published, (path, source)) in captures.into_iter().enumerate() {
            scope.check()?;
            let source = Arc::new(source);
            let outcome = job
                .write(source.clone(), written, total, Some((published, count)))
                .await;
            if let Ok(bytes) = &outcome {
                written = written.saturating_add(*bytes);
                if let Err(error) = crate::notify(
                    &job.out,
                    "progress/transfer",
                    job.progress(written, total, Some((published + 1, count))),
                )
                .await
                {
                    tracing::error!(%error, "materialization progress delivery failed");
                }
                let line = format!("Hydrated {path}");
                job.line(shellout::Stream::Stdout, &line).await;
            }
            let cleanup = source.release_reader().await;
            let result = match (outcome, cleanup) {
                (Err(operation), Err(cleanup)) => Err(AgentError::Cleanup {
                    operation: Box::new(operation),
                    cleanup: Box::new(cleanup),
                }),
                (result, Ok(())) => result,
                (Ok(_), Err(error)) => Err(error),
            };
            result.map_err(|error| materialization_error(path, error))?;
        }
        Ok(json!({ "written_bytes": written, "files": count }))
    };
    job.run(state, &scope, work).await
}

fn materialization_error(path: &str, error: AgentError) -> AgentError {
    if error.is_cancelled() {
        return error;
    }
    AgentError::Preview {
        context: format!("Hydrate {path}"),
        source: Box::new(error),
    }
}

struct Materialization<'a> {
    repo_id: &'a str,
    op_id: &'a str,
    out: SharedStdout,
}

impl Materialization<'_> {
    async fn write(
        &self,
        source: Arc<crate::repository::FileSource>,
        before: u64,
        total: u64,
        files: Option<(usize, usize)>,
    ) -> Result<u64> {
        let runtime = tokio::runtime::Handle::current();
        let job_id = self.op_id.to_owned();
        let repo_id = self.repo_id.to_owned();
        let out = self.out.clone();
        source
            .materialize(move |written| {
                let job = Materialization {
                    repo_id: &repo_id,
                    op_id: &job_id,
                    out: out.clone(),
                };
                runtime.block_on(crate::notify(
                    &out,
                    "progress/transfer",
                    job.progress(before.saturating_add(written), total, files),
                ))
            })
            .await
    }

    fn progress(&self, written: u64, total: u64, files: Option<(usize, usize)>) -> Value {
        json!({
            "op": "hydrate", "job_id": self.op_id, "repo_id": self.repo_id,
            "written_bytes": written, "total_bytes": total,
            "files_published": files.map(|(published, _)| published),
            "files_total": files.map(|(_, total)| total),
        })
    }

    async fn line(&self, stream: shellout::Stream, line: &str) {
        let event = shellout::ProgressEvent {
            op: "hydrate",
            job_id: self.op_id,
            stream,
            line: line.into(),
        };
        if let Err(error) = crate::notify(&self.out, "progress/line", event).await {
            tracing::error!(%error, "materialization progress delivery failed");
        }
    }

    async fn run(
        &self,
        state: &State,
        scope: &crate::repository::ReadScope,
        work: impl std::future::Future<Output = Result<Value>>,
    ) -> Result<Value> {
        let signal = state.cancel_registry.register(self.op_id.to_owned()).await;
        tokio::pin!(signal);
        tokio::pin!(work);
        let result = tokio::select! {
            biased;
            result = &mut work => result,
            _ = &mut signal => { scope.cancel(); work.await },
        };
        state.cancel_registry.unregister(self.op_id).await;
        // The writer checks cancellation before atomic publication. Its terminal
        // result wins a late cancel, so committed bytes cannot be called cancelled.
        let result = match scope.failure() {
            Some(error) => Err(error),
            None if matches!(&result, Err(error) if error.is_cancelled()) => {
                Err(AgentError::OpCancelled {
                    op_id: self.op_id.to_owned(),
                })
            }
            None => result,
        };
        let (status, outcome) = match &result {
            Ok(_) => (0, "completed"),
            Err(error) if error.is_cancelled() => (-1, "cancelled"),
            Err(_) => (1, "failed"),
        };
        if let Err(error) = &result {
            if !error.is_cancelled() {
                self.line(shellout::Stream::Stderr, &error.to_string())
                    .await;
            }
        }
        if let Err(error) = crate::notify(
        &self.out,
        "progress/complete",
        json!({
            "op": "hydrate", "job_id": self.op_id, "repo_id": self.repo_id, "status": status, "outcome": outcome,
        }),
    )
    .await
    {
        tracing::error!(%error, "materialization completion delivery failed");
    }
        result
    }
}
