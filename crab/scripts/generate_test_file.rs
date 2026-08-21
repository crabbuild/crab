//! Generate a file of pseudo-random incompressible bytes for benchmarking
//! `crab add` / `git push` at scale.
//!
//! Designed as a dev tool, not a user-facing command. Key properties:
//!
//! - **Incompressible** — bytes come from a `ChaCha`-based stream so the
//!   output defeats lz4/zstd (compression ratio ≈ 1.0), mirroring real
//!   binary payloads like DMGs and tarballs.
//! - **Deterministic** — same `--seed` produces byte-identical output, so
//!   a CDC benchmark can re-run against the same chunking profile.
//! - **Streaming** — writes `BLOCK_BYTES` at a time, never buffers the
//!   whole file. 50 GiB of random bytes on a PCIe 4 SSD lands in ~45 s
//!   with a modest single-thread PRNG.
//! - **Free-space preflight** — refuses to start if the target filesystem
//!   has less than `size + 1 GiB` available, so a 50 GiB run doesn't
//!   silently fill the partition.
//!
//! ## Usage
//!
//! ```text
//! cargo run --release --bin generate_test_file -- --size 50G /path/to/out.bin
//! cargo run --release --bin generate_test_file -- --size 1500M --seed 42 /tmp/a.bin
//! ```
//!
//! Sizes accept `K`, `M`, `G`, `T` suffixes (powers of 1024, case-insensitive)
//! or a raw byte count.

use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use rand::SeedableRng;
use rand::rngs::StdRng;

/// Write buffer size — large enough to amortize syscall overhead without
/// starving the page cache. 8 MiB is the sweet spot on macOS APFS and
/// most Linux ext4 setups.
const BLOCK_BYTES: usize = 8 * 1024 * 1024;

/// Headroom we require beyond the requested size so the user's FS has
/// breathing room for logs, temp files, etc. 1 GiB is overkill at small
/// sizes and still cheap at 50 GiB.
const FREE_SPACE_HEADROOM: u64 = 1 << 30;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("generate_test_file: {msg}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse()?;

    let target_bytes = args.size;
    let out_path = &args.path;

    check_free_space(out_path, target_bytes)?;

    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(out_path)
        .map_err(|e| format!("open {}: {e}", out_path.display()))?;

    let mut writer = BufWriter::with_capacity(BLOCK_BYTES, file);
    let mut rng = StdRng::seed_from_u64(args.seed);
    let mut buf = vec![0u8; BLOCK_BYTES];

    let start = Instant::now();
    let mut written: u64 = 0;
    let mut last_report = Instant::now();

    while written < target_bytes {
        let remaining = target_bytes - written;
        let n = remaining.min(BLOCK_BYTES as u64) as usize;

        // Fill the block with fresh random bytes. `StdRng` backs this
        // with the ChaCha12 stream cipher, which is ~3 GB/s per core on
        // modern x86 and aarch64 — fast enough that disk I/O dominates
        // on an NVMe target.
        rand::RngCore::fill_bytes(&mut rng, &mut buf[..n]);
        writer
            .write_all(&buf[..n])
            .map_err(|e| format!("write: {e}"))?;
        written += n as u64;

        // Rate-limit progress lines to one per second so CI logs stay
        // readable. Always print the last one at completion.
        if last_report.elapsed().as_secs() >= 1 {
            report_progress(written, target_bytes, start);
            last_report = Instant::now();
        }
    }

    writer.flush().map_err(|e| format!("flush: {e}"))?;
    // Fsync via the inner File's sync_all so the file is durable before
    // the calling script starts the benchmark.
    writer
        .into_inner()
        .map_err(|e| format!("flush into_inner: {e}"))?
        .sync_all()
        .map_err(|e| format!("fsync: {e}"))?;

    // Final line overwrites the most recent in-progress line.
    report_progress(target_bytes, target_bytes, start);
    eprintln!();

    let elapsed = start.elapsed();
    let mib_per_s = (target_bytes as f64 / 1_048_576.0) / elapsed.as_secs_f64();
    eprintln!(
        "wrote {} ({} bytes) to {} in {:.2}s ({:.1} MiB/s, seed={})",
        format_size(target_bytes),
        target_bytes,
        out_path.display(),
        elapsed.as_secs_f64(),
        mib_per_s,
        args.seed,
    );

    Ok(())
}

struct Args {
    path: PathBuf,
    size: u64,
    seed: u64,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut raw: Vec<String> = std::env::args().skip(1).collect();

        let mut size: Option<u64> = None;
        let mut seed: u64 = 0x9E3779B9_7F4A7C15;
        let mut path: Option<PathBuf> = None;

