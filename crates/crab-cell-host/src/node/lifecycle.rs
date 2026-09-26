//! Startup, drain, and shutdown.

use super::*;

impl CellNode {
    /// Opens readiness after all product startup probes have completed.
    pub fn start(&self) -> crab_cell_runtime::Result<()> {
        if !self.lease_installed.load(Ordering::Acquire) {
            return Err(Error::Control(
                "CellNode cannot become ready before its node lease is installed",
            ));
        }
        self.require_task_group()?;
        if !self
            .task_group
            .lock()
            .map_err(|_| Error::Control("CellNode task group lock poisoned"))?
            .as_ref()
            .is_some_and(|task_group| task_group.is_healthy())
        {
            return Err(Error::Control("CellNode task group is unhealthy"));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::Control("CellNode lifecycle lock poisoned"))?;
        if *state == NodeState::Starting {
            self.require_components_present()?;
            *state = NodeState::Ready;
            return Ok(());
        }
        if *state == NodeState::Ready {
            return Ok(());
        }
        Err(Error::Control(
            "CellNode cannot become ready after shutdown",
        ))
    }
    pub(super) fn require_components_present(&self) -> crab_cell_runtime::Result<()> {
        let required = self
            .required_components
            .lock()
            .map_err(|_| Error::Control("CellNode required-component lock poisoned"))?
            .clone();
        if required.is_empty() {
            return Ok(());
        }
        let facilities = self
            .facilities
            .lock()
            .map_err(|_| Error::Control("CellNode facility lock poisoned"))?;
        for name in required {
            if !facilities
                .iter()
                .any(|facility| facility.name == name && facility.owner.is_some())
            {
                return Err(Error::Control("CellNode required component is missing"));
            }
        }
        Ok(())
    }
    /// Stops admission, drains the runtime, and waits for its dispatcher.
    pub async fn drain(&self) -> crab_cell_runtime::Result<()> {
        self.drain_until(None).await
    }
    /// Stops admission and completes every owned drain phase by `deadline`.
    pub async fn drain_until(&self, deadline: Option<Instant>) -> crab_cell_runtime::Result<()> {
        let _shutdown = self.shutdown_lock.lock().await;
        self.drain_until_locked(deadline).await
    }
    pub(super) async fn drain_until_locked(
        &self,
        deadline: Option<Instant>,
    ) -> crab_cell_runtime::Result<()> {
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| Error::Control("CellNode lifecycle lock poisoned"))?;
            if *state == NodeState::Stopped {
                return Ok(());
            }
            *state = NodeState::Draining;
        }
        let task_group = self
            .task_group
            .lock()
            .map(|task_group| task_group.clone())
            .map_err(|_| Error::Control("CellNode task group lock poisoned"));
        let facilities = self
            .facilities
            .lock()
            .map(|facilities| {
                facilities
                    .iter()
                    .rev()
                    .map(|facility| (facility.name, Arc::clone(&facility.drain)))
                    .collect::<Vec<_>>()
            })
            .map_err(|_| Error::Control("CellNode facility lock poisoned"));
        let mut first_error = None;
        if let Some(error) = task_group.as_ref().err().map(|error| match error {
            Error::Control(message) => Error::Control(message),
            _ => Error::Control("CellNode task group unavailable during drain"),
        }) {
            first_error = Some(error);
        }
        if let Ok(Some(task_group)) = task_group.as_ref() {
            task_group.cancel_work();
        }
        match facilities {
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
            Ok(facilities) => {
                for (name, drain) in facilities {
                    let result = if name == "cell-coordination-tasks" {
                        // The task group is a retained owner, so its join must share the
                        // node deadline; an unbounded callback could strand shutdown.
                        match task_group.as_ref() {
                            Ok(Some(task_group)) => task_group.drain_work_until(deadline).await,
                            _ => drain().await,
                        }
                    } else {
                        match deadline {
                            Some(deadline) => {
                                match tokio::time::timeout_at(deadline.into(), drain()).await {
                                    Ok(result) => result,
                                    Err(_) => Err(Box::new(std::io::Error::new(
                                        std::io::ErrorKind::TimedOut,
                                        "CellNode facility drain deadline exceeded",
                                    ))
                                        as Box<dyn std::error::Error + Send + Sync>),
                                }
                            }
                            None => drain().await,
                        }
                    };
                    if let Err(source) = result
                        && first_error.is_none()
                    {
                        first_error = Some(Error::Facility { name, source });
                    }
                }
            }
        }
        let runtime_result = match deadline {
            Some(deadline) => {
                match tokio::time::timeout_at(deadline.into(), self.runtime.shutdown()).await {
                    Ok(result) => result,
                    Err(_) => Err(Error::Control("CellNode runtime drain deadline exceeded")),
                }
            }
            None => self.runtime.shutdown().await,
        };
        if first_error.is_none() {
            first_error = runtime_result.err();
        }
        // Session withdrawal fences the log authority. Keep its heartbeat live
        // until runtime publication and the durable log-close barrier finish.
        if let Ok(Some(task_group)) = task_group
            && let Err(source) = task_group.drain_until(deadline).await
            && first_error.is_none()
        {
            first_error = Some(Error::Facility {
                name: "cell-coordination-tasks",
                source,
            });
        }
        let result = match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        };
        let result = if result.is_ok() {
            match self.facilities.lock() {
                Ok(mut facilities) => {
                    facilities.clear();
                    Ok(())
                }
                Err(_) => Err(Error::Control("CellNode facility lock poisoned")),
            }
        } else {
            result
        };
        if result.is_ok()
            && let Ok(mut state) = self.state.lock()
        {
            *state = NodeState::Stopped;
        }
        result
    }
    /// Idempotent alias for graceful drain used by process shutdown hooks.
    pub async fn shutdown(&self) -> crab_cell_runtime::Result<()> {
        self.drain().await
    }
    /// Deadline-aware alias for graceful shutdown hooks.
    pub async fn shutdown_until(&self, deadline: Instant) -> crab_cell_runtime::Result<()> {
        self.drain_until(Some(deadline)).await
    }
}
