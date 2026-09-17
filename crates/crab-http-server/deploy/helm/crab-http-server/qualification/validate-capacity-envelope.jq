def valid_capacity_envelope:
  .version == 1 and
  .resources.memory_bytes > 0 and
  .resources.disk_limit_bytes > 0 and
  .resources.disk_capacity_bytes > 0 and
  .resources.disk_capacity_bytes <= .resources.disk_limit_bytes and
  .resources.free_disk_bytes > 0 and
  .resources.free_disk_bytes <= .resources.disk_capacity_bytes and
  .resources.available_file_descriptors > 0 and
  .resources.job_credits > 0 and
  .admission.active_cells > 0 and
  .admission.retained_bytes > 0 and
  .admission.blocking_jobs > 0 and
  .admission.dirty_jobs > 0 and
  .admission.recovery_jobs > 0 and
  .admission.dirty_jobs <= .admission.blocking_jobs and
  .admission.recovery_jobs <= .admission.dirty_jobs and
  .admission.scratch_bytes > 0 and
  .admission.local_disk_bytes >= .admission.scratch_bytes and
  .admission.disk_reserve_bytes > 0 and
  .reservations.active_cell_page_cache_bytes > 0 and
  .reservations.active_cell_native_bytes > 0 and
  .reservations.active_cell_file_descriptors > 0 and
  .reservations.dirty_job_memory_bytes > 0 and
  .reservations.maximum_recovery_jobs >= .admission.recovery_jobs;

def valid_runtime_metrics($envelope):
  .active_cells >= 0 and
  .active_cells <= .active_cell_capacity and
  .active_cell_capacity == $envelope.admission.active_cells and
  .retained_bytes >= 0 and
  .retained_bytes <= .retained_capacity_bytes and
  .retained_capacity_bytes == $envelope.admission.retained_bytes and
  .local_disk_reserved_bytes >= 0 and
  .local_disk_reserved_bytes <= .local_disk_capacity_bytes and
  .local_disk_capacity_bytes == $envelope.admission.local_disk_bytes;

type == "array" and
length >= 3 and
([.[].pod_uid] | length == (unique | length)) and
all(.[];
  .phase == $phase and
  .pod != "" and
  (.pod_uid | test("^[0-9a-f-]{36}$")) and
  (.envelope | valid_capacity_envelope) and
  (.envelope as $envelope | .metrics | valid_runtime_metrics($envelope)))