        let mut i = 0;
        while i < raw.len() {
            match raw[i].as_str() {
                "--help" | "-h" => {
                    print_usage();
                    std::process::exit(0);
                }
                "--size" | "-s" => {
                    let v = raw
                        .get(i + 1)
                        .ok_or_else(|| "--size requires an argument".to_owned())?;
                    size = Some(parse_size(v)?);
                    i += 2;
                }
                "--seed" => {
                    let v = raw
                        .get(i + 1)
                        .ok_or_else(|| "--seed requires an argument".to_owned())?;
                    seed = v
                        .parse::<u64>()
                        .map_err(|e| format!("invalid --seed {v:?}: {e}"))?;
                    i += 2;
                }
                other if other.starts_with("--") => {
                    return Err(format!("unknown flag {other:?}"));
                }
                _ => {
                    if path.is_some() {
                        return Err(format!("unexpected positional arg {:?}", raw[i]));
                    }
                    path = Some(PathBuf::from(std::mem::take(&mut raw[i])));
                    i += 1;
                }
            }
        }

        let size = size.ok_or_else(|| "missing --size (e.g. --size 50G)".to_owned())?;
        let path = path.ok_or_else(|| "missing output path".to_owned())?;
        if size == 0 {
            return Err("--size must be non-zero".to_owned());
        }
        Ok(Self { path, size, seed })
    }
}

fn print_usage() {
    eprintln!(
        "usage: generate_test_file --size <N[K|M|G|T]> [--seed <u64>] <path>\n\
         \n\
         Generate a pseudo-random incompressible file for crab benchmarks.\n\
         \n\
         Examples:\n\
           generate_test_file --size 50G /tmp/bench-50g.bin\n\
           generate_test_file --size 1500M --seed 42 /tmp/bench-1.5g.bin"
    );
}

/// Parse a size string like `50G`, `1500M`, `1048576`. Powers of 1024.
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num_part, mult): (&str, u64) = match s.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&s[..s.len() - 1], 1u64 << 10),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 1u64 << 20),
        Some(b'g' | b'G') => (&s[..s.len() - 1], 1u64 << 30),
        Some(b't' | b'T') => (&s[..s.len() - 1], 1u64 << 40),
        _ => (s, 1),
    };
    let base: u64 = num_part
        .parse()
        .map_err(|e| format!("invalid size {s:?}: {e}"))?;
    base.checked_mul(mult)
        .ok_or_else(|| format!("size {s:?} overflows u64"))
}

fn format_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("TiB", 1u64 << 40),
        ("GiB", 1u64 << 30),
        ("MiB", 1u64 << 20),
        ("KiB", 1u64 << 10),
    ];
    for (unit, div) in UNITS {
        if bytes >= div {
            return format!("{:.2} {unit}", bytes as f64 / div as f64);
        }
    }
    format!("{bytes} B")
}

fn report_progress(written: u64, total: u64, start: Instant) {
    let pct = (written as f64 / total as f64) * 100.0;
    let elapsed = start.elapsed().as_secs_f64();
    let mib_per_s = if elapsed > 0.0 {
        (written as f64 / 1_048_576.0) / elapsed
    } else {
        0.0
    };
    eprint!(
        "\rwriting: {}/{} ({:.1}%)  {:.1} MiB/s  elapsed {:.1}s\x1b[K",
        format_size(written),
        format_size(total),
        pct,
        mib_per_s,
        elapsed,
    );
}

/// Reject requests that would overflow the target filesystem. `statvfs`
/// on macOS / Linux; fall back to allowing the write if the probe fails.
fn check_free_space(path: &Path, needed: u64) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };

    let avail = match free_bytes(parent) {
        Some(n) => n,
        None => {
            eprintln!(
                "generate_test_file: warning: couldn't probe free space for {}, proceeding",
                parent.display()
            );
            return Ok(());
        }
    };

    let required = needed.saturating_add(FREE_SPACE_HEADROOM);
    if avail < required {
        return Err(format!(
            "only {} available at {}, need {} (size {} + {} headroom)",
            format_size(avail),
            parent.display(),
            format_size(required),
            format_size(needed),
            format_size(FREE_SPACE_HEADROOM),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` is a standard POSIX call; zeroed struct and a
    // nul-terminated path satisfy its contract. We only read `f_bavail`
    // and `f_frsize` from the returned struct.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    Some(u64::from(stat.f_bavail) * stat.f_frsize)
}

#[cfg(not(unix))]
fn free_bytes(_path: &Path) -> Option<u64> {
    None
}
