//! System health collection for remote monitoring.
//!
//! Runs platform-specific commands (`uname`, `df`, `free`, `uptime`,
//! `/proc/loadavg`, `nvidia-smi`) and parses their output into a
//! structured `SystemHealth` JSON response. Designed to be robust:
//! if any individual command fails, the response includes whatever
//! data is available with zeros/defaults for the rest.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;
use tracing::debug;

use crate::Result;

// ── Types ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SystemHealthParams {
    #[serde(default)]
    pub include_gpu: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskInfo {
    pub path: String,
    pub total: u64,
    pub free: u64,
    #[serde(rename = "usedPercent")]
    pub used_percent: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryInfo {
    pub total: u64,
    pub free: u64,
    pub cached: u64,
    #[serde(rename = "usedPercent")]
    pub used_percent: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GpuProcess {
    pub pid: u32,
    pub name: String,
    #[serde(rename = "memUsed")]
    pub mem_used: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GpuInfo {
    pub index: u32,
    pub name: String,
    pub utilization: u32,
    #[serde(rename = "memoryUsed")]
    pub memory_used: u64,
    #[serde(rename = "memoryTotal")]
    pub memory_total: u64,
    pub temperature: u32,
    pub processes: Vec<GpuProcess>,
}

// ── Public entry point ───────────────────────────────────────────

/// Collect system health metrics. Runs multiple commands in parallel
/// and assembles whatever data is available into the response.
pub async fn system_health(p: SystemHealthParams) -> Result<Value> {
    // Run all data collection concurrently.
    let (uname_result, disk_result, memory_result, uptime_result, load_result, gpu_result) = tokio::join!(
        collect_uname(),
        collect_disk(),
        collect_memory(),
        collect_uptime(),
        collect_load_avg(),
        async {
            if p.include_gpu {
                collect_gpu().await
            } else {
                None
            }
        },
    );

    let (os, kernel, arch, hostname) = uname_result.unwrap_or_default();
    let disk = disk_result.unwrap_or_default();
    let memory = memory_result.unwrap_or_else(|| MemoryInfo {
        total: 0,
        free: 0,
        cached: 0,
        used_percent: 0,
    });
    let uptime = uptime_result.unwrap_or(0);
    let load_avg = load_result.unwrap_or([0.0, 0.0, 0.0]);

    Ok(json!({
        "os": os,
        "kernel": kernel,
        "arch": arch,
        "hostname": hostname,
        "uptime": uptime,
        "loadAvg": load_avg,
        "disk": disk,
        "memory": memory,
        "gpus": gpu_result,
    }))
}

// ── Data collectors ──────────────────────────────────────────────

/// Parse `uname -a` output into (os, kernel, arch, hostname).
/// Falls back to individual `uname` calls if `-a` parsing is ambiguous.
async fn collect_uname() -> Option<(String, String, String, String)> {
    // Get OS name (Linux, Darwin, etc.)
    let os = run_cmd("uname", &["-s"]).await.unwrap_or_default();
    let kernel = run_cmd("uname", &["-r"]).await.unwrap_or_default();
    let arch = run_cmd("uname", &["-m"]).await.unwrap_or_default();
    let hostname = run_cmd("hostname", &[])
        .await
        .or_else(|| {
            // hostname command might not exist; try uname -n
            None
        })
        .unwrap_or_default();

    // If hostname failed, try uname -n synchronously (already ran above)
    let hostname = if hostname.is_empty() {
        run_cmd("uname", &["-n"]).await.unwrap_or_default()
    } else {
        hostname
    };

    Some((os, kernel, arch, hostname))
}

/// Collect disk usage. Uses `df -B1` (bytes) on Linux, `df -k` on macOS.
async fn collect_disk() -> Option<Vec<DiskInfo>> {
    // Try df -B1 first (Linux), fall back to df -k (macOS/BSD)
    let output = run_cmd("df", &["-B1", "--output=target,size,avail"])
        .await
        .or_else(|| None);

    if let Some(ref text) = output {
        if let Some(disks) = parse_df_linux(text) {
            return Some(disks);
        }
    }

    // macOS / BSD fallback: df -k (1K blocks)
    let output = run_cmd("df", &["-k"]).await?;
    parse_df_bsd(&output)
}

/// Parse Linux `df -B1 --output=target,size,avail` output.
fn parse_df_linux(output: &str) -> Option<Vec<DiskInfo>> {
    let mut disks = Vec::new();
    for line in output.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let path = parts[0].to_owned();
        let total: u64 = parts[1].parse().unwrap_or(0);
        let free: u64 = parts[2].parse().unwrap_or(0);

        // Skip pseudo-filesystems
        if total == 0
            || path.starts_with("/dev/shm")
            || path.starts_with("/run")
            || path == "/sys"
            || path == "/proc"
        {
            continue;
        }

        let used_percent = if total > 0 {
            ((total - free) * 100) / total
        } else {
            0
        };

        disks.push(DiskInfo {
            path,
            total,
            free,
            used_percent,
        });
    }

    if disks.is_empty() { None } else { Some(disks) }
}

/// Parse BSD/macOS `df -k` output (1K-blocks).
fn parse_df_bsd(output: &str) -> Option<Vec<DiskInfo>> {
    let mut disks = Vec::new();
    for line in output.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        // Typical columns: Filesystem 1K-blocks Used Available Capacity Mounted
        if parts.len() < 6 {
            continue;
        }

        let filesystem = parts[0];
        // Skip non-physical filesystems
        if !filesystem.starts_with("/dev") && !filesystem.starts_with("map ") {
            continue;
        }

        let total_kb: u64 = parts[1].parse().unwrap_or(0);
        let avail_kb: u64 = parts[3].parse().unwrap_or(0);
        let path = parts[parts.len() - 1].to_owned();

        if total_kb == 0 {
            continue;
        }

        let total = total_kb * 1024;
        let free = avail_kb * 1024;
        let used_percent = if total > 0 {
            ((total - free) * 100) / total
        } else {
            0
        };

        disks.push(DiskInfo {
            path,
            total,
            free,
            used_percent,
        });
    }

    if disks.is_empty() { None } else { Some(disks) }
}

/// Collect memory info. Reads `/proc/meminfo` on Linux, uses
/// `sysctl` + `vm_stat` on macOS.
async fn collect_memory() -> Option<MemoryInfo> {
    // Try /proc/meminfo first (Linux)
    if let Ok(content) = tokio::fs::read_to_string("/proc/meminfo").await {
        return parse_proc_meminfo(&content);
    }

    // macOS fallback: sysctl + vm_stat
    collect_memory_macos().await
}

/// Parse `/proc/meminfo` into MemoryInfo.
fn parse_proc_meminfo(content: &str) -> Option<MemoryInfo> {
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    let mut buffers: u64 = 0;
    let mut cached: u64 = 0;
    let mut sreclaimable: u64 = 0;

    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let value_kb: u64 = parts[1].parse().unwrap_or(0);
        match parts[0] {
            "MemTotal:" => total = value_kb * 1024,
            "MemFree:" => free = value_kb * 1024,
            "Buffers:" => buffers = value_kb * 1024,
            "Cached:" => cached = value_kb * 1024,
            "SReclaimable:" => sreclaimable = value_kb * 1024,
            _ => {}
        }
    }

    let total_cached = cached + buffers + sreclaimable;
    let used_percent = if total > 0 {
        ((total - free - total_cached) * 100) / total
    } else {
        0
    };

    Some(MemoryInfo {
        total,
        free,
        cached: total_cached,
        used_percent,
    })
}

