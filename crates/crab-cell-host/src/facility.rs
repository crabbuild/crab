//! Facility internals for the Cell node host.

use super::*;

/// One provider-owned lifecycle component attached to a [`CellNode`].
pub struct CellNodeFacility {
    pub(super) name: &'static str,
    pub(super) owner: Option<Arc<dyn Any + Send + Sync>>,
    pub(super) drain:
        Arc<dyn Fn() -> Pin<Box<dyn Future<Output = FacilityResult> + Send>> + Send + Sync>,
}

impl CellNodeFacility {
    /// Creates one named drain callback. The callback must be idempotent and
    /// must finish promptly when the node's shutdown cancellation is observed.
    pub fn new<F, Fut>(name: &'static str, drain: F) -> crab_cell_runtime::Result<Self>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = FacilityResult> + Send + 'static,
    {
        if name.is_empty() {
            return Err(Error::Control("CellNodeFacility name is empty"));
        }
        Ok(Self {
            name,
            owner: None,
            drain: Arc::new(move || Box::pin(drain())),
        })
    }

    /// Creates one named node-owned component with an idempotent drain callback.
    pub fn owned<T, F, Fut>(
        name: &'static str,
        owner: Arc<T>,
        drain: F,
    ) -> crab_cell_runtime::Result<Self>
    where
        T: Send + Sync + 'static,
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = FacilityResult> + Send + 'static,
    {
        if name.is_empty() {
            return Err(Error::Control("CellNodeFacility name is empty"));
        }
        Ok(Self {
            name,
            owner: Some(owner),
            drain: Arc::new(move || Box::pin(drain())),
        })
    }

    pub(super) fn owner<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.owner.as_ref()?.clone().downcast::<T>().ok()
    }
}
