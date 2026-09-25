//! Published node-session lease for a BeyondDB serving host.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crab_cell_runtime::node::{NodeAdvertisement, NodeDirectory, VersionedNodeAdvertisement};
use crab_cell_runtime::{Error, NodeLeaseGuard, Result};
use tokio_util::sync::CancellationToken;

const LEASE_MS: i64 = 10_000;
const HEARTBEAT: Duration = Duration::from_secs(3);
const RETRY: Duration = Duration::from_millis(500);
const FENCE_MARGIN: Duration = Duration::from_secs(1);

type SignAdvertisement = dyn Fn(i64, i64) -> Result<NodeAdvertisement> + Send + Sync;

/// Publishes signed node advertisements into the authoritative object-store directory.
pub struct NodeLeasePublisher {
    directory: NodeDirectory,
    sign: Arc<SignAdvertisement>,
}

impl NodeLeasePublisher {
    /// Bind the directory and the serving node's fixed boot-session signer.
    ///
    /// The signer must preserve its node, session, fleet, image, release, and
    /// key across renewals. The directory rejects a changed boot identity.
    pub fn new(
        directory: NodeDirectory,
        sign: impl Fn(i64, i64) -> Result<NodeAdvertisement> + Send + Sync + 'static,
    ) -> Self {
        Self {
            directory,
            sign: Arc::new(sign),
        }
    }

    /// Publish the initial lease before installing it in a Cell node.
    pub async fn publish(self) -> Result<PublishedNodeLease> {
        let now_ms = unix_time_ms()?;
        let advertisement = (self.sign)(now_ms, lease_expiry(now_ms)?)?;
        let observed = self.directory.create(advertisement, now_ms).await?;
        // Object-store publication can take time; lease the remaining
        // authoritative window, not a fresh window after the response.
        let guard = NodeLeaseGuard::new(unix_time_ms()?, observed.advertisement().expires_at_ms())?;
        Ok(PublishedNodeLease {
            publisher: self,
            observed,
            guard,
            fence_on_drop: true,
        })
    }
}

/// Retained lease task for a serving Cell node.
pub struct PublishedNodeLease {
    publisher: NodeLeasePublisher,
    observed: VersionedNodeAdvertisement,
    guard: NodeLeaseGuard,
    fence_on_drop: bool,
}

impl PublishedNodeLease {
    /// Clone the guard to install in the Cell runtime before starting requests.
    #[must_use]
    pub fn guard(&self) -> NodeLeaseGuard {
        self.guard.clone()
    }

    /// Refresh the authoritative lease until cancellation or terminal failure.
    pub async fn run(mut self, cancellation: &CancellationToken) -> Result<()> {
        let mut ticks = tokio::time::interval(HEARTBEAT);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticks.tick().await;
        loop {
            tokio::select! {
                () = cancellation.cancelled() => {
                    // The node drains its runtime after canceling owned tasks.
                    // Keep the lease valid for that drain; expiry still fences it.
                    self.fence_on_drop = false;
                    return Ok(());
                },
                _ = ticks.tick() => {}
            }
            loop {
                match self.refresh().await {
                    Ok(()) => break,
                    Err(error) if self.guard.remaining() > FENCE_MARGIN => {
                        tokio::select! {
                            () = cancellation.cancelled() => {
                                self.fence_on_drop = false;
                                return Ok(());
                            },
                            () = tokio::time::sleep(RETRY) => {}
                        }
                        self.guard.check()?;
                        if self.guard.remaining() <= FENCE_MARGIN {
                            self.guard.fence();
                            return Err(error);
                        }
                    }
                    Err(error) => {
                        self.guard.fence();
                        return Err(error);
                    }
                }
            }
        }
    }

    async fn refresh(&mut self) -> Result<()> {
        self.guard.check()?;
        let now_ms = unix_time_ms()?;
        let next = (self.publisher.sign)(now_ms, lease_expiry(now_ms)?)?;
        let observed = self
            .publisher
            .directory
            .refresh(&self.observed, next, now_ms)
            .await?;
        self.guard
            .renew(unix_time_ms()?, observed.advertisement().expires_at_ms())?;
        self.observed = observed;
        Ok(())
    }
}

impl Drop for PublishedNodeLease {
    fn drop(&mut self) {
        if self.fence_on_drop {
            self.guard.fence();
        }
    }
}

fn unix_time_ms() -> Result<i64> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Node("system clock is before Unix epoch"))?
            .as_millis(),
    )
    .map_err(|_| Error::Node("system clock exceeds node lease range"))
}

fn lease_expiry(now_ms: i64) -> Result<i64> {
    now_ms
        .checked_add(LEASE_MS)
        .ok_or(Error::Node("node lease expiry overflow"))
}
