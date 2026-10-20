//! Resource-derived admission and local staging capacity.

use crate::Result;
const MAX_ACTIVE_CELLS: usize = 10_000;
const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const FILE_DESCRIPTOR_RESERVE_MINIMUM: usize = 128;

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub(crate) struct CellRuntimeBudget {
    pub(super) max_active_cells: usize,
    local_disk_bytes: u64,
    pub(super) disk_reserve_bytes: u64,
}

#[derive(Clone, serde::Serialize)]
pub(crate) struct CellCapacityReport {
    pub(super) version: u32,
    pub(super) resources: crate::resources::LocalResources,
    pub(super) admission: CellRuntimeBudget,
}

impl CellRuntimeBudget {
    pub(crate) fn from_resources(resources: crate::resources::LocalResources) -> Result<Self> {
        if resources.memory_bytes < GIB {
            return Err(crate::Error::Config(
                "Cellar requires at least 1 GiB effective memory",
            ));
        }
        // Keep filesystem and Cellar cache headroom while admitting bounded
        // transfers on small volumes. Per-transfer reservations enforce capacity.
        let disk_reserve_bytes = (resources.free_disk_bytes / 5).max(GIB);
        let local_disk_bytes = resources.free_disk_bytes.saturating_sub(disk_reserve_bytes);
        if local_disk_bytes < crate::git::receive::INITIAL_RECEIVE_RESERVATION {
            return Err(crate::Error::Config(
                "local staging requires capacity for one Git receive",
            ));
        }
        let memory = resources
            .memory_bytes
            .saturating_sub((resources.memory_bytes / 4).max(512 * MIB));
        let descriptors = resources
            .available_file_descriptors
            .saturating_sub(FILE_DESCRIPTOR_RESERVE_MINIMUM)
            / 8;
        let max_active_cells = usize::try_from(memory / (64 * MIB))
            .unwrap_or(usize::MAX)
            .min(descriptors)
            .min(MAX_ACTIVE_CELLS);
        if max_active_cells == 0 {
            return Err(crate::Error::Config(
                "Cellar has no memory or file-descriptor capacity",
            ));
        }
        Ok(Self {
            max_active_cells,
            local_disk_bytes,
            disk_reserve_bytes,
        })
    }
    pub(crate) fn local_disk(self) -> crate::local_disk::DiskBudget {
        crate::local_disk::DiskBudget::new(self.local_disk_bytes)
    }
}

impl CellCapacityReport {
    pub(super) fn measure(directory: &std::path::Path, limit: u64) -> Result<Self> {
        Self::from_resources(crate::resources::local_resources(directory, limit)?)
    }

    fn from_resources(resources: crate::resources::LocalResources) -> Result<Self> {
        Ok(Self {
            version: 2,
            admission: CellRuntimeBudget::from_resources(resources)?,
            resources,
        })
    }
}

pub(crate) fn report(directory: &std::path::Path, limit: u64) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&CellCapacityReport::measure(
        directory, limit,
    )?)?)
}

#[cfg(test)]
pub(crate) fn for_test() -> CellCapacityReport {
    let resources = crate::resources::LocalResources {
        memory_bytes: 2 * GIB,
        disk_limit_bytes: 32 * GIB,
        disk_capacity_bytes: 32 * GIB,
        free_disk_bytes: 32 * GIB,
        available_file_descriptors: 10_000,
        job_credits: 4,
    };
    CellCapacityReport::from_resources(resources).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn local_resources(
        memory_bytes: u64,
        free_disk_bytes: u64,
        available_file_descriptors: usize,
    ) -> crate::resources::LocalResources {
        crate::resources::LocalResources {
            memory_bytes,
            disk_limit_bytes: free_disk_bytes,
            disk_capacity_bytes: free_disk_bytes,
            free_disk_bytes,
            available_file_descriptors,
            job_credits: 16,
        }
    }

    #[test]
    fn cell_runtime_budget_keeps_the_node_safety_ceiling() {
        let budget =
            CellRuntimeBudget::from_resources(local_resources(1024 * GIB, 1000 * GIB, 1_000_000))
                .unwrap();
        assert_eq!(budget.max_active_cells, MAX_ACTIVE_CELLS);
    }

    #[test]
    fn cell_runtime_budget_reserves_memory_before_admitting_cells() {
        let budget =
            CellRuntimeBudget::from_resources(local_resources(64 * GIB, 1000 * GIB, 1_000_000))
                .unwrap();
        assert_eq!(budget.max_active_cells, 768);
    }

    #[test]
    fn cell_runtime_budget_admits_small_volumes_with_headroom() {
        for free_disk in [3 * GIB, 20_000_000_000] {
            let budget =
                CellRuntimeBudget::from_resources(local_resources(3 * GIB, free_disk, 10_000))
                    .unwrap();
            assert!(budget.local_disk_bytes >= crate::git::receive::INITIAL_RECEIVE_RESERVATION);
            assert!(budget.disk_reserve_bytes >= GIB);
            assert_eq!(
                budget.local_disk_bytes + budget.disk_reserve_bytes,
                free_disk
            );
        }
    }

    #[test]
    fn cell_runtime_budget_rejects_insufficient_memory() {
        assert!(
            CellRuntimeBudget::from_resources(local_resources(GIB - 1, 30 * GIB, 10_000)).is_err()
        );
    }

    #[test]
    fn cell_runtime_budget_rejects_insufficient_disk() {
        assert!(
            CellRuntimeBudget::from_resources(local_resources(2 * GIB, 3 * GIB - 1, 10_000))
                .is_err()
        );
    }

    #[test]
    fn cell_runtime_budget_rejects_insufficient_file_descriptors() {
        assert!(
            CellRuntimeBudget::from_resources(crate::resources::LocalResources {
                memory_bytes: 2 * GIB,
                disk_limit_bytes: 30 * GIB,
                disk_capacity_bytes: 30 * GIB,
                free_disk_bytes: 30 * GIB,
                available_file_descriptors: FILE_DESCRIPTOR_RESERVE_MINIMUM,
                job_credits: 16,
            })
            .is_err()
        );
    }
}
