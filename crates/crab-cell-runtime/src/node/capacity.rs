//! Node capacity and failure-domain records.

use super::*;

/// Capacity hints published by one node boot session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeCapacity {
    /// Free memory the node reports.
    pub free_memory_bytes: u64,
    /// Free scratch disk the node reports.
    pub free_disk_bytes: u64,
    /// Free space in the node's follower store.
    pub follower_free_bytes: u64,
    /// Bytes the node's follower store retains.
    pub follower_retained_bytes: u64,
    /// Job credits the node offers to the fleet.
    pub job_credits: u32,
    /// Node-log protocol version the node speaks.
    pub log_protocol: u32,
}

/// Signed runtime capacity measurements used by the placement planner.
///
/// The ordinary capacity hints remain intentionally small and compatible with
/// older node records. This optional block carries the totals and live counts
/// required to compare a node's usable headroom without guessing from host
/// totals on the receiving side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodePlacementCapacity {
    /// Total memory the node reports.
    pub memory_capacity_bytes: u64,
    /// Total scratch disk the node reports.
    pub disk_capacity_bytes: u64,
    /// Cells the node currently owns.
    pub active_cells: u32,
    /// Cells the node admits.
    pub max_active_cells: u32,
    /// Jobs the node is running.
    pub running_jobs: u32,
    /// Jobs the node admits.
    pub job_capacity: u32,
    /// Publications waiting to be acknowledged.
    pub publication_backlog: u32,
    /// Hydrations waiting to run.
    pub hydration_backlog: u32,
    /// Primitive maintenance items waiting.
    pub primitive_backlog: u32,
}

impl NodePlacementCapacity {
    /// Validates and returns a placement snapshot with measured node totals.
    pub const fn validated(self) -> Result<Self> {
        if self.memory_capacity_bytes == 0
            || self.disk_capacity_bytes == 0
            || self.max_active_cells == 0
            || self.active_cells > self.max_active_cells
            || self.job_capacity == 0
            || self.running_jobs > self.job_capacity
        {
            return Err(Error::Node("placement capacity is invalid"));
        }
        Ok(self)
    }
}

/// Stable topology labels used only to prefer independent follower nodes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeFailureDomain {
    pub(super) zone: Option<String>,
    pub(super) host: Option<String>,
}

impl NodeFailureDomain {
    /// Validates optional zone and host labels advertised for one boot identity.
    pub fn new(zone: Option<String>, host: Option<String>) -> Result<Self> {
        let domain = Self { zone, host };
        domain.validate()?;
        Ok(domain)
    }

    /// Returns the availability zone, when the node declares one.
    #[must_use]
    pub fn zone(&self) -> Option<&str> {
        self.zone.as_deref()
    }

    /// Returns the host, when the node declares one.
    #[must_use]
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    pub(super) fn validate(&self) -> Result<()> {
        if [self.zone.as_deref(), self.host.as_deref()]
            .into_iter()
            .flatten()
            .any(|value| {
                value.is_empty()
                    || value.len() > MAX_FAILURE_DOMAIN_BYTES
                    || !value.is_ascii()
                    || value.bytes().any(|byte| !byte.is_ascii_graphic())
            })
        {
            return Err(Error::Node("node failure domain is invalid"));
        }
        Ok(())
    }
}
