use std::path::Path;
#[cfg(any(target_os = "linux", test))]
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub(crate) struct LocalResources {
    pub(crate) memory_bytes: u64,
    pub(crate) disk_limit_bytes: u64,
    pub(crate) disk_capacity_bytes: u64,
    pub(crate) free_disk_bytes: u64,
    pub(crate) available_file_descriptors: usize,
    pub(crate) job_credits: usize,
}

pub(crate) fn local_resources(
    data_dir: &Path,
    local_disk_limit_bytes: u64,
) -> crate::Result<LocalResources> {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let pid = sysinfo::get_current_pid()
        .map_err(|_| crate::Error::Config("cannot determine the server process ID"))?;
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), false);
    let open_files = system
        .process(pid)
        .and_then(sysinfo::Process::open_files)
        .ok_or(crate::Error::Config(
            "cannot determine the server's open file count",
        ))?;
    let file_limit = sysinfo::System::open_files_limit().ok_or(crate::Error::Config(
        "cannot determine the server's open file limit",
    ))?;
    // Startup budgets use the stable process limit. Reusing advertised free
    // memory would make transient boot load permanently shrink Cell admission.
    let disk = fs4::statvfs(data_dir)?;
    let disk_capacity_bytes = disk.total_space().min(local_disk_limit_bytes);
    Ok(LocalResources {
        memory_bytes: effective_memory_limit(system.total_memory()),
        disk_limit_bytes: local_disk_limit_bytes,
        disk_capacity_bytes,
        free_disk_bytes: disk.available_space().min(disk_capacity_bytes),
        available_file_descriptors: file_limit.saturating_sub(open_files),
        job_credits: std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .clamp(1, 16),
    })
}

fn effective_memory_limit(system_total: u64) -> u64 {
    #[cfg(target_os = "linux")]
    {
        let cgroup_limit = cgroup_memory_limit_from_process();
        if let Some(cgroup_limit) = cgroup_limit {
            return system_total.min(cgroup_limit);
        }
    }
    system_total
}

