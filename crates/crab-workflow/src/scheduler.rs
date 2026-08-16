//! Semaphore-bounded parallel DAG scheduler.
//!
//! Replaces the linear topo-walk in `cmd/run.rs` with a concurrent
//! scheduler that respects the configured parallelism cap. Independent
//! stages run concurrently via tokio tasks; a ready-queue seeded from
//! the DAG's source nodes ensures deterministic scheduling order
//! (min-heap on stage name for tie-breaking).
//!
//! The scheduler preserves all existing behavior:
//! - `--keep-going` / `--ignore-errors` partial-failure policies
//! - Retry loops (held within each stage's spawned task)
//! - JSONL event interleaving (serialized via `Arc<Mutex<JsonlStream>>`)
//! - Lockfile persistence at the end of the run
//! - Journal writes (SQLite WAL mode handles concurrent writers)
//!
//! Resource constraints (P9): The scheduler maintains a [`ResourcePool`]
//! that tracks available CPU/GPU/memory. A stage is eligible to start
//! only when `pool.can_fit(stage.resources)` returns true. On
//! completion, resources are released.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::sync::Arc;

use tokio::sync::Semaphore;
use tracing::warn;

use crate::Graph;
use crate::stage::{Resources, StageName};

/// Configuration for the parallel scheduler.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Maximum concurrent stage executions.
    pub parallelism: u32,
    /// Whether to continue scheduling independent branches on failure.
    pub keep_going: bool,
    /// Whether to attempt all stages regardless of upstream failures.
    pub ignore_errors: bool,
}

/// The parallel DAG scheduler.
///
/// Manages a ready-queue of stages eligible for execution, a semaphore
/// bounding concurrency, and an mpsc channel for collecting results
/// from spawned stage tasks. Also maintains a [`ResourcePool`] that
/// gates stage starts on resource availability.
pub struct DagScheduler {
    config: SchedulerConfig,
    /// Stages ready to execute (min-heap by name for determinism).
    pub ready_queue: BinaryHeap<Reverse<StageName>>,
    /// Stages currently executing.
    in_flight: BTreeSet<StageName>,
    /// Stages that completed successfully.
    pub succeeded: BTreeSet<StageName>,
    /// Stages that failed.
    pub failed: BTreeSet<StageName>,
    /// Stages blocked by upstream failures.
    pub not_started: BTreeSet<StageName>,
    /// Semaphore bounding concurrent executions.
    semaphore: Arc<Semaphore>,
    /// In-degree tracking: how many producers each stage is still waiting on.
    remaining_deps: BTreeMap<StageName, usize>,
    /// Resource pool tracking available CPU/GPU/memory.
    pub resource_pool: ResourcePool,
    /// Stages that are ready but waiting for resources. Kept sorted
    /// by name for deterministic scheduling.
    resource_waiting: Vec<StageName>,
}

impl DagScheduler {
    /// Create a new scheduler from the DAG graph and configuration.
    ///
    /// Seeds the ready-queue with source nodes (stages with no
    /// in-DAG producers) that are eligible for execution.
    pub fn new(graph: &Graph, config: SchedulerConfig, skip_stages: &BTreeSet<StageName>) -> Self {
        Self::with_resource_pool(graph, config, skip_stages, ResourcePool::detect())
    }