/// macOS memory collection via `sysctl` and `vm_stat`.
async fn collect_memory_macos() -> Option<MemoryInfo> {
    // Get total physical memory
    let total_str = run_cmd("sysctl", &["-n", "hw.memsize"]).await?;
    let total: u64 = total_str.trim().parse().ok()?;

    // Get page size and vm_stat
    let page_size_str = run_cmd("sysctl", &["-n", "hw.pagesize"]).await?;
    let page_size: u64 = page_size_str.trim().parse().unwrap_or(4096);

    let vm_stat_output = run_cmd("vm_stat", &[]).await?;

    let mut free_pages: u64 = 0;
    let mut inactive_pages: u64 = 0;
    let mut purgeable_pages: u64 = 0;

    for line in vm_stat_output.lines() {
        if line.contains("Pages free:") {
            free_pages = extract_vm_stat_value(line);
        } else if line.contains("Pages inactive:") {
            inactive_pages = extract_vm_stat_value(line);
        } else if line.contains("Pages purgeable:") {
            purgeable_pages = extract_vm_stat_value(line);
        }
    }

    let free = (free_pages + inactive_pages) * page_size;
    let cached = purgeable_pages * page_size;
    let used_percent = if total > 0 {
        ((total - free - cached) * 100) / total
    } else {
        0
    };

    Some(MemoryInfo {
        total,
        free,
        cached,
        used_percent,
    })
}

