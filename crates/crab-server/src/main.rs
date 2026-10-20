mod cli;

#[tokio::main]
async fn main() -> crab_server::Result<()> {
    cli::run().await
}
