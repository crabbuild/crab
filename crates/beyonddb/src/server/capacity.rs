//! Fresh host measurements constrained by the runtime's admission ledger.

use std::{io, path::Path};

use crab_cell_runtime::{
    CellRuntimeStats, Error, Result,
    node::{NodeCapacity, NodePlacementCapacity},
};

#[cfg(any(target_os = "linux", all(test, unix)))]
mod linux;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Memory {
    total: u64,
    available: u64,
}

struct Resources {
    memory: Memory,
    disk_total: u64,
    disk_available: u64,
}

/// Measures placement headroom within host limits and runtime reservations.
///
/// Call on a blocking worker with an existing data directory. Unknown host or
/// cgroup limits return an error; callers must not substitute assumed capacity.
pub fn measured_node_capacity(
    data_dir: &Path,
    stats: CellRuntimeStats,
) -> Result<(NodeCapacity, NodePlacementCapacity)> {
    let resources = measure(data_dir).map_err(|source| Error::Facility {
        name: "node-capacity-probe",
        source: Box::new(source),
    })?;
    constrain(resources, stats)
}

fn measure(data_dir: &Path) -> io::Result<Resources> {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let memory = Memory {
        total: system.total_memory(),
        available: system.available_memory(),
    };
    if memory.total == 0 || memory.available > memory.total {
        return Err(io::Error::other("host memory measurement is unavailable"));
    }
    #[cfg(target_os = "linux")]
    let memory = linux::memory(memory)?;
    // These platforms need their own process/container limit contract before
    // host-wide RAM can be offered to another node's placement controller.
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "placement resource measurement is unsupported on this platform",
        ));
    }
    let disk = fs4::statvfs(data_dir)?;
    Ok(Resources {
        memory,
        disk_total: disk.total_space(),
        disk_available: disk.available_space(),
    })
}

fn constrain(
    resources: Resources,
    stats: CellRuntimeStats,
) -> Result<(NodeCapacity, NodePlacementCapacity)> {
    let bytes = |value| u64::try_from(value).unwrap_or(u64::MAX);
    let memory_budget = bytes(stats.resident_capacity_bytes())
        .saturating_add(bytes(stats.retained_capacity_bytes()));
    let memory_total = resources.memory.total.min(memory_budget);
    let memory_reserved =
        bytes(stats.resident_bytes()).saturating_add(bytes(stats.retained_bytes()));
    let disk_total = resources.disk_total.min(stats.local_disk_capacity_bytes());
    let disk_reserved = stats.local_disk_reserved_bytes();
    let job_capacity = stats.placement_job_capacity();
    let running_jobs = stats.placement_running_jobs();
    // Reservations include future allocation. Charge them against OS headroom
    // too; counting only the budget could offer bytes already promised to work.
    let capacity = NodeCapacity {
        free_memory_bytes: resources
            .memory
            .available
            .saturating_sub(memory_reserved)
            .min(memory_total.saturating_sub(memory_reserved)),
        free_disk_bytes: resources
            .disk_available
            .saturating_sub(disk_reserved)
            .min(disk_total.saturating_sub(disk_reserved)),
        job_credits: job_capacity.saturating_sub(running_jobs),
        ..NodeCapacity::default()
    };
    let publication_bytes =
        bytes(stats.retained_bytes()).saturating_add(stats.unpublished_node_log_bytes());
    // Match the runtime placement wire contract: publication pressure uses MiB
    // of retained work; job counts cover SQL, primitive work, and hydration.
    let placement = NodePlacementCapacity {
        memory_capacity_bytes: memory_total,
        disk_capacity_bytes: disk_total,
        active_cells: stats.placement_active_cells(),
        max_active_cells: stats.placement_active_cell_capacity(),
        running_jobs,
        job_capacity,
        publication_backlog: u32::try_from(publication_bytes.div_ceil(1024 * 1024))
            .unwrap_or(u32::MAX),
        hydration_backlog: u32::try_from(stats.hydration_jobs()).unwrap_or(u32::MAX),
        primitive_backlog: u32::try_from(stats.primitive_jobs()).unwrap_or(u32::MAX),
    }
    .validated()?;
    Ok((capacity, placement))
}