/// Extract the numeric value from a vm_stat line like "Pages free:    123456."
fn extract_vm_stat_value(line: &str) -> u64 {
    line.split(':')
        .nth(1)
        .unwrap_or("")
        .trim()
        .trim_end_matches('.')
        .parse()
        .unwrap_or(0)
}

/// Collect system uptime in seconds.
async fn collect_uptime() -> Option<u64> {
    // Try /proc/uptime first (Linux)
    if let Ok(content) = tokio::fs::read_to_string("/proc/uptime").await {
        // Format: "12345.67 98765.43" (uptime idle_time)
        let uptime_secs: f64 = content.split_whitespace().next()?.parse().ok()?;
        return Some(uptime_secs as u64);
    }

    // macOS fallback: sysctl kern.boottime
    let output = run_cmd("sysctl", &["-n", "kern.boottime"]).await?;
    // Output format: "{ sec = 1234567890, usec = 123456 } ..."
    let sec_str = output.split("sec = ").nth(1)?.split(',').next()?.trim();
    let boot_time: u64 = sec_str.parse().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(now.saturating_sub(boot_time))
}

/// Collect load averages from `/proc/loadavg` or `sysctl`.
async fn collect_load_avg() -> Option<[f64; 3]> {
    // Try /proc/loadavg first (Linux)
    if let Ok(content) = tokio::fs::read_to_string("/proc/loadavg").await {
        let parts: Vec<&str> = content.split_whitespace().collect();
        if parts.len() >= 3 {
            let l1: f64 = parts[0].parse().unwrap_or(0.0);
            let l5: f64 = parts[1].parse().unwrap_or(0.0);
            let l15: f64 = parts[2].parse().unwrap_or(0.0);
            return Some([l1, l5, l15]);
        }
    }

    // macOS fallback: sysctl vm.loadavg
    let output = run_cmd("sysctl", &["-n", "vm.loadavg"]).await?;
    // Output: "{ 1.23 0.45 0.67 }"
    let trimmed = output.trim().trim_start_matches('{').trim_end_matches('}');
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() >= 3 {
        let l1: f64 = parts[0].parse().unwrap_or(0.0);
        let l5: f64 = parts[1].parse().unwrap_or(0.0);
        let l15: f64 = parts[2].parse().unwrap_or(0.0);
        return Some([l1, l5, l15]);
    }

    None
}

/// Collect GPU info via `nvidia-smi`. Returns `None` if nvidia-smi
/// is not available or fails.
async fn collect_gpu() -> Option<Vec<GpuInfo>> {
    // Check if nvidia-smi exists
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=index,name,utilization.gpu,memory.used,memory.total,temperature.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut gpus = parse_nvidia_smi_gpu_csv(&stdout);

    // Attach per-GPU processes
    for gpu in &mut gpus {
        gpu.processes = collect_gpu_processes(gpu.index).await.unwrap_or_default();
    }

    if gpus.is_empty() { None } else { Some(gpus) }
}

