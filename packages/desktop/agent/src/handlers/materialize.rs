use super::*;
use crate::repository::{Dehydration, FileSource, FileVersion, ReadScope, Restoration};

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
        direction: Direction::Hydrate,
    };
    let work = async {
        let file = repo.files.get(&p.file.file_id).await?;
        let (_guard, source) = file.materialization(scope.clone()).await?;
        let total_bytes = source.size;
        let version = source.version()?;
        job.source(0, version).await?;
        let result = source
            .materialize(job.reporter(0, total_bytes, Some((0, 1))))
            .await;
        let published = match &result {
            Ok(bytes) => Some(*bytes),
            Err(error) => error.published_bytes(),
        };
        if let Some(bytes) = published
            && let Err(error) = crate::notify(
                &job.out,
                "progress/transfer",
                job.progress(bytes, total_bytes, Some((1, 1))),
            )
            .await
        {
            tracing::error!(%error, "file publication receipt delivery failed");
        }
        result.map(|written| json!({ "written_bytes": written }))
    };
    job.run(state, &repo, &scope, work).await
}

#[derive(Deserialize)]
pub(super) struct FilesMaterializeParams {
    repo_id: String,
    paths: Vec<String>,
    op_id: String,
}

#[derive(Clone, Copy)]
pub(super) enum Direction {
    Hydrate,
    Dehydrate,
    Restore,
}

impl Direction {
    fn verb(self) -> &'static str {
        match self {
            Self::Hydrate => "hydrate",
            Self::Dehydrate => "dehydrate",
            Self::Restore => "restore",
        }
    }

    fn file_error(self, path: &str, error: AgentError) -> AgentError {
        if error.is_cancelled() {
            return error;
        }
        let action = match self {
            Self::Hydrate => "Hydrate",
            Self::Dehydrate => "Dehydrate",
            Self::Restore => "Restore",
        };
        AgentError::Preview {
            context: format!("{action} {path}"),
            source: Box::new(error),
        }
    }
}

enum SelectedFile {
    Hydrate(Arc<FileSource>),
    Dehydrate(Arc<Dehydration>),
    Restore(Arc<Restoration>),
}

impl SelectedFile {
    fn version(&self) -> Result<FileVersion> {
        match self {
            Self::Hydrate(source) => source.version(),
            Self::Dehydrate(source) => source.version(),
            Self::Restore(source) => source.version(),
        }
    }

    async fn capture(
        repo: &BrowseSession,
        path: &str,
        direction: Direction,
        scope: ReadScope,
    ) -> Result<Self> {
        Ok(match direction {
            Direction::Hydrate => Self::Hydrate(Arc::new(
                FileSource::capture_materialization(repo, path, scope).await?,
            )),
            Direction::Dehydrate => {
                Self::Dehydrate(Arc::new(Dehydration::capture(repo, path, scope).await?))
            }
            Direction::Restore => {
                Self::Restore(Arc::new(Restoration::capture(repo, path, scope).await?))
            }
        })
    }

    fn work_bytes(&self) -> u64 {
        match self {
            Self::Hydrate(source) => source.size,
            Self::Dehydrate(source) => source.size().saturating_mul(2),
            Self::Restore(source) => source.size(),
        }
    }

    async fn publish(
        &self,
        repo: &BrowseSession,
        progress: impl FnMut(u64) -> Result<()> + Send + 'static,
    ) -> Result<u64> {
        match self {
            Self::Hydrate(source) => source.clone().materialize(progress).await,
            Self::Dehydrate(source) => source.clone().run(repo, progress).await,
            Self::Restore(source) => source.clone().run(progress).await,
        }
    }

    async fn release_reader(&self) -> Result<()> {
        match self {
            Self::Hydrate(source) => source.release_reader().await,
            Self::Dehydrate(source) => source.release_reader().await,
            Self::Restore(source) => source.release_reader().await,
        }
    }
}

