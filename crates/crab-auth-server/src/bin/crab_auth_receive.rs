use clap::{Parser, Subcommand};
use crab_auth::{PushFinalizeResponse, PushRefUpdate};
use crab_auth_server::doctor::git_version;
use serde::Serialize;

use crab_auth_server::error::{AuthServerError, Result};
use crab_auth_server::output::{HelperOutputPolicy, emit_json_result};
use crab_auth_server::receive::{ReceiveContext, commit_receive, prepare_receive, verify_receive};

#[derive(Parser)]
#[command(name = "crab-auth-receive", version)]
struct Args {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Subcommand)]
enum CommandKind {
    Doctor,
    Prepare {
        #[arg(long)]
        repo_url: String,
        #[arg(long)]
        push_id: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        ref_updates_json: String,
        #[arg(long)]
        view_scope_json: Option<String>,
    },
    Verify {
        #[arg(long)]
        repo_url: String,
        #[arg(long)]
        push_id: String,
        #[arg(long)]
        provider: String,
    },
    Commit {
        #[arg(long)]
        repo_url: String,
        #[arg(long)]
        push_id: String,
        #[arg(long)]
        plan_digest: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        active_active_json: Option<String>,
    },
}

#[derive(Serialize)]
struct PrepareOutput {
    status: String,
    source_generation: Option<u64>,
}

#[derive(Serialize)]
struct VerifyOutput {
    ref_updates: Vec<PushRefUpdate>,
    verified_changed_paths: Vec<String>,
    plan_digest: String,
}

#[derive(Serialize)]
struct DoctorOutput {
    status: String,
    git_version: String,
}

#[derive(Serialize)]
#[serde(untagged)]
enum HelperOutput {
    Doctor(DoctorOutput),
    Prepare(PrepareOutput),
    Verify(VerifyOutput),
    Finalize(PushFinalizeResponse),
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    std::process::exit(emit_json_result(HelperOutputPolicy::Receive, run().await));
}

async fn run() -> Result<HelperOutput> {
    let args = Args::parse();
    Ok(match args.command {
        CommandKind::Doctor => HelperOutput::Doctor(DoctorOutput {
            status: "ok".to_owned(),
            git_version: git_version()?,
        }),
        CommandKind::Prepare {
            repo_url,
            push_id,
            provider,
            ref_updates_json,
            view_scope_json,
        } => {
            let ctx = ReceiveContext::open(&repo_url, &push_id, &provider)?;
            let ref_updates: Vec<PushRefUpdate> = serde_json::from_str(&ref_updates_json)
                .map_err(|e| invalid(format!("invalid prepare ref_updates JSON: {e}")))?;
            let view_scope = match view_scope_json {
                Some(json) => Some(
                    serde_json::from_str(&json)
                        .map_err(|e| invalid(format!("invalid prepare view_scope JSON: {e}")))?,
                ),
                None => None,
            };
            let prepared = prepare_receive(&ctx, ref_updates, view_scope).await?;
            HelperOutput::Prepare(PrepareOutput {
                status: "prepared".to_owned(),
                source_generation: prepared.source_generation,
            })
        }
        CommandKind::Verify {
            repo_url,
            push_id,
            provider,
        } => {
            let ctx = ReceiveContext::open(&repo_url, &push_id, &provider)?;
            let verified = verify_receive(&ctx).await?;
            warn_cleanup(
                ctx.cleanup_expired_staging().await,
                "expired staging cleanup failed",
            );
            HelperOutput::Verify(VerifyOutput {
                ref_updates: verified.ref_updates,
                verified_changed_paths: verified.verified_changed_paths,
                plan_digest: verified.plan_digest,
            })
        }
        CommandKind::Commit {
            repo_url,
            push_id,
            plan_digest,
            provider,
            active_active_json,
        } => {
            let ctx = ReceiveContext::open(&repo_url, &push_id, &provider)?;
            let response =
                commit_receive(&ctx, &repo_url, &plan_digest, active_active_json.as_deref())
                    .await?;
            warn_cleanup(ctx.cleanup_staging().await, "staging cleanup failed");
            warn_cleanup(
                ctx.cleanup_prepare_record().await,
                "prepare record cleanup failed",
            );
            warn_cleanup(
                ctx.cleanup_expired_staging().await,
                "expired staging cleanup failed",
            );
            HelperOutput::Finalize(response)
        }
    })
}

fn warn_cleanup<T>(result: Result<T>, message: &str) {
    if let Err(err) = result {
        eprintln!("warning: {message}: {err}");
    }
}

fn invalid(message: impl Into<String>) -> AuthServerError {
    AuthServerError::Configuration {
        key: message.into(),
        origin: "crab-auth-receive".to_owned(),
    }
}