/// Parse nvidia-smi CSV output (query-gpu format) into GpuInfo structs.
///
/// Expected input format (one line per GPU, comma-separated):
///   index, name, utilization.gpu [%], memory.used [MiB], memory.total [MiB], temperature.gpu
///
/// Lines with fewer than 6 fields are skipped. Memory values are
/// converted from MiB to bytes.
fn parse_nvidia_smi_gpu_csv(output: &str) -> Vec<GpuInfo> {
    let mut gpus = Vec::new();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if parts.len() < 6 {
            continue;
        }

        let index: u32 = parts[0].parse().unwrap_or(0);
        let name = parts[1].to_owned();
        let utilization: u32 = parts[2].parse().unwrap_or(0);
        // nvidia-smi reports memory in MiB
        let memory_used_mib: u64 = parts[3].parse().unwrap_or(0);
        let memory_total_mib: u64 = parts[4].parse().unwrap_or(0);
        let temperature: u32 = parts[5].parse().unwrap_or(0);

        gpus.push(GpuInfo {
            index,
            name,
            utilization,
            memory_used: memory_used_mib * 1024 * 1024,
            memory_total: memory_total_mib * 1024 * 1024,
            temperature,
            processes: Vec::new(),
        });
    }

    gpus
}

/// Parse nvidia-smi process CSV output into GpuProcess structs.
///
/// Expected input format (one line per process, comma-separated):
///   pid, process_name, used_memory [MiB]
///
/// Lines with fewer than 3 fields are skipped. Memory values are
/// converted from MiB to bytes.
fn parse_nvidia_smi_process_csv(output: &str) -> Vec<GpuProcess> {
    let mut processes = Vec::new();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if parts.len() < 3 {
            continue;
        }

        let pid: u32 = parts[0].parse().unwrap_or(0);
        let name = parts[1].to_owned();
        let mem_used_mib: u64 = parts[2].parse().unwrap_or(0);

        processes.push(GpuProcess {
            pid,
            name,
            mem_used: mem_used_mib * 1024 * 1024,
        });
    }

    processes
}