pub(super) async fn many(
    state: &State,
    p: FilesMaterializeParams,
    out: SharedStdout,
    direction: Direction,
) -> Result<Value> {
    let repo = state.get(&p.repo_id).await?;
    let scope = repo.read_scope();
    let job = Materialization {
        repo_id: &p.repo_id,
        op_id: &p.op_id,
        out,
        direction,
    };
    let work = async {
        if p.paths.is_empty() {
            return Err(AgentError::Other(format!(
                "Select at least one file to {}",
                direction.verb()
            )));
        }
        let mut unique = std::collections::HashSet::new();
        let paths: Vec<_> = p.paths.iter().filter(|path| unique.insert(*path)).collect();
        crate::notify(
            &job.out,
            "progress/transfer",
            job.progress(0, 0, Some((0, paths.len()))),
        )
        .await?;
        let mut captures = Vec::new();
        let mut total = 0u64;
        for (position, path) in paths.into_iter().enumerate() {
            scope.check()?;
            let source = SelectedFile::capture(&repo, path, direction, scope.child())
                .await
                .map_err(|error| direction.file_error(path, error))?;
            let version = source.version()?;
            job.source(position, version).await?;
            total = total.saturating_add(source.work_bytes());
            captures.push((path, source));
        }
        let count = captures.len();
        let mut processed = 0u64;
        let mut written = 0u64;
        for (published, (path, source)) in captures.into_iter().enumerate() {
            scope.check()?;
            let outcome = source
                .publish(
                    &repo,
                    job.reporter(processed, total, Some((published, count))),
                )
                .await;
            let published_bytes = match &outcome {
                Ok(bytes) => Some(*bytes),
                Err(error) => error.published_bytes(),
            };
            if let Some(bytes) = published_bytes {
                written = written.saturating_add(bytes);
                let completed_work = if matches!(direction, Direction::Restore) {
                    bytes
                } else {
                    source.work_bytes()
                };
                processed = processed.saturating_add(completed_work);
                if let Err(error) = crate::notify(
                    &job.out,
                    "progress/transfer",
                    job.progress(processed, total, Some((published + 1, count))),
                )
                .await
                {
                    tracing::error!(%error, "file transfer progress delivery failed");
                }
                let action = match direction {
                    Direction::Hydrate => "Hydrated",
                    Direction::Dehydrate => "Dehydrated",
                    Direction::Restore => "Restored",
                };
                job.line(shellout::Stream::Stdout, &format!("{action} {path}"))
                    .await;
            }
            let result = match (outcome, source.release_reader().await) {
                (Err(operation), Err(cleanup)) => Err(AgentError::Cleanup {
                    operation: Box::new(operation),
                    cleanup: Box::new(cleanup),
                }),
                (result, Ok(())) => result,
                (Ok(_), Err(error)) => Err(error),
            };
            result.map_err(|error| direction.file_error(path, error))?;
        }
        Ok(json!({ "written_bytes": written, "files": count }))
    };
    job.run(state, &repo, &scope, work).await
}

struct Materialization<'a> {
    repo_id: &'a str,
    op_id: &'a str,
    out: SharedStdout,
    direction: Direction,
}

impl Materialization<'_> {
    async fn source(&self, position: usize, version: FileVersion) -> Result<()> {
        let source = json!({
            "job_id": self.op_id, "repo_id": self.repo_id, "position": position,
            "source": version,
        });
        crate::notify(&self.out, "progress/source", source).await
    }

    fn reporter(
        &self,
        before: u64,
        total: u64,
        files: Option<(usize, usize)>,
    ) -> impl FnMut(u64) -> Result<()> + Send + 'static {
        let runtime = tokio::runtime::Handle::current();
        let job_id = self.op_id.to_owned();
        let repo_id = self.repo_id.to_owned();
        let out = self.out.clone();
        let direction = self.direction;
        move |processed| {
            let job = Materialization {
                repo_id: &repo_id,
                op_id: &job_id,
                out: out.clone(),
                direction,
            };
            runtime.block_on(crate::notify(
                &out,
                "progress/transfer",
                job.progress(before.saturating_add(processed), total, files),
            ))
        }
    }

    fn progress(&self, processed: u64, total: u64, files: Option<(usize, usize)>) -> Value {
        let (written, checked) = match self.direction {
            Direction::Hydrate => (processed, None),
            Direction::Dehydrate => (0, Some(processed)),
            Direction::Restore => (processed, None),
        };
        // Git checkout conversions can change the output size. Keep its total
        // unknown rather than label raw blob size as converted bytes written.
        let total = (!matches!(self.direction, Direction::Restore)).then_some(total);
        json!({
            "op": self.direction.verb(), "job_id": self.op_id, "repo_id": self.repo_id,
            "written_bytes": written, "checked_bytes": checked, "total_bytes": total,
            "files_published": files.map(|(published, _)| published),
            "files_total": files.map(|(_, total)| total),
        })
    }

    async fn line(&self, stream: shellout::Stream, line: &str) {
        let event = shellout::ProgressEvent {
            op: self.direction.verb(),
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
        repo: &BrowseSession,
        scope: &ReadScope,
        work: impl std::future::Future<Output = Result<Value>>,
    ) -> Result<Value> {
        let work = async {
            let _publication = tokio::select! {
                guard = repo.files.materializing.lock() => guard,
                error = scope.cancelled() => return Err(error),
            };
            scope.check()?;
            let root = repo
                .work_dir
                .as_deref()
                .ok_or_else(|| AgentError::Other("File transfers require a working tree".into()))?;
            super::local_filters::configure(root, scope.options()).await?;
            scope.check()?;
            work.await
        };
        let signal = state.cancel_registry.register(self.op_id.to_owned()).await;
        tokio::pin!(signal);
        tokio::pin!(work);
        // Publish admission only after cancellation is registered, before
        // waiting for another publisher or configuring filters. Queued work
        // must be cancellable without waiting for its first written byte.
        let admitted =
            crate::notify(&self.out, "progress/transfer", self.progress(0, 0, None)).await;
        let result = match admitted {
            Err(error) => Err(error),
            Ok(()) => tokio::select! {
                biased;
                result = &mut work => result,
                _ = &mut signal => { scope.cancel(); work.await },
            },
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
        if let Err(error) = &result
            && !error.is_cancelled()
        {
            self.line(shellout::Stream::Stderr, &error.to_string())
                .await;
        }
        let completion = json!({
            "op": self.direction.verb(), "job_id": self.op_id, "repo_id": self.repo_id, "status": status, "outcome": outcome,
        });
        if let Err(error) = crate::notify(&self.out, "progress/complete", completion).await {
            tracing::error!(%error, "file transfer completion delivery failed");
        }
        result
    }
}
