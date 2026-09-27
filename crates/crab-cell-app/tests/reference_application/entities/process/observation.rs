//! Cumulative provider operations and per-node Linux resource samples.

use super::*;
use crab_storage::{StorageObservation, StorageObserver, StorageOperation, StorageOutcome};
use std::{
    fs::File,
    io::{BufWriter, Write},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Default)]
pub(super) struct StorageCounters {
    started: AtomicU64,
    finished: AtomicU64,
    outcomes: [[AtomicU64; 9]; 11],
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
}

impl StorageObserver for StorageCounters {
    fn started(&self, _: StorageOperation) {
        self.started.fetch_add(1, Ordering::Relaxed);
    }

    fn finished(&self, observation: StorageObservation) {
        self.outcomes[observation.operation.index()][observation.outcome.index()]
            .fetch_add(1, Ordering::Relaxed);
        self.bytes_read
            .fetch_add(observation.bytes_read, Ordering::Relaxed);
        self.bytes_written
            .fetch_add(observation.bytes_written, Ordering::Relaxed);
        self.finished.fetch_add(1, Ordering::Relaxed);
    }
}

pub(super) struct NodeObservations {
    resources: BufWriter<File>,
    objects: BufWriter<File>,
    durability: BufWriter<File>,
}

impl NodeObservations {
    pub(super) fn new(sync: &Path, node: usize) -> Self {
        let create = |name| {
            BufWriter::new(File::create(sync.join(format!("node-{node}-{name}.tsv"))).unwrap())
        };
        let mut resources = create("resources");
        writeln!(resources, "at_ms\tboot_ms\tstage\tactive_cells\tworker_jobs\tprimitive_jobs\thydration_jobs\tretained_bytes\tdisk_reserved_bytes\tdisk_bytes\tcpu_usage_us\tthrottled_us\tmemory_current_bytes\tmemory_peak_bytes\tgateway_local\tgateway_forwarded\tobject_started\tobject_finished\tbytes_read\tbytes_written").unwrap();
        Self {
            resources,
            objects: create("objects"),
            durability: create("durability"),
        }
    }

    pub(super) fn sample(
        &mut self,
        stage: usize,
        host: &crab_cell_host::CellNode,
        root: &Path,
        storage: &StorageCounters,
        gateway: &GatewayStats,
    ) {
        let cpu = std::fs::read_to_string("/sys/fs/cgroup/cpu.stat").unwrap();
        let cpu_value = |name| {
            cpu.lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(' ')?;
                    (key == name).then(|| value.parse::<u64>().unwrap())
                })
                .unwrap()
        };
        let memory = |name| {
            std::fs::read_to_string(format!("/sys/fs/cgroup/{name}"))
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap()
        };
        let stats = host.stats();
        let (local, forwarded) = gateway.counts();
        let values = [
            now_ms() as u64,
            boot_ms(),
            stage as u64,
            stats.active_cells() as u64,
            stats.worker_jobs() as u64,
            stats.primitive_jobs() as u64,
            stats.hydration_jobs() as u64,
            stats.retained_bytes() as u64,
            stats.local_disk_reserved_bytes(),
            disk_bytes(root),
            cpu_value("usage_usec"),
            cpu_value("throttled_usec"),
            memory("memory.current"),
            memory("memory.peak"),
            local as u64,
            forwarded as u64,
            storage.started.load(Ordering::Relaxed),
            storage.finished.load(Ordering::Relaxed),
            storage.bytes_read.load(Ordering::Relaxed),
            storage.bytes_written.load(Ordering::Relaxed),
        ];
        writeln!(
            self.resources,
            "{}",
            values.map(|value| value.to_string()).join("\t")
        )
        .unwrap();
        self.resources.flush().unwrap();
    }

    pub(super) fn finish(&mut self, storage: &StorageCounters, waits: &[Duration]) {
        writeln!(self.objects, "operation\toutcome\tcount").unwrap();
        for operation in StorageOperation::ALL {
            for outcome in StorageOutcome::ALL {
                let count =
                    storage.outcomes[operation.index()][outcome.index()].load(Ordering::Relaxed);
                writeln!(
                    self.objects,
                    "{}\t{}\t{count}",
                    operation.label(),
                    outcome.label()
                )
                .unwrap();
            }
        }
        writeln!(self.durability, "object_wait_us").unwrap();
        assert!(!waits.is_empty());
        for wait in waits {
            writeln!(self.durability, "{}", wait.as_micros()).unwrap();
        }
        self.objects.flush().unwrap();
        self.durability.flush().unwrap();
    }
}

fn disk_bytes(root: &Path) -> u64 {
    std::fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            // Publication removes temporary files while this sampler walks.
            // A disappeared file contributes no retained bytes to this sample.
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
                Err(error) => panic!("cannot sample node disk: {error}"),
            };
            if metadata.is_dir() {
                disk_bytes(&entry.path())
            } else {
                metadata.len()
            }
        })
        .sum()
}