    /// Create a new scheduler with an explicit resource pool (for testing).
    pub fn with_resource_pool(
        graph: &Graph,
        config: SchedulerConfig,
        skip_stages: &BTreeSet<StageName>,
        resource_pool: ResourcePool,
    ) -> Self {
        let parallelism = config.parallelism.max(1) as usize;
        let semaphore = Arc::new(Semaphore::new(parallelism));

        // Compute in-degree for each stage (number of producers).
        let mut remaining_deps: BTreeMap<StageName, usize> = BTreeMap::new();
        let topo = graph.toposort();
        for stage_name in &topo {
            let producers = graph.producers_of(stage_name);
            remaining_deps.insert(stage_name.clone(), producers.len());
        }

        // Seed the ready queue with source nodes (in-degree 0) that
        // are not in the skip set.
        let mut ready_queue: BinaryHeap<Reverse<StageName>> = BinaryHeap::new();
        for (name, &deg) in &remaining_deps {
            if deg == 0 && !skip_stages.contains(name) {
                ready_queue.push(Reverse(name.clone()));
            }
        }

        Self {
            config,
            ready_queue,
            in_flight: BTreeSet::new(),
            succeeded: BTreeSet::new(),
            failed: BTreeSet::new(),
            not_started: BTreeSet::new(),
            semaphore,
            remaining_deps,
            resource_pool,
            resource_waiting: Vec::new(),
        }
    }

    /// Get the semaphore for acquiring permits.
    pub fn semaphore(&self) -> &Arc<Semaphore> {
        &self.semaphore
    }

    /// Check if the scheduler is done (no in-flight, no ready stages,
    /// and no resource-waiting stages).
    pub fn is_done(&self) -> bool {
        self.in_flight.is_empty() && self.ready_queue.is_empty() && self.resource_waiting.is_empty()
    }

    /// Pop the next ready stage, if any, that should be dispatched.
    ///
    /// Returns `None` if the ready queue is empty or if scheduling
    /// has been stopped due to a failure (when not in keep-going mode).
    ///
    /// Resource-aware: if the next stage in the ready queue cannot
    /// fit in the resource pool, it is moved to the `resource_waiting`
    /// list and the next candidate is tried. Stages waiting for
    /// resources are re-checked when resources are released.
    pub fn next_ready(
        &mut self,
        stage_resources: &BTreeMap<StageName, Resources>,
    ) -> Option<StageName> {
        // If we have failures and we're not keeping going, don't
        // start new stages.
        if !self.config.keep_going && !self.failed.is_empty() {
            return None;
        }

        // Try stages from the ready queue.
        let mut deferred: Vec<StageName> = Vec::new();
        let result = loop {
            match self.ready_queue.pop() {
                Some(Reverse(name)) => {
                    let resources = stage_resources.get(&name).cloned().unwrap_or_default();
                    if self.resource_pool.can_fit(&resources) {
                        break Some(name);
                    }
                    deferred.push(name);
                }
                None => break None,
            }
        };

        // Put deferred stages into the resource_waiting list.
        for name in deferred {
            self.resource_waiting.push(name);
        }
        self.resource_waiting.sort();

        result
    }

    /// Legacy `next_ready` without resource checking — used when
    /// no resource map is available (backward compat).
    pub fn next_ready_no_resources(&mut self) -> Option<StageName> {
        if !self.config.keep_going && !self.failed.is_empty() {
            return None;
        }
        self.ready_queue.pop().map(|Reverse(name)| name)
    }

    /// Mark a stage as dispatched (in-flight).
    pub fn mark_dispatched(&mut self, name: &StageName) {
        self.in_flight.insert(name.clone());
    }

