mod cli;

// Cellar's pressure accounting and reclamation use jemalloc; without this,
// those controls observe a different heap from the application's Rust data.
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[tokio::main]
async fn main() -> crab_server::Result<()> {
    cli::run().await
}

#[cfg(test)]
mod tests {
    #[test]
    fn memory_pressure_tracks_application_allocations() {
        // SDK statistics cover the process. Parallel CLI fixtures can free
        // memory between snapshots, so this probe needs its own process.
        let Some(receipt) = std::env::var_os("CRAB_ALLOCATOR_PROBE_RECEIPT") else {
            let directory = tempfile::tempdir().unwrap();
            let receipt = directory.path().join("allocation");
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::memory_pressure_tracks_application_allocations",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("CRAB_ALLOCATOR_PROBE_RECEIPT", &receipt)
                .output()
                .unwrap();
            assert!(
                output.status.success() && receipt.is_file(),
                "isolated allocator probe failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        };
        let before = cellar::memory::allocator_stats().unwrap().allocated_bytes;
        let buffer = std::hint::black_box(vec![0_u8; 2 * 1024 * 1024]);
        let after = cellar::memory::allocator_stats().unwrap().allocated_bytes;
        std::hint::black_box(&buffer);
        let allocated = after.saturating_sub(before);
        assert!(
            allocated >= buffer.len() as u64,
            "SDK observed {allocated} bytes of a {}-byte allocation",
            buffer.len()
        );
        std::fs::write(receipt, b"tracked").unwrap();
    }
}
