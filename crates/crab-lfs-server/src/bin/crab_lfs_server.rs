//! Standalone Git LFS HTTP gateway binary.

use std::path::PathBuf;

use clap::Parser;

use crab_lfs_server::{LfsServerConfig, run_server};

#[derive(Debug, Parser)]
#[command(
    name = "crab-lfs-server",
    version,
    about = "Serve the standard Git LFS HTTP API"
)]
struct Args {
    /// TOML gateway configuration.
    #[arg(long)]
    config: PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let config = LfsServerConfig::from_file(&args.config)?;
    tracing_subscriber::fmt::init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_server(config))?;
    Ok(())
}