    /// Handle a successful stage completion. Updates internal state,
    /// releases resources, and pushes newly-ready consumers to the
    /// ready queue. Also re-checks resource-waiting stages.
    pub fn handle_success(&mut self, stage_name: &StageName, graph: &Graph, resources: &Resources) {
        self.in_flight.remove(stage_name);
        self.succeeded.insert(stage_name.clone());
        self.resource_pool.release(resources);

        // Check consumers: decrement their remaining deps count.
        // If a consumer reaches 0, it's ready to execute.
        let consumers = graph.consumers_of(stage_name);
        for consumer in consumers {
            if self.not_started.contains(&consumer) {
                continue;
            }
            if let Some(count) = self.remaining_deps.get_mut(&consumer) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.ready_queue.push(Reverse(consumer));
                }
            }
        }

        // Re-check resource-waiting stages now that resources freed.
        self.requeue_resource_waiting();
    }

    /// Handle a failed stage. Applies the keep-going / ignore-errors
    /// policy to determine which downstream stages are blocked.
    /// Releases resources held by the failed stage.
    pub fn handle_failure(&mut self, stage_name: &StageName, graph: &Graph, resources: &Resources) {
        self.in_flight.remove(stage_name);
        self.failed.insert(stage_name.clone());
        self.resource_pool.release(resources);

        if self.config.ignore_errors {
            // Under --ignore-errors, downstream stages are still
            // attempted. Decrement their dep counts as if the stage
            // succeeded (they'll hit StageDepMissing at resolve time
            // if the output is actually needed).
            let consumers = graph.consumers_of(stage_name);
            for consumer in consumers {
                if self.not_started.contains(&consumer) {
                    continue;
                }
                if let Some(count) = self.remaining_deps.get_mut(&consumer) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        self.ready_queue.push(Reverse(consumer));
                    }
                }
            }
        } else if self.config.keep_going {
            // Under --keep-going, mark all transitive consumers of
            // the failed stage as not_started.
            let blocked = transitive_consumers(stage_name, graph);
            for blocked_name in blocked {
                self.not_started.insert(blocked_name);
            }
        }
        // Under default mode (no keep-going), next_ready() will
        // return None because failed is non-empty, effectively
        // stopping new dispatches. In-flight stages finish naturally.

        // Re-check resource-waiting stages now that resources freed.
        self.requeue_resource_waiting();
    }

    /// Move resource-waiting stages back to the ready queue so they
    /// can be re-evaluated for resource availability.
    fn requeue_resource_waiting(&mut self) {
        let waiting = std::mem::take(&mut self.resource_waiting);
        for name in waiting {
            self.ready_queue.push(Reverse(name));
        }
    }

    /// Acquire resources for a stage that is about to start.
    pub fn acquire_resources(&mut self, resources: &Resources) {
        self.resource_pool.acquire(resources);
    }

    /// Check if a stage should be skipped because it's blocked.
    pub fn is_blocked(&self, name: &StageName) -> bool {
        self.not_started.contains(name)
    }

    /// Number of stages currently in flight.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Whether there are any in-flight stages.
    pub fn has_in_flight(&self) -> bool {
        !self.in_flight.is_empty()
    }
}

/// Compute the transitive consumers of a stage (all downstream stages
/// reachable via the producer→consumer edges).
fn transitive_consumers(root: &StageName, graph: &Graph) -> BTreeSet<StageName> {
    let mut visited = BTreeSet::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back(root.clone());

    while let Some(current) = queue.pop_front() {
        let consumers = graph.consumers_of(&current);
        for consumer in consumers {
            if visited.insert(consumer.clone()) {
                queue.push_back(consumer);
            }
        }
    }
    visited
}

/// Resolve the effective parallelism value from CLI override, config,
/// and system detection.
///
/// Priority:
/// 1. `--parallelism N` CLI flag (if set)
/// 2. `[workflow] parallelism` from config
/// 3. `min(available_parallelism, 8)` as the compiled-in default
pub fn resolve_parallelism(cli_override: Option<u32>, config_value: u32) -> u32 {
    if let Some(n) = cli_override {
        return n.max(1);
    }
    if config_value > 0 {
        return config_value;
    }
    // Fallback: detect system parallelism, cap at 8.
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4);
    cpus.clamp(1, 8)
}

/// Tracks available CPU/GPU/memory resources on the machine and
/// gates stage starts on resource availability.
///
/// Initialized from system detection (logical CPUs, GPU count via
/// `CRAB_GPU_COUNT` env var, total RAM via `sysinfo` or env var).
/// When a stage's declared resources exceed the machine's capacity,
/// a `warn!` is emitted and the stage runs anyway (best-effort).
#[derive(Debug, Clone)]
pub struct ResourcePool {
    pub total_cpu: u32,
    pub total_gpu: u32,
    pub total_memory_bytes: u64,
    pub used_cpu: u32,
    pub used_gpu: u32,
    pub used_memory_bytes: u64,
}

