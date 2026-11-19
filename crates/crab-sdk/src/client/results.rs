use super::RemoteRepository;
use crate::{Error, ErrorKind, OperationOptions, Result};
use crab_workflow::results::RemoteResults;
use std::future::Future;

impl RemoteRepository {
    pub(crate) async fn results_read(
        &self,
        request: crab_workflow::results::ResultsRead,
        options: OperationOptions,
    ) -> Result<crab_workflow::results::ResultsResponse> {
        self.with_results(options, json_size, move |remote| async move {
            crab_workflow::results::ResultsReader::Remote(Box::new(remote))
                .read(request)
                .await
        })
        .await
    }
    async fn with_results<T, F, Fut>(
        &self,
        options: OperationOptions,
        size: fn(&T) -> Result<u64>,
        work: F,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(RemoteResults) -> Fut + Send + 'static,
        Fut: Future<Output = crab_workflow::Result<T>> + Send + 'static,
    {
        let options = options.with_owner_deadline()?;
        let state = self.client.0.clone();
        let store = self.results_store.clone();
        let prefix = self.results_prefix.clone();
        self.client
            .0
            .operations
            .run(options.clone(), move |cancel| async move {
                let store = state.admitted_store(&store, &options, &cancel)?;
                let remote = RemoteResults::new(
                    crab_workflow::WorkflowStore::from_storage(store),
                    &prefix,
                    cancel,
                );
                let value = work(remote).await.map_err(result_error)?;
                if size(&value)? > options.read_limits().max_response_bytes {
                    return Err(Error::new(
                        ErrorKind::LimitExceeded,
                        "Results response limit exceeded",
                    ));
                }
                Ok(value)
            })
            .await
    }
    pub(crate) async fn results_blob(
        &self,
        hash: String,
        options: OperationOptions,
    ) -> Result<Vec<u8>> {
        self.with_results(
            options,
            |bytes: &Vec<u8>| Ok(bytes.len() as u64),
            move |remote| async move { remote.blob(&hash).await },
        )
        .await
    }
}
fn json_size<T: serde::Serialize>(value: &T) -> Result<u64> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() as u64)
        .map_err(|error| {
            Error::with_source(ErrorKind::Corruption, "invalid Results response", error)
        })
}
fn result_error(error: crab_workflow::WorkflowError) -> Error {
    let kind = match &error {
        crab_workflow::WorkflowError::NotFound { .. } => ErrorKind::NotFound,
        crab_workflow::WorkflowError::Cancelled => ErrorKind::Cancelled,
        crab_workflow::WorkflowError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ErrorKind::NotFound
        }
        crab_workflow::WorkflowError::Io(error)
            if error.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            ErrorKind::Authorization
        }
        crab_workflow::WorkflowError::Io(_) => ErrorKind::Io,
        crab_workflow::WorkflowError::Storage(error)
        | crab_workflow::WorkflowError::NetworkTransient(error) => {
            crate::remote_error::object_store_kind(error)
        }
        crab_workflow::WorkflowError::StorageDomain(error) => {
            crate::remote_error::storage_kind(error)
        }
        crab_workflow::WorkflowError::Configuration { .. } => ErrorKind::InvalidInput,
        _ => ErrorKind::Corruption,
    };
    Error::with_source(kind, "workflow Results operation failed", error)
}
