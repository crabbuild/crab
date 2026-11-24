use std::{sync::Arc, time::Duration};

use crab_metadata::capsule_protocol::{
    CapsuleTransactionRecord, CapsuleTransactionStatus, MAX_CAPSULE_TRANSACTION_RECORD_BYTES,
    read_capsule_plan_intent,
};

use crate::{
    app::{self, Error},
    auth::Principal,
    cells::repository::collaboration::links::branches::creation::{
        Decision, Due, Get, Request, Reserve, Resolution, Settle, Status,
    },
    git::receive::{self, IssueBranchPublication, ReceiveError},
    server::Server,
};

use super::Repository;

type Result<T> = std::result::Result<T, ReceiveError>;

pub(crate) async fn authorize(
    server: &Server,
    target: &Repository,
    principal: &Principal,
    publication: &IssueBranchPublication,
) -> Result<()> {
    let source = &publication.source;
    let request = &publication.request;
    let origin = principal
        .related_read(target.id, source.id)
        .ok_or(ReceiveError::NotFound)?;
    if !app::membership(server, &origin, source)
        .await?
        .principal
        .can_write(source.id)
        || app::account(&origin)? != request.actor
    {
        return Err(ReceiveError::Forbidden);
    }
    if super::settings::load_lifecycle(server, source)
        .await?
        .archived
    {
        return Err(ReceiveError::Archived);
    }
    let cell = app::repository_cell(server, source)?;
    let job = app::query_output(
        cell.client
            .query::<Get>(cell.target, None, request.id)
            .await,
    )?;
    if job.is_none_or(|job| job.status != Status::Pending || job.request != *request) {
        return Err(ReceiveError::BranchChanged);
    }
    Ok(())
}

async fn settle(
    server: &Server,
    publication: &IssueBranchPublication,
    resolution: Resolution,
) -> Result<Status> {
    let cell = app::repository_cell(server, &publication.source)?;
    let status = app::command_output(
        cell.client
            .command::<Settle>(
                cell.target,
                app::mutation_identity()?,
                (publication.request.id, resolution),
            )
            .await,
    )?;
    status.ok_or_else(|| Error::CellContract("Development branch request disappeared").into())
}

async fn finish_committed(
    server: &Server,
    target: &Repository,
    publication: &IssueBranchPublication,
) -> Result<Option<Status>> {
    if !receive::issue_branch_committed(target, publication).await? {
        return Ok(None);
    }
    Ok(Some(
        settle(server, publication, Resolution::Committed).await?,
    ))
}

async fn abandon(
    server: &Server,
    target: &Repository,
    publication: &IssueBranchPublication,
) -> Result<Status> {
    let plan = publication.request.plan(publication.source.id);
    // A live publisher owns the same plan lease. Close the native reservation
    // while holding it, or resolve the terminal Git evidence if it was attempted.
    let outcome = crab_remote::publication::with_capsule_plan(
        &target.store,
        &target.layout,
        &plan,
        Duration::from_secs(300),
        &server.cancellation,
        |cancel| async move {
            if cancel.is_cancelled() {
                return Err(ReceiveError::Cancelled);
            }
            settle(server, publication, Resolution::Abandoned).await
        },
    )
    .await;
    match outcome {
        Err(ReceiveError::Metadata(
            crab_metadata::error::MetadataError::PlanAlreadyAttempted { .. },
        )) => {
            if let Some(status) = finish_committed(server, target, publication).await? {
                return Ok(status);
            }
            let Some(intent) =
                read_capsule_plan_intent(&target.store, &target.layout, &plan).await?
            else {
                return Ok(Status::Pending);
            };
            let path = target
                .layout
                .capsule_transaction_path(intent.activation_id());
            let bytes = match target
                .store
                .get_with_etag_bounded(&path, MAX_CAPSULE_TRANSACTION_RECORD_BYTES)
                .await
            {
                Ok((bytes, _)) => bytes,
                Err(crab_storage::StorageError::NotFound { .. }) => return Ok(Status::Pending),
                Err(error) => return Err(error.into()),
            };
            let record = CapsuleTransactionRecord::decode(&bytes)?;
            if record.activation_id() != intent.activation_id()
                || record.transaction_id() != intent.transaction().id()?
            {
                return Err(Error::CellContract(
                    "Branch creation coordinator has another identity",
                )
                .into());
            }
            // An attempted plan stays pending until its exact transaction is
            // terminal. An invisible commit marker alone cannot prove failure.
            if record.status() != CapsuleTransactionStatus::Aborted {
                return Ok(Status::Pending);
            }
            settle(server, publication, Resolution::Abandoned).await
        }
        outcome => outcome,
    }
}