impl ResourcePool {
    /// Create a pool from system detection.
    ///
    /// - CPU: `std::thread::available_parallelism()` (fallback: 4)
    /// - GPU: `CRAB_GPU_COUNT` env var (fallback: 0)
    /// - Memory: `CRAB_TOTAL_MEMORY_BYTES` env var (fallback: 0,
    ///   meaning memory is not tracked)
    pub fn detect() -> Self {
        let total_cpu = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(4);

        let total_gpu = std::env::var("CRAB_GPU_COUNT")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);

        let total_memory_bytes = std::env::var("CRAB_TOTAL_MEMORY_BYTES")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        Self {
            total_cpu,
            total_gpu,
            total_memory_bytes,
            used_cpu: 0,
            used_gpu: 0,
            used_memory_bytes: 0,
        }
    }

    /// Create a pool with explicit capacity values (for testing).
    pub fn with_capacity(cpu: u32, gpu: u32, memory_bytes: u64) -> Self {
        Self {
            total_cpu: cpu,
            total_gpu: gpu,
            total_memory_bytes: memory_bytes,
            used_cpu: 0,
            used_gpu: 0,
            used_memory_bytes: 0,
        }
    }

    /// Check whether the pool has enough free resources for the
    /// given stage requirements.
    ///
    /// If the stage's requirements exceed the machine's total
    /// capacity, a `warn!` is emitted and the method returns `true`
    /// (best-effort: the stage runs anyway). This handles the case
    /// where a user declares `gpu: 2` on a machine with 1 GPU.
    pub fn can_fit(&self, resources: &Resources) -> bool {
        // Check if requirements exceed machine capacity — warn and
        // allow anyway.
        let exceeds_capacity = resources.cpu > self.total_cpu
            || resources.gpu > self.total_gpu
            || (self.total_memory_bytes > 0 && resources.memory_bytes > self.total_memory_bytes);

        if exceeds_capacity {
            warn!(
                cpu_requested = resources.cpu,
                gpu_requested = resources.gpu,
                memory_requested = resources.memory_bytes,
                total_cpu = self.total_cpu,
                total_gpu = self.total_gpu,
                total_memory = self.total_memory_bytes,
                "stage resource requirements exceed machine capacity; running anyway"
            );
            return true;
        }

        let cpu_available = self.total_cpu.saturating_sub(self.used_cpu);
        let gpu_available = self.total_gpu.saturating_sub(self.used_gpu);
        let memory_available = self
            .total_memory_bytes
            .saturating_sub(self.used_memory_bytes);

        let cpu_ok = resources.cpu <= cpu_available;
        let gpu_ok = resources.gpu == 0 || resources.gpu <= gpu_available;
        let memory_ok = resources.memory_bytes == 0
            || self.total_memory_bytes == 0
            || resources.memory_bytes <= memory_available;

        cpu_ok && gpu_ok && memory_ok
    }

    /// Reserve resources for a stage that is about to start.
    pub fn acquire(&mut self, resources: &Resources) {
        self.used_cpu = self.used_cpu.saturating_add(resources.cpu);
        self.used_gpu = self.used_gpu.saturating_add(resources.gpu);
        self.used_memory_bytes = self
            .used_memory_bytes
            .saturating_add(resources.memory_bytes);
    }

    /// Release resources when a stage completes (success or failure).
    pub fn release(&mut self, resources: &Resources) {
        self.used_cpu = self.used_cpu.saturating_sub(resources.cpu);
        self.used_gpu = self.used_gpu.saturating_sub(resources.gpu);
        self.used_memory_bytes = self
            .used_memory_bytes
            .saturating_sub(resources.memory_bytes);
    }
}
