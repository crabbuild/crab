use super::*;

const HOST: Memory = Memory {
    total: 16_000,
    available: 12_000,
};

#[cfg(unix)]
mod cgroups {
    use super::*;
    use std::{fs, path::PathBuf};

    struct Cgroups {
        _directory: tempfile::TempDir,
        proc_dir: PathBuf,
        mount: PathBuf,
    }

    impl Cgroups {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let proc_dir = directory.path().join("proc");
            let mount = directory.path().join("hierarchy with space");
            fs::create_dir(&proc_dir).unwrap();
            fs::create_dir_all(mount.join("parent/leaf")).unwrap();
            fs::write(proc_dir.join("cgroup"), "0::/parent/leaf\n").unwrap();
            let encoded = mount.to_str().unwrap().replace(' ', "\\040");
            fs::write(
                proc_dir.join("mountinfo"),
                format!("29 23 0:26 / {encoded} rw,nosuid - cgroup2 cgroup rw\n"),
            )
            .unwrap();
            for path in [&mount, &mount.join("parent"), &mount.join("parent/leaf")] {
                fs::write(path.join("cgroup.controllers"), "memory cpu\n").unwrap();
                fs::write(path.join("cgroup.subtree_control"), "memory\n").unwrap();
            }
            let fixture = Self {
                _directory: directory,
                proc_dir,
                mount,
            };
            fixture.limit("parent", "8000", "1000");
            fixture.limit("parent/leaf", "max", "500");
            fixture
        }

        fn limit(&self, path: &str, limit: &str, usage: &str) {
            fs::write(self.mount.join(path).join("memory.max"), limit).unwrap();
            fs::write(self.mount.join(path).join("memory.current"), usage).unwrap();
        }

        fn sample(&self) -> io::Result<Memory> {
            linux::memory_at(&self.proc_dir, HOST)
        }
    }

    #[test]
    fn cgroup_headroom_includes_ancestor_and_host_pressure() {
        let fixture = Cgroups::new();
        for (parent, parent_used, leaf, leaf_used, expected) in [
            (
                "8000",
                "1000",
                "max",
                "500",
                Memory {
                    total: 8000,
                    available: 7000,
                },
            ),
            (
                "8000",
                "7500",
                "4000",
                "500",
                Memory {
                    total: 4000,
                    available: 500,
                },
            ),
            (
                "8000",
                "8001",
                "max",
                "500",
                Memory {
                    total: 8000,
                    available: 0,
                },
            ),
            ("max", "1000", "max", "500", HOST),
            ("20000", "1000", "max", "500", HOST),
        ] {
            fixture.limit("parent", parent, parent_used);
            fixture.limit("parent/leaf", leaf, leaf_used);
            assert_eq!(fixture.sample().unwrap(), expected);
        }
    }

    #[test]
    fn disabled_child_controller_preserves_parent_limit() {
        let fixture = Cgroups::new();
        fs::write(fixture.mount.join("parent/cgroup.subtree_control"), "cpu").unwrap();
        fs::remove_file(fixture.mount.join("parent/leaf/memory.max")).unwrap();
        assert_eq!(
            fixture.sample().unwrap(),
            Memory {
                total: 8000,
                available: 7000
            }
        );
    }

    #[test]
    fn missing_or_invalid_cgroup_measurement_is_not_host_headroom() {
        for (path, replacement) in [
            ("parent/leaf/memory.max", None),
            ("parent/memory.current", None),
            ("parent/memory.max", Some("invalid")),
            ("parent/memory.current", Some("-1")),
            ("cgroup.controllers", Some("cpu")),
            ("memory.max", Some("max")),
        ] {
            let fixture = Cgroups::new();
            let target = fixture.mount.join(path);
            match replacement {
                Some(value) => fs::write(target, value).unwrap(),
                None => fs::remove_file(target).unwrap(),
            }
            assert!(fixture.sample().is_err(), "{path}");
        }
    }

    #[test]
    fn partial_mount_and_unsafe_membership_are_ineligible() {
        let fixture = Cgroups::new();
        for membership in [
            "0::/../escape",
            "0::relative",
            "0::/parent/./leaf",
            "0::/a\n0::/b",
            "4:memory:/parent/leaf",
        ] {
            fs::write(fixture.proc_dir.join("cgroup"), membership).unwrap();
            assert!(fixture.sample().is_err(), "{membership}");
        }
        fs::write(fixture.proc_dir.join("cgroup"), "0::/parent/leaf").unwrap();
        let mountinfo = fixture.proc_dir.join("mountinfo");
        let content = fs::read_to_string(&mountinfo).unwrap();
        fs::write(mountinfo, content.replace("0:26 / ", "0:26 /parent ")).unwrap();
        assert!(fixture.sample().is_err());
    }

    #[test]
    fn root_membership_uses_host_memory_only_for_the_actual_root() {
        let fixture = Cgroups::new();
        fs::write(fixture.proc_dir.join("cgroup"), "0::/").unwrap();
        assert_eq!(fixture.sample().unwrap(), HOST);
        fixture.limit("", "9000", "1000");
        assert!(fixture.sample().is_err());
    }
}

#[tokio::test]
async fn capacity_tracks_disk_reservations_and_physical_shortfalls() {
    use crab_cell_runtime::{
        CellRuntime, SqlWorkerPool,
        ltx::{DiskBudget, Host},
    };

    let disk = DiskBudget::new(1000);
    let runtime = CellRuntime::new_with_replica_host(
        SqlWorkerPool::new(1, 4).unwrap(),
        4 * 1024 * 1024,
        crab_cell_runtime::SessionId::from_bytes([1; 16]),
        Host::default().with_local_disk_budget(disk.clone()),
    )
    .unwrap();
    let reserved = disk.try_reserve(700).unwrap();
    let memory = runtime.try_reserve_node_bytes(5000).unwrap();
    let job = runtime.try_reserve_worker_job().unwrap().unwrap();
    for (free_disk, expected) in [(9000, 300), (900, 200), (200, 0), (0, 0)] {
        let (capacity, placement) = constrain(
            Resources {
                memory: HOST,
                disk_total: 10_000,
                disk_available: free_disk,
            },
            runtime.stats(),
        )
        .unwrap();
        assert_eq!(capacity.free_disk_bytes, expected);
        assert_eq!(placement.disk_capacity_bytes, 1000);
        assert_eq!(capacity.free_memory_bytes, 7_000);
        assert_eq!(placement.publication_backlog, 1);
        assert_eq!(placement.running_jobs, 1);
        assert_eq!(
            capacity.job_credits,
            placement.job_capacity - placement.running_jobs
        );
    }
    drop(reserved);
    drop(memory);
    drop(job);
    let (capacity, _) = constrain(
        Resources {
            memory: HOST,
            disk_total: 10_000,
            disk_available: 9000,
        },
        runtime.stats(),
    )
    .unwrap();
    assert_eq!(capacity.free_disk_bytes, 1000);
    runtime.shutdown().await.unwrap();
}
