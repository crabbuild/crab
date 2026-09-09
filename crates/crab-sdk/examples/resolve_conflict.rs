use std::path::PathBuf;

use crab_sdk::{Client, DirectStoreOptions, IntegrationId, LocalTools, PullOutcome};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let [store, checkout, git, crab, integration, action] = args.as_slice() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "usage: resolve_conflict STORE CHECKOUT GIT CRAB INTEGRATION_ID continue|abort",
        )
        .into());
    };
    let store = PathBuf::from(store);
    let checkout = PathBuf::from(checkout);
    let tools = LocalTools::new(PathBuf::from(git), PathBuf::from(crab))?;
    let integration = integration.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "INTEGRATION_ID must be UTF-8",
        )
    })?;
    let integration = IntegrationId::from_string(integration)?;
    let client = Client::builder()
        .direct_store(DirectStoreOptions::filesystem(&store)?)
        .local_tools(tools)
        .build()?;
    let result = async {
        let repository = client.open_local(checkout).await?;
        match action.to_str() {
            Some("continue") => match repository.continue_integration(integration).await? {
                PullOutcome::Updated { head, .. } | PullOutcome::UpToDate { head, .. } => {
                    println!("integration completed at {head}");
                }
                PullOutcome::Conflict(conflict) => {
                    return Err(format!("conflicts remain in {:?}", conflict.paths()).into());
                }
                _ => return Err("SDK returned an unsupported pull outcome".into()),
            },
            Some("abort") => {
                let head = repository.abort_integration(integration).await?;
                println!("integration aborted at {head}");
            }
            _ => return Err("ACTION must be continue or abort".into()),
        }
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;
    let close = client.close().await;
    match (result, close) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), Err(close)) => Err(format!("{error}; client close failed: {close}").into()),
    }
}
