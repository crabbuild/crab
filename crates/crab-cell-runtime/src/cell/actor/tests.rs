use super::{CellRuntimeStats, bounded_u32};

#[test]
fn placement_projection_saturates_large_node_counters() {
    let stats = CellRuntimeStats {
        active_cells: usize::MAX,
        active_cell_capacity: usize::MAX,
        resident_bytes: 0,
        resident_capacity_bytes: 0,
        file_descriptors: 0,
        file_descriptor_capacity: 0,
        retained_bytes: 0,
        retained_capacity_bytes: 0,
        worker_jobs: usize::MAX,
        worker_job_capacity: usize::MAX,
        primitive_jobs: usize::MAX,
        primitive_job_capacity: usize::MAX,
        hydration_jobs: usize::MAX,
        hydration_job_capacity: usize::MAX,
        io_slots: 0,
        io_slot_capacity: 0,
        blocking_jobs: 0,
        blocking_job_capacity: 0,
        recovery_jobs: 0,
        recovery_job_capacity: 0,
        dirty_jobs: 0,
        dirty_job_capacity: 0,
        scratch_units: 0,
        scratch_unit_capacity: 0,
        local_disk_reserved_bytes: 0,
        local_disk_capacity_bytes: 0,
        unpublished_node_log_bytes: 0,
    };

    assert_eq!(bounded_u32(usize::MAX), u32::MAX);
    assert_eq!(stats.placement_active_cells(), u32::MAX);
    assert_eq!(stats.placement_active_cell_capacity(), u32::MAX);
    assert_eq!(stats.placement_running_jobs(), u32::MAX);
    assert_eq!(stats.placement_job_capacity(), u32::MAX);
}