#[cfg(target_os = "linux")]
fn cgroup_memory_limit_from_process() -> Option<u64> {
    for (limit_path, _) in cgroup_memory_paths() {
        match std::fs::read_to_string(limit_path) {
            Ok(value) => return parse_cgroup_limit(&value),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn cgroup_memory_paths() -> Vec<(PathBuf, PathBuf)> {
    let membership = std::fs::read_to_string("/proc/self/cgroup").ok();
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").ok();
    let mut paths = Vec::new();
    if let (Some(membership), Some(mounts)) = (membership.as_deref(), mounts.as_deref()) {
        for (v2, limit_name, usage_name) in [
            (true, "memory.max", "memory.current"),
            (false, "memory.limit_in_bytes", "memory.usage_in_bytes"),
        ] {
            let Some(relative) = parse_cgroup_membership(membership, v2) else {
                continue;
            };
            for mount in parse_cgroup_mountpoints(mounts, v2) {
                let Some(base) = join_cgroup_path(&mount, &relative) else {
                    continue;
                };
                let candidate = (base.join(limit_name), base.join(usage_name));
                if !paths.contains(&candidate) {
                    paths.push(candidate);
                }
            }
        }
    }
    for candidate in [
        (
            PathBuf::from("/sys/fs/cgroup/memory.max"),
            PathBuf::from("/sys/fs/cgroup/memory.current"),
        ),
        (
            PathBuf::from("/sys/fs/cgroup/memory/memory.limit_in_bytes"),
            PathBuf::from("/sys/fs/cgroup/memory/memory.usage_in_bytes"),
        ),
    ] {
        if !paths.contains(&candidate) {
            paths.push(candidate);
        }
    }
    paths
}

#[cfg(any(target_os = "linux", test))]
fn parse_cgroup_membership(contents: &str, v2: bool) -> Option<String> {
    contents.lines().find_map(|line| {
        let mut fields = line.splitn(3, ':');
        let hierarchy = fields.next()?;
        let controllers = fields.next()?;
        let relative = fields.next()?.trim();
        let matches = if v2 {
            hierarchy == "0" && controllers.is_empty()
        } else {
            controllers
                .split(',')
                .any(|controller| controller == "memory")
        };
        matches.then(|| relative.to_owned())
    })
}

#[cfg(any(target_os = "linux", test))]
fn parse_cgroup_mountpoints(contents: &str, v2: bool) -> Vec<PathBuf> {
    contents
        .lines()
        .filter_map(|line| {
            let (mount_info, filesystem_info) = line.split_once(" - ")?;
            let mount_fields = mount_info.split_whitespace().collect::<Vec<_>>();
            let filesystem_fields = filesystem_info.split_whitespace().collect::<Vec<_>>();
            let filesystem = filesystem_fields.first().copied()?;
            let mount_options = filesystem_fields.get(2).copied().unwrap_or_default();
            let super_options = filesystem_fields.get(3).copied().unwrap_or_default();
            let is_memory_mount = v2
                .then_some(filesystem == "cgroup2")
                .or_else(|| {
                    (!v2 && filesystem == "cgroup").then(|| {
                        mount_options
                            .split(',')
                            .chain(super_options.split(','))
                            .any(|option| option == "memory")
                    })
                })
                .unwrap_or(false);
            if !is_memory_mount {
                return None;
            }
            decode_mountinfo_path(mount_fields.get(4).copied()?)
        })
        .collect()
}

#[cfg(any(target_os = "linux", test))]
fn decode_mountinfo_path(encoded: &str) -> Option<PathBuf> {
    let mut decoded = String::with_capacity(encoded.len());
    let mut chars = encoded.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        let escape = chars.next()?;
        decoded.push(match escape {
            '0' => match (chars.next()?, chars.next()?) {
                ('4', '0') => ' ',
                ('1', '1') => '\t',
                _ => return None,
            },
            '1' => {
                if chars.next()? != '3' || chars.next()? != '4' {
                    return None;
                }
                '\\'
            }
            _ => return None,
        });
    }
    Some(PathBuf::from(decoded))
}

#[cfg(any(target_os = "linux", test))]
fn join_cgroup_path(mountpoint: &Path, relative: &str) -> Option<PathBuf> {
    if relative
        .split('/')
        .any(|segment| segment == ".." || segment.contains('\0'))
    {
        return None;
    }
    Some(if relative == "/" || relative.is_empty() {
        mountpoint.to_owned()
    } else {
        mountpoint.join(relative.trim_start_matches('/'))
    })
}

#[cfg(any(target_os = "linux", test))]
fn parse_cgroup_limit(value: &str) -> Option<u64> {
    let value = value.trim();
    (!value.is_empty() && value != "max")
        .then(|| value.parse::<u64>().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cgroup_probe_parsing_is_fail_closed_and_monotonic() {
        assert_eq!(parse_cgroup_limit(" 4096\n"), Some(4096));
        assert_eq!(parse_cgroup_limit("max"), None);
        assert_eq!(parse_cgroup_limit(""), None);
        assert_eq!(parse_cgroup_limit("not-a-number"), None);
        assert_eq!(parse_cgroup_limit("0"), Some(0));
    }

    #[test]
    fn cgroup_fixture_resolution_follows_nested_membership() {
        let membership = "11:memory:/kubepods.slice/pod.slice\n0::/user.slice/crab";
        assert_eq!(
            parse_cgroup_membership(membership, true),
            Some("/user.slice/crab".to_owned())
        );
        assert_eq!(
            parse_cgroup_membership(membership, false),
            Some("/kubepods.slice/pod.slice".to_owned())
        );

        let mounts = concat!(
            "29 23 0:26 / /sys/fs/cgroup rw,nosuid - cgroup2 cgroup rw\n",
            "30 23 0:27 / /sys/fs/cgroup/memory rw - cgroup cgroup rw,memory\n",
            "31 23 0:28 / /sys/fs/cgroup/cpu rw - cgroup cgroup rw,cpu\n",
        );
        assert_eq!(
            parse_cgroup_mountpoints(mounts, true),
            vec![PathBuf::from("/sys/fs/cgroup")]
        );
        assert_eq!(
            parse_cgroup_mountpoints(mounts, false),
            vec![PathBuf::from("/sys/fs/cgroup/memory")]
        );
        assert_eq!(
            join_cgroup_path(Path::new("/sys/fs/cgroup"), "/user.slice/crab"),
            Some(PathBuf::from("/sys/fs/cgroup/user.slice/crab"))
        );
        assert!(join_cgroup_path(Path::new("/sys/fs/cgroup"), "/../host").is_none());
        assert_eq!(
            decode_mountinfo_path("/sys/with\\040space"),
            Some(PathBuf::from("/sys/with space"))
        );
    }
}