/// Collect processes running on a specific GPU.
async fn collect_gpu_processes(gpu_index: u32) -> Option<Vec<GpuProcess>> {
    let output = Command::new("nvidia-smi")
        .args([
            &format!("--id={gpu_index}"),
            "--query-compute-apps=pid,process_name,used_memory",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Some(parse_nvidia_smi_process_csv(&stdout))
}

// ── Helpers ──────────────────────────────────────────────────────

/// Run a command and return its trimmed stdout, or `None` on failure.
async fn run_cmd(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().await.ok()?;

    if !output.status.success() {
        debug!(program, ?args, status = %output.status, "command failed");
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Some(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_proc_meminfo_typical() {
        let content = "\
MemTotal:       16384000 kB
MemFree:         2048000 kB
MemAvailable:    8192000 kB
Buffers:          512000 kB
Cached:          4096000 kB
SwapCached:            0 kB
SReclaimable:     256000 kB
";
        let info = parse_proc_meminfo(content).unwrap();
        assert_eq!(info.total, 16384000 * 1024);
        assert_eq!(info.free, 2048000 * 1024);
        // cached = Cached + Buffers + SReclaimable
        assert_eq!(info.cached, (4096000 + 512000 + 256000) * 1024);
        // used = total - free - cached = 16384000 - 2048000 - 4864000 = 9472000
        // percent = 9472000 * 100 / 16384000 = 57
        assert_eq!(info.used_percent, 57);
    }

    #[test]
    fn parse_df_linux_typical() {
        let output = "\
Mounted on      Size         Avail
/               500000000000 200000000000
/boot           1073741824   536870912
/dev/shm        8589934592   8589934592
";
        let disks = parse_df_linux(output).unwrap();
        assert_eq!(disks.len(), 2); // /dev/shm filtered out
        assert_eq!(disks[0].path, "/");
        assert_eq!(disks[0].total, 500000000000);
        assert_eq!(disks[0].free, 200000000000);
        assert_eq!(disks[0].used_percent, 60);
    }

    #[test]
    fn parse_df_bsd_typical() {
        let output = "\
Filesystem   1024-blocks      Used Available Capacity  Mounted on
/dev/disk1s1  976562500 488281250 488281250    50%    /
devfs               200       200         0   100%    /dev
";
        let disks = parse_df_bsd(output).unwrap();
        assert_eq!(disks.len(), 1); // devfs filtered out
        assert_eq!(disks[0].path, "/");
        assert_eq!(disks[0].total, 976562500 * 1024);
        assert_eq!(disks[0].free, 488281250 * 1024);
        assert_eq!(disks[0].used_percent, 50);
    }

    #[test]
    fn extract_vm_stat_value_parses_correctly() {
        assert_eq!(extract_vm_stat_value("Pages free:    123456."), 123456);
        assert_eq!(extract_vm_stat_value("Pages inactive:  789."), 789);
        assert_eq!(extract_vm_stat_value("Pages purgeable:  0."), 0);
    }

    #[test]
    fn parse_proc_meminfo_empty() {
        let info = parse_proc_meminfo("").unwrap();
        assert_eq!(info.total, 0);
        assert_eq!(info.free, 0);
        assert_eq!(info.cached, 0);
        assert_eq!(info.used_percent, 0);
    }

    #[test]
    fn parse_df_linux_empty_returns_none() {
        let output = "Mounted on      Size         Avail\n";
        assert!(parse_df_linux(output).is_none());
    }

    #[test]
    fn parse_df_bsd_no_physical_returns_none() {
        let output = "\
Filesystem   1024-blocks Used Available Capacity Mounted on
devfs              200  200       0   100%    /dev
";
        assert!(parse_df_bsd(output).is_none());
    }

    // ── GPU parsing tests ────────────────────────────────────────

    #[test]
    fn parse_gpu_csv_single_gpu() {
        let csv = "0, NVIDIA A100-SXM4-80GB, 87, 45000, 81920, 72\n";
        let gpus = parse_nvidia_smi_gpu_csv(csv);

        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].index, 0);
        assert_eq!(gpus[0].name, "NVIDIA A100-SXM4-80GB");
        assert_eq!(gpus[0].utilization, 87);
        assert_eq!(gpus[0].memory_used, 45000 * 1024 * 1024);
        assert_eq!(gpus[0].memory_total, 81920 * 1024 * 1024);
        assert_eq!(gpus[0].temperature, 72);
        assert!(gpus[0].processes.is_empty());
    }

    #[test]
    fn parse_gpu_csv_multiple_gpus() {
        let csv = "\
0, NVIDIA A100-SXM4-80GB, 87, 45000, 81920, 72
1, NVIDIA A100-SXM4-80GB, 92, 60000, 81920, 75
2, NVIDIA A100-SXM4-80GB, 15, 2048, 81920, 45
3, NVIDIA A100-SXM4-80GB, 0, 512, 81920, 38
";
        let gpus = parse_nvidia_smi_gpu_csv(csv);

        assert_eq!(gpus.len(), 4);
        assert_eq!(gpus[0].index, 0);
        assert_eq!(gpus[1].index, 1);
        assert_eq!(gpus[2].index, 2);
        assert_eq!(gpus[3].index, 3);
        assert_eq!(gpus[1].utilization, 92);
        assert_eq!(gpus[1].temperature, 75);
        assert_eq!(gpus[3].utilization, 0);
    }

    #[test]
    fn parse_gpu_csv_empty_output() {
        let gpus = parse_nvidia_smi_gpu_csv("");
        assert!(gpus.is_empty());
    }

    #[test]
    fn parse_gpu_csv_whitespace_only() {
        let gpus = parse_nvidia_smi_gpu_csv("   \n  \n");
        assert!(gpus.is_empty());
    }

    #[test]
    fn parse_gpu_csv_malformed_line_skipped() {
        // Line with too few fields is skipped
        let csv = "\
0, NVIDIA A100, 87, 45000, 81920, 72
bad line
1, NVIDIA V100, 50
2, NVIDIA A100, 30, 10000, 81920, 55
";
        let gpus = parse_nvidia_smi_gpu_csv(csv);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].index, 0);
        assert_eq!(gpus[1].index, 2);
    }

    #[test]
    fn parse_gpu_csv_non_numeric_fields_default_to_zero() {
        let csv = "abc, NVIDIA RTX 4090, xyz, 12000, 24576, def\n";
        let gpus = parse_nvidia_smi_gpu_csv(csv);

        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].index, 0); // "abc" -> 0
        assert_eq!(gpus[0].name, "NVIDIA RTX 4090");
        assert_eq!(gpus[0].utilization, 0); // "xyz" -> 0
        assert_eq!(gpus[0].memory_used, 12000 * 1024 * 1024);
        assert_eq!(gpus[0].temperature, 0); // "def" -> 0
    }

    #[test]
    fn parse_gpu_csv_memory_conversion_mib_to_bytes() {
        // 1 MiB = 1024 * 1024 bytes
        let csv = "0, GPU, 50, 1, 1, 60\n";
        let gpus = parse_nvidia_smi_gpu_csv(csv);

        assert_eq!(gpus[0].memory_used, 1_048_576); // 1 MiB
        assert_eq!(gpus[0].memory_total, 1_048_576); // 1 MiB
    }

    #[test]
    fn parse_gpu_csv_mixed_gpu_types() {
        let csv = "\
0, NVIDIA GeForce RTX 4090, 45, 8192, 24576, 65
1, NVIDIA Tesla T4, 100, 15360, 15360, 82
";
        let gpus = parse_nvidia_smi_gpu_csv(csv);

        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].name, "NVIDIA GeForce RTX 4090");
        assert_eq!(gpus[1].name, "NVIDIA Tesla T4");
        assert_eq!(gpus[1].utilization, 100);
        assert_eq!(gpus[1].memory_used, gpus[1].memory_total);
    }

    #[test]
    fn parse_process_csv_typical() {
        let csv = "\
1234, python3, 28000
5678, jupyter-lab, 4096
9012, tensorboard, 512
";
        let procs = parse_nvidia_smi_process_csv(csv);

        assert_eq!(procs.len(), 3);
        assert_eq!(procs[0].pid, 1234);
        assert_eq!(procs[0].name, "python3");
        assert_eq!(procs[0].mem_used, 28000 * 1024 * 1024);
        assert_eq!(procs[1].pid, 5678);
        assert_eq!(procs[1].name, "jupyter-lab");
        assert_eq!(procs[2].pid, 9012);
        assert_eq!(procs[2].name, "tensorboard");
    }

    #[test]
    fn parse_process_csv_empty_no_processes() {
        let procs = parse_nvidia_smi_process_csv("");
        assert!(procs.is_empty());
    }

    #[test]
    fn parse_process_csv_whitespace_only() {
        let procs = parse_nvidia_smi_process_csv("  \n  \n");
        assert!(procs.is_empty());
    }

    #[test]
    fn parse_process_csv_malformed_lines_skipped() {
        let csv = "\
1234, python3, 28000
bad
5678
9012, tensorboard, 512
";
        let procs = parse_nvidia_smi_process_csv(csv);
        assert_eq!(procs.len(), 2);
        assert_eq!(procs[0].pid, 1234);
        assert_eq!(procs[1].pid, 9012);
    }

    #[test]
    fn parse_process_csv_non_numeric_pid_defaults_to_zero() {
        let csv = "abc, python3, 1024\n";
        let procs = parse_nvidia_smi_process_csv(csv);

        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].pid, 0);
        assert_eq!(procs[0].name, "python3");
        assert_eq!(procs[0].mem_used, 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_process_csv_memory_conversion() {
        let csv = "100, train.py, 40960\n";
        let procs = parse_nvidia_smi_process_csv(csv);

        // 40960 MiB = 40960 * 1024 * 1024 bytes
        assert_eq!(procs[0].mem_used, 40960 * 1024 * 1024);
    }
}
