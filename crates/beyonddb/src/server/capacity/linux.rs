//! Cgroup-v2 headroom from the process membership and mounted hierarchy.

use std::{
    fs::File,
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

use super::Memory;

#[cfg(target_os = "linux")]
pub(super) fn memory(host: Memory) -> io::Result<Memory> {
    memory_at(Path::new("/proc/self"), host)
}

pub(super) fn memory_at(proc_dir: &Path, host: Memory) -> io::Result<Memory> {
    let membership = read(&proc_dir.join("cgroup"))?;
    let mounts = read(&proc_dir.join("mountinfo"))?;
    let relative = membership_path(&membership)?;
    let mount = root_mount(&mounts)?;
    // A namespace or subtree mount can hide a tighter ancestor. A real v2
    // root exposes the memory controller but has no memory.max of its own.
    if !read(&mount.join("cgroup.controllers"))?
        .split_whitespace()
        .any(|controller| controller == "memory")
    {
        return Err(invalid("cgroup memory controller is unavailable"));
    }
    match File::open(mount.join("memory.max")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
        Ok(_) => return Err(invalid("cgroup ancestors are hidden by a namespace")),
    }
    let mut current = mount.join(
        relative
            .strip_prefix("/")
            .map_err(|_| invalid("cgroup path"))?,
    );
    let mut result = host;
    while current != mount {
        // Even a child without an enabled memory controller must exist. Missing
        // files must not be mistaken for an unconstrained or removed cgroup.
        read(&current.join("cgroup.controllers"))?;
        let parent = current.parent().ok_or_else(|| invalid("cgroup parent"))?;
        let enabled = read(&parent.join("cgroup.subtree_control"))?
            .split_whitespace()
            .any(|controller| controller == "memory");
        match read(&current.join("memory.max")) {
            Ok(limit) => {
                let used = number(&read(&current.join("memory.current"))?)?;
                if limit.trim() != "max" {
                    let limit = number(&limit)?;
                    result.total = result.total.min(limit);
                    result.available = result.available.min(limit.saturating_sub(used));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && !enabled => {}
            Err(error) => return Err(error),
        }
        current = parent.to_owned();
    }
    if membership != read(&proc_dir.join("cgroup"))? {
        return Err(invalid("cgroup membership changed during measurement"));
    }
    result.available = result.available.min(result.total);
    Ok(result)
}

fn membership_path(contents: &str) -> io::Result<PathBuf> {
    let mut selected = None;
    for line in contents.lines() {
        let mut fields = line.splitn(3, ':');
        let id = fields.next().ok_or_else(|| invalid("cgroup membership"))?;
        let controllers = fields.next().ok_or_else(|| invalid("cgroup membership"))?;
        let path = fields.next().ok_or_else(|| invalid("cgroup membership"))?;
        if controllers.split(',').any(|name| name == "memory") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "placement memory measurement requires cgroup v2",
            ));
        }
        if id == "0" && controllers.is_empty() {
            if selected.is_some() {
                return Err(invalid("duplicate unified cgroup membership"));
            }
            selected = Some(absolute(path)?);
        }
    }
    selected.ok_or_else(|| invalid("unified cgroup membership is unavailable"))
}

fn root_mount(contents: &str) -> io::Result<PathBuf> {
    for line in contents.lines() {
        let (mount, filesystem) = line
            .split_once(" - ")
            .ok_or_else(|| invalid("mountinfo separator"))?;
        if filesystem.split_whitespace().next() != Some("cgroup2") {
            continue;
        }
        let mut fields = mount.split_whitespace().skip(3);
        let root = decode(fields.next().ok_or_else(|| invalid("mountinfo root"))?)?;
        let point = decode(
            fields
                .next()
                .ok_or_else(|| invalid("mountinfo mountpoint"))?,
        )?;
        if root == Path::new("/") {
            return Ok(point);
        }
    }
    Err(invalid("full cgroup hierarchy is not mounted"))
}

fn decode(encoded: &str) -> io::Result<PathBuf> {
    let mut output = String::with_capacity(encoded.len());
    let mut chars = encoded.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        output.push(match (chars.next(), chars.next(), chars.next()) {
            (Some('0'), Some('4'), Some('0')) => ' ',
            (Some('0'), Some('1'), Some('1')) => '\t',
            (Some('0'), Some('1'), Some('2')) => '\n',
            (Some('1'), Some('3'), Some('4')) => '\\',
            _ => return Err(invalid("mountinfo path escape")),
        });
    }
    absolute(&output)
}

fn absolute(value: &str) -> io::Result<PathBuf> {
    let path = Path::new(value);
    if !path.is_absolute()
        || value.contains('\0')
        || value.split('/').any(|part| matches!(part, "." | ".."))
        || path
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(invalid("cgroup path must be absolute without traversal"));
    }
    Ok(path.to_owned())
}

fn number(value: &str) -> io::Result<u64> {
    value
        .trim()
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn read(path: &Path) -> io::Result<String> {
    const MAX_BYTES: u64 = 1024 * 1024;
    let mut contents = String::new();
    File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_string(&mut contents)?;
    if contents.len() as u64 > MAX_BYTES {
        return Err(invalid("cgroup metadata exceeds probe bound"));
    }
    Ok(contents)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
