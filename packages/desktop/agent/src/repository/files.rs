use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock, RwLockReadGuard};

use super::{FileSource, ReadScope};
use crate::{AgentError, Result};

const MAX_OPEN_FILES: usize = 128;

#[derive(Clone, Default)]
pub(crate) struct Files {
    registry: Arc<Mutex<Registry>>,
    pub(crate) materializing: Arc<Mutex<()>>,
}

#[derive(Default)]
struct Registry {
    closed: bool,
    files: HashMap<String, Arc<OpenedFile>>,
}

pub(crate) struct OpenedFile {
    source: Arc<FileSource>,
    active: RwLock<()>,
}

impl Files {
    pub(crate) async fn insert(&self, id: String, source: FileSource) -> Result<Arc<OpenedFile>> {
        let mut registry = self.registry.lock().await;
        if registry.closed || registry.files.len() >= MAX_OPEN_FILES {
            let message = if registry.closed {
                "repository file reads are closed"
            } else {
                "close an existing preview before opening more files"
            };
            drop(registry);
            source.close().await?;
            return Err(AgentError::Other(message.into()));
        }
        let file = Arc::new(OpenedFile {
            source: Arc::new(source),
            active: RwLock::new(()),
        });
        registry.files.insert(id, file.clone());
        Ok(file)
    }

    pub(crate) async fn get(&self, id: &str) -> Result<Arc<OpenedFile>> {
        let registry = self.registry.lock().await;
        if registry.closed {
            return Err(AgentError::Other("repository file reads are closed".into()));
        }
        registry
            .files
            .get(id)
            .cloned()
            .ok_or_else(|| AgentError::Other("file reader is closed; reopen the file".into()))
    }

    pub(crate) async fn remove(&self, id: &str) -> Result<()> {
        let file = self.registry.lock().await.files.get(id).cloned();
        if let Some(file) = file {
            file.close().await?;
            self.registry.lock().await.files.remove(id);
        }
        Ok(())
    }

    pub(crate) async fn close(&self) -> Result<()> {
        let _materializing = self.materializing.lock().await;
        let ids = {
            let mut registry = self.registry.lock().await;
            registry.closed = true;
            registry.files.keys().cloned().collect::<Vec<_>>()
        };
        let mut result = Ok(());
        for id in ids {
            if let Err(error) = self.remove(&id).await {
                result = Err(error);
            }
        }
        result
    }
}

impl OpenedFile {
    pub(crate) fn describe(&self) -> serde_json::Value {
        self.source.describe()
    }

    pub(crate) async fn query(&self) -> Result<(RwLockReadGuard<'_, ()>, Arc<FileSource>)> {
        let guard = self.active.read().await;
        self.source.check_cancelled()?;
        Ok((guard, Arc::new(self.source.query())))
    }

    pub(crate) async fn materialization(
        &self,
        scope: ReadScope,
    ) -> Result<(RwLockReadGuard<'_, ()>, Arc<FileSource>)> {
        let guard = self.active.read().await;
        self.source.check_cancelled()?;
        Ok((guard, Arc::new(self.source.materialization(scope)?)))
    }

    async fn close(&self) -> Result<()> {
        // Cancellation stops admission and reaches each query. Keep the native
        // source until all SDK streams, parsers and cancellation acknowledgements
        // have drained; dropping the last UI reference does not establish that.
        self.source.cancel();
        let _guard = self.active.write().await;
        self.source.close().await
    }
}
