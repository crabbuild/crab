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
    }
}
