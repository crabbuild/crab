use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Serve Crab repositories and their React web application")]
struct Arguments {
    #[arg(long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> crab_http_server::Result<()> {
    let arguments = Arguments::parse();
    let config = crab_http_server::Config::read(&arguments.config)?;
    crab_http_server::serve(config).await
}