pub(crate) async fn submit(
    server: Arc<Server>,
    principal: Principal,
    source: Arc<Repository>,
    target: Arc<Repository>,
    request: Request,
) -> Result<Status> {
    let cell = app::repository_cell(&server, &source)?;
    let outcome = app::command_output(
        cell.client
            .command::<Reserve>(cell.target, app::mutation_identity()?, request.clone())
            .await,
    )?;
    match outcome {
        Decision::Saved(Status::Pending) => {}
        Decision::Saved(status) => return Ok(status),
        Decision::Conflict => return Err(Error::Conflict.into()),
        Decision::Missing => return Err(ReceiveError::NotFound),
        Decision::Archived => return Err(ReceiveError::Archived),
        Decision::Limit => {
            return Err(Error::Invalid("An issue can link up to 10 development branches").into());
        }
    }
    let publication = IssueBranchPublication { source, request };
    if let Some(status) = finish_committed(&server, &target, &publication).await? {
        return Ok(status);
    }
    if target
        .ref_oid(&publication.request.branch.reference)
        .await?
        .is_some()
    {
        return match abandon(&server, &target, &publication).await? {
            Status::Created => Ok(Status::Created),
            _ => Err(crab_git::receive_plan::ReceivePlanError::Stale {
                name: publication.request.branch.reference.clone(),
            }
            .into()),
        };
    }
    // The receive worker survives a dropped HTTP reply. The issue reservation
    // remains on disk so a peer can finish the link after that worker commits.
    let result = receive::create_issue_branch(
        Arc::clone(&server),
        principal,
        &target,
        IssueBranchPublication {
            source: Arc::clone(&publication.source),
            request: publication.request.clone(),
        },
    )
    .await;
    if let Some(status) = finish_committed(&server, &target, &publication).await? {
        return Ok(status);
    }
    match result {
        Ok(()) => Err(Error::CellContract("Branch creation has no publication receipt").into()),
        Err(
            error @ (ReceiveError::Forbidden
            | ReceiveError::NotFound
            | ReceiveError::Archived
            | ReceiveError::Protected
            | ReceiveError::RefRule(_)
            | ReceiveError::BranchChanged
            | ReceiveError::Graph(
                crab_git::receive_plan::ReceivePlanError::Stale { .. }
                | crab_git::receive_plan::ReceivePlanError::Namespace(_)
                | crab_git::receive_plan::ReceivePlanError::Missing { .. }
                | crab_git::receive_plan::ReceivePlanError::Kind { .. },
            )
            | ReceiveError::Write(
                crab_write::WriteError::RefChanged { .. } | crab_write::WriteError::Namespace(_),
            )
            | ReceiveError::Metadata(
                crab_metadata::error::MetadataError::PlanAlreadyAttempted { .. },
            )),
        ) => {
            if abandon(&server, &target, &publication).await? == Status::Created {
                return Ok(Status::Created);
            }
            Err(error)
        }
        Err(error) => Err(error),
    }
}

pub(crate) async fn recover(server: &Server, source: &Arc<Repository>) -> Result<()> {
    let cell = app::repository_cell(server, source)?;
    let due = app::query_output(cell.client.query::<Due>(cell.target, None, ()).await)?;
    for job in due {
        if server.cancellation.is_cancelled() {
            break;
        }
        let publication = IssueBranchPublication {
            source: Arc::clone(source),
            request: job.request,
        };
        // Back off before remote reads so one unavailable target cannot starve
        // the other branches waiting in this Repository Cell.
        if settle(server, &publication, Resolution::Defer).await? != Status::Pending {
            continue;
        }
        let Some(target) = server
            .repositories
            .by_id(publication.request.branch.repository)
        else {
            continue;
        };
        let result = if job.expired {
            abandon(server, &target, &publication).await.map(Some)
        } else {
            finish_committed(server, &target, &publication).await
        };
        if let Err(error) = result {
            tracing::warn!(repository = %source.id, %error, "development branch link recovery will retry");
        }
    }
    Ok(())
}
